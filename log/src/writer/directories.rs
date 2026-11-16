//! The derived files on `kvik-maint` (`format.md` A.18): the log directory at each seal, the series directory on
//! the byte-bounded cadence of [`crate::directory::cadence`], and the status file on its interval. Nothing is
//! written naming a position above the last synced mark (`spec.md` §3 Durable mark): a sealed segment waits for the
//! mark sync before a directory lists it. A failed write is counted and tried again; it never stops the writer,
//! since every reader treats a missing file as absent.
//!
//! The series entries mean what the crate that registers the [`SealHook`] says; this module asks for them and lays
//! them out.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::directory::cadence::{Uncovered, series_due};
use crate::directory::logdir::{
    self, Catalogue, Group, GroupEntry, LostEntry, Root, SegmentRecord,
};
use crate::directory::series::{SeriesDirectory, SeriesEntry};
use crate::directory::status::StatusFile;
use crate::directory::{self, Family, Frame, read_latest};
use crate::fs::Fs;
use crate::manifest::read_manifest;
use crate::mark::ResetStamp;
use crate::store::{MANIFEST, read_all};

use super::Position;

/// How long a due write that could not finish waits before it is tried again.
pub const RETRY: Duration = Duration::from_millis(100);
/// How often the status is looked at for a change of health or pressure.
pub const STATUS_POLL: Duration = Duration::from_secs(1);
/// The default status interval (`format.md` A.18).
pub const STATUS_INTERVAL: Duration = Duration::from_secs(60);

/// The writer's health, reasons and counters for the status file ([`crate::directory::status`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusContent {
    /// The health level.
    pub health: u8,
    /// Whether the pressure flag is set (roadmap 9.0g).
    pub pressure: bool,
    /// Reason codes.
    pub reasons: Vec<u16>,
    /// Counters, in ascending id.
    pub counters: Vec<(u16, u64)>,
}

/// The seal hook: registered by the crate that gives series entries their meaning, called on `kvik-maint` after a
/// seal (and at the cadence's other times) for the series directory, and on the status interval.
pub trait SealHook: Send + Sync {
    /// The series directory's entries as of a position at or below `limit`, with that position; `written_at` is
    /// the write time the live candidates are judged against. `None` when they cannot be given yet.
    fn series(&self, limit: Position, written_at: i64) -> Option<(Position, Vec<SeriesEntry>)>;

    /// The status file's content now; `None`: no status file.
    fn status(&self) -> Option<StatusContent> {
        None
    }
}

/// What the derived files are written with ([`super::Log::start_with`]).
#[derive(Clone)]
pub struct DirectoryOptions {
    /// The seal hook; `None`: no series directory and no status file.
    pub hook: Option<Arc<dyn SealHook>>,
    /// The status interval; `None`: the status file is written on a change of health or pressure only.
    pub status_interval: Option<Duration>,
}

impl Default for DirectoryOptions {
    fn default() -> Self {
        DirectoryOptions {
            hook: None,
            status_interval: Some(STATUS_INTERVAL),
        }
    }
}

impl fmt::Debug for DirectoryOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirectoryOptions")
            .field("hook", &self.hook.is_some())
            .field("status_interval", &self.status_interval)
            .finish()
    }
}

/// What a start tells the directories.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectoryStart {
    /// The store generation.
    pub generation: u32,
    /// The mark's reset stamp after the start.
    pub stamp: Option<ResetStamp>,
    /// The last kept position before this start's recovery entry (its `after`), if it wrote or adopted one.
    pub reset_after: Option<Position>,
    /// `log.segment_size`.
    pub segment_size: u64,
}

impl DirectoryStart {
    /// Whether a derived file written in `generation` under `stamp`, complete through `through`, is still a function
    /// of the kept log (A.18 Validation, as the writer applies it): same generation, and the same stamp or a
    /// `through` at or below this start's reset.
    pub fn keeps(&self, generation: u32, stamp: Option<ResetStamp>, through: Position) -> bool {
        generation == self.generation
            && (stamp == self.stamp || self.reset_after.is_some_and(|k| through <= k))
    }
}

/// Counts of the derived file writes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirectoryStats {
    /// Files written.
    pub written: u64,
    /// Writes that failed and are tried again.
    pub failed: u64,
    /// Sealed segments described again from their files at the start (not reused from a group file).
    pub described: u64,
}

