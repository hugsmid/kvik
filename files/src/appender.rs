//! The one run-time writer of `MANIFEST` (`spec.md` §7 Manifest, §3 Order of a start step 4).
//!
//! - **Open** is the start's repair: a `MANIFEST.new` left by a crash is removed; a torn tail is dropped and the
//!   file truncated after the last valid record; the last valid record is written again in place and synced, so that
//!   a record kept from the page cache after a failed sync is on disk before a successor is appended. A corrupt
//!   record is not repaired here: [`Opened::Corrupt`] hands the image to [`crate::rebuild`].
//! - **Append** writes a record only after the previous record's sync has returned, and syncs it. New epochs
//!   continue above every epoch the last valid record names and above every lease (`format.md` A.14), then by one.
//!   A failed write or sync makes the appender read-only: the record may be in the page cache only, and the next
//!   start writes it again.
//! - **Settled**: the last record is neither uploaded nor acted on irreversibly until a later record has been
//!   synced. A [`Settled`] is the proof that a record has a synced successor; [`Appender::settle`] appends an empty
//!   record when one is needed. An upload, eviction, removal or act on a retirement takes an [`Irreversible`],
//!   which only [`Appender::irreversible`] gives: the record is settled and no provisional rebuild holds it.
//! - **Provisional rebuilds** (`spec.md` §7 Deep unreachable): at open, a rebuild record that no acknowledgement
//!   names is provisional, so its holds survive a restart, until the mirror path and deep have answered in this run
//!   for every corrupt record it stands for (its field 7; [`crate::rebuild::resolve`]), or are not configured. A provisional
//!   rebuild cannot be acknowledged.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use kvik_log::fs::{Fs, FsFile, OpenMode};

use crate::catalogue::{Catalogue, CatalogueError};
use crate::compaction::{Breach, Judgement, judge, publish_shape};
use crate::desc::{FileRef, kind};
use crate::image::{Image, ReadError, read};
use crate::record::{Acknowledgement, Body, EncodeError, Record};
use crate::{MANIFEST, MANIFEST_NEW};

/// Proof that a record has a synced successor, so dropping a torn tail can never remove it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settled {
    epoch: u64,
}

impl Settled {
    /// The record's epoch.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
}

/// Proof that a record may be acted on irreversibly now (uploaded; a file it retires, moves or quarantines
/// removed): it has a synced successor and no provisional rebuild holds it. Only [`Appender::irreversible`] gives one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Irreversible {
    epoch: u64,
}

impl Irreversible {
    /// The record's epoch.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
}

/// A corrupt record a rebuild stands for, between two epochs (exclusive): a copy of it must lie between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Gap {
    /// The epoch of the last valid record before it, 0 for none.
    pub after: u64,
    /// The next valid record's epoch (the rebuild record's field 7 keeps it across a restart); `None` for a span
    /// with no later record (Q-650).
    pub before: Option<u64>,
}

/// Why an irreversible act on a record must wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Held {
    /// The record has no synced successor yet.
    #[error("the record has no synced successor")]
    NotSettled,
    /// A provisional rebuild holds every upload, eviction, removal and retirement (`spec.md` §7 Deep unreachable).
    #[error("a provisional manifest rebuild holds it")]
    Provisional,
}

/// Where a manifest rebuild stands in this run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebuildState {
    /// No rebuild in this run.
    None,
    /// A rebuild that the mirror path and deep have not yet both answered for, in this run: a rebuild record that no
    /// acknowledgement names, found at open, or one written in this run while deep could not be reached.
    Provisional {
        /// The rebuild record's identity epoch.
        epoch: u64,
    },
    /// Rebuilt, and the mirror path and deep have answered or are not configured.
    Final {
        /// The rebuild record's identity epoch.
        epoch: u64,
    },
}

