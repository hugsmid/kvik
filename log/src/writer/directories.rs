//! The derived files on `kvik-maint` (`format.md` A.18): the log directory at each seal, the series directory on
//! the byte-bounded cadence of [`crate::directory::cadence`], and the status file on its interval. Nothing is
//! written naming a position above the last synced mark (`spec.md` §3 Durable mark), and `through` never passes a
//! sealed segment that is not listed (A.18 Contiguity). A failed write is counted and tried again with a growing
//! wait; it never stops the writer, since every reader treats a missing file as absent.
//!
//! The start's work (reading the files there are, describing the segments no valid group lists) runs in bounded
//! steps ([`start`]), each a few trailers or a few MiB hashed, between the thread's other duties.
//!
//! The series entries mean what the crate that registers the [`SealHook`] says; this module asks for them and lays
//! them out.

mod start;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::directory::cadence::{Uncovered, series_due};
use crate::directory::logdir::{
    self, Catalogue, Group, GroupEntry, LostEntry, Root, SegmentRecord,
};
use crate::directory::series::{SeriesDirectory, SeriesEntry};
use crate::directory::status::{StatusFile, counter, flag};
use crate::directory::{self, DerivedError, Family, Frame, read_latest};
use crate::fs::Fs;
use crate::manifest::read_manifest;
use crate::mark::ResetStamp;
use crate::store::{MANIFEST, read_all};

use super::Position;

/// The first wait before a failed write is tried again; it doubles at each failure in a row.
pub const RETRY: Duration = Duration::from_millis(100);
/// The longest wait before a failed write is tried again.
pub const RETRY_MAX: Duration = Duration::from_secs(60);
/// How often the health is looked at for a change.
pub const STATUS_POLL: Duration = Duration::from_secs(1);
/// The default status interval (`format.md` A.18).
pub const STATUS_INTERVAL: Duration = Duration::from_secs(60);
/// Seals whose sizes stand for the next seal in the series directory's cadence.
const RECENT_SEALS: usize = 16;

/// The writer's health, reasons and counters for the status file ([`crate::directory::status`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusContent {
    /// The health level, over the reasons below only.
    pub health: u8,
    /// Whether the pressure flag is set (roadmap 9.0g).
    pub pressure: bool,
    /// Reason codes.
    pub reasons: Vec<u16>,
    /// Counters, in ascending id.
    pub counters: Vec<(u16, u64)>,
}

/// A series directory's content as a hook gives it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SeriesEntries {
    /// The position the entries are as of; at or below the limit asked for.
    pub through: Position,
    /// The time the candidates and pending facts were judged at.
    pub judged_at: i64,
    /// The entries, in strictly ascending series id.
    pub entries: Vec<SeriesEntry>,
}

/// The seal hook: registered by the crate that gives series entries their meaning, called on `kvik-maint`. A hook
/// holds nothing that keeps the store alive: what it is called on must never be the last owner of the writer.
pub trait SealHook: Send + Sync {
    /// The series directory's entries as of a position at or below `limit`; `written_at` is the write time.
    /// `None` when they cannot be given yet.
    fn series(&self, limit: Position, written_at: i64) -> Option<SeriesEntries>;

    /// The health level and pressure flag now, looked at once a second; cheap.
    fn health(&self) -> Option<(u8, bool)> {
        self.status().map(|s| (s.health, s.pressure))
    }

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
    /// `limits.disk_critical`: no derived file is written while free space is below it.
    pub disk_critical: u64,
    /// The series directory's bound on uncovered sealed bytes ([`crate::directory::cadence::SERIES_BYTES`]; tests
    /// use a smaller one).
    pub series_bytes: u64,
}

impl DirectoryStart {
    /// Whether a derived file written in `generation` under `stamp`, complete through `through`, is still a function
    /// of the kept log as far as resets go (A.18 Validation, as the writer applies it): same generation, and the
    /// same stamp or a `through` at or below this start's reset.
    pub fn keeps(&self, generation: u32, stamp: Option<ResetStamp>, through: Position) -> bool {
        generation == self.generation
            && (stamp == self.stamp || self.reset_after.is_some_and(|k| through <= k))
    }
}

