//! The start's work for the derived files, in bounded steps (`format.md` A.18 Crash and rebuild): each call of
//! [`Directories::start_step`] reads at most [`TRAILERS_PER_STEP`] trailers, or reuses at most
//! [`REUSED_PER_STEP`] group records, or hashes at most [`DESCRIBE_STEP`] bytes of one segment no valid group lists,
//! so `kvik-maint` creates and seals segments between steps and a shutdown never waits for the rest. The whole
//! costs two small reads per sealed segment and one read of each segment that must be described again; the
//! groups and root are written as they fill and at the end.

use std::collections::BTreeMap;

use super::{Directories, Position, kept_series, lost_of};
use crate::batch::SegmentIdentity;
use crate::directory::catalogue::{Catalogue, DESCRIBE_STEP, Describe, read_trailer};
use crate::directory::logdir::{Group, Root, SegmentRecord};
use crate::directory::{self, DerivedError, Family};
use crate::manifest::read_manifest;
use crate::name::SegmentName;
use crate::store::{LOG_DIR, MANIFEST, read_all};

/// Trailers read per step.
pub const TRAILERS_PER_STEP: usize = 64;
/// Group records reused per step.
pub const REUSED_PER_STEP: usize = 256;

#[derive(Debug, Default)]
enum Stage {
    #[default]
    Begin,
    Trailers,
    Describe,
}

/// A sealed segment as its trailer gives it.
#[derive(Clone, Copy, Debug)]
struct Seen {
    file_id: u64,
    generation: u32,
    last_sequence: u64,
    len: u64,
    file_crc32c: u32,
}

/// See the module documentation.
#[derive(Debug, Default)]
pub(super) struct Start {
    stage: Stage,
    names: Vec<(u64, u32)>,
    next: usize,
    seen: Vec<Seen>,
    known: BTreeMap<u64, SegmentRecord>,
    describing: Option<Describe>,
}

impl Directories {
    /// One step of the start's work; `true` once it is done.
    pub(super) fn start_step(&mut self, now: std::time::Duration) -> bool {
        let Some(mut s) = self.starting.take() else {
            return true;
        };
        let done = match s.stage {
            Stage::Begin => {
                if self.backing_off(now) {
                    false
                } else if self.begin(&mut s).is_err() {
                    self.failed(now);
                    false
                } else {
                    s.stage = Stage::Trailers;
                    false
                }
            }
            Stage::Trailers => {
                self.trailers(&mut s);
                false
            }
            Stage::Describe => self.describe(&mut s),
        };
        if !done {
            self.starting = Some(s);
        }
        done
    }

    fn begin(&mut self, s: &mut Start) -> std::io::Result<()> {
        let fs = self.fs.clone();
        let fs = &*fs;
        directory::remove_temporaries(fs, &self.dir)?;
        for (family, id, _) in directory::list(fs, &self.dir)? {
            let next = self.next_id.entry(family).or_insert(1);
            *next = (*next).max(id + 1);
        }
        let bytes = read_all(fs, &self.dir.join(MANIFEST)).ok();
        let manifest = bytes
            .as_ref()
            .and_then(|b| read_manifest(b, &self.store_id).ok());
        let mut cat = Catalogue {
            generation: self.start.generation,
            ..Catalogue::default()
        };
        if let (Some(m), Some(b)) = (&manifest, &bytes) {
            cat = cat.with_manifest(m, b);
            if let Some(r) = m.records.last() {
                self.epoch = r.epoch;
                self.manifest_end = r.end;
                self.manifest_crc32c = cat.records.get(&r.end).map_or(0, |x| x.1);
            }
            self.lost = lost_of(m);
        }
        // A root still naming a manifest record bounds what a root written during the start may say.
        let root = directory::read_latest(fs, &self.dir, Family::Root, |b| {
            let (f, r) = Root::decode(b, &self.store_id)?;
            if cat.root_matches(f.generation, &r) {
                Ok((f, r))
            } else {
                Err(DerivedError::Invalid("root does not match the manifest"))
            }
        });
        match root {
            Ok(r) => self.floor = r.map_or(Position::default(), |(_, r)| r.through),
            Err(_) => {
                self.refused.insert(Family::Root);
            }
        }
        for (family, id, path) in directory::list(fs, &self.dir)? {
            let Family::Group(number) = family else {
                continue;
            };
            let Ok(b) = read_all(fs, &path) else {
                continue;
            };
            match Group::decode(&b, &self.store_id) {
                Ok((f, (g, crc))) if f.file_id == id && f.generation == self.start.generation => {
                    self.on_disk.entry(number).or_default().insert(crc);
                    for r in g.segments {
                        s.known.entry(r.file_id).or_insert(r);
                    }
                }
                Ok(_) | Err(DerivedError::Invalid(_)) => {}
                Err(DerivedError::Unsupported(_)) => {
                    self.refused.insert(Family::Group(number));
                }
            }
        }
        match kept_series(fs, &self.dir, &self.store_id, &self.start) {
            Some((_, d)) => self.series_through = Some(d.through),
            None => {
                let refused = directory::read_latest(fs, &self.dir, Family::Series, |b| {
                    crate::directory::series::SeriesDirectory::decode(b, &self.store_id)
                })
                .is_err();
                if refused {
                    self.refused.insert(Family::Series);
                }
            }
        }
        let mut names: Vec<(u64, u32)> = fs
            .read_dir(&self.dir.join(LOG_DIR))?
            .into_iter()
            .filter_map(|e| SegmentName::parse(&e.name).filter(|n| n.sealed))
            .map(|n| (n.file_id, n.generation))
            .collect();
        names.sort_unstable();
        s.names = names;
        Ok(())
    }

