//! The manifest rebuild (`spec.md` §7 Manifest; settled by Q-52 (a), Q-103 and Q-107), the procedure of the
//! operator call `rebuild_manifest`, and what comes before and after it.
//!
//! - [`CorruptStart`]: what a start does with a corrupt record. A copy from the mirror path or deep is written back
//!   ([`repair_with_copy`]); with no copy anywhere the manifest is rebuilt, final; while deep cannot say, the start
//!   stays read-only for `manifest.rebuild_wait`, counted on the monotonic clock from the first failed ask of this
//!   start, and then rebuilds provisionally.
//! - [`plan`]: the rebuilt manifest as records, from the old manifest's valid records, the valid local files (each
//!   self-describing) and the losses the scan of the log finds. It carries every valid record with its old epoch
//!   (field 100) and restates only what no valid record states.
//! - [`write`] and [`rebuild_manifest`]: `MANIFEST.new`, synced, renamed over `MANIFEST` while no handle holds it,
//!   and the directory synced; then the manifest is opened again.
//! - [`resolve`]: deep answers after a provisional rebuild. A copy of the corrupt record is appended, carried with its
//!   old epoch, over what was reconstructed; an answer that it holds none makes the rebuild final.
//!
//! The seams are injected: the local files come described ([`LocalFile`]), an output is verified against its inputs
//! by a [`Verifier`] (the §7 procedure, roadmap 6.2), and the losses come from the log's scan ([`FoundLoss`]).
//! What a rebuild cannot settle from the local files is reported, never guessed: a file that no local path holds and
//! whose location no valid record gives is **unlocated**; an output whose live inputs are not local is
//! **undecided** (Q-172), kept on disk and published by nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use std::time::Duration;

use kvik_log::fs::{Fs, OpenMode};
use kvik_log::header::{FileHeader, HEADER_LEN, kind as file_kind};
use kvik_log::manifest::{LossCause, LostRange, RecordPosition};

use crate::appender::{
    AppendError, Appender, OpenError, Opened, RebuildState, read_file, remove_leftover,
};
use crate::catalogue::{Catalogue, CatalogueError};
use crate::desc::{FileDescription, FileRef, kind};
use crate::image::{CorruptSpan, Image, ReadError, read, record_at};
use crate::record::{Body, EncodeError, Publish, Rebuild, Record, record_type};
use crate::{MANIFEST, MANIFEST_NEW};

/// Where a local file was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Place {
    /// The hot path.
    Hot,
    /// The cold path.
    Cold,
    /// Under `quarantine/`: never live.
    Quarantine,
}

/// A local file as it describes itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalFile {
    /// Its header and trailer's self-description.
    pub description: FileDescription,
    /// Where it was found.
    pub place: Place,
    /// Whether it passed its own checksums and its plaintext hash. A file that did not is not a valid local file.
    pub verified: bool,
}

/// The §7 procedure: whether `output` holds exactly the facts of `inputs` (roadmap 6.2 implements it).
pub trait Verifier {
    /// Whether `output` verifies against `inputs`.
    fn verify(&self, output: &LocalFile, inputs: &[&LocalFile]) -> bool;
}

/// A range of positions the scan of the log finds missing (between the batches of a segment, between segments, or
/// below the mark), with what the scan knows about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundLoss {
    /// The first missing position.
    pub first: RecordPosition,
    /// The last missing position.
    pub last: RecordPosition,
    /// The segment, 0 if none.
    pub file_id: u64,
    /// Byte offset.
    pub offset: u64,
    /// Byte length.
    pub length: u64,
    /// The boot id of the start that found it.
    pub boot_id: [u8; 16],
    /// The id floor.
    pub id_floor: u32,
    /// The commit time before it.
    pub time_before: Option<i64>,
    /// The commit time after it.
    pub time_after: Option<i64>,
    /// The cause a recovery entry gives, if one does.
    pub cause: Option<LossCause>,
}

