//! Shared test support: generated records, a simulated store history that yields a valid manifest with its local
//! files and found losses, a naive reference fold, and a fake verifier.

#![allow(dead_code)]

pub mod rebuild;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use kvik_files::catalogue::Tier;
use kvik_files::desc::{FileClass, FileDescription, FileRef, kind};
use kvik_files::rebuild::{FoundLoss, LocalFile, Place, Verifier};
use kvik_files::record::{AckedLoss, Acknowledgement, Body, Publish, Rebuild, Record};
use kvik_files::restore::{RestoreRecord, RestoredFile};
use kvik_files::tier::{CopyVerified, DeepCopy, tier_of};
use kvik_log::fs::{Fs, MemFs, OpenMode};
use kvik_log::header::{FileHeader, kind as file_kind};
use kvik_log::manifest::{LossCause, LostRange, RecordPosition};
use kvik_log::mark::ResetStamp;
use kvik_log::setaside::RestoredEnd;
use proptest::prelude::*;

pub const STORE_ID: [u8; 16] = [3; 16];
pub const T0: i64 = 1_700_000_000_000_000_000;

pub fn dir() -> PathBuf {
    PathBuf::from("store")
}

pub fn manifest_path() -> PathBuf {
    dir().join("MANIFEST")
}

/// The bytes of a manifest holding `records`.
pub fn manifest_bytes(records: &[Record]) -> Vec<u8> {
    let mut out = FileHeader::new(file_kind::MANIFEST, 0, 0, T0, STORE_ID)
        .encode()
        .to_vec();
    for r in records {
        out.extend(r.encode().unwrap());
    }
    out
}

/// A file system holding a durable manifest of `bytes`.
pub fn fs_with(bytes: &[u8]) -> MemFs {
    let fs = MemFs::new();
    let f = fs.open(&manifest_path(), OpenMode::CreateNew).unwrap();
    f.write_at(bytes, 0).unwrap();
    f.sync_all().unwrap();
    drop(f);
    fs.sync_dir(&dir()).unwrap();
    fs
}

pub fn hash_of(r: FileRef) -> [u8; 32] {
    let mut h = [0u8; 32];
    h[0] = r.kind;
    h[1..9].copy_from_slice(&r.file_id.to_le_bytes());
    h[31] = 0x5A;
    h
}

pub fn segment(id: u64, first: u64, last: u64) -> FileDescription {
    let mut d = FileDescription {
        kind: kind::LOG_SEGMENT,
        class: FileClass::None,
        generation: 0,
        file_id: id,
        range: Some(((0, first), (0, last))),
        hash: [0; 32],
        inputs: Vec::new(),
    };
    d.hash = hash_of(d.file_ref());
    d
}

pub fn compacted(
    id: u64,
    class: FileClass,
    range: (u64, u64),
    inputs: &[FileRef],
) -> FileDescription {
    let mut inputs = inputs.to_vec();
    inputs.sort();
    let mut d = FileDescription {
        kind: kind::COMPACTED,
        class,
        generation: 0,
        file_id: id,
        range: Some(((0, range.0), (0, range.1))),
        hash: [0; 32],
        inputs,
    };
    d.hash = hash_of(d.file_ref());
    d
}

pub fn seg_ref(id: u64) -> FileRef {
    FileRef {
        kind: kind::LOG_SEGMENT,
        file_id: id,
    }
}

pub fn comp_ref(id: u64) -> FileRef {
    FileRef {
        kind: kind::COMPACTED,
        file_id: id,
    }
}

pub fn lost(identity: u64, first: u64, last: u64, cause: LossCause, extends: bool) -> LostRange {
    LostRange {
        identity: (0, identity),
        first: (0, first),
        last: (0, last),
        file_id: 1,
        offset: 64,
        length: 8 * (last.saturating_sub(first) + 1),
        cause,
        boot_id: [0xB0; 16],
        id_floor: 1,
        time_before: Some(T0),
        time_after: None,
        extends,
        stamp: None,
    }
}

pub fn local(d: &FileDescription, place: Place) -> LocalFile {
    LocalFile {
        description: d.clone(),
        place,
        verified: true,
    }
}

