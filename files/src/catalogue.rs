//! The file catalogue: what the manifest's records say, folded in order (`spec.md` §7 Manifest). Which files are
//! live and retired at each epoch, where each is and where it is verified, the losses with their acknowledgements,
//! the rebuilds and the `file_id` floors.
//!
//! The fold is [`Catalogue::apply`]; it refuses a record that contradicts the records before it and then changes
//! nothing. A file a record retires without any record having published it is entered as retired: every record
//! that names a file repeats its description, so the retirement alone says what the file was. A record of a type
//! whose layout a later roadmap item gives (restore, quarantine, tier move) or of an unknown skippable type changes
//! nothing here until its item lays it out; so a quarantined file stays listed as live until roadmap 8.1.

use std::collections::{BTreeMap, BTreeSet};

use kvik_log::manifest::{LossCause, LostRange, RecordPosition};

use crate::desc::{FileDescription, FileRef, kind};
use crate::record::{Body, Rebuild, Record};

/// A storage tier (`spec.md` §8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Tier {
    /// Fast local disk.
    Hot,
    /// Another local path.
    Cold,
    /// The bucket.
    Deep,
}

/// One file the manifest names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    /// What the records say about it.
    pub description: FileDescription,
    /// The epoch of the record that published it (or listed it, for a sealed log segment); `None` when no record
    /// in this manifest did.
    pub published: Option<u64>,
    /// The epoch of the record that retired it.
    pub retired: Option<u64>,
    /// Where it is. A file is published or listed on hot; tier moves are laid out by roadmap 8.1.
    pub tier: Tier,
    /// The tiers other than its own where a copy is verified. A rebuild record clears every mark (`spec.md` §7: a
    /// rebuilt manifest takes no file as verified in another tier until it is verified there again).
    pub verified: BTreeSet<Tier>,
}

impl FileEntry {
    /// Whether the file is listed at `epoch`: published at or before it (or by no record of this manifest), and
    /// not retired at or before it.
    pub fn live_at(&self, epoch: u64) -> bool {
        self.published.is_none_or(|p| p <= epoch) && self.retired.is_none_or(|r| r > epoch)
    }

    /// Whether it is live after the last record.
    pub fn is_live(&self) -> bool {
        self.retired.is_none()
    }
}

/// One loss, under its identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loss {
    /// The latest record that names it, with `first` and `last` widened to every range recorded for it.
    pub record: LostRange,
    /// The epochs of the records that name it, in order.
    pub epochs: Vec<u64>,
    /// Whether an acknowledgement names its identity with a range that covers its present range.
    pub acknowledged: bool,
}

/// One rebuild record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebuildEntry {
    /// The epoch other records refer to it by: its field 100 where it has one.
    pub identity_epoch: u64,
    /// Its epoch in this manifest.
    pub epoch: u64,
    /// What it says.
    pub record: Rebuild,
    /// Whether an acknowledgement names it.
    pub acknowledged: bool,
}

/// Why a record cannot be applied.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CatalogueError {
    /// Its epoch is not above the previous record's.
    #[error("epoch {epoch} is not above {last}")]
    EpochNotAbove {
        /// Its epoch.
        epoch: u64,
        /// The previous record's.
        last: u64,
    },
    /// It describes a file differently from an earlier record.
    #[error("file {0:?} is described differently by an earlier record")]
    Conflict(FileRef),
    /// It publishes a file that is retired.
    #[error("file {0:?} is published again after its retirement")]
    Republished(FileRef),
    /// A live compacted file would name a live compacted file as an input: both hold the same positions (`spec.md`
    /// §10 invariant 14).
    #[error("file {output:?} would be live with its input {input:?}")]
    InputStillLive {
        /// The output.
        output: FileRef,
        /// The input.
        input: FileRef,
    },
}

/// The fold of a manifest's records.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Catalogue {
    files: BTreeMap<FileRef, FileEntry>,
    losses: BTreeMap<RecordPosition, Loss>,
    /// Acknowledged ranges by identity, also for losses not yet recorded (`spec.md` §8 Acknowledging a loss).
    acks: BTreeMap<RecordPosition, Vec<(RecordPosition, RecordPosition)>>,
    rebuilds: BTreeMap<u64, RebuildEntry>,
    rebuild_acks: BTreeSet<u64>,
    floors: BTreeMap<u8, u64>,
    last_epoch: u64,
    generation: u32,
}

impl Catalogue {
    /// An empty catalogue.
    pub fn new() -> Self {
        Self::default()
    }