/// The lost ranges at or below `through`, in its generation.
pub fn lost_below(lost: &[LostEntry], through: Position) -> u32 {
    lost.iter()
        .filter(|l| l.first.generation == through.generation && l.first <= through)
        .count() as u32
}

/// The manifest's lost ranges, one per identity, widened to every range recorded for it.
fn lost_of(m: &crate::manifest::ManifestRead) -> Vec<LostEntry> {
    let p = |(generation, sequence)| Position {
        generation,
        sequence,
    };
    let mut by: BTreeMap<(u32, u64), LostEntry> = BTreeMap::new();
    for l in m.lost_ranges() {
        let e = by.entry(l.identity).or_insert(LostEntry {
            first: p(l.first),
            last: p(l.last),
            file_id: l.file_id,
            cause: l.cause as u8,
        });
        e.first = e.first.min(p(l.first));
        e.last = e.last.max(p(l.last));
    }
    let mut v: Vec<LostEntry> = by.into_values().collect();
    v.sort_by_key(|l| l.first);
    v
}

/// The newest series directory that is still a function of the kept log (A.18 Validation, as the writer applies
/// it): of this generation, not behind a reset of this start, and with no lost range at or below its `through`
/// recorded since it was written.
pub fn kept_series(
    fs: &dyn Fs,
    dir: &Path,
    store_id: &[u8; 16],
    start: &DirectoryStart,
) -> Option<(Frame, SeriesDirectory)> {
    let lost = read_all(fs, &dir.join(MANIFEST))
        .ok()
        .and_then(|b| read_manifest(&b, store_id).ok())
        .map(|m| lost_of(&m))
        .unwrap_or_default();
    read_latest(fs, dir, Family::Series, |b| {
        let (f, d) = SeriesDirectory::decode(b, store_id)?;
        if start.keeps(f.generation, d.stamp, d.through) && lost_below(&lost, d.through) <= d.lost {
            Ok((f, d))
        } else {
            Err(DerivedError::Invalid("behind a reset or a loss"))
        }
    })
    .ok()
    .flatten()
}

/// Counts of the derived file writes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirectoryStats {
    /// Files written.
    pub written: u64,
    /// Writes that failed (or were not tried for lack of space) and wait to be tried again.
    pub failed: u64,
    /// Sealed segments described again from their files at the start (not reused from a group file).
    pub described: u64,
    /// Sealed segments the log directory cannot list: unreadable at the start, so `through` stays below them.
    pub unlisted: u64,
    /// Calls of the seal hook that panicked.
    pub hook_panics: u64,
}

