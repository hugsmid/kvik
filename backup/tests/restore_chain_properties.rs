//! Property tests of the chain of uploaded records at a restore (Q-611, Q-614): chains built as the upload driver
//! writes them, each record followed by its last-uploaded marker, with skipped epochs and rebuilt manifests, restore;
//! one random break among the records a restore checks refuses it, or makes it retry where the bucket still holds the
//! record; a break before a whole rebuilt manifest refuses nothing.

mod support;

use std::sync::Arc;

use kvik_backup::fake::{MemSink, block_on};
use kvik_backup::keys::Keys;
use kvik_backup::marker::Summary;
use kvik_backup::plan::{PlanMode, records_to_read};
use kvik_backup::restore::{RestoreDriver, RestoreError};
use kvik_backup::uploaded::opens_manifest;
use kvik_files::record::{Body, Rebuild, Record};
use kvik_log::fs::MemFs;
use proptest::prelude::*;
use support::*;

/// A generation's records as uploaded: (previous, record).
#[derive(Clone, Debug)]
struct Chain(Vec<(u64, Record)>);

/// Old records with skipped epochs, the second and third naming the two segments; then, if drawn, a rebuilt manifest
/// carrying them, whole or not (always whole with `whole_only`), and records after it.
fn chain(src: &Source, whole_only: bool) -> impl Strategy<Value = Chain> + use<> {
    let bodies = [
        Body::Sealed(vec![src.segments[0].0.clone()]),
        Body::Sealed(vec![src.segments[1].0.clone()]),
    ];
    (
        prop::collection::vec(1u64..4, 3..9),
        prop::option::of((any::<bool>(), 1u64..4, 0usize..3)),
    )
        .prop_map(move |(steps, rebuild)| {
            let rebuild = match rebuild {
                _ if !whole_only => rebuild,
                Some((_, step, after)) => Some((true, step, after)),
                None => Some((true, 1, 0)),
            };
            let mut epoch = 0;
            let mut next = |step: u64| {
                epoch += step;
                epoch
            };
            let mut list: Vec<Record> = Vec::new();
            for (i, step) in steps.iter().enumerate() {
                let body = match i {
                    1 | 2 => bodies[i - 1].clone(),
                    _ => Body::Empty,
                };
                list.push(Record::new(
                    if i == 0 { next(1) } else { next(*step) },
                    0,
                    9,
                    body,
                ));
            }
            if let Some((whole, step, after)) = rebuild {
                let old = list.clone();
                list.push(Record::new(
                    next(step),
                    0,
                    9,
                    Body::Rebuild(Rebuild::default()),
                ));
                for r in &old {
                    list.push(Record {
                        old_epoch: Some(r.epoch),
                        ..Record::new(next(1), 0, 9, r.body.clone())
                    });
                }
                if whole {
                    list.push(Record::new(next(1), 0, 9, Body::Empty));
                    for _ in 0..after {
                        list.push(Record::new(next(1), 0, 9, Body::Empty));
                    }
                }
            }
            let mut previous = 0;
            Chain(
                list.into_iter()
                    .map(|r| {
                        let p = if opens_manifest(&r) { 0 } else { previous };
                        previous = r.epoch;
                        (p, r)
                    })
                    .collect(),
            )
        })
}

#[derive(Clone, Copy, Debug)]
enum Break {
    /// The bucket loses the record.
    Lose,
    /// Listings leave the record out.
    Hide,
    /// The record names another predecessor.
    Rename,
}

fn bucket(mut src: Source, c: &Chain) -> Arc<FaultSink> {
    let mem = MemSink::new();
    upload_files(&mem, &mut src);
    use_copies(&src);
    for (i, (p, r)) in c.0.iter().enumerate() {
        put_record(&mem, ID, *p, r);
        let summary = Summary::of(c.0[..=i].iter().map(|(_, r)| r));
        put_marker(&mem, ID, 0, i as u64 + 1, summary);
    }
    Arc::new(FaultSink::new(mem))
}

fn mode(drill: bool) -> PlanMode {
    if drill {
        PlanMode::Drill
    } else {
        PlanMode::Full
    }
}

fn prepare(
    sink: &Arc<FaultSink>,
    drill: bool,
) -> Result<kvik_backup::restore::Prepared, RestoreError> {
    let fs = MemFs::new();
    block_on(restorer(sink.clone(), &fs, mode(drill), KEY).prepare(100))
}

