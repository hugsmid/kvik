//! The tree's files (design 7.1b sections 3, 5 and 7): parts and roots written through [`Fs`], the start that picks
//! the root to continue from, and the removal of retired parts after their grace time. What goes into a checkpoint is
//! the caller's; this module keeps invariants T5, T6 and T8.

mod nodes;
mod rebuild;
#[cfg(test)]
mod rebuild_tests;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kvik_log::fs::{Fs, OpenMode, is_sharing_error};
use kvik_log::header::FileHeader;
use kvik_log::mark::ResetStamp;
use kvik_log::writer::Position;

pub use nodes::{Fact, folded_levels, quarter_entries};
pub use rebuild::{Rebuild, Runs, backfill_cover, series_entries};

use crate::frame::EncodeError;
use crate::level::{LEVELS, Level};
use crate::part::{self, Class, Entry, Part, Run};
use crate::root::{self, Covered, PartRef, Root};
use crate::set::{DAY, part_ref};

/// The tree's directory under the store directory (section 3).
pub const DIR: &str = "tree";
/// How long a retired part stays after this writer first saw it retired (section 7 Retired files, T6).
pub const GRACE: Duration = Duration::from_secs(3_600);

/// A part's file name: `<generation:8 hex>-<file_id:16 hex>.kvt`.
pub fn part_name(generation: u32, file_id: u64) -> String {
    format!("{generation:08x}-{file_id:016x}.kvt")
}

/// A root's file name: `root-<seq:16 hex>.kvr`.
pub fn root_name(seq: u64) -> String {
    format!("root-{seq:016x}.kvr")
}

fn parse_part(name: &str) -> Option<(u32, u64)> {
    let stem = name.strip_suffix(".kvt")?;
    let (g, f) = stem.split_once('-')?;
    (g.len() == 8 && f.len() == 16).then(|| {
        Some((
            u32::from_str_radix(g, 16).ok()?,
            u64::from_str_radix(f, 16).ok()?,
        ))
    })?
}

fn parse_root(name: &str) -> Option<u64> {
    let hex = name.strip_prefix("root-")?.strip_suffix(".kvr")?;
    (hex.len() == 16).then(|| u64::from_str_radix(hex, 16).ok())?
}

/// What the files are written under: the log's generation and reset stamp, the store's id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The store generation.
    pub generation: u32,
    /// The mark's reset stamp.
    pub reset: ResetStamp,
    /// The store id (`format.md` A.12).
    pub store_id: [u8; 16],
}

/// Why a write of the tree failed; the job is abandoned and nothing it wrote is used (section 5).
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// A file operation failed.
    #[error("{op} {path}: {source}")]
    Io {
        /// What was being done.
        op: &'static str,
        /// The file.
        path: PathBuf,
        /// The error.
        source: io::Error,
    },
    /// The checkpoint is above the synced mark (T5).
    #[error("checkpoint {k:?} above the synced mark {synced:?}")]
    AboveMark {
        /// The checkpoint.
        k: Position,
        /// The synced mark.
        synced: Position,
    },
    /// The checkpoint is below the root's (T8).
    #[error("checkpoint {k:?} below the root's {root:?}")]
    Backwards {
        /// The checkpoint.
        k: Position,
        /// The root's.
        root: Position,
    },
    /// A rebuild is not at the root's K.
    #[error("rebuild at {k:?}, not at the root's K {root:?}")]
    NotAtCheckpoint {
        /// The rebuild's cut.
        k: Position,
        /// The root's.
        root: Position,
    },
    /// A live part is missing, short or corrupt; the scrub rebuilds its period.
    #[error("part {0:#x} is damaged")]
    Damaged(u64),
    /// A merge or roll-up could not be formed.
    #[error(transparent)]
    Merge(#[from] crate::set::MergeError),
    /// A part or root would not encode.
    #[error(transparent)]
    Encode(#[from] EncodeError),
}

/// What a start is given besides the files: the log's synced mark, whether the store holds no fact yet, and the
/// clocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opening {
    /// The synced mark after the log's start; a root above it is not continued.
    pub mark: Position,
    /// The store holds no fact: with no root, the tree covers everything.
    pub fresh: bool,
    /// Wall time, for file headers and retirement times.
    pub wall: i64,
    /// Monotonic time, for grace times.
    pub now: Duration,
}