/// See the module documentation.
pub struct Directories {
    fs: Arc<dyn Fs>,
    dir: PathBuf,
    store_id: [u8; 16],
    start: DirectoryStart,
    opts: DirectoryOptions,
    starting: Option<start::Start>,
    next_id: BTreeMap<Family, u64>,
    refused: BTreeSet<Family>,
    /// The body CRC32C of each group generation found valid at the start, by group number.
    on_disk: BTreeMap<u32, BTreeSet<u32>>,
    failures: u32,
    retry_at: Option<Duration>,
    // The log directory.
    epoch: u64,
    manifest_end: u64,
    manifest_crc32c: u32,
    lost: Vec<LostEntry>,
    full: Vec<GroupEntry>,
    open: Group,
    open_written: Option<u32>,
    unwritten: Vec<Group>,
    pending: Vec<SegmentRecord>,
    /// A sealed segment the start could not read: nothing at or above this `file_id` is listed.
    hole: Option<u64>,
    root_dirty: bool,
    /// The `through` of a root found valid at the start, which a root written during the start never goes below.
    floor: Position,
    // The series directory.
    series_through: Option<Position>,
    uncovered: Uncovered,
    seen: Position,
    recent: VecDeque<u64>,
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
            .field("starting", &self.starting.is_some())
            .field("segments", &self.segment_count())
            .field("pending", &self.pending.len())
            .field("series_through", &self.series_through)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

fn encode_group(g: &Group, f: Frame, store_id: [u8; 16]) -> Vec<u8> {
    g.encode(f, store_id).unwrap_or_default()
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
            starting: Some(start::Start::default()),
            next_id: BTreeMap::new(),
            refused: BTreeSet::new(),
            on_disk: BTreeMap::new(),
            failures: 0,
            retry_at: None,
            epoch: 0,
            manifest_end: 0,
            manifest_crc32c: 0,
            lost: Vec::new(),
            full: Vec::new(),
            open: Group {
                number: 0,
                segments: Vec::new(),
            },
            open_written: None,
            unwritten: Vec::new(),
            pending: Vec::new(),
            hole: None,
            root_dirty: false,
            floor: Position::default(),
            series_through: None,
            uncovered: Uncovered::default(),
            seen: Position::default(),
            recent: VecDeque::new(),
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

    /// Whether the start's work is done.
    pub fn started(&self) -> bool {
        self.starting.is_none()
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
        self.recent.push_back(rec.len);
        if self.recent.len() > RECENT_SEALS {
            self.recent.pop_front();
        }
        self.pending.push(rec);
    }

    /// Whether a sealed segment waits for the mark to be synced past it.
    pub fn waits_for(&self, synced: Position) -> bool {
        self.pending.iter().any(|r| r.last() > synced)
    }

    fn backing_off(&self, now: Duration) -> bool {
        self.retry_at.is_some_and(|t| now < t)
    }

    fn failed(&mut self, now: Duration) {
        self.stats.failed += 1;
        self.failures = self.failures.saturating_add(1);
        let wait = RETRY
            .saturating_mul(1 << self.failures.min(16).saturating_sub(1))
            .min(RETRY_MAX);
        self.retry_at = Some(now + wait);
    }

    /// Writes a file of `family`: the same `file_id` is tried again until it succeeds. A refused family counts as
    /// written. `false`: not written now (failed, low on space or waiting to try again).
    fn write(
        &mut self,
        family: Family,
        encode: impl FnOnce(Frame) -> Vec<u8>,
        now: Duration,
        wall: i64,
    ) -> bool {
        if self.refused.contains(&family) {
            return true;
        }
        if self.backing_off(now) {
            return false;
        }
        let file_id = *self.next_id.entry(family).or_insert(1);
        let frame = Frame {
            generation: self.start.generation,
            file_id,
            written_at: wall,
        };
        let bytes = encode(frame);
        let low = self
            .fs
            .free_space(&self.dir)
            .is_ok_and(|free| free < self.start.disk_critical.saturating_add(bytes.len() as u64));
        if low || directory::write_file(&*self.fs, &self.dir, family, file_id, &bytes).is_err() {
            self.failed(now);
            return false;
        }
        self.next_id.insert(family, file_id + 1);
        self.stats.written += 1;
        self.failures = 0;
        self.retry_at = None;
        true
    }

    fn listed_through(&self) -> Option<Position> {
        self.open
            .segments
            .last()
            .map(SegmentRecord::last)
            .or_else(|| self.full.last().map(|g| g.last))
    }

    fn root(&self) -> Root {
        let mut groups = self.full.clone();
        if !self.open.segments.is_empty() {
            groups.push(self.open.entry());
        }
        Root {
            epoch: self.epoch,
            manifest_end: self.manifest_end,
            manifest_crc32c: self.manifest_crc32c,
            through: groups.last().map_or(Position::default(), |g| g.last),
            segment_count: self.segment_count(),
            stamp: self.start.stamp,
            compacted_through: None,
            groups,
            lost: self.lost.clone(),
            quarantined: Vec::new(),
        }
    }

    /// Moves the sealed segments at or below `synced` into the listing, in position order, each only if it joins
    /// what is listed (A.18 Contiguity): a later seal waits behind an earlier one not handed over yet.
    fn list_ready(&mut self, synced: Position) {
        self.pending.sort_by_key(SegmentRecord::last);
        let mut i = 0;
        while i < self.pending.len() {
            let r = &self.pending[i];
            if r.last() > synced {
                break;
            }
            if r.last() > self.seen {
                self.seen = r.last();
                if self.series_through.is_none_or(|t| r.last() > t) {
                    self.uncovered.sealed(r.last(), r.len);
                }
            }
            let listed = self.listed_through();
            if listed.is_some_and(|l| r.last() <= l) {
                // A segment sealed before the files were read, handed over again.
                self.pending.remove(i);
                continue;
            }
            let start = Position {
                generation: r.generation,
                sequence: 0,
            };
            let joins = match listed {
                Some(l) => logdir::joins(l, r.first(), &self.lost),
                None => r.generation != 0 || logdir::joins(start, r.first(), &self.lost),
            };
            if !joins || self.hole.is_some_and(|h| r.file_id >= h) {
                i += 1;
                continue;
            }
            let r = self.pending.remove(i);
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
                if self.open_written != Some(full.segments.len() as u32) {
                    self.unwritten.push(full);
                }
                self.open_written = Some(0);
            }
            self.open.segments.push(r);
            self.root_dirty = true;
            i = 0;
        }
    }

