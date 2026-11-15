//! The file catalogue: what the manifest's records say, folded in order (`spec.md` §7 Manifest). Which files are
//! live and retired at each epoch, where each is and where it is verified, the losses with their acknowledgements,
//! the rebuilds and the `file_id` floors.
//!
//! The fold is [`Catalogue::apply`]; it refuses a record that contradicts the records before it and then changes
//! nothing. A file a record retires without any record having published it is entered as retired: every record
//! that names a file repeats its description, so the retirement alone says what the file was. A restore record
//! enters the files it lists, with their copies, and the losses and acknowledgements it carries, so a generation's
//! records are complete without those before it (`spec.md` §8 Generations). A record of a type whose layout a later
//! roadmap item gives (quarantine) or of an unknown skippable type changes nothing here until its item lays it out;
//! so a quarantined file stays listed as live until roadmap 8.1. A copy-verified record (`0x27`) sets the file's deep
//! copy and marks its tier verified; one carried into a rebuilt manifest sets the copy only (Q-280).

use std::collections::{BTreeMap, BTreeSet};

use kvik_log::manifest::{LossCause, LostRange, RecordPosition};

use crate::desc::{FileDescription, FileRef, kind};
use crate::record::{AckedLoss, Body, Rebuild, Record, RecordError, record_type};
use crate::restore::RestoreRecord;
use crate::tier::{CopyVerified, DeepCopy};

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
    /// Its copy in the deep tier, from a copy-verified record (`0x27`); kept by a rebuild record, which clears only
    /// `verified` (Q-280).
    pub deep: Option<DeepCopy>,
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
    /// A restore record whose fields do not decode (`format.md` A.14).
    #[error("restore record: {0}")]
    Restore(RecordError),
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
    restores: Vec<RestoreRecord>,
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
            b @ Body::Other {
                record_type: record_type::RESTORE,
                ..
            } => {
                let rr = restore_of(b)?;
                let listed: Vec<FileDescription> =
                    rr.files.iter().map(|f| f.description.clone()).collect();
                self.check_publish(&listed, &[])
            }
            Body::Other { .. } => match CopyVerified::from_body(&r.body) {
                Some(Ok(c)) => self.check_described(&c.file),
                _ => Ok(()),
            },
            _ => Ok(()),
        }
    }

    /// Applies one record. On an error nothing changes.
    pub fn apply(&mut self, r: &Record) -> Result<(), CatalogueError> {
        self.check(r)?;
        match &r.body {
            Body::Empty => {}
            b @ Body::Other {
                record_type: record_type::RESTORE,
                ..
            } => {
                let rr = restore_of(b)?;
                self.apply_restore(r.epoch, rr);
            }
            Body::Other { .. } => {
                if let Some(Ok(c)) = CopyVerified::from_body(&r.body) {
                    self.apply_copy(c, r.old_epoch.is_some());
                }
            }
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
            Body::LostRange(l) => self.record_loss(r.epoch, l),
            Body::Acknowledgement(a) => {
                for l in &a.losses {
                    self.record_ack(l);
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

    /// A copy-verified record. A carried one (field 100) was verified before a rebuild, so it gives the copy but no
    /// verified mark (`spec.md` §7 Manifest: verified there again; Q-280).
    fn apply_copy(&mut self, c: CopyVerified, carried: bool) {
        let f = c.file.file_ref();
        match (c.tier, carried) {
            (Tier::Deep, false) => self.record_copy(f, c.tier, c.copy),
            (Tier::Deep, true) => {
                if let Some(e) = self.files.get_mut(&f) {
                    e.deep = Some(c.copy);
                }
            }
            (t, false) => self.mark_verified(f, t),
            (_, true) => {}
        }
    }

    /// A record naming a file must describe it as earlier records do.
    fn check_described(&self, d: &FileDescription) -> Result<(), CatalogueError> {
        match self.files.get(&d.file_ref()) {
            Some(e) if e.description != *d => Err(CatalogueError::Conflict(d.file_ref())),
            _ => Ok(()),
        }
    }

    fn record_loss(&mut self, epoch: u64, l: &LostRange) {
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
        loss.epochs.push(epoch);
        self.refresh_ack(l.identity);
    }

    fn record_ack(&mut self, l: &AckedLoss) {
        self.acks
            .entry(l.identity)
            .or_default()
            .push((l.first, l.last));
        self.refresh_ack(l.identity);
    }

    fn apply_restore(&mut self, epoch: u64, rr: RestoreRecord) {
        for f in &rr.files {
            let e = self
                .files
                .entry(f.description.file_ref())
                .or_insert_with(|| entry(&f.description, Some(epoch)));
            e.tier = f.tier;
            e.deep = Some(f.copy.clone());
            e.verified.insert(Tier::Deep);
        }
        for l in &rr.losses {
            self.record_loss(epoch, l);
        }
        for a in &rr.acknowledgements {
            self.record_ack(a);
        }
        self.restores.push(rr);
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

    /// Records a verified copy `c` of a live file in tier `t`: sets [`FileEntry::deep`] and marks `t` verified. The
    /// copy-verified record's fold (roadmap 8.1c) calls it.
    pub fn record_copy(&mut self, f: FileRef, t: Tier, c: DeepCopy) {
        if let Some(e) = self.files.get_mut(&f) {
            e.deep = Some(c);
            e.verified.insert(t);
        }
    }

    /// The restore records, in order.
    pub fn restores(&self) -> &[RestoreRecord] {
        &self.restores
    }

    /// The cause a recorded loss gives, if a loss holds `p`.
    pub fn cause_at(&self, p: RecordPosition) -> Option<LossCause> {
        self.loss_holding(p).map(|l| l.record.cause)
    }
}

fn restore_of(b: &Body) -> Result<RestoreRecord, CatalogueError> {
    RestoreRecord::from_body(b)
        .unwrap_or(Err(RecordError::Invalid("not a restore record")))
        .map_err(CatalogueError::Restore)
}

fn entry(d: &FileDescription, published: Option<u64>) -> FileEntry {
    FileEntry {
        description: d.clone(),
        published,
        retired: None,
        tier: Tier::Hot,
        verified: BTreeSet::new(),
        deep: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desc::FileClass;

    fn segment(file_id: u64) -> FileDescription {
        FileDescription {
            kind: kind::LOG_SEGMENT,
            class: FileClass::None,
            generation: 0,
            file_id,
            range: Some(((0, 1), (0, 9))),
            hash: [3; 32],
            inputs: Vec::new(),
        }
    }

    #[test]
    fn record_copy_sets_the_deep_copy_and_marks_the_tier_verified() {
        let mut c = Catalogue::new();
        c.apply(&Record::new(2, 0, 0, Body::Sealed(vec![segment(1)])))
            .unwrap();
        let f = segment(1).file_ref();
        assert_eq!(c.file(f).unwrap().deep, None);
        let copy = DeepCopy {
            size: 100,
            sha256: [4; 32],
        };
        c.record_copy(f, Tier::Deep, copy.clone());
        let e = c.file(f).unwrap();
        assert_eq!(e.deep, Some(copy.clone()));
        assert!(e.verified.contains(&Tier::Deep));
        let absent = segment(2).file_ref();
        c.record_copy(absent, Tier::Deep, copy);
        assert!(c.file(absent).is_none());
    }

    fn copy(file_id: u64, carried: bool, epoch: u64) -> Record {
        let c = CopyVerified {
            file: segment(file_id),
            tier: Tier::Deep,
            copy: DeepCopy {
                size: 10 + file_id,
                sha256: [file_id as u8; 32],
            },
        };
        let mut r = Record::new(epoch, 0, 0, c.to_body());
        r.old_epoch = carried.then_some(epoch - 1);
        r
    }

    #[test]
    fn a_copy_verified_record_sets_the_copy_and_marks_deep_verified() {
        let c = Catalogue::replay(&[
            Record::new(2, 0, 0, Body::Sealed(vec![segment(1)])),
            copy(1, false, 3),
        ])
        .unwrap();
        let e = c.file(segment(1).file_ref()).unwrap();
        assert_eq!(e.deep.as_ref().map(|d| d.size), Some(11));
        assert!(e.verified.contains(&Tier::Deep));
    }

    #[test]
    fn q280_a_carried_copy_verified_record_sets_the_copy_but_no_verified_mark() {
        let rebuild = Record::new(10, 0, 0, Body::Rebuild(Rebuild::default()));
        let mut sealed = Record::new(11, 0, 0, Body::Sealed(vec![segment(1)]));
        sealed.old_epoch = Some(2);
        let c = Catalogue::replay(&[rebuild, sealed, copy(1, true, 12)]).unwrap();
        let e = c.file(segment(1).file_ref()).unwrap();
        assert_eq!(e.deep.as_ref().map(|d| d.size), Some(11));
        assert!(e.verified.is_empty());
        let mut c = c;
        c.apply(&copy(1, false, 13)).unwrap();
        assert!(
            c.file(segment(1).file_ref())
                .unwrap()
                .verified
                .contains(&Tier::Deep)
        );
    }

    #[test]
    fn a_rebuild_record_clears_verified_but_keeps_the_copy() {
        let mut c = Catalogue::replay(&[
            Record::new(2, 0, 0, Body::Sealed(vec![segment(1)])),
            copy(1, false, 3),
        ])
        .unwrap();
        c.apply(&Record::new(4, 0, 0, Body::Rebuild(Rebuild::default())))
            .unwrap();
        let e = c.file(segment(1).file_ref()).unwrap();
        assert!(e.verified.is_empty());
        assert!(e.deep.is_some());
    }

    #[test]
    fn q281_a_copy_verified_record_describing_a_file_otherwise_is_refused() {
        let mut c =
            Catalogue::replay(&[Record::new(2, 0, 0, Body::Sealed(vec![segment(1)]))]).unwrap();
        let mut other = segment(1);
        other.hash = [9; 32];
        let r = Record::new(
            3,
            0,
            0,
            CopyVerified {
                file: other,
                tier: Tier::Deep,
                copy: DeepCopy {
                    size: 1,
                    sha256: [0; 32],
                },
            }
            .to_body(),
        );
        assert_eq!(
            c.apply(&r),
            Err(CatalogueError::Conflict(segment(1).file_ref()))
        );
        assert!(c.file(segment(1).file_ref()).unwrap().deep.is_none());
    }

    fn lost(identity: RecordPosition, last: RecordPosition) -> LostRange {
        LostRange {
            identity,
            first: identity,
            last,
            file_id: 1,
            offset: 0,
            length: 10,
            cause: LossCause::Corruption,
            boot_id: [1; 16],
            id_floor: 0,
            time_before: None,
            time_after: None,
            extends: false,
            stamp: None,
        }
    }

    fn restore_record() -> RestoreRecord {
        use crate::restore::RestoredFile;
        use kvik_log::mark::ResetStamp;
        RestoreRecord {
            generation: 1,
            stamp: ResetStamp {
                sequence: 1,
                nonce: 7,
            },
            source_generation: 0,
            restored_end: (0, 9),
            carried_ends: Vec::new(),
            losses: vec![lost((0, 4), (0, 5))],
            acknowledgements: vec![AckedLoss {
                identity: (0, 4),
                first: (0, 4),
                last: (0, 5),
            }],
            entry_losses: Vec::new(),
            files: vec![RestoredFile {
                description: segment(1),
                tier: Tier::Hot,
                copy: DeepCopy {
                    size: 50,
                    sha256: [6; 32],
                },
            }],
            drill: false,
        }
    }

    #[test]
    fn a_restore_record_enters_its_files_copies_losses_and_acknowledgements() {
        let rr = restore_record();
        let c = Catalogue::replay(&[Record::new(1, 1, 0, rr.to_body())]).unwrap();
        assert_eq!(c.restores(), std::slice::from_ref(&rr));
        let e = c.file(segment(1).file_ref()).unwrap();
        assert_eq!(e.published, Some(1));
        assert_eq!(e.tier, Tier::Hot);
        assert_eq!(e.deep, Some(rr.files[0].copy.clone()));
        assert!(e.verified.contains(&Tier::Deep));
        let l = c.loss((0, 4)).unwrap();
        assert_eq!(l.record, rr.losses[0]);
        assert!(l.acknowledged);
        assert_eq!(c.generation(), 1);
    }

    #[test]
    fn a_restore_record_that_does_not_decode_or_contradicts_is_refused_and_changes_nothing() {
        let mut c = Catalogue::new();
        let bad = Body::Other {
            record_type: record_type::RESTORE,
            skippable: false,
            fields: vec![(1, vec![0; 12])],
        };
        assert!(matches!(
            c.apply(&Record::new(1, 1, 0, bad)),
            Err(CatalogueError::Restore(RecordError::Invalid(_)))
        ));
        let mut other = segment(1);
        other.hash = [9; 32];
        c.apply(&Record::new(1, 1, 0, Body::Sealed(vec![other])))
            .unwrap();
        let before = c.clone();
        assert_eq!(
            c.apply(&Record::new(2, 1, 0, restore_record().to_body())),
            Err(CatalogueError::Conflict(segment(1).file_ref()))
        );
        assert_eq!(c, before);
    }
}