/// What a start found (section 5 Start, section 7).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StartReport {
    /// The `seq` of the root continued from, if one was.
    pub continued: Option<u64>,
    /// Roots found and not used: undecodable, of another generation or reset stamp, naming a missing or short part,
    /// or with K above the mark.
    pub refused: usize,
    /// Temporary files, unlisted parts and old roots removed.
    pub removed: usize,
    /// The tree was discarded: a loss below the root's K (Q-1429).
    pub discarded: bool,
}

/// The tree's files as this writer keeps them.
#[derive(Debug)]
pub struct TreeFiles {
    fs: Arc<dyn Fs>,
    dir: PathBuf,
    id: Identity,
    root: Root,
    /// The root before `root`, on disk; its live parts are kept with the current root's (T6).
    prev: Option<Root>,
    /// The next root `seq` and part `file_id`: past every name renamed, even by a failed job (T6).
    next_seq: u64,
    next_file: u64,
    /// When this writer first saw each retired part retired, on its monotonic clock.
    retired_seen: BTreeMap<u64, Duration>,
    /// Retired parts unlinked since the root was written, left out of the next.
    unlinked: BTreeSet<u64>,
    grace: Duration,
}

fn io_err<'a>(op: &'static str, path: &'a Path) -> impl FnOnce(io::Error) -> WriteError + 'a {
    move |source| WriteError::Io {
        op,
        path: path.to_path_buf(),
        source,
    }
}

/// Every level's whole range, for a tree that starts with its store and so has seen every fact (section 3.2).
pub fn covered_all(levels: &[Level]) -> Vec<Covered> {
    folded_levels(levels)
        .iter()
        .map(|l| Covered {
            level: l.id,
            from: match i64::MIN.rem_euclid(l.width) {
                0 => i64::MIN,
                r => i64::MIN + (l.width - r),
            },
            to: i64::MAX - i64::MAX.rem_euclid(l.width),
        })
        .collect()
}

