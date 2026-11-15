//! The restore plan (`kvik_backup::plan`) against the reference model's `Model::restore_from` on generated histories:
//! ingest, seals, uploads that the bucket rejects or that cannot reach it, and crashes. The model's sealed segments
//! become sealed-segment records, and its bucket's objects copy-verified records (a placeholder of type `0x27`
//! folded by `Catalogue::record_copy` until roadmap 8.1c lays that record out), sometimes carried into a whole
//! rebuilt manifest that the restore reads instead (Q-108, Q-295). Both must give the same restored end,
//! list the same files and set aside the same ones. Where nothing was set aside, the restored model runs on in its
//! generation and is restored again, from the first plan's record and the new generation's records.
//!
//! Left out, with the reason (`model/OPEN-QUESTIONS.md`):
//! - Lost ranges: the model's restored end stops at one, the specification's does not (Q-294).
//! - A second restore after one that set positions aside: the model reads every generation's files (the model
//!   differences table, "Restore record and the files a restore reads").
//! - Recovery entries: the model writes none; the plan's rule for them is pinned in `backup/tests/restore_plan.rs`.
//! - A bucket holding no file: the model refuses it, the plan does not (Q-294).

use std::collections::BTreeSet;

use kvik_backup::plan::{PlanInput, PlanMode, RestorePlan, plan, records_to_read};
use kvik_files::catalogue::{Catalogue, Tier};
use kvik_files::desc::{FileClass, FileDescription, FileRef, kind};
use kvik_files::record::{Body, Rebuild, Record, record_type};
use kvik_files::tier::DeepCopy;
use kvik_model::engine::{Config, FileClass as ModelClass, FileId, Model};
use kvik_model::registry::SeriesOptions;
use kvik_model::types::{Kind, Nanos, Position, Reading, SECOND, SeriesId, ValueType};
use proptest::prelude::*;

const T0: Nanos = 1_774_742_400 * SECOND;

#[derive(Clone, Copy, Debug)]
enum Op {
    Ingest(u8),
    Seal,
    Upload,
    UploadRejected,
    Crash,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (1u8..4).prop_map(Op::Ingest),
        3 => Just(Op::Seal),
        2 => Just(Op::Upload),
        1 => Just(Op::UploadRejected),
        1 => Just(Op::Crash),
    ]
}

struct Run {
    m: Model,
    s: SeriesId,
    t: Nanos,
}

impl Run {
    fn apply(&mut self, op: Op) {
        match op {
            Op::Ingest(n) => {
                for _ in 0..n {
                    self.t += SECOND;
                    self.m
                        .ingest(vec![Reading::num(self.s, self.t, 1.0)])
                        .unwrap();
                }
            }
            Op::Seal => self.m.seal_active(),
            Op::Upload => {
                self.m.upload();
            }
            Op::UploadRejected => {
                self.m.disk.bucket.corrupt_next_upload = true;
                self.m.upload();
                self.m.disk.bucket.corrupt_next_upload = false;
            }
            Op::Crash => {
                self.m.crash();
                self.m.restart();
            }
        }
    }

    /// Ends the history as a store whose last segment is sealed and offered for upload.
    fn finish(&mut self) {
        self.m.seal_active();
    }
}

fn description(id: FileId, positions: &[Position], generation: u32) -> FileDescription {
    FileDescription {
        kind: kind::LOG_SEGMENT,
        class: FileClass::None,
        generation,
        file_id: id.0,
        range: positions.first().map(|a| {
            let b = positions.last().unwrap();
            ((a.generation, a.sequence), (b.generation, b.sequence))
        }),
        hash: [id.0 as u8; 32],
        inputs: Vec::new(),
    }
}

fn copy_of(id: u64) -> DeepCopy {
    DeepCopy {
        size: id,
        sha256: [id as u8; 32],
    }
}