    fn path_of(&self, file_id: u64, generation: u32) -> std::path::PathBuf {
        let name = SegmentName {
            generation,
            file_id,
            sealed: true,
        };
        self.dir.join(LOG_DIR).join(name.to_string())
    }

    fn unreadable(&mut self, file_id: u64) {
        self.stats.unlisted += 1;
        self.hole = Some(self.hole.map_or(file_id, |h| h.min(file_id)));
    }

    fn trailers(&mut self, s: &mut Start) {
        let end = (s.next + TRAILERS_PER_STEP).min(s.names.len());
        for i in s.next..end {
            let (file_id, generation) = s.names[i];
            match read_trailer(&*self.fs, &self.path_of(file_id, generation)) {
                Some((len, t)) if t.batch_count > 0 => s.seen.push(Seen {
                    file_id,
                    generation,
                    last_sequence: t.last_sequence,
                    len,
                    file_crc32c: t.file_crc32c,
                }),
                _ => self.unreadable(file_id),
            }
        }
        s.next = end;
        if s.next == s.names.len() {
            s.seen.sort_by_key(|x| (x.generation, x.last_sequence));
            s.next = 0;
            s.stage = Stage::Describe;
        }
    }

    /// Reuses the group records that still describe the segments, and describes one other segment a part at a
    /// time; `true` once every segment is done.
    fn describe(&mut self, s: &mut Start) -> bool {
        let mut reused = 0;
        while s.next < s.seen.len() {
            let x = s.seen[s.next];
            if s.describing.is_none() {
                let same = s.known.get(&x.file_id).filter(|r| {
                    r.generation == x.generation
                        && r.last_sequence == x.last_sequence
                        && r.len == x.len
                        && r.file_crc32c == x.file_crc32c
                });
                if let Some(r) = same {
                    self.pending.push(r.clone());
                    s.next += 1;
                    reused += 1;
                    if reused == super::start::REUSED_PER_STEP {
                        return false;
                    }
                    continue;
                }
                let id = SegmentIdentity {
                    store_id: self.store_id,
                    file_id: x.file_id,
                    generation: x.generation,
                };
                match Describe::open(&*self.fs, &self.path_of(x.file_id, x.generation), &id) {
                    Some(d) => s.describing = Some(d),
                    None => {
                        self.unreadable(x.file_id);
                        s.next += 1;
                        continue;
                    }
                }
            }
            let d = s.describing.as_mut().expect("set above");
            match d.step(DESCRIBE_STEP) {
                Some(None) => return false,
                Some(Some(r)) => {
                    self.stats.described += 1;
                    self.pending.push(r);
                }
                None => self.unreadable(x.file_id),
            }
            s.describing = None;
            s.next += 1;
            return false;
        }
        s.known.clear();
        true
    }
}