    /// The fold of `records`, in order.
    pub fn replay<'a>(
        records: impl IntoIterator<Item = &'a Record>,
    ) -> Result<Self, CatalogueError> {
        let mut c = Self::new();
        for r in records {
            c.apply(r)?;
        }
        Ok(c)
    }

    /// Whether [`Catalogue::apply`] would accept `r`, without applying it.
    pub fn check(&self, r: &Record) -> Result<(), CatalogueError> {
        if r.epoch <= self.last_epoch {
            return Err(CatalogueError::EpochNotAbove {
                epoch: r.epoch,
                last: self.last_epoch,
            });
        }
        match &r.body {
            Body::Publish(p) => self.check_publish(&p.published, &p.retired),
            Body::Sealed(s) => self.check_publish(s, &[]),
            _ => Ok(()),
        }
    }

    /// Applies one record. On an error nothing changes.
    pub fn apply(&mut self, r: &Record) -> Result<(), CatalogueError> {
        self.check(r)?;
        match &r.body {
            Body::Empty | Body::Other { .. } => {}
            Body::Publish(p) => {
                for d in &p.published {
                    self.files
                        .entry(d.file_ref())
                        .or_insert_with(|| entry(d, Some(r.epoch)));
                }
                for d in &p.retired {
                    let e = self
                        .files
                        .entry(d.file_ref())
                        .or_insert_with(|| entry(d, None));
                    e.retired.get_or_insert(r.epoch);
                }
            }
            Body::Sealed(s) => {
                for d in s {
                    self.files
                        .entry(d.file_ref())
                        .or_insert_with(|| entry(d, Some(r.epoch)));
                }
            }
            Body::LostRange(l) => {
                let loss = self.losses.entry(l.identity).or_insert_with(|| Loss {
                    record: l.clone(),
                    epochs: Vec::new(),
                    acknowledged: false,
                });
                let (first, last) = (loss.record.first.min(l.first), loss.record.last.max(l.last));
                loss.record = LostRange {
                    first,
                    last,
                    ..l.clone()
                };
                loss.epochs.push(r.epoch);
                self.refresh_ack(l.identity);
            }
            Body::Acknowledgement(a) => {
                for l in &a.losses {
                    self.acks
                        .entry(l.identity)
                        .or_default()
                        .push((l.first, l.last));
                    self.refresh_ack(l.identity);
                }
                if let Some(e) = a.rebuild {
                    self.rebuild_acks.insert(e);
                    if let Some(rb) = self.rebuilds.get_mut(&e) {
                        rb.acknowledged = true;
                    }
                }
            }
            Body::Rebuild(rb) => {
                for (k, id) in &rb.floors {
                    let f = self.floors.entry(*k).or_default();
                    *f = (*f).max(*id);
                }
                for f in self.files.values_mut() {
                    f.verified.clear();
                }
                let id = r.identity_epoch();
                self.rebuilds.insert(
                    id,
                    RebuildEntry {
                        identity_epoch: id,
                        epoch: r.epoch,
                        record: rb.clone(),
                        acknowledged: self.rebuild_acks.contains(&id),
                    },
                );
            }
        }
        self.last_epoch = r.epoch;
        self.generation = r.generation;
        Ok(())
    }

    /// Checks a record that publishes `published` and retires `retired`.
    fn check_publish(
        &self,
        published: &[FileDescription],
        retired: &[FileDescription],
    ) -> Result<(), CatalogueError> {
        for d in published.iter().chain(retired) {
            if let Some(e) = self.files.get(&d.file_ref())
                && e.description != *d
            {
                return Err(CatalogueError::Conflict(d.file_ref()));
            }
        }
        for d in published {
            if self
                .files
                .get(&d.file_ref())
                .is_some_and(|e| e.retired.is_some())
            {
                return Err(CatalogueError::Republished(d.file_ref()));
            }
        }
        let retiring: BTreeSet<FileRef> = retired.iter().map(FileDescription::file_ref).collect();
        let live_after = |f: &FileRef| -> bool {
            !retiring.contains(f)
                && (published.iter().any(|d| d.file_ref() == *f)
                    || self.files.get(f).is_some_and(FileEntry::is_live))
        };
        // Invariant 14 as the records can show it: no live compacted file names a live compacted file as input.
        for d in published.iter().filter(|d| d.kind == kind::COMPACTED) {
            if let Some(i) = d
                .inputs
                .iter()
                .find(|i| i.kind == kind::COMPACTED && live_after(i))
            {
                return Err(CatalogueError::InputStillLive {
                    output: d.file_ref(),
                    input: *i,
                });
            }
        }
        for d in published {
            let f = d.file_ref();
            if let Some(o) = self.files.values().find(|e| {
                e.is_live()
                    && !retiring.contains(&e.description.file_ref())
                    && e.description.inputs.contains(&f)
                    && f.kind == kind::COMPACTED
            }) {
                return Err(CatalogueError::InputStillLive {
                    output: o.description.file_ref(),
                    input: f,
                });
            }
        }
        Ok(())
    }

    fn refresh_ack(&mut self, identity: RecordPosition) {
        if let Some(l) = self.losses.get_mut(&identity) {
            l.acknowledged = self.acks.get(&identity).is_some_and(|v| {
                v.iter()
                    .any(|(a, b)| *a <= l.record.first && l.record.last <= *b)
            });
        }
    }

    /// Every file a record names, by identity.
    pub fn files(&self) -> impl Iterator<Item = &FileEntry> {
        self.files.values()
    }

    /// One file.
    pub fn file(&self, f: FileRef) -> Option<&FileEntry> {
        self.files.get(&f)
    }

    /// The files listed at `epoch`, for a reader that took it.
    pub fn at_epoch(&self, epoch: u64) -> Vec<&FileEntry> {
        self.files.values().filter(|e| e.live_at(epoch)).collect()
    }

    /// The files live after the last record.
    pub fn live(&self) -> impl Iterator<Item = &FileEntry> {
        self.files.values().filter(|e| e.is_live())
    }

    /// Every loss, by identity.
    pub fn losses(&self) -> impl Iterator<Item = &Loss> {
        self.losses.values()
    }

    /// One loss.
    pub fn loss(&self, identity: RecordPosition) -> Option<&Loss> {
        self.losses.get(&identity)
    }

    /// The loss whose recorded range holds `p`, if any.
    pub fn loss_holding(&self, p: RecordPosition) -> Option<&Loss> {
        self.losses
            .values()
            .find(|l| l.record.first <= p && p <= l.record.last)
    }

    /// Every acknowledged range, by identity, recorded or not: `(identity, first, last)`.
    pub fn acknowledged_ranges(
        &self,
    ) -> impl Iterator<Item = (RecordPosition, RecordPosition, RecordPosition)> + '_ {
        self.acks
            .iter()
            .flat_map(|(id, v)| v.iter().map(move |(a, b)| (*id, *a, *b)))
    }

    /// Every rebuild record, by identity epoch.
    pub fn rebuilds(&self) -> impl Iterator<Item = &RebuildEntry> {
        self.rebuilds.values()
    }

    /// The rebuild record other records refer to by `identity_epoch`.
    pub fn rebuild(&self, identity_epoch: u64) -> Option<&RebuildEntry> {
        self.rebuilds.get(&identity_epoch)
    }

    /// The highest `file_id` of kind `k` the records name (files, inputs, lost ranges for log segments) or a
    /// rebuild record's floor gives; the next id is above it and above the local names and the mark (`format.md`
    /// A.12).
    pub fn id_floor(&self, k: u8) -> u64 {
        let named = self
            .files
            .values()
            .flat_map(|e| {
                std::iter::once(e.description.file_ref())
                    .chain(e.description.inputs.iter().copied())
            })
            .filter(|f| f.kind == k)
            .map(|f| f.file_id);
        let lost = self
            .losses
            .values()
            .filter(|_| k == kind::LOG_SEGMENT)
            .map(|l| l.record.file_id);
        named
            .chain(lost)
            .chain(self.floors.get(&k).copied())
            .max()
            .unwrap_or(0)
    }

    /// The epoch of the last record applied, 0 for none.
    pub fn last_epoch(&self) -> u64 {
        self.last_epoch
    }

    /// The generation of the last record applied.
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Records a verified copy of a live file in tier `t` (the records that do so are laid out by roadmap 8.1).
    pub fn mark_verified(&mut self, f: FileRef, t: Tier) {
        if let Some(e) = self.files.get_mut(&f) {
            e.verified.insert(t);
        }
    }

    /// The cause a recorded loss gives, if a loss holds `p`.
    pub fn cause_at(&self, p: RecordPosition) -> Option<LossCause> {
        self.loss_holding(p).map(|l| l.record.cause)
    }
}

fn entry(d: &FileDescription, published: Option<u64>) -> FileEntry {
    FileEntry {
        description: d.clone(),
        published,
        retired: None,
        tier: Tier::Hot,
        verified: BTreeSet::new(),
    }
}
