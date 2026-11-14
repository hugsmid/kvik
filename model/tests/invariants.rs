//! Property tests for the invariants of spec §10, one test per invariant.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::*;
use jiff::tz::TimeZone;
use kvik_model::engine::{ConsumerId, Locality};
use kvik_model::oracle;
use kvik_model::summary::{Body, Node, direct, merge};
use kvik_model::tree::{LEVELS, SeriesTree, SlidingWindow};
use kvik_model::types::{Exact, Kind, MINUTE, Nanos, QUARTER, SECOND, Value, floor_to};
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
            let keys = seqs(m.disk.all_entries().keys().copied());
            let expect: Vec<u64> = (1..=keys.len() as u64).collect();
            prop_assert_eq!(&keys, &expect, "durable positions not gap-free after {:?}", op);
            if let Op::Commit(None) = op
                && m.is_up()
            {
                // The running engine knows exactly the durable entries.
                let live = seqs(m.entries().unwrap().iter().map(|e| e.position));
                prop_assert_eq!(&live, &keys);
            }
        }
        // After a restart, the next position follows the last durable one.
        m.restart();
        let n = m.disk.all_entries().len() as u64;
        let _ = m.emit(kvik_model::types::Reading::num(ids[0], T0 + 7, 1.0e9));
        let r = m.commit(None).unwrap();
        prop_assert_eq!(r.positions, vec![p0(n + 1)]);
    }

    /// 1. Only a restore starts a generation. After a random history, with all
    /// or only some sealed files uploaded, and possibly one uploaded file
    /// missing from the bucket (so the log has a gap), a restore claims the next
    /// generation and starts it at sequence 1, and restarts keep it; a bucket
    /// with nothing backed up is refused. The restored end is the last
    /// position before the first gap. A consumer whose kept cursor is at or
    /// below it catches up to it, flagged `replayed`; one without a cursor, or
    /// beyond the restored end, gets no replay. Every consumer receives
    /// `generation_changed` and `resync` (§8 Restore steps 1-2).
    #[test]
    fn inv01_only_restore_starts_generation(
        ops in proptest::collection::vec(arb_op(), 1..80),
        upload_all in any::<bool>(),
        missing in proptest::option::weighted(0.5, any::<proptest::sample::Index>()),
    ) {
        use kvik_model::engine::{Error, Model, Selection, Signal};
        use kvik_model::types::Position;
        let (mut m, _) = run(&ops)?;
        settle(&mut m);
        if upload_all {
            m.seal_active();
            m.disk.bucket.reachable = true;
            m.upload();
        }
        if let Some(i) = missing {
            let files: Vec<_> = m.disk.bucket.objects.keys().copied().collect();
            if !files.is_empty() {
                m.disk.bucket.objects.remove(i.get(&files));
            }
        }
        let mut restored: Vec<Position> = m.disk.bucket.objects.values().flat_map(|(_, f)| f.entries().map(|e| e.position)).collect();
        restored.sort_unstable();
        restored.dedup();
        let cursors = m.disk.cursors.clone();
        let r = Model::restore_with_cursors(m.config.clone(), consumers(), &mut m.disk.bucket, &cursors, m.wall, false, false);
        if restored.is_empty() {
            prop_assert_eq!(r.err(), Some(Error::EmptyBucket));
            return Ok(());
        }
        let mut r = r.unwrap();
        // Positions up to the first gap, in generation 0 from sequence 1.
        let end = restored
            .iter()
            .zip(1u64..)
            .take_while(|(p, s)| **p == Position::new(0, *s))
            .last()
            .map_or(Position::new(0, 0), |(p, _)| *p);
        prop_assert_eq!(r.disk.generation, 1);
        prop_assert!(m.disk.bucket.claims.contains(&1));
        prop_assert_eq!(r.disk.restored_end, Some(end));
        r.drain().unwrap();
        for d in consumers() {
            let kept = cursors.get(&d.id).copied();
            let got: Vec<(Position, bool)> = r.observed.get(&d.id).map(|o| o.iter().map(|x| (x.position, x.replayed)).collect()).unwrap_or_default();
            prop_assert_eq!(
                r.signals[&d.id].clone(),
                vec![Signal::GenerationChanged { cursor: kept, restored_end: end }, Signal::ResyncDue]
            );
            let want: Vec<(Position, bool)> = match kept {
                Some(cursor) if cursor <= end => r
                    .disk
                    .all_facts()
                    .into_iter()
                    .filter(|f| f.position > cursor && f.position <= end)
                    .filter(|f| match &d.selection {
                        Selection::All => true,
                        Selection::Series(set) => set.contains(&f.series),
                    })
                    .map(|f| (f.position, true))
                    .collect(),
                _ => Vec::new(),
            };
            prop_assert_eq!(got, want);
        }
        // Nothing beyond the restored end is in the views.
        prop_assert!(r.facts().unwrap().iter().all(|f| f.position <= end));
        r.restart();
        // The first new log entry (a definition) opens the new generation.
        r.define("check/restore/marker", kvik_model::types::Kind::Gauge, kvik_model::types::ValueType::Number, "1", None).unwrap();
        prop_assert_eq!(r.entries().unwrap().last().map(|e| e.position), Some(Position::new(1, 1)));
        r.crash();
        r.restart();
        prop_assert_eq!(r.disk.generation, 1);
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
            let mut by_session: BTreeMap<u32, Vec<kvik_model::types::Position>> = BTreeMap::new();
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
                prop_assert_eq!(tree.window(start, end, now), want);
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

    /// 11. No reader or consumer observes a position beyond the durable mark,
    /// whether the writer is up or down: a reader gets exactly the listed
    /// files' content up to the mark.
    #[test]
    fn inv11_nothing_observed_beyond_mark(ops in proptest::collection::vec(arb_op(), 1..80)) {
        let (mut m, ids) = machine();
        for op in &ops {
            apply(&mut m, &ids, op).map_err(TestCaseError::fail)?;
            let last = m.disk.mark.map_or(kvik_model::types::Position::ORIGIN, |k| k.last);
            let r = m.open_reader();
            let got: BTreeSet<kvik_model::types::Position> = m.read(&r).unwrap().1.iter().map(|f| f.position).collect();
            // The raw content, including batches past the mark.
            let raw: BTreeSet<kvik_model::types::Position> = r
                .files
                .iter()
                .filter(|f| m.disk.locality(**f) != Locality::Missing)
                .flat_map(|f| m.disk.files[f].entries().filter_map(|e| e.fact().map(|x| x.position)))
                .collect();
            prop_assert_eq!(&got, &raw.iter().copied().filter(|p| *p <= last).collect::<BTreeSet<_>>());
            for obs in m.observed.values() {
                prop_assert!(obs.iter().all(|o| o.position <= last));
            }
        }
    }

    /// 17. For a counter, the increases over adjacent windows that partition a
    /// span add up to the increase over the span: each step is credited once.
    #[test]
    fn inv17_counter_increase_adds_over_adjacent_windows(
        p in arb_params().prop_map(|mut p| { p.kind = Kind::Counter; p }),
        rs in arb_readings(Kind::Counter, 16),
        mut cuts in proptest::collection::vec(arb_offset(), 2..6),
        n in arb_offset(),
    ) {
        cuts.sort_unstable();
        let now = T0 + n;
        let tree = SeriesTree::build(p, &rs);
        let (start, end) = (T0 + cuts[0], T0 + cuts[cuts.len() - 1]);
        let whole = increase(&tree.window(start, end, now));
        let parts = cuts
            .windows(2)
            .map(|w| increase(&tree.window(T0 + w[0], T0 + w[1], now)))
            .fold(Exact::from_integer(0.into()), |a, b| a + b);
        prop_assert_eq!(parts, whole.clone());
        prop_assert_eq!(whole, increase(&oracle::window(&p, &rs, start, end, now)));
    }

    /// 17. For a state series, the transitions over adjacent windows that
    /// partition a span add up to those over the span, whatever S.
    #[test]
    fn inv17_state_transitions_add_over_adjacent_windows(
        p in arb_params().prop_map(|mut p| { p.kind = Kind::State; p }),
        rs in arb_readings(Kind::State, 16),
        mut cuts in proptest::collection::vec(arb_offset(), 2..6),
        n in arb_offset(),
    ) {
        cuts.sort_unstable();
        let now = T0 + n;
        let tree = SeriesTree::build(p, &rs);
        let (start, end) = (T0 + cuts[0], T0 + cuts[cuts.len() - 1]);
        let whole = transitions(&tree.window(start, end, now));
        let parts: u64 = cuts
            .windows(2)
            .map(|w| transitions(&tree.window(T0 + w[0], T0 + w[1], now)))
            .sum();
        prop_assert_eq!(parts, whole);
        prop_assert_eq!(whole, transitions(&oracle::window(&p, &rs, start, end, now)));
    }

    /// 18. No time is credited past `now`, nor past a reading's staleness limit:
    /// coverage never exceeds the part of `[start, min(end, now))` that lies
    /// within S after some reading at or before `now`.
    #[test]
    fn inv18_nothing_credited_past_now_or_staleness(
        (p, rs) in arb_params().prop_flat_map(|p| (Just(p), arb_readings(p.kind, 16))),
        (a, b, n) in (arb_offset(), arb_offset(), arb_offset()),
    ) {
        let (start, end, now) = (T0 + a.min(b), T0 + a.max(b), T0 + n);
        let tree = SeriesTree::build(p, &rs);
        let w = tree.window(start, end, now);
        let until = end.min(now);
        prop_assert!(w.coverage <= i128::from((until - start).max(0)));
        let valid: Vec<(Nanos, Nanos)> = rs
            .iter()
            .filter(|(t, _)| *t <= now)
            .map(|(t, _)| (*t, p.staleness.map_or(Nanos::MAX, |s| t.saturating_add(s))))
            .collect();
        prop_assert!(w.coverage <= union_within(&valid, start, until));
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

    /// 9. Compaction, tiering and backup preserve the set of positions with
    /// their content; restore preserves the backed-up ones.
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
        // Restore onto an empty disk from the bucket alone. Everything was
        // sealed and uploaded above, so the backed-up positions are all of
        // them (the partial case is `rules.rs` `s10_9_…`).
        let r = kvik_model::engine::Model::restore_from(m.config.clone(), consumers(), &mut m.disk.bucket, m.wall, false).unwrap();
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

fn increase(n: &Node) -> Exact {
    match &n.body {
        Body::Counter { increase, .. } => increase.clone(),
        _ => unreachable!("counter node"),
    }
}

fn transitions(n: &Node) -> u64 {
    match &n.body {
        Body::State { transitions, .. } => *transitions,
        _ => unreachable!("state node"),
    }
}

/// Length of the union of `intervals` (half-open) clipped to `[lo, hi)`.
fn union_within(intervals: &[(Nanos, Nanos)], lo: Nanos, hi: Nanos) -> i128 {
    let mut clipped: Vec<(Nanos, Nanos)> = intervals
        .iter()
        .map(|(a, b)| ((*a).max(lo), (*b).min(hi)))
        .filter(|(a, b)| a < b)
        .collect();
    clipped.sort_unstable();
    let (mut total, mut reach) = (0i128, Nanos::MIN);
    for (a, b) in clipped {
        let a = a.max(reach);
        if b > a {
            total += i128::from(b - a);
        }
        reach = reach.max(b);
    }
    total
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
    let (mut tails, mut kept_after_reboot, mut recovery_failed) = (0usize, 0usize, 0usize);
    for _ in 0..200 {
        let ops = strat.new_tree(&mut runner).unwrap().current();
        let (mut m, ids) = machine();
        for op in &ops {
            let tail_before = m.disk.tail_len();
            let discarded_before = m.counters.batches_discarded;
            let was_up = m.is_up();
            apply(&mut m, &ids, op).unwrap();
            tails += usize::from(m.disk.tail_len() > 0);
            recovery_failed += usize::from(
                matches!(op, Op::Restart(true) | Op::PowerLoss(Some(true))) && m.is_read_only(),
            );
            // A restart after a reboot that kept a batch past the old mark.
            // Explicit, or before an operation that needs the engine up.
            let restarted =
                matches!(op, Op::Restart(_) | Op::PowerLoss(Some(_))) || (!was_up && m.is_up());
            let lost = m.counters.batches_discarded - discarded_before;
            if restarted && !m.is_read_only() && tail_before as u64 > lost {
                kept_after_reboot += 1;
            }
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
        "facts {facts} revisions {revisions} discarded {discarded} overflows {overflows} compacted {compacted} uploaded {uploaded} tails {tails} kept-after-reboot {kept_after_reboot} recovery-failed {recovery_failed}"
    );
    assert!(facts > 1000 && revisions > 50 && discarded > 10 && overflows > 10);
    assert!(compacted > 20 && uploaded > 20);
    assert!(tails > 50 && kept_after_reboot > 15 && recovery_failed > 5);
}