/// A verifier that accepts an output given every one of its inputs, unless its id is listed as bad.
#[derive(Default)]
pub struct FakeVerifier {
    pub bad: BTreeSet<FileRef>,
}

impl Verifier for FakeVerifier {
    fn verify(&self, output: &LocalFile, inputs: &[&LocalFile]) -> bool {
        let given: BTreeSet<FileRef> = inputs.iter().map(|i| i.description.file_ref()).collect();
        !self.bad.contains(&output.description.file_ref())
            && output
                .description
                .inputs
                .iter()
                .filter(|i| i.kind == kind::COMPACTED)
                .all(|i| given.contains(i))
    }
}

// ---- Generated records, valid by construction ----

pub fn position() -> impl Strategy<Value = RecordPosition> {
    (0u32..3, 0u64..1_000_000)
}

pub fn ordered_pair() -> impl Strategy<Value = (RecordPosition, RecordPosition)> {
    (position(), position()).prop_map(|(a, b)| if a <= b { (a, b) } else { (b, a) })
}

pub fn description(k: u8) -> impl Strategy<Value = FileDescription> {
    (
        any::<u32>(),
        1u64..1_000,
        proptest::option::of(ordered_pair()),
        any::<[u8; 32]>(),
        proptest::collection::btree_set((1u8..=2, 1u64..1_000), 1..5),
        any::<bool>(),
    )
        .prop_map(move |(g, id, range, hash, inputs, rev)| {
            if k == kind::LOG_SEGMENT {
                FileDescription {
                    kind: k,
                    class: FileClass::None,
                    generation: g,
                    file_id: id,
                    range,
                    hash,
                    inputs: Vec::new(),
                }
            } else {
                let me = FileRef {
                    kind: k,
                    file_id: id,
                };
                FileDescription {
                    kind: k,
                    class: if rev {
                        FileClass::Revision
                    } else {
                        FileClass::Current
                    },
                    generation: g,
                    file_id: id,
                    range: Some(range.unwrap_or(((0, 1), (0, 2)))),
                    hash,
                    inputs: inputs
                        .into_iter()
                        .map(|(kind, file_id)| FileRef { kind, file_id })
                        .filter(|r| *r != me)
                        .chain(std::iter::once(FileRef {
                            kind: 1,
                            file_id: 1_000 + id,
                        }))
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect(),
                }
            }
        })
}

fn unique(mut v: Vec<FileDescription>) -> Vec<FileDescription> {
    v.sort_by_key(FileDescription::file_ref);
    v.dedup_by_key(|d| d.file_ref());
    v
}

/// A restore record that follows every rule of `format.md` A.14.
pub fn restore_record() -> impl Strategy<Value = RestoreRecord> {
    let file = (
        prop_oneof![description(kind::LOG_SEGMENT), description(kind::COMPACTED)],
        prop_oneof![Just(Tier::Hot), Just(Tier::Cold), Just(Tier::Deep)],
        any::<u64>(),
        any::<[u8; 32]>(),
    )
        .prop_map(|(description, tier, size, sha256)| RestoredFile {
            description,
            tier,
            copy: DeepCopy { size, sha256 },
        });
    (
        (2u32..u32::MAX, any::<prop::sample::Index>(), 1u64..),
        (
            any::<u64>(),
            proptest::collection::vec(any::<prop::sample::Index>(), 0..4),
        ),
        proptest::collection::vec(lost_range(), 0..3),
        proptest::collection::vec((position(), ordered_pair()), 0..3),
        proptest::collection::vec(lost_range(), 0..2),
        proptest::collection::vec(file, 0..4),
        any::<bool>(),
    )
        .prop_map(
            |(
                (generation, src, nonce),
                (end_seq, ends),
                losses,
                acks,
                entry_losses,
                files,
                drill,
            )| {
                let source_generation = src.index(generation as usize) as u32;
                let mut carried: Vec<u32> = ends
                    .iter()
                    .map(|i| i.index(source_generation as usize + 1) as u32)
                    .filter(|g| *g > 0)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                carried.reverse();
                let mut losses = losses;
                losses.sort_by_key(|l| l.identity);
                losses.dedup_by_key(|l| l.identity);
                let mut acknowledgements: Vec<AckedLoss> = acks
                    .into_iter()
                    .map(|(identity, (first, last))| AckedLoss {
                        identity,
                        first,
                        last,
                    })
                    .collect();
                acknowledgements.sort();
                acknowledgements.dedup();
                let mut files: Vec<RestoredFile> = files
                    .into_iter()
                    .map(|f| (f.description.file_ref(), f))
                    .collect::<BTreeMap<_, _>>()
                    .into_values()
                    .collect();
                files.sort_by_key(|f| {
                    (
                        f.description.kind,
                        f.description.generation,
                        f.description.file_id,
                    )
                });
                RestoreRecord {
                    generation,
                    stamp: ResetStamp { sequence: 1, nonce },
                    source_generation,
                    restored_end: (source_generation, end_seq),
                    carried_ends: carried
                        .into_iter()
                        .map(|g| RestoredEnd {
                            generation: g,
                            end: (g - 1, end_seq),
                        })
                        .collect(),
                    losses,
                    acknowledgements,
                    entry_losses,
                    files,
                    drill,
                }
            },
        )
}