    /// Writes a group unless a valid file found at the start already holds it.
    fn write_group(&mut self, g: &Group, now: Duration, wall: i64) -> bool {
        let crc = g.entry().body_crc32c;
        if self
            .on_disk
            .get(&g.number)
            .is_some_and(|s| s.contains(&crc))
        {
            return true;
        }
        let store_id = self.store_id;
        self.write(
            Family::Group(g.number),
            |f| encode_group(g, f, store_id),
            now,
            wall,
        )
    }

    fn write_log_directory(&mut self, now: Duration, wall: i64) -> bool {
        while let Some(g) = self.unwritten.first().cloned() {
            if !self.write_group(&g, now, wall) {
                return false;
            }
            self.unwritten.remove(0);
            // The full group's earlier generations hold fewer records: no root will name them again.
            let gens = directory::generations(&*self.fs, &self.dir, Family::Group(g.number));
            for (_, p) in gens.into_iter().skip(1) {
                let _ = self.fs.remove_file(&p);
            }
        }
        let n = self.open.segments.len() as u32;
        if n > 0 && self.open_written != Some(n) {
            let g = self.open.clone();
            if !self.write_group(&g, now, wall) {
                return false;
            }
            self.open_written = Some(n);
        }
        let root = self.root();
        if self.starting.is_some() && root.through < self.floor {
            return true;
        }
        let store_id = self.store_id;
        if !self.write(
            Family::Root,
            |f| root.encode(f, store_id).unwrap_or_default(),
            now,
            wall,
        ) {
            return false;
        }
        self.root_dirty = false;
        true
    }

    /// Does what is due at monotonic time `now` and wall time `wall`, with the mark synced through `synced`; returns
    /// how long until it needs to run again, if it does (zero while the start's steps remain).
    pub fn run(&mut self, synced: Position, now: Duration, wall: i64) -> Option<Duration> {
        if self.starting.is_some() && !self.start_step(now) {
            self.list_ready(synced);
            if self.open.segments.len() == logdir::GROUP_SEGMENTS && self.root_dirty {
                self.write_log_directory(now, wall);
            }
            return Some(Duration::ZERO);
        }
        let mut wait: Option<Duration> = None;
        let mut soon = |d: Duration| wait = Some(wait.map_or(d, |w| w.min(d)));
        self.list_ready(synced);
        if self.pending.iter().any(|r| r.last() > synced) {
            soon(RETRY);
        }
        if self.root_dirty && !self.write_log_directory(now, wall) {
            soon(self.retry_at.map_or(RETRY, |t| t.saturating_sub(now)));
        }
        if self.opts.hook.is_some() && !self.refused.contains(&Family::Series) {
            let since = self.series_at.map(|t| now.saturating_sub(t));
            let hint = self.recent.iter().copied().max().unwrap_or(0);
            if series_due(self.uncovered.bytes(), hint, self.start.series_bytes, since)
                && !self.write_series(synced, now, wall)
            {
                soon(
                    self.retry_at
                        .map_or(RETRY, |t| t.saturating_sub(now).max(RETRY)),
                );
            }
        }
        if let Some(d) = self.status(synced, now, wall) {
            soon(d);
        }
        wait
    }

