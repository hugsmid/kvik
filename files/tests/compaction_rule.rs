//! The catalogue's rule for compacted files (compaction design §3.4): invariant 14 and C(g) at every epoch of
//! generated manifests, with a corrupt record rebuilt at every epoch; the writer's refusals; the start's hold.

mod support;

use std::collections::{BTreeMap, BTreeSet};

use kvik_files::appender::{AppendError, Appender, Opened};
use kvik_files::catalogue::Catalogue;
use kvik_files::compaction::{Judgement, compacted_through, judge};
use kvik_files::desc::{FileClass, FileDescription, FileRef};
use kvik_files::layout::CompactedLayout;
use kvik_files::rebuild::{Place, RebuildInput, plan};
use kvik_files::record::{Body, Publish, Record};
use proptest::prelude::*;
use support::rebuild::image_with_corrupt;
use support::*;

fn laid(d: &FileDescription, run_id: u64, run_count: u32, facts: u64) -> CompactedLayout {
    let mut l = layout_of(d);
    l.run_id = run_id;
    l.run_count = run_count;
    l.fact_count = facts;
    l.positions = vec![(0, facts)];
    l
}

fn publish(out: &[(FileDescription, CompactedLayout)], retired: &[FileDescription]) -> Body {
    let mut out = out.to_vec();
    out.sort_by_key(|o| o.0.file_ref());
    let mut retired = retired.to_vec();
    retired.sort_by_key(FileDescription::file_ref);
    Body::Publish(Publish {
        published: out.iter().map(|o| o.0.clone()).collect(),
        layouts: out.into_iter().map(|o| o.1).collect(),
        retired,
    })
}

/// A run's output holding `first..=last`.
fn output(id: u64, first: u64, last: u64, inputs: &[FileRef]) -> FileDescription {
    compacted(id, FileClass::Current, (first, last), inputs)
}

fn appender_with(records: &[Record]) -> Appender {
    let fs = fs_with(&manifest_bytes(records));
    let Opened::Ready(a) = Appender::open(&fs, &dir(), STORE_ID, 0).unwrap() else {
        panic!("corrupt")
    };
    *a
}

fn sealed(epoch: u64, segs: &[FileDescription]) -> Record {
    Record::new(epoch, 0, T0, Body::Sealed(segs.to_vec()))
}

fn breach_rule(e: AppendError) -> u8 {
    match e {
        AppendError::Breach(b) => b.rule,
        e => panic!("not a breach: {e:?}"),
    }
}

#[test]
fn a_run_that_skips_a_listed_segment_is_refused_by_rule_2() {
    let mut a = appender_with(&[sealed(1, &[segment(1, 1, 10), segment(2, 11, 20)])]);
    let o = output(100, 11, 20, &[seg_ref(2)]);
    let l = laid(&o, 7, 1, 10);
    assert_eq!(
        breach_rule(a.append(publish(&[(o, l)], &[]), T0).unwrap_err()),
        2
    );
    let o = output(100, 1, 10, &[seg_ref(1)]);
    let l = laid(&o, 7, 1, 10);
    a.append(publish(&[(o, l)], &[]), T0).unwrap();
    assert_eq!(compacted_through(a.catalogue()), BTreeMap::from([(0, 10)]));
}

#[test]
fn a_merge_with_one_row_dropped_is_refused_by_rule_3() {
    let segs = [segment(1, 1, 10), segment(2, 11, 20)];
    let a1 = output(100, 1, 10, &[seg_ref(1), seg_ref(2)]);
    let a2 = output(101, 11, 20, &[seg_ref(1), seg_ref(2)]);
    let run = publish(
        &[
            (a1.clone(), laid(&a1, 7, 2, 10)),
            (a2.clone(), laid(&a2, 7, 2, 10)),
        ],
        &[],
    );
    let mut a = appender_with(&[sealed(1, &segs), Record::new(2, 0, T0, run)]);
    assert_eq!(a.compaction(), &Judgement::Holds);
    let m = output(102, 1, 20, &[comp_ref(100), comp_ref(101)]);
    let short = publish(
        &[(m.clone(), laid(&m, 8, 1, 19))],
        &[a1.clone(), a2.clone()],
    );
    assert_eq!(breach_rule(a.append(short, T0).unwrap_err()), 3);
    let whole = publish(&[(m.clone(), laid(&m, 8, 1, 20))], &[a1, a2]);
    a.append(whole, T0).unwrap();
    assert_eq!(a.compaction(), &Judgement::Holds);
}

#[test]
fn a_run_that_retires_a_file_and_a_merge_that_keeps_an_input_are_refused_by_rule_4() {
    let segs = [segment(1, 1, 10), segment(2, 11, 20)];
    let a1 = output(100, 1, 10, &[seg_ref(1)]);
    let run = publish(&[(a1.clone(), laid(&a1, 7, 1, 10))], &[]);
    let mut a = appender_with(&[sealed(1, &segs), Record::new(2, 0, T0, run)]);
    let b = output(101, 11, 20, &[seg_ref(2)]);
    let retiring = publish(
        &[(b.clone(), laid(&b, 9, 1, 10))],
        std::slice::from_ref(&a1),
    );
    assert_eq!(breach_rule(a.append(retiring, T0).unwrap_err()), 4);
    let m = output(102, 1, 10, &[comp_ref(100)]);
    let keeping = publish(&[(m.clone(), laid(&m, 8, 1, 10))], &[]);
    assert!(a.append(keeping, T0).is_err());
}