/// Where a live file of the rebuilt manifest is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Location {
    /// Found on the hot path.
    Hot,
    /// Found on the cold path.
    Cold,
    /// No local path holds it and no valid record gives its location: queries that need it return an error, and it
    /// is not a lost range until cold, the mirror path and deep have each answered (`spec.md` §7 step 2).
    Unlocated,
}

/// What a rebuild starts from.
#[derive(Clone, Copy, Debug)]
pub struct RebuildInput<'a> {
    /// The old manifest as read.
    pub image: &'a Image,
    /// Every local file found on the hot and cold paths and under `quarantine/`.
    pub files: &'a [LocalFile],
    /// The losses the scan of the sealed and unsealed log segments finds.
    pub losses: &'a [FoundLoss],
    /// The highest epoch any lease file names.
    pub lease_epoch: u64,
    /// The mark's `file_id`.
    pub mark_file_id: u64,
    /// The wall time for the new records.
    pub written_at: i64,
}

/// The rebuilt manifest and what the rebuild found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebuildPlan {
    /// The new manifest's records, in order: the rebuild record, every valid record carried, the restated records,
    /// an empty record.
    pub records: Vec<Record>,
    /// The rebuild record's epoch.
    pub rebuild_epoch: u64,
    /// The fold of the new records.
    pub catalogue: Catalogue,
    /// Where each live file is.
    pub locations: BTreeMap<FileRef, Location>,
    /// Outputs no valid record names that do not verify against their inputs: discarded, as an unlisted output is
    /// (`spec.md` §9 row 10), once the rebuilt manifest is synced and nothing holds removals.
    pub discard: Vec<FileRef>,
    /// Outputs no valid record names whose live inputs are not local, so they cannot be verified (Q-172): kept on
    /// disk, published by nothing.
    pub undecided: Vec<FileRef>,
    /// Files under `quarantine/`: never live.
    pub quarantined: Vec<FileRef>,
    /// Local files that failed their own checksums or plaintext hash.
    pub invalid: Vec<FileRef>,
}

impl RebuildPlan {
    /// The rebuild record.
    pub fn rebuild_record(&self) -> Option<&Rebuild> {
        match self.records.first().map(|r| &r.body) {
            Some(Body::Rebuild(r)) => Some(r),
            _ => None,
        }
    }

    /// Live files that no local path holds and whose location no valid record gives.
    pub fn unlocated(&self) -> impl Iterator<Item = FileRef> + '_ {
        self.locations
            .iter()
            .filter(|(_, l)| **l == Location::Unlocated)
            .map(|(f, _)| *f)
    }
}