pub fn lost_range() -> impl Strategy<Value = LostRange> {
    (
        position(),
        ordered_pair(),
        any::<(u64, u64, u64)>(),
        1u8..=5,
        any::<[u8; 16]>(),
        any::<u32>(),
        proptest::option::of(any::<i64>()),
        proptest::option::of(any::<i64>()),
        any::<bool>(),
        proptest::option::of(any::<(u64, u64)>()),
    )
        .prop_map(
            |(identity, (first, last), (f, o, l), c, boot, floor, tb, ta, ext, st)| LostRange {
                identity,
                first,
                last,
                file_id: f,
                offset: o,
                length: l,
                cause: match c {
                    1 => LossCause::Corruption,
                    2 => LossCause::TruncatedTail,
                    3 => LossCause::LostInBucket,
                    4 => LossCause::InvalidContent,
                    _ => LossCause::Unknown,
                },
                boot_id: boot,
                id_floor: floor,
                time_before: tb,
                time_after: ta,
                extends: ext,
                stamp: st.map(|(sequence, nonce)| kvik_log::mark::ResetStamp { sequence, nonce }),
            },
        )
}

pub fn body() -> impl Strategy<Value = Body> {
    prop_oneof![
        Just(Body::Empty),
        (
            proptest::collection::vec(prop_oneof![description(1), description(2)], 0..4),
            proptest::collection::vec(prop_oneof![description(1), description(2)], 0..4),
        )
            .prop_map(|(p, r)| {
                let p = unique(p);
                let r: Vec<_> = unique(r)
                    .into_iter()
                    .filter(|d| !p.iter().any(|x| x.file_ref() == d.file_ref()))
                    .collect();
                if p.is_empty() && r.is_empty() {
                    Body::Empty
                } else {
                    Body::Publish(Publish {
                        published: p,
                        retired: r,
                    })
                }
            }),
        proptest::collection::vec(description(1), 1..4).prop_map(|v| Body::Sealed(unique(v))),
        lost_range().prop_map(Body::LostRange),
        (
            proptest::collection::btree_map(position(), ordered_pair(), 0..4),
            proptest::option::of(any::<u64>()),
        )
            .prop_map(|(m, rebuild)| {
                let losses: Vec<_> = m
                    .into_iter()
                    .map(|(identity, (first, last))| AckedLoss {
                        identity,
                        first,
                        last,
                    })
                    .collect();
                if losses.is_empty() && rebuild.is_none() {
                    Body::Empty
                } else {
                    Body::Acknowledgement(Acknowledgement { losses, rebuild })
                }
            }),
        (
            any::<(u64, u64, u64, u32, u32)>(),
            proptest::collection::btree_map(1u8..15, any::<u64>(), 0..4),
            proptest::collection::btree_set(1..u64::MAX, 0..6),
            any::<bool>(),
        )
            .prop_map(
                |((a, b, c, d, e), floors, ends, open)| Body::Rebuild(Rebuild {
                    presumed_epoch: a,
                    last_valid_before: b,
                    files_found: c,
                    losses_restated: d,
                    floors: floors.into_iter().collect(),
                    identities_rebuilt: e,
                    gaps: ends
                        .into_iter()
                        .collect::<Vec<_>>()
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|w| (w[0], Some(w[1])))
                        .chain(open.then_some((u64::MAX, None)))
                        .collect(),
                })
            ),
        (
            prop_oneof![Just(0x26u8), 0x29u8..0x7F],
            proptest::collection::vec(
                (1u16..0x9000, proptest::collection::vec(any::<u8>(), 0..20)),
                0..4
            ),
        )
            .prop_map(|(t, f)| {
                let mut fields: Vec<_> = f.into_iter().filter(|(id, _)| *id != 100).collect();
                fields.sort_by_key(|(id, _)| *id);
                Body::Other {
                    record_type: t,
                    skippable: !(0x25..=0x26).contains(&t),
                    fields,
                }
            }),
        (
            description(kind::LOG_SEGMENT),
            1u8..4,
            any::<u64>(),
            any::<[u8; 32]>()
        )
            .prop_map(|(file, tier, size, sha256)| {
                CopyVerified {
                    file,
                    tier: tier_of(tier).unwrap(),
                    copy: DeepCopy { size, sha256 },
                }
                .to_body()
            }),
        restore_record().prop_map(|r| r.to_body()),
    ]
}

