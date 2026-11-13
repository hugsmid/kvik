//! Property tests for the invariants of spec §10, one test per invariant.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::*;
use jiff::tz::TimeZone;
use kvik_model::engine::ConsumerId;
use kvik_model::oracle;
use kvik_model::summary::{Node, direct, merge};
use kvik_model::tree::{LEVELS, SeriesTree, SlidingWindow};
use kvik_model::types::{MINUTE, Nanos, QUARTER, SECOND, Value, floor_to};
use kvik_model::view;
use proptest::prelude::*;

fn cases(n: u32) -> ProptestConfig {
    ProptestConfig {
        cases: std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(n),
        ..ProptestConfig::default()
    }
}

fn run(
    ops: &[Op],
) -> Result<(kvik_model::engine::Model, Vec<kvik_model::types::SeriesId>), TestCaseError> {
    let (mut m, ids) = machine();
    for op in ops {
        apply(&mut m, &ids, op).map_err(TestCaseError::fail)?;
    }
    Ok((m, ids))
}

proptest! {
    #![proptest_config(cases(256))]

    /// 1. Positions are strictly increasing and gap-free across restarts.
    #[test]
    fn inv01_positions_gap_free_across_restarts(ops in proptest::collection::vec(arb_op(), 1..80)) {
        let (mut m, ids) = machine();
        for op in &ops {
            apply(&mut m, &ids, op).map_err(TestCaseError::fail)?;
            let keys: Vec<u64> = m.disk.all_entries().keys().copied().collect();
            let expect: Vec<u64> = (1..=keys.len() as u64).collect();
            prop_assert_eq!(&keys, &expect, "durable positions not gap-free after {:?}", op);
            if let Op::Commit(None) = op
                && m.is_up()
            {
                // The running engine knows exactly the durable entries.
                let live: Vec<u64> = m.entries().unwrap().iter().map(|e| e.position).collect();
                prop_assert_eq!(&live, &keys);
            }
        }
        // After a restart, the next position follows the last durable one.
        m.restart();
        let n = m.disk.all_entries().len() as u64;
        let _ = m.emit(kvik_model::types::Reading::num(ids[0], T0 + 7, 1.0e9));
        let r = m.commit(None).unwrap();
        prop_assert_eq!(r.positions, vec![n + 1]);
    }

    /// 2. Every delivered position is durable; every durable position is
    /// eventually delivered to each matching consumer.
    #[test]
    fn inv02_delivered_iff_durable(ops in proptest::collection::vec(arb_op(), 1..80)) {
        let (mut m, ids) = run(&ops)?;
        settle(&mut m);
        m.drain().unwrap();
        let dur = durable(&m);
        for (c, sel) in [(0u32, None), (1u32, Some(ids[0]))] {
            let seen = observed_positions(&m, c);
            for p in &seen {
                prop_assert!(dur.contains_key(p), "consumer {} saw non-durable {}", c, p);
            }
            for (p, f) in &dur {
                if sel.is_none_or(|s| s == f.series) {
                    prop_assert!(seen.contains(p), "consumer {} never got {}", c, p);
                }
            }
        }
    }

    /// 3. For any consumer, deliveries are in position order (strictly
    /// increasing within a session; a new session resumes at or before the
    /// cursor + 1).
    #[test]
    fn inv03_delivery_in_position_order(ops in proptest::collection::vec(arb_op(), 1..80)) {
        let (mut m, _) = run(&ops)?;
        settle(&mut m);
        m.drain().unwrap();
        for c in [0u32, 1] {
            let obs = m.observed.get(&ConsumerId(c)).cloned().unwrap_or_default();
            let mut by_session: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
            for o in obs { by_session.entry(o.session).or_default().push(o.position); }
            for (s, ps) in by_session {
                prop_assert!(ps.windows(2).all(|w| w[0] < w[1]), "consumer {} session {}: {:?}", c, s, ps);
            }
        }
    }

    /// 4. The current view equals "highest position per (series, t)" over all
    /// facts in all tiers.
    #[test]
    fn inv04_current_view_over_all_tiers(ops in proptest::collection::vec(arb_op(), 1..80)) {
        let (mut m, _) = run(&ops)?;
        settle(&mut m);
        let all = m.disk.all_facts();
        prop_assert_eq!(m.current().unwrap(), &view::current(all.iter()));
        // Order and repetition of the input do not matter.
        let mut shuffled: Vec<_> = all.iter().rev().cloned().collect();
        shuffled.extend(all.iter().cloned());
        prop_assert_eq!(view::current(shuffled.iter()), view::current(all.iter()));
    }

    /// 5. Every tree node equals the direct computation over the current view
    /// of its span.
    #[test]
    fn inv05_tree_nodes_equal_direct(ops in proptest::collection::vec(arb_op(), 1..80)) {
        let (mut m, _) = run(&ops)?;
        settle(&mut m);
        let cur = m.current().unwrap().clone();
        for (s, tree) in m.trees().unwrap() {
            let rs = oracle::readings(&cur, *s);
            for (level, width) in LEVELS.iter().enumerate() {
                let mut starts: BTreeSet<Nanos> = tree.stored(level).map(|n| n.start).collect();
                // Also some empty nodes next to stored ones.
                let extra: Vec<Nanos> = starts.iter().map(|s| s + width).collect();
                starts.extend(extra);
                for st in starts {
                    let want = oracle::node(&tree.params, &rs, st, st + width);
                    prop_assert_eq!(tree.node(level, st), want, "series {} level {} start {}", s, level, st);
                }
            }
        }
    }

    /// 6. `merge` is associative and agrees with direct computation over the
    /// union.
    #[test]
    fn inv06_merge_associative_and_direct(
        (p, rs, cuts) in arb_params().prop_flat_map(|p| (
            Just(p),
            arb_readings(p.kind, 30),
            proptest::collection::btree_set(arb_offset(), 2..5),
        )),
    ) {
        let mut bounds: Vec<Nanos> = cuts.into_iter().map(|c| T0 + c).collect();
        bounds.insert(0, T0 - SECOND);
        bounds.push(T0 + 41 * MINUTE);
        bounds.sort();
        bounds.dedup();
        let nodes: Vec<Node> = bounds.windows(2).map(|w| oracle::node(&p, &rs, w[0], w[1])).collect();
        let whole = oracle::node(&p, &rs, bounds[0], *bounds.last().unwrap());
        // Left fold, right fold and every split of three agree with direct.
        let left = nodes.iter().skip(1).fold(nodes[0].clone(), |a, n| merge(&p, &a, n));
        let right = nodes.iter().rev().skip(1).fold(nodes.last().unwrap().clone(), |a, n| merge(&p, n, &a));
        prop_assert_eq!(&left, &whole);
        prop_assert_eq!(&right, &whole);
        if nodes.len() >= 3 {
            let (a, b, c) = (&nodes[0], &nodes[1], &nodes[2]);
            prop_assert_eq!(merge(&p, &merge(&p, a, b), c), merge(&p, a, &merge(&p, b, c)));
        }
        // The empty node is the identity on both sides.
        let e = kvik_model::summary::Node::empty(p.kind, bounds[0], bounds[0]);
        prop_assert_eq!(merge(&p, &e, &nodes[0]), nodes[0].clone());
        let e = kvik_model::summary::Node::empty(p.kind, nodes[0].end, nodes[0].end);
        prop_assert_eq!(merge(&p, &nodes[0], &e), nodes[0].clone());
        prop_assert_eq!(direct(&p, bounds[0], bounds[1], &rs.iter().filter(|(t, _)| *t >= bounds[0] && *t < bounds[1]).cloned().collect::<Vec<_>>()), nodes[0].clone());
    }

    /// 7. A window result equals the direct computation over the current view
    /// at evaluation time, including after late facts and corrections, from the
    /// tree and from the cached sliding window, for rolling and calendar
    /// windows.
    #[test]
    fn inv07_windows_equal_direct(
        p in arb_params(),
        batches in proptest::collection::vec(arb_readings(kvik_model::types::Kind::Gauge, 12), 1..5),
        evals in proptest::collection::vec((arb_offset(), arb_offset(), arb_offset()), 1..6),
        zone in prop_oneof![Just("UTC"), Just("Europe/Paris"), Just("America/Santiago"), Just("Asia/Kathmandu")],
    ) {
        // Reuse gauge-valued readings for every kind by mapping values.
        let conv = |v: &Value, i: usize| -> Value {
            match p.kind {
                kvik_model::types::Kind::Gauge => v.clone(),
                kvik_model::types::Kind::Counter => Value::Num(v.as_f64().unwrap().abs().floor()),
                kvik_model::types::Kind::State => Value::Str(["a", "b", "c"][i % 3].into()),
                kvik_model::types::Kind::Text => Value::Str(["x", "y"][i % 2].into()),
            }
        };
        let mut current: BTreeMap<Nanos, Value> = BTreeMap::new();
        let mut tree = SeriesTree::new(p);
        let mut sliding: BTreeMap<i64, SlidingWindow> = BTreeMap::new();
        for (bi, batch) in batches.iter().enumerate() {
            // Apply a batch (late facts and corrections relative to earlier ones).
            let mut touched = BTreeSet::new();
            for (i, (t, v)) in batch.iter().enumerate() {
                current.insert(*t, conv(v, i + bi));
                touched.insert(floor_to(*t, MINUTE));
            }
            for leaf in touched {
                let rs: Vec<(Nanos, Value)> = current.range(leaf..leaf + MINUTE).map(|(t, v)| (*t, v.clone())).collect();
                tree.update_minute(leaf, &rs);
            }
            let all: Vec<(Nanos, Value)> = current.iter().map(|(t, v)| (*t, v.clone())).collect();
            for (a, b, n) in &evals {
                let (start, end) = (T0 + (*a).min(*b), T0 + (*a).max(*b));
                let now = T0 + n;
                let want = oracle::window(&p, &all, start, end, now);
                prop_assert_eq!(tree.window(start, end, now), want.clone());
                // Invariant 18: nothing credited past now or a reading's limit.
                prop_assert!(want.coverage <= i128::from((end.min(now) - start).max(0)));
                if let Some(s) = p.staleness {
                    let bound: i128 = all.iter().map(|_| i128::from(s)).sum::<i128>() + i128::from(s);
                    prop_assert!(want.coverage <= bound);
                }
                // Invariant 17: counter increases over adjacent windows add up.
                if p.kind == kvik_model::types::Kind::Counter && start < end {
                    let mid = start + (end - start) / 2;
                    let l = oracle::window(&p, &all, start, mid, now);
                    let r = oracle::window(&p, &all, mid, end, now);
                    let inc = |n: &kvik_model::summary::Node| match &n.body {
                        kvik_model::summary::Body::Counter { increase, .. } => increase.clone(),
                        _ => unreachable!(),
                    };
                    prop_assert_eq!(inc(&l) + inc(&r), inc(&want));
                }
                let len = (end - start).max(1);
                let sw = sliding.entry(len).or_insert_with(|| SlidingWindow::new(len));
                prop_assert_eq!(sw.evaluate(&tree, now), oracle::window(&p, &all, now - len, now, now));
            }
            // Calendar window: since local midnight.
            let tz = TimeZone::get(zone).unwrap();
            let now = T0 + 30 * MINUTE;
            let date = jiff::Timestamp::from_nanosecond(i128::from(now)).unwrap().to_zoned(tz.clone()).date();
            let (ds, _) = kvik_model::tree::day_span(&tz, date).unwrap();
            prop_assert_eq!(tree.window(ds, now, now), oracle::window(&p, &all, ds, now, now));
        }
    }

    /// 8. Rebuilding derived structures from facts yields exactly the live
    /// structures.
    #[test]
    fn inv08_rebuild_equals_live(ops in proptest::collection::vec(arb_op(), 1..80)) {
        let (mut m, _) = run(&ops)?;
        settle(&mut m);
        let rebuilt = m.rebuild_trees().unwrap();
        let live = m.trees().unwrap();
        prop_assert_eq!(rebuilt.len(), live.len());
        for (s, t) in live {
            prop_assert!(t.same_summaries(&rebuilt[s]), "series {} differs", s);
        }
        // And after a restart (everything volatile rebuilt from disk).
        let before = m.trees().unwrap().clone();
        let cur = m.current().unwrap().clone();
        m.restart();
        prop_assert_eq!(m.current().unwrap(), &cur);
        for (s, t) in &before {
            prop_assert!(t.same_summaries(&m.trees().unwrap()[s]));
        }
    }

    /// 9. Compaction, tiering, backup and restore preserve the multiset of facts.
    #[test]
    fn inv09_tiering_backup_restore_preserve_facts(ops in proptest::collection::vec(arb_op(), 1..80)) {
        let (mut m, _) = run(&ops)?;
        settle(&mut m);
        let committed = durable(&m);
        // Compact, copy, upload, evict; the facts must be the same.
        let _ = m.compact(None);
        m.seal_active();
        let _ = m.compact(None);
        m.disk.bucket.reachable = true;
        m.upload();
        m.advance(100 * 24 * 3600 * SECOND);
        let _ = m.housekeeping();
        prop_assert_eq!(&durable(&m), &committed);
        // Multiset: no position appears with two different contents anywhere.
        for (id, f) in &m.disk.files {
            for e in f.entries() {
                if let Some(fact) = e.fact()
                    && m.disk.published().contains(id)
                {
                        prop_assert_eq!(Some(fact), committed.get(&e.position));
                }
            }
        }
        // Restore onto an empty disk from the bucket alone.
        let r = kvik_model::engine::Model::restore_from(m.config.clone(), consumers(), &m.disk.bucket, m.wall, false);
        prop_assert_eq!(&durable(&r), &committed);
        prop_assert_eq!(r.current().unwrap(), m.current().unwrap());
    }

    /// 10. No operation reduces the set of facts.
    #[test]
    fn inv10_no_operation_reduces_facts(ops in proptest::collection::vec(arb_op(), 1..120)) {
        // `apply` checks the durable set before and after every operation.
        let (mut m, _) = run(&ops)?;
        let before = durable(&m);
        m.restart();
        let after = durable(&m);
        for (p, f) in &before {
            prop_assert_eq!(after.get(p), Some(f));
        }
    }
}