impl TreeFiles {
    /// Starts on the files under `store_dir` (section 5 Start): removes temporary files, continues from the newest
    /// root that decodes, is of `id`, names its parts at their lengths and has K at or below `mark`; removes parts
    /// neither that root nor the one before it lists, and older roots. With no root, the tree starts empty, covering
    /// everything when `fresh` (the store holds no fact) and nothing otherwise. A root with a loss below its K
    /// (`lost_below`) is replaced at once by a root that raises the global floor and covers nothing (Q-1429).
    pub fn start(
        fs: Arc<dyn Fs>,
        store_dir: &Path,
        id: Identity,
        at: Opening,
        lost_below: impl Fn(Position) -> bool,
    ) -> Result<(TreeFiles, StartReport), WriteError> {
        let Opening {
            mark,
            fresh,
            wall,
            now,
        } = at;
        let dir = store_dir.join(DIR);
        fs.create_dir_all(&dir).map_err(io_err("create", &dir))?;
        let entries = fs.read_dir(&dir).map_err(io_err("list", &dir))?;
        let mut report = StartReport::default();
        let mut roots: Vec<(u64, String)> = Vec::new();
        let mut parts: BTreeMap<String, u64> = BTreeMap::new();
        let mut max_seq = 0;
        let mut max_file = 0;
        for e in &entries {
            if e.name.ends_with(".tmp") {
                if fs.remove_file(&dir.join(&e.name)).is_ok() {
                    report.removed += 1;
                }
            } else if let Some(s) = parse_root(&e.name) {
                max_seq = max_seq.max(s);
                roots.push((s, e.name.clone()));
            } else if let Some((_, f)) = parse_part(&e.name) {
                max_file = max_file.max(f);
                parts.insert(e.name.clone(), e.len);
            }
        }
        roots.sort_unstable_by(|a, b| b.cmp(a));
        let valid = |r: &Root| {
            r.header.generation == id.generation
                && r.header.store_id == id.store_id
                && r.reset == id.reset
                && r.checkpoint <= mark
                && r.parts
                    .iter()
                    .all(|p| parts.get(&part_name(id.generation, p.file_id)) == Some(&p.byte_len))
        };
        let mut kept: Vec<Root> = Vec::new();
        for (_, name) in &roots {
            if kept.len() == 2 {
                break;
            }
            let r = read_file(&*fs, &dir.join(name))
                .ok()
                .and_then(|b| Root::decode(&b).ok());
            match r {
                Some(r) if valid(&r) => kept.push(r),
                _ => report.refused += 1,
            }
        }
        let mut it = kept.into_iter();
        let current = it.next();
        let prev = it.next();
        report.continued = current.as_ref().map(|r| r.seq);
        let listed: BTreeSet<u64> = [current.as_ref(), prev.as_ref()]
            .into_iter()
            .flatten()
            .flat_map(|r| {
                r.parts
                    .iter()
                    .map(|p| p.file_id)
                    .chain(r.retired.iter().map(|x| x.file_id))
            })
            .collect();
        for name in parts.keys() {
            let ours =
                parse_part(name).is_some_and(|(g, f)| g == id.generation && listed.contains(&f));
            if !ours && fs.remove_file(&dir.join(name)).is_ok() {
                report.removed += 1;
            }
        }
        let keep_from = prev
            .as_ref()
            .or(current.as_ref())
            .map_or(u64::MAX, |r| r.seq);
        for (s, name) in &roots {
            let is_kept = [current.as_ref(), prev.as_ref()]
                .into_iter()
                .flatten()
                .any(|r| r.seq == *s);
            if *s < keep_from && !is_kept && fs.remove_file(&dir.join(name)).is_ok() {
                report.removed += 1;
            }
        }
        let root = current.unwrap_or_else(|| Root {
            header: FileHeader::new(root::KIND, id.generation, max_seq, wall, id.store_id),
            seq: max_seq,
            checkpoint: Position::default(),
            reset: id.reset,
            next_file_id: max_file + 1,
            floor: 0,
            levels: LEVELS.to_vec(),
            parts: Vec::new(),
            covered: if fresh {
                covered_all(&LEVELS)
            } else {
                Vec::new()
            },
            floors: Vec::new(),
            retired: Vec::new(),
        });
        let mut files = TreeFiles {
            next_seq: max_seq.max(root.seq) + 1,
            next_file: (max_file + 1).max(root.next_file_id),
            retired_seen: root.retired.iter().map(|x| (x.file_id, now)).collect(),
            fs,
            dir,
            id,
            root,
            prev,
            unlinked: BTreeSet::new(),
            grace: GRACE,
        };
        if report.continued.is_some() && lost_below(files.root.checkpoint) {
            files.discard(wall, now)?;
            report.discarded = true;
        }
        Ok((files, report))
    }

    /// Sets the grace time of retired parts.
    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    /// The current root: on disk, or the empty tree a start began with.
    pub fn root(&self) -> &Root {
        &self.root
    }