/// The outcome of [`Appender::open`].
#[derive(Debug)]
pub enum Opened {
    /// Repaired and ready.
    Ready(Box<Appender>),
    /// A corrupt record: fetched again or rebuilt by [`crate::rebuild`]. No handle on `MANIFEST` is held.
    Corrupt(Box<Image>),
}

/// Why the manifest cannot be opened.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// A file-system operation failed: the start is read-only.
    #[error("manifest: {0}")]
    Io(#[from] io::Error),
    /// A header that is not a manifest of this store, or a version refusal.
    #[error(transparent)]
    Read(#[from] ReadError),
    /// Valid records that contradict each other.
    #[error(transparent)]
    Catalogue(#[from] CatalogueError),
    /// No epoch is left above the manifest's and the leases'.
    #[error("no manifest epoch is left")]
    EpochsExhausted,
}

/// Why a record was not appended.
#[derive(Debug, thiserror::Error)]
pub enum AppendError {
    /// An earlier write or sync failed: the manifest is read-only until the next start.
    #[error("the manifest is read-only after a failed write")]
    ReadOnly,
    /// The write or the sync failed; the record may be in the page cache only.
    #[error("manifest write: {0}")]
    Io(#[from] io::Error),
    /// The record cannot be encoded.
    #[error(transparent)]
    Encode(#[from] EncodeError),
    /// The record contradicts the manifest.
    #[error(transparent)]
    Catalogue(#[from] CatalogueError),
    /// No epoch is left.
    #[error("no manifest epoch is left")]
    EpochsExhausted,
    /// No record of that epoch has been written, so none can be settled.
    #[error("no record of epoch {0}")]
    NoRecord(u64),
    /// A rebuild acknowledgement while the rebuild is provisional (`api.md` `WriteError::NotFinal`).
    #[error("the manifest rebuild is provisional")]
    NotFinal,
    /// A publish record that would break the catalogue's rule for compacted files (compaction design §3.4).
    #[error(transparent)]
    Breach(#[from] Breach),
    /// A new publish record while the catalogue's rule is undecided or broken (compaction design §3.4 hold).
    #[error("compaction is held: the catalogue's rule is not decided")]
    CompactionHeld,
    /// A new publish record after which the catalogue's rule would be undecided; names the files it lacks
    /// (compaction design §3.4, Q-2121).
    #[error("the record would leave the compaction rule undecided for {0:?}")]
    Undecided(BTreeSet<FileRef>),
    /// A new publish record giving a compacted file no layout (Q-2120).
    #[error("compacted file {0:?} published without a layout")]
    Unlaid(FileRef),
}

/// Why a rebuild acknowledgement was refused.
#[derive(Debug, thiserror::Error)]
pub enum AcknowledgeError {
    /// The rebuild is provisional (`api.md` `WriteError::NotFinal`).
    #[error("the manifest rebuild is provisional")]
    NotFinal,
    /// No rebuild record has that epoch.
    #[error("no rebuild record has epoch {0}")]
    UnknownRebuild(u64),
    /// The acknowledgement record was not written.
    #[error(transparent)]
    Append(#[from] AppendError),
}

/// The manifest's writer.
#[derive(Debug)]
pub struct Appender {
    file: Box<dyn FsFile>,
    path: PathBuf,
    /// Where the next record goes.
    end: u64,
    next_epoch: u64,
    /// The epoch of the last record known synced; every record below it is settled.
    synced: u64,
    generation: u32,
    catalogue: Catalogue,
    faulted: bool,
    rebuild: RebuildState,
    /// The corrupt records of a provisional rebuild not yet answered for in this run.
    pending: Vec<Gap>,
    dropped_tail: u64,
    /// The catalogue's rule as last judged: at open, and after each publish, sealed-segment or carried record.
    compaction: Judgement,
    /// Outputs a rebuild could neither publish nor discard, until a record names them (§3.5, Q-2122).
    unlocated: BTreeSet<FileRef>,
}

pub(crate) fn read_file(fs: &dyn Fs, path: &Path) -> io::Result<Vec<u8>> {
    let f = fs.open(path, OpenMode::Read)?;
    let len = f.len()?;
    let mut b = vec![0u8; usize::try_from(len).map_err(io::Error::other)?];
    let mut at = 0;
    while at < b.len() {
        let n = f.read_at(&mut b[at..], at as u64)?;
        if n == 0 {
            b.truncate(at);
            break;
        }
        at += n;
    }
    Ok(b)
}

/// Removes a `MANIFEST.new` left by a crash, then syncs the directory (`format.md` A.12).
pub(crate) fn remove_leftover(fs: &dyn Fs, dir: &Path) -> io::Result<bool> {
    let present = fs.read_dir(dir)?.iter().any(|e| e.name == MANIFEST_NEW);
    if present {
        fs.remove_file(&dir.join(MANIFEST_NEW))?;
        fs.sync_dir(dir)?;
    }
    Ok(present)
}

/// The corrupt spans a rebuild record stands for: its field 7, or, for a record without it, the first span (fields 1
/// and 2), bounded above by the first record it carried whose old epoch is above the last valid one before the span.
fn gaps_of(image: &Image, epoch: u64, r: &crate::record::Rebuild) -> Vec<Gap> {
    if !r.gaps.is_empty() {
        return r
            .gaps
            .iter()
            .map(|&(after, before)| Gap { after, before })
            .collect();
    }
    let before = image
        .records()
        .skip_while(|x| x.epoch != epoch)
        .skip(1)
        .map_while(|x| x.old_epoch)
        .find(|o| *o > r.last_valid_before)
        .unwrap_or(r.presumed_epoch.saturating_add(1));
    vec![Gap {
        after: r.last_valid_before,
        before: Some(before),
    }]
}

impl Appender {
    /// The start's repair of the manifest in `dir` (see the module documentation). `lease_epoch` is the highest
    /// epoch any lease file names.
    pub fn open(
        fs: &dyn Fs,
        dir: &Path,
        store_id: [u8; 16],
        lease_epoch: u64,
    ) -> Result<Opened, OpenError> {
        remove_leftover(fs, dir)?;
        let path = dir.join(MANIFEST);
        let bytes = read_file(fs, &path)?;
        let image = read(&bytes, &store_id)?;
        if image.is_corrupt() {
            return Ok(Opened::Corrupt(Box::new(image)));
        }
        let catalogue = Catalogue::replay(image.records())?;
        // A rebuild record that no acknowledgement names is provisional until the copy sources answer in this run.
        let (rebuild, pending) = match catalogue
            .rebuilds()
            .filter(|r| !r.acknowledged)
            .max_by_key(|r| r.epoch)
        {
            None => (RebuildState::None, Vec::new()),
            Some(r) if r.record.presumed_epoch == 0 => (
                RebuildState::Final {
                    epoch: r.identity_epoch,
                },
                Vec::new(),
            ),
            Some(r) => (
                RebuildState::Provisional {
                    epoch: r.identity_epoch,
                },
                gaps_of(&image, r.epoch, &r.record),
            ),
        };
        let file = fs.open(&path, OpenMode::ReadWrite)?;
        let mut end = image.valid_end;
        let dropped_tail = bytes.len() as u64 - image.valid_end;
        if image.torn {
            file.set_len(image.valid_end)?;
        }
        if let Some(last) = image.records.last() {
            let have = (last.end as usize).min(bytes.len());
            let mut b = bytes[last.offset as usize..have].to_vec();
            b.resize((last.end - last.offset) as usize, 0);
            file.write_at(&b, last.offset)?;
            end = last.end;
        }
        if image.torn {
            file.sync_all()?;
        } else {
            file.sync_data()?;
        }
        let named = image.records.last().map_or(0, |p| {
            p.record.epochs_named().into_iter().max().unwrap_or(0)
        });
        let next_epoch = named
            .max(lease_epoch)
            .checked_add(1)
            .ok_or(OpenError::EpochsExhausted)?;
        Ok(Opened::Ready(Box::new(Self {
            file,
            path,
            end,
            next_epoch,
            synced: image.last_epoch(),
            generation: image.records.last().map_or(0, |p| p.record.generation),
            compaction: judge(&catalogue),
            catalogue,
            unlocated: BTreeSet::new(),
            faulted: false,
            rebuild,
            pending,
            dropped_tail,
        })))
    }

    /// Appends a record holding `body` and syncs it; returns its epoch.
    pub fn append(&mut self, body: Body, written_at: i64) -> Result<u64, AppendError> {
        let r = Record::new(self.next_epoch, self.generation, written_at, body);
        self.append_record(r)
    }

    /// Appends `r` with the next epoch, keeping its old epoch and ignorable fields (a carried record).
    pub(crate) fn append_record(&mut self, mut r: Record) -> Result<u64, AppendError> {
        if self.faulted {
            return Err(AppendError::ReadOnly);
        }
        r.epoch = self.next_epoch;
        if self.holds() && matches!(&r.body, Body::Acknowledgement(a) if a.rebuild.is_some()) {
            return Err(AppendError::NotFinal);
        }
        self.catalogue.check(&r)?;
        if let (Body::Publish(p), None) = (&r.body, r.old_epoch)
            && p.published.iter().any(|d| d.kind == kind::COMPACTED)
        {
            if let Some(d) = p.published.iter().find(|d| {
                d.kind == kind::COMPACTED && !p.layouts.iter().any(|l| l.file_ref() == d.file_ref())
            }) {
                return Err(AppendError::Unlaid(d.file_ref()));
            }
            if self.compaction.holds_compaction() {
                return Err(AppendError::CompactionHeld);
            }
            publish_shape(p)?;
            let mut after = self.catalogue.clone();
            after.apply(&r)?;
            match judge(&after) {
                Judgement::Holds => {}
                Judgement::Undecided(f) => return Err(AppendError::Undecided(f)),
                Judgement::Broken(b) => return Err(b.into()),
            }
        }
        let bytes = r.encode()?;
        let next = self
            .next_epoch
            .checked_add(1)
            .ok_or(AppendError::EpochsExhausted)?;
        if let Err(e) = self
            .file
            .write_at(&bytes, self.end)
            .and_then(|()| self.file.sync_data())
        {
            self.faulted = true;
            return Err(e.into());
        }
        self.end += bytes.len() as u64;
        self.catalogue.apply(&r)?;
        if matches!(r.body, Body::Publish(_) | Body::Sealed(_)) || r.old_epoch.is_some() {
            self.rejudge();
        }
        self.synced = r.epoch;
        self.next_epoch = next;
        Ok(r.epoch)
    }

    /// A [`Settled`] for the record of `epoch` if a later record has been synced.
    pub fn settled(&self, epoch: u64) -> Option<Settled> {
        (epoch > 0 && epoch < self.synced).then_some(Settled { epoch })
    }

    /// A [`Settled`] for the record of `epoch`, appending an empty record first if it has no synced successor. A
    /// record not yet written cannot be settled.
    pub fn settle(&mut self, epoch: u64, written_at: i64) -> Result<Settled, AppendError> {
        if let Some(s) = self.settled(epoch) {
            return Ok(s);
        }
        if epoch == 0 || epoch > self.catalogue.last_epoch() {
            return Err(AppendError::NoRecord(epoch));
        }
        self.append(Body::Empty, written_at)?;
        self.settled(epoch).ok_or(AppendError::NoRecord(epoch))
    }

    /// An [`Irreversible`] for the record of `epoch` if it may be acted on irreversibly now: it has a synced
    /// successor and no provisional rebuild holds it.
    pub fn irreversible(&self, epoch: u64) -> Result<Irreversible, Held> {
        if self.holds() {
            return Err(Held::Provisional);
        }
        self.settled(epoch)
            .map(|s| Irreversible { epoch: s.epoch })
            .ok_or(Held::NotSettled)
    }

    /// Whether uploads, evictions, removals and acting on retirements are held by a provisional rebuild.
    pub fn holds(&self) -> bool {
        matches!(self.rebuild, RebuildState::Provisional { .. })
    }

    /// Acknowledges the rebuild record other records refer to by `epoch` (`api.md` `acknowledge_manifest_rebuild`):
    /// refused while the rebuild is provisional; writes nothing if it is acknowledged already. Returns whether a
    /// record was written.
    pub fn acknowledge_rebuild(
        &mut self,
        epoch: u64,
        written_at: i64,
    ) -> Result<bool, AcknowledgeError> {
        if self.holds() {
            return Err(AcknowledgeError::NotFinal);
        }
        let entry = self
            .catalogue
            .rebuild(epoch)
            .ok_or(AcknowledgeError::UnknownRebuild(epoch))?;
        if entry.acknowledged {
            return Ok(false);
        }
        self.append(
            Body::Acknowledgement(Acknowledgement {
                losses: Vec::new(),
                rebuild: Some(epoch),
            }),
            written_at,
        )?;
        Ok(true)
    }

    /// The catalogue's rule for compacted files as last judged; anything but [`Judgement::Holds`] holds runs,
    /// merges, removals and releases (compaction design §3.4).
    pub fn compaction(&self) -> &Judgement {
        &self.compaction
    }

    /// Holds runs, merges, removals and releases while a record has not named `files`: a rebuild's
    /// [`crate::rebuild::RebuildPlan::held`] (compaction design §3.5, Q-2122).
    pub fn hold_unlocated(&mut self, files: impl IntoIterator<Item = FileRef>) {
        self.unlocated.extend(files);
        self.rejudge();
    }

    fn rejudge(&mut self) {
        let c = &self.catalogue;
        self.unlocated.retain(|f| c.file(*f).is_none());
        self.compaction = match judge(c) {
            Judgement::Holds if self.unlocated.is_empty() => Judgement::Holds,
            Judgement::Holds => Judgement::Undecided(self.unlocated.clone()),
            Judgement::Undecided(mut u) => {
                u.extend(self.unlocated.iter().copied());
                Judgement::Undecided(u)
            }
            b @ Judgement::Broken(_) => b,
        };
    }

    /// The fold of every record, those appended in this run included.
    pub fn catalogue(&self) -> &Catalogue {
        &self.catalogue
    }

    /// The epoch the next record takes.
    pub fn next_epoch(&self) -> u64 {
        self.next_epoch
    }

    /// Whether a failed write has made the manifest read-only.
    pub fn is_faulted(&self) -> bool {
        self.faulted
    }

    /// Where a rebuild stands in this run.
    pub fn rebuild_state(&self) -> RebuildState {
        self.rebuild
    }

    pub(crate) fn set_rebuild_state(&mut self, s: RebuildState, pending: Vec<Gap>) {
        self.rebuild = s;
        self.pending = pending;
    }

    /// The corrupt records of a provisional rebuild that the copy sources have not answered for in this run.
    pub fn pending(&self) -> &[Gap] {
        &self.pending
    }

    /// Records that the copy sources have answered for `gap`; the rebuild is final once none is pending.
    pub(crate) fn answered(&mut self, gap: Gap) {
        self.pending.retain(|g| *g != gap);
        if let RebuildState::Provisional { epoch } = self.rebuild
            && self.pending.is_empty()
        {
            self.rebuild = RebuildState::Final { epoch };
        }
    }

    /// Bytes of a torn tail the open dropped.
    pub fn dropped_tail(&self) -> u64 {
        self.dropped_tail
    }

    /// The manifest's path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}