#[test]
fn inv07_window_examples_at_quarter_edges() {
    use kvik_model::summary::Params;
    use kvik_model::types::Kind;
    let p = Params::new(Kind::Gauge, 20 * MINUTE);
    let rs = vec![
        (T0 + QUARTER - 1, Value::Num(1.0)),
        (T0 + QUARTER, Value::Num(2.0)),
        (T0 + 2 * QUARTER + SECOND / 2, Value::Num(3.0)),
    ];
    let tree = SeriesTree::build(p, &rs);
    for (s, e) in [
        (T0, T0 + 3 * QUARTER),
        (T0 + SECOND, T0 + 2 * QUARTER + SECOND),
        (T0 + QUARTER, T0 + QUARTER + SECOND),
    ] {
        assert_eq!(tree.window(s, e, e), oracle::window(&p, &rs, s, e, e));
    }
}

/// Guards the generators: the random histories must actually exercise
/// commits, crashes, recovery, overflow, compaction and uploads.
#[test]
fn generator_coverage() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let strat = proptest::collection::vec(arb_op(), 60..80);
    let (mut facts, mut discarded, mut overflows, mut compacted, mut uploaded, mut revisions) =
        (0usize, 0u64, 0u64, 0usize, 0usize, 0usize);
    for _ in 0..200 {
        let ops = strat.new_tree(&mut runner).unwrap().current();
        let (mut m, ids) = machine();
        for op in &ops {
            apply(&mut m, &ids, op).unwrap();
        }
        settle(&mut m);
        facts += durable(&m).len();
        revisions += view::revisions(m.disk.all_facts().iter()).len();
        discarded += m.counters.batches_discarded;
        overflows += m.counters.overflows;
        compacted += m
            .disk
            .files
            .values()
            .filter(|f| f.class != kvik_model::engine::FileClass::Segment)
            .count();
        uploaded += m.disk.bucket.objects.len();
    }
    eprintln!(
        "facts {facts} revisions {revisions} discarded {discarded} overflows {overflows} compacted {compacted} uploaded {uploaded}"
    );
    assert!(facts > 1000 && revisions > 50 && discarded > 10 && overflows > 10);
    assert!(compacted > 20 && uploaded > 20);
}