    /// The tree directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes one checkpoint (section 5 steps 4 and 5): a delta part per UTC day the entries fall in, then the root
    /// that lists them at `k`; then removes old roots and the retired parts whose time has come. `entries` are per
    /// `(series, level)` in ascending start; each series of `rebuilding` gains an open floor, so the tree stops
    /// serving it until a rebuild closes it. On an error the temporary files are removed and the root stays as it
    /// was, unless the new root was already renamed, when it becomes the root (a reader may hold it).
    pub fn checkpoint(
        &mut self,
        k: Position,
        synced: Position,
        entries: BTreeMap<(u64, u8), Vec<Entry>>,
        rebuilding: &BTreeSet<u64>,
        wall: i64,
        now: Duration,
    ) -> Result<(), WriteError> {
        if k > synced {
            return Err(WriteError::AboveMark { k, synced });
        }
        if k < self.root.checkpoint {
            return Err(WriteError::Backwards {
                k,
                root: self.root.checkpoint,
            });
        }
        let seq = self.next_seq;
        let mut days: BTreeMap<i64, Vec<Run>> = BTreeMap::new();
        for ((series, level), es) in entries {
            for e in es {
                let day = e.start - e.start.rem_euclid(DAY);
                let runs = days.entry(day).or_default();
                match runs.last_mut() {
                    Some(r) if r.series == series && r.level == level => r.entries.push(e),
                    _ => runs.push(Run {
                        series,
                        level,
                        entries: vec![e],
                    }),
                }
            }
        }
        let parts: Vec<Part> = days
            .into_iter()
            .map(|(day, runs)| {
                let file_id = self.next_file;
                self.next_file += 1;
                Part {
                    header: FileHeader::new(
                        part::KIND,
                        self.id.generation,
                        file_id,
                        wall,
                        self.id.store_id,
                    ),
                    seq,
                    class: Class::Delta,
                    period_start: day,
                    period_len: DAY as u64,
                    through: k,
                    levels: self.root.levels.clone(),
                    runs,
                }
            })
            .collect();
        let mut next = self.root.clone();
        next.checkpoint = k;
        for &series in rebuilding {
            let open = root::Floor {
                series,
                seq,
                rebuilding: true,
            };
            match next.floors.binary_search_by_key(&series, |f| f.series) {
                Err(i) => next.floors.insert(i, open),
                // A closed floor rises: what was rebuilt before this entry is no longer the series' truth.
                Ok(i) if !next.floors[i].rebuilding => next.floors[i] = open,
                Ok(_) => {}
            }
        }
        self.publish(next, parts, wall, now)
    }

    /// Publishes `next` (its `seq`, header and `next_file_id` set here) with the new `parts` (their file ids taken
    /// from [`TreeFiles::take_file_id`] or a checkpoint): each part written, synced and renamed, then the root, then
    /// the directory synced. Parts of the old root that `next` no longer lists must be in its `retired`.
    pub fn publish(
        &mut self,
        mut next: Root,
        parts: Vec<Part>,
        wall: i64,
        now: Duration,
    ) -> Result<(), WriteError> {
        let seq = self.next_seq;
        self.next_seq += 1;
        let mut written = Vec::with_capacity(parts.len());
        let r = (|| {
            for p in &parts {
                let bytes = p.encode()?;
                self.write_new(&part_name(self.id.generation, p.header.file_id), &bytes)?;
                written.push(part_ref(p, &bytes));
            }
            next.seq = seq;
            next.header =
                FileHeader::new(root::KIND, self.id.generation, seq, wall, self.id.store_id);
            next.reset = self.id.reset;
            next.next_file_id = self.next_file;
            next.parts.extend(written.iter().copied());
            next.parts.sort_by_key(|p| p.file_id);
            next.retired.retain(|x| !self.unlinked.contains(&x.file_id));
            let bytes = next.encode()?;
            self.write_new(&root_name(seq), &bytes)
        })();
        if let Err(e) = r {
            // No root lists the parts renamed so far, so no reader can hold them.
            for p in &written {
                let _ = self
                    .fs
                    .remove_file(&self.dir.join(part_name(self.id.generation, p.file_id)));
            }
            return Err(e);
        }
        let synced = self
            .fs
            .sync_dir(&self.dir)
            .map_err(io_err("sync", &self.dir));
        self.retired_seen
            .retain(|f, _| next.retired.iter().any(|x| x.file_id == *f));
        for x in &next.retired {
            self.retired_seen.entry(x.file_id).or_insert(now);
        }
        self.unlinked.clear();
        let old = std::mem::replace(&mut self.root, next);
        let older = self.prev.replace(old);
        synced?;
        if let Some(o) = older {
            self.remove_roots_below(o.seq + 1);
        }
        self.remove_retired(now);
        Ok(())
    }

    /// Reserves a part `file_id` for a part published next.
    pub fn take_file_id(&mut self) -> u64 {
        let f = self.next_file;
        self.next_file += 1;
        f
    }