pub fn record() -> impl Strategy<Value = Record> {
    (
        1u64..u64::MAX,
        any::<u32>(),
        any::<i64>(),
        proptest::option::of(any::<u64>()),
        body(),
        proptest::collection::btree_map(
            0x8000u16..=0xFFFF,
            proptest::collection::vec(any::<u8>(), 0..12),
            0..3,
        ),
    )
        .prop_map(|(epoch, generation, written_at, old_epoch, body, ign)| {
            let ignorable = match body {
                Body::Other { .. } => Vec::new(),
                _ => ign.into_iter().collect(),
            };
            Record {
                epoch,
                generation,
                written_at,
                old_epoch,
                body,
                ignorable,
            }
        })
}

// ---- A simulated store history ----

/// One step of a store's life, as the manifest sees it.
#[derive(Clone, Copy, Debug)]
pub enum Step {
    /// A segment is sealed, holding `len` positions, perhaps with a corrupt region found in it.
    Seal {
        len: u64,
        lost: Option<u64>,
        cause_given: bool,
    },
    /// Every sealed segment not yet compacted is compacted into one or two outputs.
    Compact { two: bool },
    /// Live compacted files are merged into one output.
    Merge,
    /// A loss grows by one position.
    Extend(usize),
    /// Losses are acknowledged.
    Acknowledge(usize),
    /// An empty record.
    Empty,
    /// The next epoch jumps, as after a start above a lease.
    Jump(u64),
    /// Retired compacted files are removed locally (after their grace period, once replaced and verified
    /// elsewhere).
    RemoveRetired,
}

pub fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        4 => (4u64..12, proptest::option::of(1u64..3), any::<bool>())
            .prop_map(|(len, lost, cause_given)| Step::Seal { len, lost, cause_given }),
        2 => any::<bool>().prop_map(|two| Step::Compact { two }),
        2 => Just(Step::Merge),
        1 => any::<usize>().prop_map(Step::Extend),
        1 => any::<usize>().prop_map(Step::Acknowledge),
        1 => Just(Step::Empty),
        1 => (1u64..5).prop_map(Step::Jump),
        1 => Just(Step::RemoveRetired),
    ]
}

/// A store's history: its manifest records, its local files and what the scan of its log finds.
#[derive(Clone, Debug, Default)]
pub struct History {
    pub records: Vec<Record>,
    pub files: Vec<LocalFile>,
    pub losses: Vec<FoundLoss>,
    pub mark_file_id: u64,
}

#[derive(Clone, Debug)]
struct SimLoss {
    identity: u64,
    first: u64,
    last: u64,
    segment_last: u64,
    file_id: u64,
    cause_given: bool,
}

