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
//! - [`write()`] and [`rebuild_manifest`]: `MANIFEST.new`, synced, renamed over `MANIFEST` while no handle holds it,
//!   and the directory synced; then the manifest is opened again.
//! - [`resolve`]: deep answers after a provisional rebuild. A copy of the corrupt record is appended, carried with its
//!   old epoch, over what was reconstructed; an answer that it holds none makes the rebuild final.
//!
//! The seams are injected: the local files come described ([`LocalFile`]), an output is verified against its inputs
//! by a [`Verifier`] (the §7 procedure, roadmap 6.2), and the losses come from the log's scan ([`FoundLoss`]).
//! What a rebuild cannot settle from the local files is reported, never guessed: a file that no local path holds and
//! whose location no valid record gives is **unlocated**.

use std::collections::{BTreeMap, BTreeSet};

use kvik_log::header::kind as file_kind;
use kvik_log::manifest::{LossCause, LostRange, RecordPosition};

use crate::catalogue::{Catalogue, CatalogueError};
use crate::desc::{FileDescription, FileRef, kind};
use crate::image::{CorruptSpan, Image};
use crate::layout::CompactedLayout;
use crate::record::{Body, EncodeError, Publish, Rebuild, Record, record_type};

mod copies;
mod write;

pub use copies::{
    Answer, CopyError, CorruptStart, Decision, RebuildWait, repair_with_copy, resolve,
};
pub use write::{
    Finality, LocalState, RebuildManifestError, WriteError, encode_manifest, rebuild_manifest,
    replace_manifest, write,
};

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
    /// A compacted file's layout, read from its own description and trailer, for a restated publish record.
    pub layout: Option<CompactedLayout>,
}

/// What the §7 procedure says of a set of outputs against their inputs (compaction design §3.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The outputs hold exactly the positions of their inputs.
    Holds,
    /// The outputs fail a check of compaction design §3.2 against their inputs.
    Fails,
    /// Neither can be said: the outputs found are not the whole run, or a check could not run.
    CannotSay,
}