/// The index of the first record a restore reads.
fn start(c: &Chain) -> usize {
    let records: Vec<Record> = c.0.iter().map(|(_, r)| r.clone()).collect();
    let read = records_to_read(&records);
    records
        .iter()
        .position(|r| Some(r.epoch) == read.first().map(|f| f.epoch))
        .unwrap()
}

/// The records a break may hit and the restore must see, from the first record read. A lost or hidden one may be
/// any: the last, and one whose successor opens a rebuilt manifest and so names none, the marker sees (Q-614). A
/// renamed one is any but the first and those that open a rebuilt manifest, whose predecessor is fixed.
fn targets(c: &Chain, b: Break) -> Vec<usize> {
    let s = start(c);
    let n = c.0.len();
    let opens = |i: usize| opens_manifest(&c.0[i].1);
    match b {
        Break::Lose | Break::Hide => (s..n).collect(),
        Break::Rename => (s.max(1)..n).filter(|&i| !opens(i)).collect(),
    }
}

fn source_and_chain(whole_only: bool) -> impl Strategy<Value = (Source, Chain)> {
    let src = source(ID, 2, false);
    chain(&src, whole_only).prop_map(move |c| (src.clone(), c))
}

/// Gives record `i` a predecessor other than its own, drawn from `wrong`.
fn rename(c: &mut Chain, i: usize, wrong: u64) {
    let (right, epoch) = (c.0[i].0, c.0[i].1.epoch);
    let mut w = wrong % (epoch - 1);
    if w >= right {
        w += 1;
    }
    c.0[i].0 = w;
}

proptest! {
    #[test]
    fn a_chain_as_the_upload_driver_writes_it_restores((src, c) in source_and_chain(false), drill in any::<bool>()) {
        let sink = bucket(src, &c);
        let p = prepare(&sink, drill);
        prop_assert!(p.is_ok(), "{:?}", p);
    }

    #[test]
    fn one_break_in_the_records_a_restore_checks_refuses_it_or_makes_it_retry(
        (src, mut c) in source_and_chain(false),
        kind in prop_oneof![Just(Break::Lose), Just(Break::Hide), Just(Break::Rename)],
        at in any::<prop::sample::Index>(),
        wrong in any::<u64>(),
        drill in any::<bool>(),
    ) {
        let i = at.get(&targets(&c, kind)).to_owned();
        let epoch = c.0[i].1.epoch;
        if let Break::Rename = kind {
            rename(&mut c, i, wrong);
        }
        let sink = bucket(src, &c);
        let key = Keys::new(ID).record(0, epoch);
        match kind {
            Break::Lose => sink.set(&key, Some(Fault::Lose)),
            Break::Hide => sink.set(&key, Some(Fault::Unlisted)),
            Break::Rename => {}
        }
        let r = prepare(&sink, drill);
        match kind {
            Break::Hide => prop_assert!(
                matches!(r, Err(RestoreError::ListingIncomplete { generation: 0 })),
                "{:?}", r
            ),
            Break::Lose => prop_assert!(
                matches!(
                    r,
                    Err(RestoreError::BrokenChain { generation: 0, .. }
                        | RestoreError::MarkerUnreached { generation: 0, .. })
                ),
                "{:?}", r
            ),
            Break::Rename => prop_assert!(
                matches!(r, Err(RestoreError::BrokenChain { generation: 0, .. })),
                "{:?}", r
            ),
        }
    }

    #[test]
    fn a_break_before_a_whole_rebuilt_manifest_never_refuses_the_restore_that_reads_it(
        (src, mut c) in source_and_chain(true),
        lose in any::<bool>(),
        at in any::<prop::sample::Index>(),
        wrong in any::<u64>(),
        drill in any::<bool>(),
    ) {
        let old: Vec<usize> = (0..start(&c)).filter(|&i| lose || i > 0).collect();
        let i = at.get(&old).to_owned();
        let epoch = c.0[i].1.epoch;
        if !lose {
            rename(&mut c, i, wrong);
        }
        let sink = bucket(src, &c);
        if lose {
            sink.set(&Keys::new(ID).record(0, epoch), Some(Fault::Lose));
        }
        let p = prepare(&sink, drill);
        prop_assert!(p.is_ok(), "{:?}", p);
    }
}