/// Plays `steps` and returns the history. Every record is valid and the catalogue accepts every one.
pub fn play(steps: &[Step]) -> History {
    let mut h = History::default();
    let mut epoch = 1u64;
    h.records.push(Record::new(1, 0, T0, Body::Empty));
    let mut seq = 1u64;
    let mut next_seg = 1u64;
    let mut next_comp = 1u64;
    let mut uncompacted: Vec<FileDescription> = Vec::new();
    let mut live_comp: Vec<FileDescription> = Vec::new();
    let mut losses: Vec<SimLoss> = Vec::new();
    let mut retired_refs: Vec<FileRef> = Vec::new();
    let mut gap = 0u64;
    let push = |h: &mut History, epoch: &mut u64, gap: &mut u64, body: Body| {
        *epoch += 1 + std::mem::take(gap);
        h.records
            .push(Record::new(*epoch, 0, T0 + *epoch as i64, body));
    };
    for s in steps {
        match *s {
            Step::Seal {
                len,
                lost: l,
                cause_given,
            } => {
                let d = segment(next_seg, seq, seq + len - 1);
                h.files.push(local(&d, Place::Hot));
                push(&mut h, &mut epoch, &mut gap, Body::Sealed(vec![d.clone()]));
                if let Some(n) = l {
                    let first = seq + 1;
                    let last = first + n - 1;
                    let cause = if cause_given {
                        LossCause::TruncatedTail
                    } else {
                        LossCause::Corruption
                    };
                    push(
                        &mut h,
                        &mut epoch,
                        &mut gap,
                        Body::LostRange(LostRange {
                            file_id: next_seg,
                            ..lost(first, first, last, cause, false)
                        }),
                    );
                    losses.push(SimLoss {
                        identity: first,
                        first,
                        last,
                        segment_last: seq + len - 2,
                        file_id: next_seg,
                        cause_given,
                    });
                }
                uncompacted.push(d);
                seq += len;
                next_seg += 1;
                h.mark_file_id = next_seg;
            }
            Step::Compact { two } => {
                if uncompacted.is_empty() {
                    continue;
                }
                let inputs: Vec<FileRef> =
                    uncompacted.iter().map(FileDescription::file_ref).collect();
                let lo = uncompacted[0].range.unwrap().0.1;
                let hi = uncompacted.last().unwrap().range.unwrap().1.1;
                let mut outs = vec![compacted(next_comp, FileClass::Current, (lo, hi), &inputs)];
                next_comp += 1;
                if two {
                    outs.push(compacted(next_comp, FileClass::Revision, (lo, hi), &inputs));
                    next_comp += 1;
                }
                for o in &outs {
                    h.files.push(local(o, Place::Hot));
                }
                push(
                    &mut h,
                    &mut epoch,
                    &mut gap,
                    Body::Publish(Publish {
                        published: outs.clone(),
                        retired: Vec::new(),
                    }),
                );
                live_comp.extend(outs);
                uncompacted.clear();
            }
            Step::Merge => {
                let current: Vec<FileDescription> = live_comp
                    .iter()
                    .filter(|d| d.class == FileClass::Current)
                    .cloned()
                    .collect();
                if current.len() < 2 {
                    continue;
                }
                let inputs: Vec<FileRef> = current.iter().map(FileDescription::file_ref).collect();
                let lo = current.iter().map(|d| d.range.unwrap().0.1).min().unwrap();
                let hi = current.iter().map(|d| d.range.unwrap().1.1).max().unwrap();
                let out = compacted(next_comp, FileClass::Current, (lo, hi), &inputs);
                next_comp += 1;
                h.files.push(local(&out, Place::Hot));
                push(
                    &mut h,
                    &mut epoch,
                    &mut gap,
                    Body::Publish(Publish {
                        published: vec![out.clone()],
                        retired: current.clone(),
                    }),
                );
                retired_refs.extend(inputs);
                live_comp.retain(|d| d.class != FileClass::Current);
                live_comp.push(out);
            }
            Step::Extend(i) => {
                if losses.is_empty() {
                    continue;
                }
                let n = losses.len();
                let l = &mut losses[i % n];
                if l.last >= l.segment_last {
                    continue;
                }
                l.last += 1;
                let cause = if l.cause_given {
                    LossCause::TruncatedTail
                } else {
                    LossCause::Corruption
                };
                let rec = LostRange {
                    file_id: l.file_id,
                    ..lost(l.identity, l.first, l.last, cause, true)
                };
                push(&mut h, &mut epoch, &mut gap, Body::LostRange(rec));
            }
            Step::Acknowledge(i) => {
                if losses.is_empty() {
                    continue;
                }
                let l = &losses[i % losses.len()];
                push(
                    &mut h,
                    &mut epoch,
                    &mut gap,
                    Body::Acknowledgement(Acknowledgement {
                        losses: vec![AckedLoss {
                            identity: (0, l.identity),
                            first: (0, l.first),
                            last: (0, l.last),
                        }],
                        rebuild: None,
                    }),
                );
            }
            Step::Empty => push(&mut h, &mut epoch, &mut gap, Body::Empty),
            Step::RemoveRetired => {
                h.files
                    .retain(|f| !retired_refs.contains(&f.description.file_ref()));
            }
            Step::Jump(n) => gap = n,
        }
    }
    h.losses = losses
        .iter()
        .map(|l| FoundLoss {
            first: (0, l.first),
            last: (0, l.last),
            file_id: l.file_id,
            offset: 64,
            length: 8 * (l.last - l.first + 1),
            boot_id: [0xB0; 16],
            id_floor: 1,
            time_before: Some(T0),
            time_after: None,
            cause: l.cause_given.then_some(LossCause::TruncatedTail),
        })
        .collect();
    h
}