/// The §7 procedure, judging the files of one run found locally against that run's inputs (compaction design §3.5).
pub trait Verifier {
    /// What `outputs`, the files of one run no valid record names, are against `inputs`.
    fn verify(&self, outputs: &[&LocalFile], inputs: &[&LocalFile]) -> Verdict;
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
    /// Outputs no valid record names that can be neither published nor discarded (compaction design §3.5): kept,
    /// unlisted, and held through [`crate::appender::Appender::hold_unlocated`] until a record names them (Q-2122).
    pub held: Vec<FileRef>,
    /// Files under `quarantine/`: never live.
    pub quarantined: Vec<FileRef>,
    /// Compacted files a corrupt record retired, named only as an input, that neither a valid record nor a local
    /// file describes, so their own inputs cannot be followed (Q-175).
    pub undescribed: Vec<FileRef>,
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

/// The candidates grouped into runs: outputs that share an input, transitively. A log segment is the input of one
/// run (compaction design §3.4 rule 1) and a compacted file of one merge, so the files of a run share their inputs.
fn runs(candidates: &[(FileRef, &LocalFile)]) -> Vec<Vec<FileRef>> {
    let mut groups: Vec<(BTreeSet<FileRef>, Vec<FileRef>)> = Vec::new();
    for (r, f) in candidates {
        let mut inputs: BTreeSet<FileRef> = f.description.inputs.iter().copied().collect();
        let mut members = vec![*r];
        let (joined, rest): (Vec<_>, Vec<_>) = groups
            .into_iter()
            .partition(|(i, _)| !i.is_disjoint(&inputs));
        for (i, m) in joined {
            inputs.extend(i);
            members.extend(m);
        }
        members.sort();
        groups = rest;
        groups.push((inputs, members));
    }
    groups.into_iter().map(|(_, m)| m).collect()
}

/// The layouts of a restated publish record: every published compacted file's, from the file, or none when one is
/// not known.
fn restated_layouts(
    published: &[FileDescription],
    local: &BTreeMap<FileRef, &LocalFile>,
) -> Vec<CompactedLayout> {
    published
        .iter()
        .filter(|d| d.kind == kind::COMPACTED)
        .map(|d| local.get(&d.file_ref()).and_then(|l| l.layout.clone()))
        .collect::<Option<Vec<_>>>()
        .unwrap_or_default()
}

/// How far above the highest id seen each `file_id` floor is (`format.md` A.14, rebuild record field 5).
pub const FLOOR_MARGIN: u64 = 1 << 32;

/// The rebuilt manifest (`spec.md` §7 Manifest, steps 1 to 3; step 4 is [`write()`]).
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
        .or_else(|| image.missing_restore_record())
    {
        return Err(RebuildError::RestoreRecord(g));
    }
    let old = Catalogue::replay(valid.iter().copied())?;
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
    // and no valid record published), the retirement is restated. An input no valid record names was itself an
    // output of a corrupt record, so its own inputs are followed, through its description, to a fixpoint.
    let mut gone: BTreeSet<FileRef> = BTreeSet::new();
    let mut undescribed = Vec::new();
    let mut work: Vec<FileDescription> = old.files().map(|e| e.description.clone()).collect();
    while let Some(d) = work.pop() {
        for i in d.inputs.iter().filter(|i| i.kind == kind::COMPACTED) {
            let unnamed = old.file(*i).is_none();
            if (live.remove(i) || unnamed) && gone.insert(*i) {
                match describe(i) {
                    Some(di) => {
                        if unnamed {
                            work.push(di.clone());
                        }
                        retired.insert(*i, di);
                    }
                    None => undescribed.push(*i),
                }
            }
        }
    }
    // Outputs no valid record names, oldest first, so that an output of an output is judged after it.
    let candidates: Vec<(FileRef, &LocalFile)> = local
        .iter()
        .filter(|(r, _)| r.kind == kind::COMPACTED && old.file(**r).is_none() && !gone.contains(*r))
        .map(|(r, f)| (*r, *f))
        .collect();
    // Files a valid record names, published, retired or as an input, with their inputs: a sibling among them makes
    // its run's other outputs never discarded (compaction design §3.5).
    let named: Vec<FileDescription> = old
        .files()
        .map(|e| e.description.clone())
        .chain(gone.iter().filter_map(&describe))
        .collect();
    let mut verdicts: BTreeMap<FileRef, Verdict> = BTreeMap::new();
    for run in runs(&candidates) {
        let files: Vec<&LocalFile> = run.iter().map(|r| local[r]).collect();
        let inputs_of: BTreeSet<FileRef> = files
            .iter()
            .flat_map(|f| f.description.inputs.iter().copied())
            .collect();
        let sibling_named = named
            .iter()
            .any(|d| d.inputs.iter().any(|i| inputs_of.contains(i)));
        let all_at_hand = files.iter().all(|f| {
            f.description
                .inputs
                .iter()
                .filter(|i| {
                    i.kind == kind::LOG_SEGMENT || live.contains(i) || old.file(**i).is_none()
                })
                .all(|i| local.contains_key(i))
        });
        let inputs: Vec<&LocalFile> = inputs_of
            .iter()
            .filter_map(|i| local.get(i).copied())
            .collect();
        let v = if all_at_hand {
            verifier.verify(&files, &inputs)
        } else {
            Verdict::CannotSay
        };
        let v = match v {
            Verdict::Fails if sibling_named => Verdict::CannotSay,
            v => v,
        };
        for r in run {
            verdicts.insert(r, v);
        }
    }
    let mut held = Vec::new();
    for (r, f) in candidates {
        let r = &r;
        let live_inputs: Vec<FileRef> = f
            .description
            .inputs
            .iter()
            .filter(|i| i.kind == kind::COMPACTED && live.contains(i))
            .copied()
            .collect();
        // Verified against its inputs (its log segments and its live compacted inputs) where they are local. Where
        // one is not, liveness follows from the named inputs alone: an input leaves the local tiers only once its
        // replacement is verified in another tier (`spec.md` §7 inputs kept), so the output was published and
        // verified.
        // An input that no valid record names and no local path holds was an output of a corrupt record retired
        // since, described nowhere (Q-175): it is not at hand either, and an output is never discarded because an
        // input is missing.
        let at_hand = f
            .description
            .inputs
            .iter()
            .filter(|i| {
                i.kind == kind::LOG_SEGMENT || live_inputs.contains(i) || old.file(**i).is_none()
            })
            .all(|i| local.contains_key(i));
        let verdict = verdicts[r];
        if verdict == Verdict::CannotSay && at_hand {
            held.push(*r);
            continue;
        }
        if !at_hand || verdict == Verdict::Holds {
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
            // Such an input's own inputs cannot be followed (Q-175).
            for i in f.description.inputs.iter().filter(|i| {
                i.kind == kind::COMPACTED && old.file(**i).is_none() && !local.contains_key(*i)
            }) {
                if gone.insert(*i) {
                    undescribed.push(*i);
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

    // Step 3: lost ranges no valid record holds. A position held by a file a valid record names is not missing:
    // the scan cannot see a live log segment that is on no local path at all (moved to deep), so a range that such a
    // segment holds is not a loss. A segment found locally, valid or not, or under `quarantine/`, is the scan's to
    // judge.
    let found: BTreeSet<FileRef> = input
        .files
        .iter()
        .map(|f| f.description.file_ref())
        .collect();
    let held_elsewhere = |l: &FoundLoss| {
        old.live().any(|e| {
            let d = &e.description;
            d.kind == kind::LOG_SEGMENT
                && !found.contains(&d.file_ref())
                && d.holds(l.first)
                && d.holds(l.last)
        })
    };
    let acks: Vec<_> = old.acknowledged_ranges().collect();
    // Two corrupt records, or one corrupt span whose epochs leave room for more than one record (Q-174); a span with
    // no later record may hold any number (Q-650).
    let several = image.corrupt.len() >= 2
        || image.corrupt.iter().any(|s| {
            s.before_epoch
                .is_none_or(|b| b.saturating_sub(s.after_epoch) > 2)
        });
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
            // Restated with no stamp known: later than every reset (`spec.md` §7 Readers).
            stamp: None,
        });
    }

    // Step 4's records: epochs above every epoch a valid record names, every lease and every epoch a corrupt span
    // may hold (Q-872).
    let top = valid
        .iter()
        .flat_map(|r| r.epochs_named())
        .chain(image.corrupt.iter().map(|s| s.epoch_ceiling()))
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
            gaps: image
                .corrupt
                .iter()
                .map(|s| (s.after_epoch, s.before_epoch))
                .collect(),
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
                layouts: restated_layouts(&published, &local),
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
    let catalogue = Catalogue::replay(records.iter())?;
    Ok(RebuildPlan {
        records,
        rebuild_epoch,
        catalogue,
        locations,
        discard,
        held,
        quarantined,
        invalid,
        undescribed,
    })
}