#[test]
fn a_second_run_naming_a_compacted_segment_is_refused_by_rule_1() {
    let segs = [segment(1, 1, 10)];
    let a1 = output(100, 1, 10, &[seg_ref(1)]);
    let run = publish(&[(a1.clone(), laid(&a1, 7, 1, 10))], &[]);
    let mut a = appender_with(&[sealed(1, &segs), Record::new(2, 0, T0, run)]);
    let again = output(101, 1, 10, &[seg_ref(1)]);
    let body = publish(&[(again.clone(), laid(&again, 9, 1, 10))], &[]);
    assert_eq!(breach_rule(a.append(body, T0).unwrap_err()), 1);
}

#[test]
fn a_run_missing_one_of_its_files_is_refused_by_rule_4() {
    let a1 = output(100, 1, 10, &[seg_ref(1)]);
    let mut a = appender_with(&[sealed(1, &[segment(1, 1, 10)])]);
    let body = publish(&[(a1.clone(), laid(&a1, 7, 2, 10))], &[]);
    assert_eq!(breach_rule(a.append(body, T0).unwrap_err()), 4);
}

#[test]
fn an_input_segment_no_record_lists_leaves_the_rule_undecided_and_holds_publishes() {
    let a1 = output(100, 1, 10, &[seg_ref(1)]);
    let run = publish(&[(a1.clone(), laid(&a1, 7, 1, 10))], &[]);
    let mut a = appender_with(&[Record::new(1, 0, T0, run)]);
    assert_eq!(
        a.compaction(),
        &Judgement::Undecided(BTreeSet::from([seg_ref(1)]))
    );
    let b = output(101, 11, 20, &[seg_ref(2)]);
    let body = publish(&[(b.clone(), laid(&b, 9, 1, 10))], &[]);
    assert!(matches!(
        a.append(body, T0),
        Err(AppendError::CompactionHeld)
    ));
    // The record that lists the segment ends the hold.
    a.append(Body::Sealed(vec![segment(1, 1, 10)]), T0).unwrap();
    assert_eq!(a.compaction(), &Judgement::Holds);
}

#[test]
fn a_manifest_that_breaks_the_rule_opens_held_rather_than_refused() {
    let segs = [segment(1, 1, 10), segment(2, 11, 20)];
    let b = output(101, 11, 20, &[seg_ref(2)]);
    let skipping = publish(&[(b.clone(), laid(&b, 9, 1, 10))], &[]);
    let mut a = appender_with(&[sealed(1, &segs), Record::new(2, 0, T0, skipping)]);
    assert!(matches!(a.compaction(), Judgement::Broken(x) if x.rule == 2));
    assert!(compacted_through(a.catalogue()).is_empty());
    let c = output(100, 1, 10, &[seg_ref(1)]);
    let body = publish(&[(c.clone(), laid(&c, 7, 1, 10))], &[]);
    assert!(matches!(
        a.append(body, T0),
        Err(AppendError::CompactionHeld)
    ));
}

// ---- Generated compaction histories ----

#[derive(Clone, Debug)]
enum Op {
    Seal(u64),
    Run(usize, bool),
    Merge,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (1u64..6).prop_map(Op::Seal),
        2 => (1usize..4, any::<bool>()).prop_map(|(n, two)| Op::Run(n, two)),
        1 => Just(Op::Merge),
    ]
}

struct Built {
    records: Vec<Record>,
    files: Vec<(FileDescription, Option<CompactedLayout>)>,
}