pub fn history() -> impl Strategy<Value = History> {
    proptest::collection::vec(step(), 1..40).prop_map(|s| play(&s))
}

// ---- A naive reference fold ----

/// The files listed at `epoch` by a plain scan: published or sealed at or before it and not retired at or before it.
pub fn naive_listed_at(records: &[Record], epoch: u64) -> BTreeSet<FileRef> {
    let mut set = BTreeSet::new();
    let mut retired = BTreeSet::new();
    for r in records.iter().filter(|r| r.epoch <= epoch) {
        match &r.body {
            Body::Publish(p) => {
                set.extend(p.published.iter().map(FileDescription::file_ref));
                retired.extend(p.retired.iter().map(FileDescription::file_ref));
            }
            Body::Sealed(s) => set.extend(s.iter().map(FileDescription::file_ref)),
            _ => {}
        }
    }
    set.difference(&retired).copied().collect()
}

/// The losses by a plain scan: identity to (first, last, acknowledged).
pub fn naive_losses(
    records: &[Record],
) -> BTreeMap<RecordPosition, (RecordPosition, RecordPosition, bool)> {
    let mut out: BTreeMap<RecordPosition, (RecordPosition, RecordPosition)> = BTreeMap::new();
    let mut acks: Vec<AckedLoss> = Vec::new();
    for r in records {
        match &r.body {
            Body::LostRange(l) => {
                let e = out.entry(l.identity).or_insert((l.first, l.last));
                e.0 = e.0.min(l.first);
                e.1 = e.1.max(l.last);
            }
            Body::Acknowledgement(a) => acks.extend(a.losses.iter().copied()),
            _ => {}
        }
    }
    out.into_iter()
        .map(|(id, (a, b))| {
            let acked = acks
                .iter()
                .any(|k| k.identity == id && k.first <= a && b <= k.last);
            (id, (a, b, acked))
        })
        .collect()
}

/// Damages the record at `index` of a manifest of `records`: one bit of its body checksum.
pub fn corrupt_record(bytes: &mut [u8], records: &[Record], index: usize) {
    let mut at = 64usize;
    for r in &records[..index] {
        at += r.encode().unwrap().len();
    }
    bytes[at + 8] ^= 0x01;
}

pub fn read_all(fs: &MemFs, p: &Path) -> Vec<u8> {
    fs.read(p).unwrap()
}