    /// Raises the global floor to the next root's `seq`, retires every live part and covers nothing: the tree is
    /// rebuilt and reads take facts meanwhile (section 5 Start, Q-1429).
    pub fn discard(&mut self, wall: i64, now: Duration) -> Result<(), WriteError> {
        let mut next = self.root.clone();
        next.floor = self.next_seq;
        next.retired
            .extend(next.parts.drain(..).map(|p| root::Retired {
                file_id: p.file_id,
                retired_at: wall,
            }));
        next.retired.sort_by_key(|x| x.file_id);
        next.covered.clear();
        self.publish(next, Vec::new(), wall, now)
    }

    /// Unlinks the retired parts neither kept root lists as live once [`GRACE`] has passed since this writer saw them
    /// retired (T6); a removal that fails, as one a reader's open handle refuses, is tried again at the next call.
    /// Returns how many were unlinked.
    pub fn remove_retired(&mut self, now: Duration) -> usize {
        let live: BTreeSet<u64> = [Some(&self.root), self.prev.as_ref()]
            .into_iter()
            .flatten()
            .flat_map(|r| r.parts.iter().map(|p| p.file_id))
            .collect();
        let mut n = 0;
        for x in &self.root.retired {
            let due = self
                .retired_seen
                .get(&x.file_id)
                .is_some_and(|&t| now.saturating_sub(t) >= self.grace);
            if !due || live.contains(&x.file_id) || self.unlinked.contains(&x.file_id) {
                continue;
            }
            let path = self.dir.join(part_name(self.id.generation, x.file_id));
            match self.fs.remove_file(&path) {
                Ok(()) => n += 1,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) if is_sharing_error(&e) => continue,
                Err(_) => continue,
            }
            self.unlinked.insert(x.file_id);
        }
        n
    }

    fn remove_roots_below(&self, seq: u64) {
        let Ok(entries) = self.fs.read_dir(&self.dir) else {
            return;
        };
        for e in entries {
            if parse_root(&e.name).is_some_and(|s| s < seq) {
                let _ = self.fs.remove_file(&self.dir.join(&e.name));
            }
        }
    }

    /// Writes `bytes` to `<name>.tmp`, syncs it and renames it to `name`; the temporary file is removed on a failure
    /// before the rename.
    fn write_new(&self, name: &str, bytes: &[u8]) -> Result<(), WriteError> {
        let tmp = self.dir.join(format!("{name}.tmp"));
        let path = self.dir.join(name);
        let r = (|| {
            let f = self
                .fs
                .open(&tmp, OpenMode::CreateNew)
                .map_err(io_err("create", &tmp))?;
            f.write_at(bytes, 0).map_err(io_err("write", &tmp))?;
            f.sync_all().map_err(io_err("sync", &tmp))?;
            drop(f);
            self.fs.rename(&tmp, &path).map_err(io_err("rename", &tmp))
        })();
        if r.is_err() {
            let _ = self.fs.remove_file(&tmp);
        }
        r
    }

    /// The parts the current root lists, read and decoded; `None` for one that will not read or decode.
    pub fn read_parts(&self) -> Vec<(PartRef, Option<Part>)> {
        self.root
            .parts
            .iter()
            .map(|r| {
                let p = read_file(
                    &*self.fs,
                    &self.dir.join(part_name(self.id.generation, r.file_id)),
                )
                .ok()
                .and_then(|b| Part::decode(&b).ok());
                (*r, p)
            })
            .collect()
    }
}

/// A whole file's bytes.
pub fn read_file(fs: &dyn Fs, path: &Path) -> io::Result<Vec<u8>> {
    let f = fs.open(path, OpenMode::Read)?;
    let len = usize::try_from(f.len()?).map_err(|_| io::Error::other("file too long"))?;
    let mut buf = vec![0; len];
    let mut at = 0;
    while at < len {
        let n = f.read_at(&mut buf[at..], at as u64)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        at += n;
    }
    Ok(buf)
}