fn build(ops: &[Op]) -> Built {
    let mut records = Vec::new();
    let mut files = Vec::new();
    let mut segs: Vec<FileDescription> = Vec::new();
    let mut compacted_segs = 0;
    let mut l0: Vec<(FileDescription, CompactedLayout)> = Vec::new();
    let (mut seq, mut seg_id, mut comp_id, mut run_id) = (1u64, 1u64, 1000u64, 1u64);
    let mut epoch = 1;
    let mut push = |records: &mut Vec<Record>, body| {
        records.push(Record::new(epoch, 0, T0, body));
        epoch += 1;
    };
    for o in ops {
        match o {
            Op::Seal(len) => {
                let s = segment(seg_id, seq, seq + len - 1);
                seq += len;
                seg_id += 1;
                segs.push(s.clone());
                files.push((s.clone(), None));
                push(&mut records, Body::Sealed(vec![s]));
            }
            Op::Run(n, two) => {
                let take: Vec<_> = segs.iter().skip(compacted_segs).take(*n).cloned().collect();
                if take.is_empty() {
                    continue;
                }
                compacted_segs += take.len();
                let inputs: Vec<FileRef> = take.iter().map(|s| s.file_ref()).collect();
                let lo = take[0].range.unwrap().0.1;
                let hi = take.last().unwrap().range.unwrap().1.1;
                let parts = if *two && hi > lo {
                    vec![(lo, lo), (lo + 1, hi)]
                } else {
                    vec![(lo, hi)]
                };
                let out: Vec<_> = parts
                    .iter()
                    .map(|(a, b)| {
                        let d = output(comp_id, *a, *b, &inputs);
                        comp_id += 1;
                        let l = laid(&d, run_id, parts.len() as u32, b - a + 1);
                        (d, l)
                    })
                    .collect();
                run_id += 1;
                for (d, l) in &out {
                    files.push((d.clone(), Some(l.clone())));
                }
                l0.extend(out.iter().cloned());
                push(&mut records, publish(&out, &[]));
            }
            Op::Merge => {
                if l0.len() < 2 {
                    continue;
                }
                let ins: Vec<_> = std::mem::take(&mut l0);
                let lo = ins.iter().map(|x| x.0.range.unwrap().0.1).min().unwrap();
                let hi = ins.iter().map(|x| x.0.range.unwrap().1.1).max().unwrap();
                let facts = ins.iter().map(|x| x.1.fact_count).sum();
                let refs: Vec<FileRef> = ins.iter().map(|x| x.0.file_ref()).collect();
                let d = output(comp_id, lo, hi, &refs);
                comp_id += 1;
                let l = laid(&d, run_id, 1, facts);
                run_id += 1;
                files.push((d.clone(), Some(l.clone())));
                let retired: Vec<_> = ins.into_iter().map(|x| x.0).collect();
                push(&mut records, publish(&[(d, l)], &retired));
            }
        }
    }
    push(&mut records, Body::Empty);
    Built { records, files }
}

/// No epoch lists a compacted file live with a compacted file it names as an input (invariant 14).
fn invariant_14(c: &Catalogue) -> bool {
    (1..=c.last_epoch()).all(|e| {
        let live: BTreeSet<FileRef> = c
            .at_epoch(e)
            .iter()
            .map(|f| f.description.file_ref())
            .collect();
        c.at_epoch(e).iter().all(|f| {
            f.description
                .inputs
                .iter()
                .all(|i| i.kind == 1 || !live.contains(i))
        })
    })
}

proptest! {
    #[test]
    fn the_rule_holds_and_c_g_never_falls_at_every_epoch_of_a_generated_manifest(
        ops in proptest::collection::vec(op(), 1..30)
    ) {
        let b = build(&ops);
        let mut a = appender_with(&b.records[..1]);
        let mut last = 0;
        for r in &b.records[1..] {
            a.append(r.body.clone(), T0).unwrap();
            prop_assert_eq!(a.compaction(), &Judgement::Holds);
            let c = compacted_through(a.catalogue()).get(&0).copied().unwrap_or(0);
            prop_assert!(c >= last);
            last = c;
        }
        prop_assert!(invariant_14(a.catalogue()));
    }

    #[test]
    fn a_corrupt_record_rebuilt_at_any_epoch_keeps_the_rule_and_c_g(
        ops in proptest::collection::vec(op(), 1..30),
        at in any::<prop::sample::Index>(),
    ) {
        let b = build(&ops);
        if b.records.len() < 2 {
            return Ok(());
        }
        let i = at.index(b.records.len() - 1);
        let h = History { records: b.records.clone(), ..History::default() };
        let (_, img) = image_with_corrupt(&h, &[i]);
        let files: Vec<_> = b
            .files
            .iter()
            .map(|(d, l)| {
                let mut f = local(d, Place::Hot);
                f.layout = l.clone();
                f
            })
            .collect();
        let input = RebuildInput {
            image: &img,
            files: &files,
            losses: &[],
            lease_epoch: 0,
            mark_file_id: 0,
            written_at: T0,
        };
        let p = plan(&input, &FakeVerifier::default()).unwrap();
        let want = Catalogue::replay(&b.records).unwrap();
        // A corrupt run record whose outputs a later merge retired leaves the merge's counts undecided: the
        // restated retirement carries no layout, so the catalogue holds until a copy of the record is supplied.
        let merged_away = match &b.records[i].body {
            Body::Publish(pb) => pb.published.iter().any(|d| want.file(d.file_ref()).is_some_and(|e| e.retired.is_some())),
            _ => false,
        };
        match judge(&p.catalogue) {
            Judgement::Holds => {}
            Judgement::Undecided(_) => prop_assert!(merged_away),
            j @ Judgement::Broken(_) => prop_assert!(false, "{:?}", j),
        }
        prop_assert!(invariant_14(&p.catalogue));
        prop_assert_eq!(compacted_through(&p.catalogue), compacted_through(&want));
        prop_assert!(p.discard.is_empty() && p.held.is_empty());
    }
}