/// The records of `m`'s sealed segments of generation `g` with ids from `from`, after `head`, then their copies; with
/// `rebuild`, a whole rebuilt manifest carrying the records before some point, which is what a restore then reads.
fn records(
    m: &Model,
    g: u32,
    from: u64,
    head: Vec<Body>,
    rebuild: Option<prop::sample::Index>,
) -> Catalogue {
    let mut bodies = head;
    let mut sealed = BTreeSet::new();
    for (id, f) in &m.disk.files {
        if f.class != ModelClass::Segment || !f.sealed || id.0 < from {
            continue;
        }
        let ps: Vec<Position> = f.entries().map(|e| e.position).collect();
        bodies.push(Body::Sealed(vec![description(*id, &ps, g)]));
        sealed.insert(id.0);
    }
    for id in m
        .disk
        .bucket
        .objects
        .keys()
        .filter(|f| sealed.contains(&f.0))
    {
        bodies.push(Body::Other {
            record_type: record_type::TIER_MOVE,
            skippable: false,
            fields: vec![(1, id.0.to_le_bytes().to_vec())],
        });
    }
    let mut list: Vec<(Body, Option<u64>)> = bodies.into_iter().map(|b| (b, None)).collect();
    if let Some(at) = rebuild {
        let cut = 1 + at.index(list.len());
        let carried = list[..cut]
            .iter()
            .enumerate()
            .map(|(i, (b, _))| (b.clone(), Some(i as u64 + 1)));
        let mut whole = list[..cut].to_vec();
        whole.push((Body::Rebuild(Rebuild::default()), None));
        whole.extend(carried);
        whole.push((Body::Empty, None));
        whole.extend(list[cut..].iter().cloned());
        list = whole;
    }
    let records: Vec<Record> = list
        .into_iter()
        .enumerate()
        .map(|(i, (b, old))| Record {
            old_epoch: old,
            ..Record::new(i as u64 + 1, g, 0, b)
        })
        .collect();
    let mut c = Catalogue::new();
    for r in records_to_read(&records) {
        c.apply(r).unwrap();
        if let Body::Other {
            record_type: record_type::TIER_MOVE,
            fields,
            ..
        } = &r.body
        {
            let id = u64::from_le_bytes(fields[0].1[..].try_into().unwrap());
            let f = FileRef {
                kind: kind::LOG_SEGMENT,
                file_id: id,
            };
            c.record_copy(f, Tier::Deep, copy_of(id));
        }
    }
    c
}

/// Restores `m`'s bucket with the model and plans the same restore; checks they agree and returns both.
fn restore_both(m: &mut Model, c: &Catalogue) -> Result<(Model, RestorePlan), TestCaseError> {
    let r =
        Model::restore_from(m.config.clone(), vec![], &mut m.disk.bucket, m.wall, false).unwrap();
    let state = r.disk.restore.clone().unwrap();
    let p = plan(&PlanInput {
        generation: state.generation,
        nonce: 1,
        source_generation: c.generation(),
        source: c,
        mode: PlanMode::Full,
        recovery_after: &[],
    })
    .unwrap();
    let end = state.restored_end;
    prop_assert_eq!(p.record.restored_end, (end.generation, end.sequence));
    let model_files: BTreeSet<u64> = state.sources.iter().map(|f| f.0).collect();
    let planned: BTreeSet<u64> = p
        .record
        .files
        .iter()
        .map(|f| f.description.file_id)
        .collect();
    prop_assert_eq!(&planned, &model_files);
    let model_aside: BTreeSet<u64> = m
        .disk
        .bucket
        .objects
        .iter()
        .filter(|(_, (_, f))| {
            f.entries()
                .any(|e| e.position.generation < state.generation && e.position > end)
        })
        .map(|(id, _)| id.0)
        .collect();
    let aside = p.record.set_aside();
    let planned_aside: BTreeSet<u64> = p
        .record
        .files
        .iter()
        .filter(|f| {
            f.description
                .range
                .is_some_and(|(_, last)| aside.holds(last))
        })
        .map(|f| f.description.file_id)
        .collect();
    prop_assert_eq!(&planned_aside, &model_aside);
    Ok((r, p))
}

proptest! {
    #[test]
    fn the_plan_restores_what_the_model_restores(
        first in proptest::collection::vec(op(), 1..30),
        second in proptest::collection::vec(op(), 0..20),
        rebuilds in (proptest::option::of(any::<prop::sample::Index>()), proptest::option::of(any::<prop::sample::Index>())),
    ) {
        let mut m = Model::new(Config::default(), vec![], T0);
        let s = m
            .define_with("a/b", Kind::Gauge, ValueType::Number, "1", 10 * SECOND, SeriesOptions::expected(SECOND))
            .unwrap();
        let mut run = Run { m, s, t: T0 };
        // At least one segment is backed up: the model refuses an empty bucket (Q-294).
        run.apply(Op::Ingest(1));
        run.apply(Op::Seal);
        run.apply(Op::Upload);
        for o in first {
            run.apply(o);
        }
        run.finish();
        let c0 = records(&run.m, 0, 0, vec![Body::Empty], rebuilds.0);
        let (restored, p1) = restore_both(&mut run.m, &c0)?;
        if !p1.record.files.iter().all(|f| {
            f.description.range.is_none_or(|(_, last)| last <= p1.record.restored_end)
        }) {
            return Ok(());
        }
        let g = p1.record.generation;
        let from = restored.disk.next_file;
        let mut run = Run { m: restored, s, t: run.t };
        for o in second {
            run.apply(o);
        }
        run.finish();
        run.apply(Op::Upload);
        let c1 = records(&run.m, g, from, vec![p1.record.to_body()], rebuilds.1);
        restore_both(&mut run.m, &c1)?;
    }
}