    /// Does the due work that a clean shutdown should not lose, unless the start's work is unfinished, which a
    /// shutdown never waits for.
    pub fn finish(&mut self, synced: Position, now: Duration, wall: i64) {
        if self.starting.is_none() {
            self.retry_at = None;
            self.run(synced, now, wall);
        }
    }

    fn call<T>(&mut self, f: impl FnOnce(&dyn SealHook) -> Option<T>) -> Option<T> {
        let hook = self.opts.hook.clone()?;
        match catch_unwind(AssertUnwindSafe(|| f(&*hook))) {
            Ok(v) => v,
            Err(_) => {
                self.stats.hook_panics += 1;
                None
            }
        }
    }

    fn write_series(&mut self, synced: Position, now: Duration, wall: i64) -> bool {
        if self.backing_off(now) {
            return false;
        }
        let Some(s) = self.call(|h| h.series(synced, wall)) else {
            self.failed(now);
            return false;
        };
        if s.through > synced || s.through.sequence == 0 {
            self.failed(now);
            return false;
        }
        if self.series_through.is_some_and(|t| s.through <= t) {
            // No progress: the hook is behind the seals; tried again after a wait.
            self.failed(now);
            return false;
        }
        let d = SeriesDirectory {
            through: s.through,
            lost: lost_below(&self.lost, s.through),
            stamp: self.start.stamp,
            judged_at: s.judged_at,
            entries: s.entries,
        };
        let store_id = self.store_id;
        if !self.write(
            Family::Series,
            |f| d.encode(f, store_id).unwrap_or_default(),
            now,
            wall,
        ) {
            return false;
        }
        self.series_through = Some(s.through);
        self.series_at = Some(now);
        self.uncovered.covered(s.through);
        self.uncovered.newest().is_none_or(|n| n <= s.through)
    }

    fn status(&mut self, synced: Position, now: Duration, wall: i64) -> Option<Duration> {
        self.opts.hook.as_ref()?;
        if let Some(t) = self.status_polled.filter(|t| now < *t + STATUS_POLL) {
            return Some(t + STATUS_POLL - now);
        }
        self.status_polled = Some(now);
        let Some(health) = self.call(|h| h.health()) else {
            return Some(STATUS_POLL);
        };
        let changed = self.status_last != Some(health);
        let due = self
            .opts
            .status_interval
            .is_some_and(|i| self.status_at.is_none_or(|t| now >= t + i));
        if (changed || due)
            && !self.backing_off(now)
            && let Some(content) = self.call(|h| h.status())
        {
            let mut counters = content.counters;
            counters.retain(|(id, _)| *id != counter::DERIVED_FAILED && *id != counter::UNLISTED);
            counters.push((counter::DERIVED_FAILED, self.stats.failed));
            counters.push((counter::UNLISTED, self.stats.unlisted));
            counters.sort_unstable();
            let s = StatusFile {
                interval_ns: self
                    .opts
                    .status_interval
                    .map(|i| u64::try_from(i.as_nanos()).unwrap_or(u64::MAX)),
                mark_synced: synced,
                health: content.health,
                flags: flag::PARTIAL,
                pressure: content.pressure,
                pressure_since: None,
                slot_count: 0,
                reasons: content.reasons,
                counters,
            };
            let store_id = self.store_id;
            if self.write(
                Family::Status,
                |f| s.encode(f, store_id).unwrap_or_default(),
                now,
                wall,
            ) {
                self.status_at = Some(now);
                self.status_last = Some((content.health, content.pressure));
            }
        }
        Some(STATUS_POLL)
    }
}

/// The catalogue the writer checks against, for tests and tools: [`logdir::catalogue_from_disk`] with the manifest
/// read from the store.
pub fn writer_catalogue(
    fs: &dyn Fs,
    dir: &Path,
    store_id: &[u8; 16],
    generation: u32,
) -> std::io::Result<Catalogue> {
    let bytes = read_all(fs, &dir.join(MANIFEST)).ok();
    let manifest = bytes.as_ref().and_then(|b| read_manifest(b, store_id).ok());
    logdir::catalogue_from_disk(fs, dir, generation, manifest.as_ref().zip(bytes.as_deref()))
}