/// See the module documentation.
pub struct Directories {
    fs: Arc<dyn Fs>,
    dir: PathBuf,
    store_id: [u8; 16],
    start: DirectoryStart,
    opts: DirectoryOptions,
    opened: bool,
    next_id: BTreeMap<Family, u64>,
    refused: Vec<Family>,
    // The log directory.
    epoch: u64,
    manifest_end: u64,
    lost: Vec<LostEntry>,
    full: Vec<GroupEntry>,
    open: Group,
    open_written: Option<u32>,
    pending: Vec<SegmentRecord>,
    root_dirty: bool,
    // The series directory.
    series_through: Option<Position>,
    uncovered: Uncovered,
    series_at: Option<Duration>,
    // The status file.
    status_at: Option<Duration>,
    status_polled: Option<Duration>,
    status_last: Option<(u8, bool)>,
    stats: DirectoryStats,
}

impl fmt::Debug for Directories {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Directories")
            .field("dir", &self.dir)
            .field("opened", &self.opened)
            .field("segments", &self.segment_count())
            .field("pending", &self.pending.len())
            .field("series_through", &self.series_through)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl Directories {
    /// Directories of the store in `dir`; nothing is read until [`Directories::run`].
    pub fn new(
        fs: Arc<dyn Fs>,
        dir: &Path,
        store_id: [u8; 16],
        start: DirectoryStart,
        opts: DirectoryOptions,
    ) -> Self {
        Directories {
            fs,
            dir: dir.to_path_buf(),
            store_id,
            start,
            opts,
            opened: false,
            next_id: BTreeMap::new(),
            refused: Vec::new(),
            epoch: 0,
            manifest_end: 0,
            lost: Vec::new(),
            full: Vec::new(),
            open: Group {
                number: 0,
                segments: Vec::new(),
            },
            open_written: None,
            pending: Vec::new(),
            root_dirty: false,
            series_through: None,
            uncovered: Uncovered::default(),
            series_at: None,
            status_at: None,
            status_polled: None,
            status_last: None,
            stats: DirectoryStats::default(),
        }
    }

    /// Counts so far.
    pub fn stats(&self) -> DirectoryStats {
        self.stats
    }

    /// The series directory's `through` as last written or found valid.
    pub fn series_through(&self) -> Option<Position> {
        self.series_through
    }

    /// Sealed segments listed.
    pub fn segment_count(&self) -> u64 {
        self.full
            .iter()
            .map(|g| u64::from(g.segment_count))
            .sum::<u64>()
            + self.open.segments.len() as u64
    }

    /// A segment was sealed.
    pub fn sealed(&mut self, rec: SegmentRecord) {
        self.pending.push(rec);
    }

    /// Whether a sealed segment waits for the mark to be synced past it.
    pub fn waits_for(&self, synced: Position) -> bool {
        self.pending.iter().any(|r| r.last() > synced)
    }

    fn take_id(&mut self, family: Family) -> u64 {
        let id = self.next_id.entry(family).or_insert(1);
        let out = *id;
        *id += 1;
        out
    }

    fn write(&mut self, family: Family, encode: impl FnOnce(Frame) -> Vec<u8>, wall: i64) -> bool {
        if self.refused.contains(&family) {
            return false;
        }
        let file_id = self.take_id(family);
        let frame = Frame {
            generation: self.start.generation,
            file_id,
            written_at: wall,
        };
        let bytes = encode(frame);
        match directory::write_file(&*self.fs, &self.dir, family, file_id, &bytes) {
            Ok(()) => {
                self.stats.written += 1;
                true
            }
            Err(_) => {
                self.stats.failed += 1;
                false
            }
        }
    }

    /// Reads what the files already hold (A.18 Crash and rebuild): the newest valid generation of each, falling back
    /// to the previous; a group's records are reused wherever they still match the catalogue, and only the sealed
    /// segments no valid group lists are read again.
    fn open(&mut self, wall: i64) -> io::Result<()> {
        let fs = self.fs.clone();
        let fs = &*fs;
        directory::remove_temporaries(fs, &self.dir)?;
        for (family, id, _) in directory::list(fs, &self.dir)? {
            let next = self.next_id.entry(family).or_insert(1);
            *next = (*next).max(id + 1);
        }
        let manifest = read_all(fs, &self.dir.join(MANIFEST))
            .ok()
            .and_then(|b| read_manifest(&b, &self.store_id).ok());
        if let Some(m) = &manifest {
            let last = m.records.last();
            self.epoch = last.map_or(0, |r| r.epoch);
            self.manifest_end = last.map_or(0, |r| r.end);
            self.lost = m
                .lost_ranges()
                .map(|l| LostEntry {
                    first: Position {
                        generation: l.first.0,
                        sequence: l.first.1,
                    },
                    last: Position {
                        generation: l.last.0,
                        sequence: l.last.1,
                    },
                    file_id: l.file_id,
                    cause: l.cause as u8,
                })
                .collect();
        }
        let cat =
            logdir::catalogue_from_disk(fs, &self.dir, self.start.generation, manifest.as_ref())?;
        let loaded = match logdir::load(fs, &self.dir, &self.store_id, &cat) {
            Ok(d) => d,
            Err(_) => {
                self.refused.push(Family::Root);
                None
            }
        };
        let known = logdir::known_records(fs, &self.dir, &self.store_id, &cat);
        let log = self.dir.join(crate::store::LOG_DIR);
        let mut records = Vec::new();
        for (file_id, l) in logdir::ordered(&cat) {
            if let Some(r) = known.get(&file_id) {
                records.push(r.clone());
                continue;
            }
            let name = crate::name::SegmentName {
                generation: l.generation,
                file_id,
                sealed: true,
            };
            let id = crate::batch::SegmentIdentity {
                store_id: self.store_id,
                file_id,
                generation: l.generation,
            };
            let Ok(bytes) = read_all(fs, &log.join(name.to_string())) else {
                continue;
            };
            if let Some(r) = SegmentRecord::of_segment(&bytes, &id) {
                self.stats.described += 1;
                records.push(r);
            }
        }
        let uncovered: Vec<(Position, u64)> = records.iter().map(|r| (r.last(), r.len)).collect();
        let mut groups = logdir::group(records);
        self.open = groups.pop().unwrap_or(Group {
            number: 0,
            segments: Vec::new(),
        });
        if self.open.segments.len() == logdir::GROUP_SEGMENTS {
            let number = self.open.number + 1;
            let full = std::mem::replace(
                &mut self.open,
                Group {
                    number,
                    segments: Vec::new(),
                },
            );
            groups.push(full);
        }
        let named: BTreeMap<u32, u32> = loaded
            .iter()
            .flat_map(|d| d.root.groups.iter().zip(&d.groups))
            .filter(|(_, g)| g.is_some())
            .map(|(e, _)| (e.number, e.body_crc32c))
            .collect();
        self.full = Vec::with_capacity(groups.len());
        for g in groups {
            let e = g.entry();
            if named.get(&g.number) != Some(&e.body_crc32c) {
                let store_id = self.store_id;
                if !self.write(
                    Family::Group(g.number),
                    |f| encode_group(&g, f, store_id),
                    wall,
                ) {
                    return Err(io::Error::other("group write"));
                }
            }
            self.full.push(e);
        }
        let open_crc = self.open.entry().body_crc32c;
        self.open_written = named
            .get(&self.open.number)
            .filter(|c| **c == open_crc)
            .map(|_| self.open.segments.len() as u32);
        let expected = self.root();
        self.root_dirty = loaded.is_none_or(|d| d.root != expected) || self.open_written.is_none();
        // The series directory, as the writer judges it (A.18 Validation).
        let start = self.start;
        let series = read_latest(fs, &self.dir, Family::Series, |b| {
            let (f, d) = SeriesDirectory::decode(b, &self.store_id)?;
            if start.keeps(f.generation, d.stamp, d.through) {
                Ok((f, d))
            } else {
                Err(directory::DerivedError::Invalid("behind a reset"))
            }
        });
        match series {
            Ok(found) => self.series_through = found.map(|(_, d)| d.through),
            Err(_) => self.refused.push(Family::Series),
        }
        let through = self.series_through.unwrap_or_default();
        self.uncovered = Uncovered::default();
        for r in &uncovered {
            if r.0 > through {
                self.uncovered.sealed(r.0, r.1);
            }
        }
        self.opened = true;
        Ok(())
    }

    fn root(&self) -> Root {
        let mut groups = self.full.clone();
        if !self.open.segments.is_empty() {
            groups.push(self.open.entry());
        }
        Root {
            epoch: self.epoch,
            manifest_end: self.manifest_end,
            through: groups.last().map_or(Position::default(), |g| g.last),
            segment_count: self.segment_count(),
            stamp: self.start.stamp,
            compacted_through: None,
            groups,
            lost: self.lost.clone(),
            quarantined: Vec::new(),
        }
    }

    /// Does what is due at monotonic time `now` and wall time `wall`, with the mark synced through `synced`; returns
    /// how long until it needs to run again, if it does.
    pub fn run(&mut self, synced: Position, now: Duration, wall: i64) -> Option<Duration> {
        if !self.opened && self.open(wall).is_err() {
            self.stats.failed += 1;
            return Some(RETRY);
        }
        let mut wait: Option<Duration> = None;
        let mut soon = |d: Duration| wait = Some(wait.map_or(d, |w| w.min(d)));
        self.pending.sort_by_key(SegmentRecord::last);
        let ready = self.pending.partition_point(|r| r.last() <= synced);
        for r in self.pending.drain(..ready).collect::<Vec<_>>() {
            if self.open.segments.len() == logdir::GROUP_SEGMENTS {
                let number = self.open.number + 1;
                let full = std::mem::replace(
                    &mut self.open,
                    Group {
                        number,
                        segments: Vec::new(),
                    },
                );
                self.full.push(full.entry());
                self.open_written = Some(0);
            }
            if self.series_through.is_none_or(|t| r.last() > t) {
                self.uncovered.sealed(r.last(), r.len);
            }
            self.open.segments.push(r);
            self.root_dirty = true;
        }
        if !self.pending.is_empty() {
            soon(RETRY);
        }
        if self.root_dirty && !self.write_log_directory(wall) {
            soon(RETRY);
        }
        if self.opts.hook.is_some() && !self.refused.contains(&Family::Series) {
            let since = self.series_at.map(|t| now.saturating_sub(t));
            if series_due(self.uncovered.bytes(), self.start.segment_size, since)
                && !self.write_series(synced, now, wall)
            {
                soon(RETRY);
            }
        }
        if let Some(d) = self.status(synced, now, wall) {
            soon(d);
        }
        wait
    }

    fn write_log_directory(&mut self, wall: i64) -> bool {
        let store_id = self.store_id;
        let n = self.open.segments.len() as u32;
        if n > 0 && self.open_written != Some(n) {
            let g = self.open.clone();
            if !self.write(
                Family::Group(g.number),
                |f| encode_group(&g, f, store_id),
                wall,
            ) {
                return false;
            }
            self.open_written = Some(n);
        }
        let root = self.root();
        if !self.write(Family::Root, |f| encode_root(&root, f, store_id), wall) {
            return false;
        }
        self.root_dirty = false;
        true
    }

    fn write_series(&mut self, synced: Position, now: Duration, wall: i64) -> bool {
        let Some(hook) = self.opts.hook.clone() else {
            return true;
        };
        let Some((through, entries)) = hook.series(synced, wall) else {
            return false;
        };
        if through > synced || self.series_through.is_some_and(|t| through <= t) {
            return false;
        }
        let d = SeriesDirectory {
            through,
            stamp: self.start.stamp,
            entries,
        };
        let store_id = self.store_id;
        if !self.write(
            Family::Series,
            |f| d.encode(f, store_id).unwrap_or_default(),
            wall,
        ) {
            return false;
        }
        self.series_through = Some(through);
        self.series_at = Some(now);
        self.uncovered.covered(through);
        self.uncovered.newest().is_none_or(|n| n <= through)
    }

    fn status(&mut self, synced: Position, now: Duration, wall: i64) -> Option<Duration> {
        let hook = self.opts.hook.clone()?;
        if self.status_polled.is_some_and(|t| now < t + STATUS_POLL) {
            return Some(self.status_polled? + STATUS_POLL - now);
        }
        self.status_polled = Some(now);
        let content = hook.status()?;
        let changed = self.status_last != Some((content.health, content.pressure));
        let due = self
            .opts
            .status_interval
            .is_some_and(|i| self.status_at.is_none_or(|t| now >= t + i));
        if changed || due {
            let s = StatusFile {
                interval_ns: self
                    .opts
                    .status_interval
                    .map(|i| u64::try_from(i.as_nanos()).unwrap_or(u64::MAX)),
                mark_synced: synced,
                health: content.health,
                pressure: content.pressure,
                pressure_since: None,
                slot_count: 0,
                reasons: content.reasons,
                counters: content.counters,
            };
            let store_id = self.store_id;
            if self.write(
                Family::Status,
                |f| s.encode(f, store_id).unwrap_or_default(),
                wall,
            ) {
                self.status_at = Some(now);
                self.status_last = Some((content.health, content.pressure));
            }
        }
        Some(STATUS_POLL)
    }
}

fn encode_group(g: &Group, f: Frame, store_id: [u8; 16]) -> Vec<u8> {
    g.encode(f, store_id).unwrap_or_default()
}

fn encode_root(r: &Root, f: Frame, store_id: [u8; 16]) -> Vec<u8> {
    r.encode(f, store_id).unwrap_or_default()
}

/// The catalogue the writer checks against, for tests and tools: [`logdir::catalogue_from_disk`] with the manifest
/// read from the store.
pub fn writer_catalogue(
    fs: &dyn Fs,
    dir: &Path,
    store_id: &[u8; 16],
    generation: u32,
) -> io::Result<Catalogue> {
    let manifest = read_all(fs, &dir.join(MANIFEST))
        .ok()
        .and_then(|b| read_manifest(&b, store_id).ok());
    logdir::catalogue_from_disk(fs, dir, generation, manifest.as_ref())
}