/// Why a manifest cannot be rebuilt.
#[derive(Debug, thiserror::Error)]
pub enum RebuildError {
    /// The corrupt record may be a restore record, which is never rebuilt (`spec.md` §7, §8 Generations): every
    /// generation above 0 begins with one, and no valid record of this generation is one. The store stays
    /// read-only (`manifest_corrupt`); the restore record is read from its local file or deep (roadmap 8.3).
    #[error("generation {0} has no valid restore record: it is never rebuilt")]
    RestoreRecord(u32),
    /// The valid records contradict each other.
    #[error(transparent)]
    Catalogue(#[from] CatalogueError),
    /// A record cannot be encoded.
    #[error(transparent)]
    Encode(#[from] EncodeError),
    /// No epoch is left above the manifest's and the leases'.
    #[error("no manifest epoch is left")]
    EpochsExhausted,
}

/// The fold of `records`, passing over records whose layouts later roadmap items give (they are carried as field
/// lists and change nothing the catalogue holds yet).
fn fold<'a>(records: impl IntoIterator<Item = &'a Record>) -> Result<Catalogue, CatalogueError> {
    let mut c = Catalogue::new();
    for r in records {
        if matches!(r.body, Body::Other { .. }) {
            let mut stand_in = r.clone();
            stand_in.body = Body::Empty;
            c.apply(&stand_in)?;
        } else {
            c.apply(r)?;
        }
    }
    Ok(c)
}

/// How far above the highest id seen each `file_id` floor is (`format.md` A.14, rebuild record field 5).
pub const FLOOR_MARGIN: u64 = 1 << 32;

/// The rebuilt manifest (`spec.md` §7 Manifest, steps 1 to 3; step 4 is [`write`]).
pub fn plan(
    input: &RebuildInput<'_>,
    verifier: &dyn Verifier,
) -> Result<RebuildPlan, RebuildError> {
    let image = input.image;
    let valid: Vec<&Record> = image.records().collect();
    // A restore record is never rebuilt: every generation above 0 present must begin with a valid one.
    let restored: BTreeSet<u32> = valid
        .iter()
        .filter(|r| r.body.record_type() == record_type::RESTORE)
        .map(|r| r.generation)
        .collect();
    if let Some(g) = valid
        .iter()
        .map(|r| r.generation)
        .find(|g| *g > 0 && !restored.contains(g))
    {
        return Err(RebuildError::RestoreRecord(g));
    }
    let old = fold(valid.iter().copied())?;
    let generation = valid.last().map_or(0, |r| r.generation);

    // Step 1: the valid local files.
    let mut local: BTreeMap<FileRef, &LocalFile> = BTreeMap::new();
    let mut quarantined = Vec::new();
    let mut invalid = Vec::new();
    for f in input.files {
        let r = f.description.file_ref();
        if !f.verified {
            invalid.push(r);
        } else if f.place == Place::Quarantine {
            quarantined.push(r);
        } else {
            local.insert(r, f);
        }
    }

    // Step 2: liveness, from the valid records and the files' named inputs alone.
    let mut live: BTreeSet<FileRef> = old.live().map(|e| e.description.file_ref()).collect();
    for q in &quarantined {
        live.remove(q);
    }
    let mut sealed: Vec<FileDescription> = Vec::new();
    let mut published: Vec<FileDescription> = Vec::new();
    let mut retired: BTreeMap<FileRef, FileDescription> = BTreeMap::new();
    let mut discard = Vec::new();
    let mut undecided = Vec::new();
    let describe = |f: &FileRef| -> Option<FileDescription> {
        old.file(*f)
            .map(|e| e.description.clone())
            .or_else(|| local.get(f).map(|l| l.description.clone()))
    };
    // Every local log segment is live; one no valid record lists is listed again.
    for (r, f) in &local {
        if r.kind == kind::LOG_SEGMENT && old.file(*r).is_none() {
            sealed.push(f.description.clone());
            live.insert(*r);
        }
    }
    // A compacted file that a file named by a valid record names as an input is retired: the record that published
    // the output retired its compacted inputs. Where that record is the corrupt one (a file a valid record retires
    // and no valid record published), the retirement is restated.
    let mut gone: BTreeSet<FileRef> = BTreeSet::new();
    for e in old.files() {
        for i in e
            .description
            .inputs
            .iter()
            .filter(|i| i.kind == kind::COMPACTED)
        {
            let unnamed = old.file(*i).is_none();
            if live.remove(i) || unnamed {
                gone.insert(*i);
                if let Some(d) = describe(i) {
                    retired.insert(*i, d);
                }
            }
        }
    }
    // Outputs no valid record names, oldest first, so that an output of an output is judged after it.
    for (r, f) in local
        .iter()
        .filter(|(r, _)| r.kind == kind::COMPACTED && old.file(**r).is_none() && !gone.contains(*r))
    {
        let live_inputs: Vec<FileRef> = f
            .description
            .inputs
            .iter()
            .filter(|i| i.kind == kind::COMPACTED && live.contains(i))
            .copied()
            .collect();
        if live_inputs.is_empty() {
            // It overlaps no live file it names: the corrupt record published it.
            published.push(f.description.clone());
            live.insert(*r);
            continue;
        }
        if live_inputs.iter().any(|i| !local.contains_key(i)) {
            undecided.push(*r);
            continue;
        }
        let inputs: Vec<&LocalFile> = f
            .description
            .inputs
            .iter()
            .filter_map(|i| local.get(i).copied())
            .collect();
        if verifier.verify(f, &inputs) {
            published.push(f.description.clone());
            live.insert(*r);
            for i in live_inputs {
                live.remove(&i);
                // An output restated above and now an input: retired, with its own description.
                if let Some(k) = published.iter().position(|d| d.file_ref() == i) {
                    retired.insert(i, published.remove(k));
                } else if let Some(d) = describe(&i) {
                    retired.insert(i, d);
                }
            }
        } else {
            discard.push(*r);
        }
    }
    let mut locations = BTreeMap::new();
    for f in &live {
        let at = match local.get(f).map(|l| l.place) {
            Some(Place::Hot) => Location::Hot,
            Some(Place::Cold) => Location::Cold,
            _ => Location::Unlocated,
        };
        locations.insert(*f, at);
    }

    // Step 3: lost ranges no valid record holds.
    let held_elsewhere = |l: &FoundLoss| {
        old.files().any(|e| {
            !local.contains_key(&e.description.file_ref())
                && e.description.holds(l.first)
                && e.description.holds(l.last)
        })
    };
    let acks: Vec<_> = old.acknowledged_ranges().collect();
    let several = image.corrupt.len() >= 2;
    let mut restated: Vec<LostRange> = Vec::new();
    let mut identities_rebuilt = 0u32;
    for l in input.losses {
        if held_elsewhere(l)
            || old
                .losses()
                .any(|v| v.record.first <= l.first && l.last <= v.record.last)
        {
            continue;
        }
        let overlapping = old
            .losses()
            .find(|v| v.record.first <= l.last && l.first <= v.record.last);
        let (identity, first, last, cause, extends) = match overlapping {
            Some(v) => (
                v.record.identity,
                v.record.first.min(l.first),
                v.record.last.max(l.last),
                v.record.cause,
                true,
            ),
            None => {
                let named = acks
                    .iter()
                    .find(|(_, a, b)| *a <= l.last && l.first <= *b)
                    .map(|(id, _, _)| *id);
                if named.is_none() && several {
                    identities_rebuilt += 1;
                }
                (
                    named.unwrap_or(l.first),
                    l.first,
                    l.last,
                    l.cause.unwrap_or(LossCause::Unknown),
                    false,
                )
            }
        };
        restated.push(LostRange {
            identity,
            first,
            last,
            file_id: l.file_id,
            offset: l.offset,
            length: l.length,
            cause,
            boot_id: l.boot_id,
            id_floor: l.id_floor,
            time_before: l.time_before,
            time_after: l.time_after,
            extends,
        });
    }

    // Step 4's records: epochs above every epoch a valid record names and every lease.
    let top = valid
        .iter()
        .flat_map(|r| r.epochs_named())
        .chain(image.corrupt.iter().map(|s| s.before_epoch))
        .chain([input.lease_epoch])
        .max()
        .unwrap_or(0);
    let mut epoch = top.checked_add(1).ok_or(RebuildError::EpochsExhausted)?;
    let mut floors: BTreeMap<u8, u64> = BTreeMap::new();
    let mut see = |k: u8, id: u64| {
        let f = floors.entry(k).or_default();
        *f = (*f).max(id);
    };
    see(file_kind::LOG_SEGMENT, input.mark_file_id);
    for f in input.files {
        see(f.description.kind, f.description.file_id);
        for i in &f.description.inputs {
            see(i.kind, i.file_id);
        }
    }
    for k in [kind::LOG_SEGMENT, kind::COMPACTED] {
        see(k, old.id_floor(k));
    }
    let first_span = image.corrupt.first();
    let rebuild_epoch = epoch;
    let mut records = vec![Record::new(
        epoch,
        generation,
        input.written_at,
        Body::Rebuild(Rebuild {
            presumed_epoch: first_span.map_or(0, CorruptSpan::presumed_epoch),
            last_valid_before: first_span.map_or(0, |s| s.after_epoch),
            files_found: local.len() as u64,
            losses_restated: restated.len() as u32,
            floors: floors
                .into_iter()
                .map(|(k, id)| (k, id.saturating_add(FLOOR_MARGIN)))
                .collect(),
            identities_rebuilt,
        }),
    )];
    let next = |epoch: &mut u64| -> Result<u64, RebuildError> {
        *epoch = epoch.checked_add(1).ok_or(RebuildError::EpochsExhausted)?;
        Ok(*epoch)
    };
    for r in &valid {
        let mut c = (*r).clone();
        c.old_epoch = Some(r.old_epoch.unwrap_or(r.epoch));
        c.epoch = next(&mut epoch)?;
        records.push(c);
    }
    if !sealed.is_empty() {
        sealed.sort_by_key(FileDescription::file_ref);
        records.push(Record::new(
            next(&mut epoch)?,
            generation,
            input.written_at,
            Body::Sealed(sealed),
        ));
    }
    if !published.is_empty() || !retired.is_empty() {
        published.sort_by_key(FileDescription::file_ref);
        records.push(Record::new(
            next(&mut epoch)?,
            generation,
            input.written_at,
            Body::Publish(Publish {
                published,
                retired: retired.into_values().collect(),
            }),
        ));
    }
    for l in restated {
        records.push(Record::new(
            next(&mut epoch)?,
            generation,
            input.written_at,
            Body::LostRange(l),
        ));
    }
    records.push(Record::new(
        next(&mut epoch)?,
        generation,
        input.written_at,
        Body::Empty,
    ));
    for r in &records {
        r.encode()?;
    }
    let catalogue = fold(records.iter())?;
    Ok(RebuildPlan {
        records,
        rebuild_epoch,
        catalogue,
        locations,
        discard,
        undecided,
        quarantined,
        invalid,
    })
}

/// The bytes of a manifest holding `records`.
pub fn encode_manifest(
    records: &[Record],
    store_id: [u8; 16],
    generation: u32,
    created_at: i64,
) -> Result<Vec<u8>, EncodeError> {
    let mut out = FileHeader::new(file_kind::MANIFEST, generation, 0, created_at, store_id)
        .encode()
        .to_vec();
    for r in records {
        out.extend(r.encode()?);
    }
    Ok(out)
}

/// Writes `bytes` as `MANIFEST.new`, syncs it, renames it over `MANIFEST` and syncs the directory (`spec.md` §7
/// step 4). No handle may hold `MANIFEST` open. A crash leaves the old manifest, or the new one, and perhaps a
/// `MANIFEST.new` that the next open removes.
pub fn replace_manifest(fs: &dyn Fs, dir: &Path, bytes: &[u8]) -> io::Result<()> {
    remove_leftover(fs, dir)?;
    let new = dir.join(MANIFEST_NEW);
    let f = fs.open(&new, OpenMode::CreateNew)?;
    f.write_at(bytes, 0)?;
    f.sync_all()?;
    drop(f);
    fs.rename(&new, &dir.join(MANIFEST))?;
    fs.sync_dir(dir)
}

/// Writes the rebuilt manifest ([`replace_manifest`]).
pub fn write(
    fs: &dyn Fs,
    dir: &Path,
    store_id: [u8; 16],
    plan: &RebuildPlan,
    created_at: i64,
) -> Result<(), WriteError> {
    let generation = plan.records.first().map_or(0, |r| r.generation);
    let bytes = encode_manifest(&plan.records, store_id, generation, created_at)?;
    replace_manifest(fs, dir, &bytes)?;
    Ok(())
}

/// Why writing a rebuilt or repaired manifest failed.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// A file-system operation failed.
    #[error("manifest: {0}")]
    Io(#[from] io::Error),
    /// A record cannot be encoded.
    #[error(transparent)]
    Encode(#[from] EncodeError),
}

/// Why `rebuild_manifest` failed.
#[derive(Debug, thiserror::Error)]
pub enum RebuildManifestError {
    /// The manifest could not be read.
    #[error(transparent)]
    Read(#[from] ReadError),
    /// It cannot be rebuilt.
    #[error(transparent)]
    Rebuild(#[from] RebuildError),
    /// The new manifest could not be written.
    #[error(transparent)]
    Write(#[from] WriteError),
    /// The new manifest could not be opened.
    #[error(transparent)]
    Open(#[from] OpenError),
    /// The new manifest is not readable as written.
    #[error("the rebuilt manifest does not read back")]
    NotReadBack,
    /// Reading the old manifest failed.
    #[error("manifest: {0}")]
    Io(#[from] io::Error),
}

/// Whether the rebuild is final or provisional.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finality {
    /// Deep has answered that it holds no copy, or none is configured.
    Final,
    /// Deep cannot be reached: uploads, evictions, removals and retirements are held until it answers.
    Provisional,
}

/// The local inputs of a rebuild besides the manifest.
#[derive(Clone, Copy, Debug)]
pub struct LocalState<'a> {
    /// Every local file found.
    pub files: &'a [LocalFile],
    /// The losses the scan of the log finds.
    pub losses: &'a [FoundLoss],
    /// The highest epoch any lease file names.
    pub lease_epoch: u64,
    /// The mark's `file_id`.
    pub mark_file_id: u64,
}

/// The operator call `rebuild_manifest` and the automatic rebuild (`spec.md` §7 Manifest): reads `MANIFEST`,
/// plans the rebuild, writes it, and opens the new manifest, provisional or final. No handle may hold `MANIFEST`.
pub fn rebuild_manifest(
    fs: &dyn Fs,
    dir: &Path,
    store_id: [u8; 16],
    state: LocalState<'_>,
    verifier: &dyn Verifier,
    finality: Finality,
    written_at: i64,
) -> Result<(Appender, RebuildPlan), RebuildManifestError> {
    remove_leftover(fs, dir)?;
    let bytes = read_file(fs, &dir.join(MANIFEST))?;
    let image = read(&bytes, &store_id)?;
    let p = plan(
        &RebuildInput {
            image: &image,
            files: state.files,
            losses: state.losses,
            lease_epoch: state.lease_epoch,
            mark_file_id: state.mark_file_id,
            written_at,
        },
        verifier,
    )?;
    write(fs, dir, store_id, &p, written_at)?;
    let Opened::Ready(mut app) = Appender::open(fs, dir, store_id, state.lease_epoch)? else {
        return Err(RebuildManifestError::NotReadBack);
    };
    app.set_rebuild_state(match finality {
        Finality::Final => RebuildState::Final {
            epoch: p.rebuild_epoch,
        },
        Finality::Provisional => RebuildState::Provisional {
            epoch: p.rebuild_epoch,
        },
    });
    Ok((*app, p))
}

/// The configuration `manifest.rebuild_wait` (`api.md` Configuration).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebuildWait {
    /// Rebuild provisionally after this long without an answer from deep (default 15 minutes).
    After(Duration),
    /// Stay read-only until deep answers.
    Never,
}

impl Default for RebuildWait {
    fn default() -> Self {
        RebuildWait::After(Duration::from_secs(15 * 60))
    }
}

/// What a copy source answered for a corrupt record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// It holds a copy: the record's block.
    Holds(Vec<u8>),
    /// It holds none, or none is configured.
    HoldsNone,
    /// It could not be reached, so nobody can say whether a copy exists.
    Unreachable,
}

/// What the start does next with a corrupt record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Write this copy back ([`repair_with_copy`]).
    Repair(Vec<u8>),
    /// No copy exists: rebuild, final.
    RebuildFinal,
    /// Stay read-only (`manifest_corrupt`), writing nothing, until this monotonic time (`None`: until deep answers);
    /// ask again meanwhile.
    Wait(Option<Duration>),
    /// The wait is over: rebuild provisionally.
    RebuildProvisional,
}

/// A start that found a corrupt manifest record: the wait counted from its first failed ask (`spec.md` §7 Deep
/// unreachable). A restart makes a new one, so the wait starts again.
#[derive(Clone, Copy, Debug)]
pub struct CorruptStart {
    wait: RebuildWait,
    failed_at: Option<Duration>,
}

impl CorruptStart {
    /// A start configured with `wait`.
    pub fn new(wait: RebuildWait) -> Self {
        Self {
            wait,
            failed_at: None,
        }
    }

    /// Decides from the answers of the mirror path and of deep, at monotonic time `now`.
    pub fn decide(&mut self, mirror: &Answer, deep: &Answer, now: Duration) -> Decision {
        match (mirror, deep) {
            (Answer::Holds(c), _) | (_, Answer::Holds(c)) => return Decision::Repair(c.clone()),
            (Answer::HoldsNone, Answer::HoldsNone) => return Decision::RebuildFinal,
            _ => {}
        }
        let since = *self.failed_at.get_or_insert(now);
        match self.wait {
            RebuildWait::Never => Decision::Wait(None),
            RebuildWait::After(d) => {
                let until = since.saturating_add(d);
                if now >= until {
                    Decision::RebuildProvisional
                } else {
                    Decision::Wait(Some(until))
                }
            }
        }
    }
}

/// Why a copy of a corrupt record was not taken.
#[derive(Debug, thiserror::Error)]
pub enum CopyError {
    /// The copy is not a valid record, or its epoch lies outside the corrupt span's.
    #[error("the copy is not the corrupt record")]
    NotTheRecord,
    /// The manifest has no corrupt record.
    #[error("the manifest has no corrupt record")]
    NothingCorrupt,
    /// Writing it failed.
    #[error(transparent)]
    Write(#[from] WriteError),
    /// Appending it failed.
    #[error(transparent)]
    Append(#[from] AppendError),
}

/// The record in `copy` if it is one valid record whose epoch lies inside `span`.
fn check_copy(copy: &[u8], span: &CorruptSpan) -> Result<Record, CopyError> {
    match record_at(copy, 0, span.after_epoch) {
        Ok(Some(p)) if p.end as usize == copy.len() && p.record.epoch < span.before_epoch => {
            Ok(p.record)
        }
        _ => Err(CopyError::NotTheRecord),
    }
}

/// Writes a verified copy of the first corrupt record back (`spec.md` §7 Manifest): in place over the corrupt bytes
/// when it fills them exactly, else by writing the manifest again with the copy in their place, as `MANIFEST.new`
/// renamed over `MANIFEST`. No handle may hold `MANIFEST`. The next open finds any other corrupt record.
pub fn repair_with_copy(
    fs: &dyn Fs,
    dir: &Path,
    image: &Image,
    bytes: &[u8],
    copy: &[u8],
) -> Result<(), CopyError> {
    let span = image.corrupt.first().ok_or(CopyError::NothingCorrupt)?;
    check_copy(copy, span)?;
    if (span.end - span.offset) as usize == copy.len() {
        let f = fs
            .open(&dir.join(MANIFEST), OpenMode::ReadWrite)
            .map_err(WriteError::from)?;
        f.write_at(copy, span.offset)
            .and_then(|()| f.sync_data())
            .map_err(WriteError::from)?;
        return Ok(());
    }
    let mut out = bytes[..span.offset as usize].to_vec();
    out.extend_from_slice(copy);
    out.extend_from_slice(&bytes[span.end as usize..]);
    debug_assert!(out.len() >= HEADER_LEN);
    replace_manifest(fs, dir, &out).map_err(WriteError::from)?;
    Ok(())
}

/// Deep answers after a provisional rebuild (`spec.md` §7 Deep unreachable). With a copy of the corrupt record (the
/// first corrupt span of the old manifest), the copy is appended, carried with its old epoch, so it replaces what
/// was reconstructed; with none, the rebuild is final as it stands. Returns the state after the answer.
pub fn resolve(
    app: &mut Appender,
    span: &CorruptSpan,
    answer: &Answer,
) -> Result<RebuildState, CopyError> {
    let RebuildState::Provisional { epoch } = app.rebuild_state() else {
        return Ok(app.rebuild_state());
    };
    match answer {
        Answer::Unreachable => return Ok(app.rebuild_state()),
        Answer::HoldsNone => {}
        Answer::Holds(copy) => {
            let mut r = check_copy(copy, span)?;
            r.old_epoch = Some(r.old_epoch.unwrap_or(r.epoch));
            app.append_record(r)?;
        }
    }
    app.set_rebuild_state(RebuildState::Final { epoch });
    Ok(app.rebuild_state())
}
