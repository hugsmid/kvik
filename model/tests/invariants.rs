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
    /// below it catches up to it, flagged `replayed`, if log segments holding
    /// every position after it up to the restored end are local; otherwise,
    /// and without a cursor or beyond the restored end, it gets no replay.
    /// Every consumer receives
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
        let in_segments: BTreeSet<Position> = m.disk.bucket.objects.values()
            .filter(|(_, f)| f.class == kvik_model::engine::FileClass::Segment)
            .flat_map(|(_, f)| f.entries().map(|e| e.position))
            .collect();
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
                Some(cursor)
                    if cursor <= end
                        && restored.iter().filter(|p| **p > cursor && **p <= end).all(|p| in_segments.contains(p)) =>
                {
                    r
                    .disk
                    .all_facts()
                    .into_iter()
                    .filter(|f| f.position > cursor && f.position <= end)
                    .filter(|f| match &d.selection {
                        Selection::All => true,
                        Selection::Series(set) => set.contains(&f.series),
                    })
                    .map(|f| (f.position, true))
                    .collect()
                }
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
    /// eventually delivered to each matching consumer, or superseded by a
    /// `resync` delivered to it in place of the replay of that position
    /// because its cursor was older than the oldest local log segment.
    #[test]
    fn inv02_delivered_iff_durable(ops in proptest::collection::vec(arb_op(), 1..80)) {
        let (mut m, ids) = machine();
        let mut basis = ResyncBasis::new();
        for op in &ops {
            track_resyncs(&mut m, &mut basis, |m| apply(m, &ids, op)).map_err(TestCaseError::fail)?;
        }
        track_resyncs(&mut m, &mut basis, settle);
        track_resyncs(&mut m, &mut basis, |m| m.drain()).unwrap();
        let dur = durable(&m);
        for (c, sel) in [(0u32, None), (1u32, Some(ids[0]))] {
            let seen = observed_positions(&m, c);
            let replaced = resync_replaced_justified(&m, &basis, c);
            for p in &seen {
                prop_assert!(dur.contains_key(p), "consumer {} saw non-durable {}", c, p);
            }
            for (p, f) in &dur {
                if sel.is_none_or(|s| s == f.series) {
                    let superseded = replaced.iter().any(|(a, b)| a < p && p <= b);
                    prop_assert!(seen.contains(p) || superseded, "consumer {} never got {}", c, p);
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

    /// 2, with run-time routes, conditions and timers: every durable position
    /// routed to a consumer by the routes in force at it is delivered to it,
    /// or superseded by a `resync` delivered in place of its replay because
    /// the cursor was older than the oldest local log segment; every
    /// delivered fact is durable (checked as it is taken).
    #[test]
    fn inv02_routed_positions_delivered_or_resynced(ops in proptest::collection::vec(arb_delivery_op(), 1..100)) {
        let (mut m, ids) = delivery_machine();
        let mut basis = ResyncBasis::new();
        for op in &ops {
            track_resyncs(&mut m, &mut basis, |m| apply_delivery(m, &ids, op)).map_err(TestCaseError::fail)?;
        }
        track_resyncs(&mut m, &mut basis, settle);
        track_resyncs(&mut m, &mut basis, |m| m.drain()).unwrap();
        let dur = durable(&m);
        for d in delivery_consumers() {
            let seen = observed_positions(&m, d.id.0);
            let replaced = resync_replaced_justified(&m, &basis, d.id.0);
            for p in &seen {
                prop_assert!(dur.contains_key(p), "consumer {} saw non-durable {}", d.id.0, p);
            }
            for (p, f) in &dur {
                if routed_at(&m, &d, *p, f.series) {
                    let superseded = replaced.iter().any(|(a, b)| a < p && p <= b);
                    prop_assert!(seen.contains(p) || superseded, "consumer {} never got {}", d.id.0, p);
                } else {
                    prop_assert!(!seen.contains(p), "consumer {} got unrouted {}", d.id.0, p);
                }
            }
        }
    }

    /// 3, with run-time routes, conditions and timers: within a consumer
    /// session, fact deliveries are in strictly increasing position order,
    /// whatever condition deliveries and `resync` lie between them.
    #[test]
    fn inv03_fact_deliveries_in_order_among_other_deliveries(ops in proptest::collection::vec(arb_delivery_op(), 1..100)) {
        let (mut m, ids) = delivery_machine();
        for op in &ops {
            apply_delivery(&mut m, &ids, op).map_err(TestCaseError::fail)?;
        }
        settle(&mut m);
        m.drain().unwrap();
        for d in delivery_consumers() {
            let mut by_session: BTreeMap<u32, Vec<kvik_model::types::Position>> = BTreeMap::new();
            for r in m.received.get(&d.id).into_iter().flatten() {
                if r.delivery.fact().is_some() {
                    by_session.entry(r.session).or_default().push(r.delivery.position);
                }
            }
            for (s, ps) in by_session {
                prop_assert!(ps.windows(2).all(|w| w[0] < w[1]), "consumer {} session {}: {:?}", d.id.0, s, ps);
            }
        }
    }

    /// 11, for deliveries of every kind: no fact, condition delivery,
    /// `generation_changed` or `resync` names a position beyond the durable
    /// mark.
    #[test]
    fn inv11_no_delivery_of_any_kind_beyond_mark(ops in proptest::collection::vec(arb_delivery_op(), 1..100)) {
        let (mut m, ids) = delivery_machine();
        for op in &ops {
            apply_delivery(&mut m, &ids, op).map_err(TestCaseError::fail)?;
            let last = m.disk.mark.map_or(kvik_model::types::Position::ORIGIN, |k| k.last);
            for r in m.received.values().flatten() {
                prop_assert!(r.delivery.position <= last, "{:?} beyond {}", r.delivery, last);
            }
        }
    }

    /// 13. A cursor or latch on disk is never ahead of the consumer's
    /// confirmation: after every operation, each cursor is at or below the
    /// highest position the consumer has confirmed, and each latch holds the
    /// state and position of a delivery the consumer has confirmed.
    #[test]
    fn inv13_cursor_and_latch_never_ahead_of_confirmation(ops in proptest::collection::vec(arb_delivery_op(), 1..100)) {
        use kvik_model::engine::DeliveryKind;
        let (mut m, ids) = delivery_machine();
        for op in &ops {
            apply_delivery(&mut m, &ids, op).map_err(TestCaseError::fail)?;
            for d in delivery_consumers() {
                let confirmed = m.confirmed.get(&d.id).cloned().unwrap_or_default();
                let cursor = m.disk.cursors.get(&d.id).copied().unwrap_or(kvik_model::types::Position::ORIGIN);
                let top = confirmed.iter().map(|x| x.position).max().unwrap_or(kvik_model::types::Position::ORIGIN);
                prop_assert!(cursor <= top, "consumer {} cursor {} above confirmed {}", d.id.0, cursor, top);
                for ((c, f), latch) in &m.disk.latches {
                    if *c != d.id {
                        continue;
                    }
                    let carried = confirmed.iter().any(|x| x.position == latch.position && match &x.kind {
                        DeliveryKind::Trigger(t) => t.funnel == *f && t.state == latch.state,
                        DeliveryKind::Resync(r) => r.funnels.iter().any(|fs| {
                            fs.funnel == *f && fs.condition.as_ref().is_some_and(|cn| cn.state == latch.state)
                        }),
                        _ => false,
                    });
                    prop_assert!(carried, "consumer {} latch {:?} for {:?} never confirmed", d.id.0, latch, f);
                }
            }
        }
    }

    /// 16. After resume, every condition true now is delivered once in
    /// `resync`: each condition funnel in force appears exactly once, with
    /// its state now computed directly from the current view (a held-for or
    /// stale-for condition true now is reported true; a threshold between its
    /// levels keeps the latch, Q-63), and a transition exactly when that
    /// differs from the latch on disk of the same funnel incarnation.
    #[test]
    fn inv16_resume_delivers_each_condition_once_in_resync(
        ops in proptest::collection::vec(arb_delivery_op(), 1..100),
        by_reconnect in any::<bool>(),
    ) {
        let (mut m, ids) = delivery_machine();
        for op in &ops {
            apply_delivery(&mut m, &ids, op).map_err(TestCaseError::fail)?;
        }
        settle(&mut m);
        let latches = m.disk.latches.clone();
        if by_reconnect {
            for d in delivery_consumers() {
                m.reconnect(d.id).unwrap();
            }
        } else {
            m.crash();
            m.restart();
        }
        m.drain().unwrap();
        for d in delivery_consumers() {
            let routes = m.routes(d.id).unwrap();
            let last = m.received[&d.id].last().unwrap();
            let resync = m.received[&d.id]
                .iter()
                .filter(|r| r.session == last.session)
                .find_map(|r| r.delivery.resync().cloned())
                .expect("a resync after resume");
            let listed: Vec<_> = resync.funnels.iter().map(|f| f.funnel).collect();
            prop_assert_eq!(listed, routes.keys().copied().collect::<Vec<_>>());
            for fs in &resync.funnels {
                let f = &routes[&fs.funnel];
                let Some(cond) = &f.condition else {
                    prop_assert!(fs.condition.is_none());
                    continue;
                };
                let latch = latches
                    .get(&(d.id, fs.funnel))
                    .filter(|l| l.incarnation == fs.incarnation)
                    .map(|l| &l.state);
                let want = condition_now(&m, f, latch);
                let got = fs.condition.clone().unwrap();
                prop_assert_eq!(&got.state, &want, "consumer {} funnel {:?}", d.id.0, fs.funnel);
                prop_assert_eq!(got.transition, cond.is_transition(&want, latch));
            }
        }
    }

    /// 4. The current view equals "highest rank per `(series, t)`" (§2.2:
    /// the position, or in a versioned series `(changed_at, position)`) over
    /// all facts in all tiers. Checked against the view module and against a
    /// naive maximum computed here.
    #[test]
    fn inv04_current_view_over_all_tiers(ops in proptest::collection::vec(arb_op(), 1..80)) {
        let (mut m, ids) = run(&ops)?;
        settle(&mut m);
        let all = m.disk.all_facts();
        prop_assert_eq!(m.current().unwrap(), &view::current(all.iter()));
        let mut naive: BTreeMap<_, kvik_model::types::Fact> = BTreeMap::new();
        for f in &all {
            let key = (f.series, f.t);
            let better = naive.get(&key).is_none_or(|g| {
                let c = |x: &kvik_model::types::Fact| if x.series == ids[VERSIONED] { x.changed_at.unwrap() } else { 0 };
                c(f) > c(g) || (c(f) == c(g) && f.position > g.position)
            });
            if better {
                naive.insert(key, f.clone());
            }
        }
        prop_assert_eq!(m.current().unwrap(), &naive);
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

    /// 9. Compaction, compaction of compacted files, tiering and backup
    /// preserve the set of positions with their content; restore preserves
    /// the backed-up ones, leaving out files retired by a merge.
    #[test]
    fn inv09_tiering_backup_restore_preserve_facts(ops in proptest::collection::vec(arb_op(), 1..80)) {
        let (mut m, _) = run(&ops)?;
        settle(&mut m);
        let committed = durable(&m);
        // Compact, copy, upload, evict; the facts must be the same.
        let _ = m.compact(None);
        m.seal_active();
        let _ = m.compact(None);
        let _ = m.merge_chunks(None);
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

    /// 9 and 8. A restore from a tree checkpoint, possibly with a gap in the
    /// backed-up log, possibly crashing during step 1, brought back step by
    /// step with a crash and restart at a random later step (§8 Restore, row
    /// 28a),
    /// with ingest while restoring, keeps the running engine's trees equal to
    /// what a restart derives, and ends with the facts up to the restored end
    /// and trees equal to a rebuild from the facts. A checkpoint taken
    /// from a tree equal to its rebuild shows no difference, and the
    /// `derived-unverified` label is removed.
    #[test]
    fn inv09_stepwise_restore_from_checkpoint(
        before in proptest::collection::vec(arb_op(), 1..60),
        after in proptest::collection::vec(arb_op(), 0..30),
        crash_at in 0usize..6,
        missing in proptest::option::weighted(0.3, any::<proptest::sample::Index>()),
        interrupt in proptest::option::of(0usize..3),
    ) {
        use kvik_model::engine::{Error, Model};
        let (mut m, ids) = run(&before)?;
        settle(&mut m);
        let rebuilt = m.rebuild_trees().unwrap();
        let clean = m.trees().unwrap().iter().all(|(s, t)| t.same_summaries(&rebuilt[s]));
        m.checkpoint().unwrap();
        for op in &after {
            apply(&mut m, &ids, op).map_err(TestCaseError::fail)?;
        }
        settle(&mut m);
        m.seal_active();
        m.disk.bucket.reachable = true;
        m.upload_checkpoints();
        // Possibly one backed-up file missing, so the log has a gap and the
        // checkpoint may lie beyond the restored end.
        if let Some(i) = missing {
            let files: Vec<_> = m.disk.bucket.objects.keys().copied().collect();
            if !files.is_empty() {
                m.disk.bucket.objects.remove(i.get(&files));
            }
        }
        let cursors = m.disk.cursors.clone();
        let started = match interrupt {
            // A crash after the record, during step 1; the restart finishes it.
            Some(n) => Model::restore_interrupted_after_record(m.config.clone(), consumers(), &mut m.disk.bucket, &cursors, m.wall, n).map(|mut r| {
                prop_assert!(!r.is_up());
                r.restart();
                Ok(r)
            }),
            None => Model::restore_with_cursors(m.config.clone(), consumers(), &mut m.disk.bucket, &cursors, m.wall, true, false).map(Ok),
        };
        let mut r = match started {
            Err(Error::EmptyBucket) => return Ok(()),
            r => r.unwrap()?,
        };
        prop_assert!(r.is_up() && r.disk.restore.as_ref().unwrap().step_one_done);
        let end = r.disk.restored_end.unwrap();
        for i in 0..crash_at {
            if i == 1 {
                // Ingest while restoring: one fact inside restored minutes, one after.
                let _ = r.ingest(vec![
                    kvik_model::types::Reading::num(ids[0], T0 + 7, 1.0e9),
                    kvik_model::types::Reading::num(ids[0], T0 + 50 * MINUTE, 3.0),
                ]);
            }
            r.restore_step().unwrap();
        }
        // What the running engine holds equals what a restart derives.
        let live = r.trees().unwrap().clone();
        r.crash();
        r.restart();
        for (s, t) in &live {
            prop_assert!(t.same_summaries(&r.trees().unwrap()[s]), "series {} differs after restart", s);
        }
        r.restore_rest().unwrap();
        prop_assert!(!r.restoring());
        prop_assert!(!r.checkpoint_unverified());
        let want: BTreeMap<_, _> = durable(&m).into_iter().filter(|(p, _)| *p <= end).collect();
        let got: BTreeMap<_, _> = r.facts().unwrap().into_iter().filter(|f| f.position <= end).map(|f| (f.position, f)).collect();
        prop_assert_eq!(&got, &want);
        prop_assert_eq!(r.current().unwrap(), &view::current(r.facts().unwrap().iter()));
        let rebuilt = r.rebuild_trees().unwrap();
        for (s, t) in r.trees().unwrap() {
            prop_assert!(t.same_summaries(&rebuilt[s]), "series {} differs", s);
        }
        if clean {
            prop_assert!(r.disk.restore.as_ref().unwrap().differences.is_empty());
        }
    }

    /// 14. "No manifest epoch lists two live files holding the same
    /// position" (§7 Manifest), checked at every epoch after every
    /// operation, among compacted files (Q-71: log segments stay listed
    /// beside the chunks compacted from them), with compaction, compaction
    /// of compacted files and crashes on either side of their records (row
    /// 10); and in a store restored from the bucket afterwards.
    #[test]
    fn inv14_no_epoch_lists_two_files_with_one_position(ops in proptest::collection::vec(arb_files_op(), 1..80)) {
        let (mut m, ids) = machine();
        for op in &ops {
            apply(&mut m, &ids, op).map_err(TestCaseError::fail)?;
            prop_assert_eq!(inv14_overlap(&m), None, "after {:?}", op);
        }
        settle(&mut m);
        m.seal_active();
        let _ = m.compact(None);
        let _ = m.merge_chunks(None);
        m.disk.bucket.reachable = true;
        m.upload();
        prop_assert_eq!(inv14_overlap(&m), None);
        if !m.disk.bucket.objects.is_empty() {
            let mut r = kvik_model::engine::Model::restore_from(m.config.clone(), consumers(), &mut m.disk.bucket, m.wall, false).unwrap();
            prop_assert_eq!(inv14_overlap(&r), None, "restored");
            r.seal_active();
            let _ = r.compact(None);
            prop_assert_eq!(inv14_overlap(&r), None, "compacted after the restore");
        }
    }

    /// 12. "No file leaves a tier unless a hash-verified copy exists in
    /// another tier", checked after every operation, with eviction at 90
    /// days, retention classes, fill pressure, retirement and copies to
    /// cold; and nothing is lost doing so (`apply` checks invariant 10).
    #[test]
    fn inv12_no_file_leaves_a_tier_without_a_verified_copy(ops in proptest::collection::vec(arb_files_op(), 1..80)) {
        let (mut m, ids) = files_machine();
        for op in &ops {
            let before = m.clone();
            apply(&mut m, &ids, op).map_err(TestCaseError::fail)?;
            if let Some(e) = inv12_unverified_departure(&before, &m) {
                return Err(TestCaseError::fail(format!("{e} after {op:?}")));
            }
        }
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
    let mut outranked = 0u64;
    let (mut retired, mut leases) = (0usize, 0usize);
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
        outranked += m.counters.outranked_on_arrival;
        retired += m.disk.retired().len();
        leases += m.disk.leases.len();
    }
    eprintln!(
        "facts {facts} revisions {revisions} discarded {discarded} overflows {overflows} compacted {compacted} uploaded {uploaded} tails {tails} kept-after-reboot {kept_after_reboot} recovery-failed {recovery_failed} outranked {outranked} retired {retired} leases {leases}"
    );
    assert!(retired > 10 && leases > 50);
    assert!(facts > 1000 && revisions > 50 && discarded > 10 && overflows > 10);
    assert!(outranked > 20);
    assert!(compacted > 20 && uploaded > 20);
    assert!(tails > 50 && kept_after_reboot > 15 && recovery_failed > 5);
}

/// Guards the files generator: its histories must merge compacted files,
/// remove retired files from the fast disk and keep leases.
#[test]
fn files_generator_coverage() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let strat = proptest::collection::vec(arb_files_op(), 60..80);
    let (mut retired, mut gone, mut leases) = (0usize, 0usize, 0usize);
    let (mut moved, mut current_moved) = (0usize, 0usize);
    for _ in 0..200 {
        let ops = strat.new_tree(&mut runner).unwrap().current();
        let (mut m, ids) = machine();
        for op in &ops {
            apply(&mut m, &ids, op).unwrap();
        }
        let r = m.disk.retired();
        retired += r.len();
        gone += r.keys().filter(|f| !m.disk.hot.contains(f)).count();
        leases += m.disk.leases.len();
        let (mut m, ids) = files_machine();
        for op in &ops {
            apply(&mut m, &ids, op).unwrap();
        }
        moved += m
            .disk
            .manifest
            .iter()
            .map(|r| match &r.op {
                kvik_model::engine::ManifestOp::Publish { moved, .. } => moved.len(),
                _ => 0,
            })
            .sum::<usize>();
        current_moved += m
            .disk
            .published()
            .iter()
            .filter(|f| {
                m.disk.files[f].class == kvik_model::engine::FileClass::Current
                    && !m.disk.hot.contains(f)
            })
            .count();
    }
    eprintln!(
        "retired {retired} removed {gone} leases {leases} moved {moved} current-moved {current_moved}"
    );
    assert!(retired > 200 && gone > 15 && leases > 30);
    assert!(moved > 200 && current_moved > 50);
}

/// Guards invariant 2's exception: a `resync` in place of a replay is
/// excused only if, when it was issued, some position it replaced was not in
/// a local log segment. Segments that leave the fast disk later do not excuse
/// it.
#[test]
fn inv02_resync_excused_by_locality_when_issued() {
    use kvik_model::types::{DAY, Reading, SECOND};
    let (mut m, ids) = delivery_machine();
    let c = ConsumerId(0);
    let mut basis = ResyncBasis::new();
    m.ingest(vec![Reading::num(ids[0], m.wall, 1.0)]).unwrap();
    m.drain().unwrap();
    m.advance(SECOND);
    m.ingest(vec![Reading::num(ids[0], m.wall, 2.0)]).unwrap();
    m.seal_active();
    m.upload();
    m.advance(91 * DAY);
    m.housekeeping().unwrap();
    track_resyncs(&mut m, &mut basis, |m| m.reconnect(c)).unwrap();
    m.drain().unwrap();
    let justified = resync_replaced_justified(&m, &basis, 0);
    assert_eq!(
        justified.len(),
        1,
        "the segment was gone when it was issued"
    );
    // The same `resync`, had it been issued while every position was in a
    // local log segment, is not excused by the segment's later departure.
    let all: BTreeSet<_> = m.disk.all_entries().into_keys().collect();
    for v in basis.values_mut() {
        *v = all.clone();
    }
    assert!(resync_replaced_justified(&m, &basis, 0).is_empty());
}

/// Guards the delivery generator: the random histories must actually
/// exercise condition deliveries of each cause, resync in place of a replay,
/// overflow, run-time routes and latches.
#[test]
fn delivery_generator_coverage() {
    use kvik_model::engine::{DeliveryKind, TriggerCause};
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let strat = proptest::collection::vec(arb_delivery_op(), 80..100);
    let mut causes: BTreeMap<String, usize> = BTreeMap::new();
    let (mut replaced, mut resyncs, mut overflows, mut routes, mut latches, mut equal) =
        (0usize, 0usize, 0u64, 0usize, 0usize, 0usize);
    for _ in 0..200 {
        let ops = strat.new_tree(&mut runner).unwrap().current();
        let (mut m, ids) = delivery_machine();
        for op in &ops {
            apply_delivery(&mut m, &ids, op).unwrap();
        }
        settle(&mut m);
        m.drain().unwrap();
        for r in m.received.values().flatten() {
            match &r.delivery.kind {
                DeliveryKind::Trigger(t) => {
                    let k = match t.cause {
                        TriggerCause::Fact => "fact",
                        TriggerCause::Timer => "timer",
                        TriggerCause::Reattach => "reattach",
                    };
                    *causes.entry(k.into()).or_default() += 1;
                }
                DeliveryKind::Resync(x) => {
                    resyncs += 1;
                    replaced += usize::from(x.replaced.is_some());
                    equal += x
                        .funnels
                        .iter()
                        .filter(|f| f.condition.as_ref().is_some_and(|c| !c.transition))
                        .count();
                }
                _ => {}
            }
        }
        overflows += m.counters.overflows;
        latches += m.disk.latches.len();
        routes += m
            .entries()
            .unwrap()
            .iter()
            .filter(|e| matches!(e.body, kvik_model::engine::EntryBody::Route(_)))
            .count();
    }
    eprintln!(
        "causes {causes:?} resyncs {resyncs} replaced {replaced} equal-to-latch {equal} overflows {overflows} routes {routes} latches {latches}"
    );
    assert!(causes["fact"] > 50 && causes["timer"] > 30 && causes["reattach"] > 10);
    assert!(resyncs > 500 && replaced > 10 && equal > 100);
    assert!(overflows > 30 && routes > 400 && latches > 200);
}

// ----- the duplicate rule and versioned series (invariants 15 and 19) -----

/// One step of the duplicate machine: a reading on a small pool of keys,
/// values, qualities and `changed_at` (for the versioned series), committed
/// alone, or a step that moves detail.
#[derive(Clone, Debug)]
enum DupOp {
    Emit {
        versioned: bool,
        key: usize,
        value: usize,
        quality: usize,
        changed_at: u64,
        revise: bool,
    },
    Seal,
    Compact,
    Upload,
    CopyCold,
    /// Advance past the eviction age, then housekeeping (fill pressure too).
    Evict,
    Restart,
}

const DUP_VALUES: [f64; 4] = [1.0, 2.0, -0.0, f64::NAN];
const DUP_QUALITIES: [kvik_model::types::Quality; 3] = [
    kvik_model::types::Quality::Good,
    kvik_model::types::Quality::Estimated,
    kvik_model::types::Quality::Bad,
];
const DUP_KEYS: [Nanos; 3] = [T0, T0 + SECOND, T0 + SECOND + 1];

fn arb_dup_step() -> impl Strategy<Value = DupOp> {
    prop_oneof![
        2 => Just(DupOp::Seal),
        2 => Just(DupOp::Compact),
        2 => Just(DupOp::Upload),
        1 => Just(DupOp::CopyCold),
        2 => Just(DupOp::Evict),
        1 => Just(DupOp::Restart),
    ]
}

fn arb_dup_op() -> impl Strategy<Value = DupOp> {
    prop_oneof![
        12 => (any::<bool>(), 0usize..3, 0usize..DUP_VALUES.len(), 0usize..DUP_QUALITIES.len(), 0u64..4, proptest::bool::weighted(0.1))
            .prop_map(|(versioned, key, value, quality, changed_at, revise)| DupOp::Emit {
                versioned, key, value, quality, changed_at, revise: revise && !versioned,
            }),
        10 => arb_dup_step(),
    ]
}

/// A store with a gauge and a versioned gauge whose fast disk is small
/// enough for fill pressure to move current detail out (§8 Fill pressure).
fn dup_machine() -> (kvik_model::engine::Model, [kvik_model::types::SeriesId; 2]) {
    use kvik_model::engine::{Config, Model};
    use kvik_model::registry::{ChangedAtKind, SeriesOptions};
    use kvik_model::types::{HOUR, Kind, ValueType};
    let cfg = Config {
        disk_bytes: 1_200,
        entry_bytes: 64,
        low_mark: 0,
        critical_mark: 0,
        seal_after_batches: 4,
        ..Config::default()
    };
    let mut m = Model::new(cfg, vec![], T0 + HOUR);
    let s = m
        .define(NAMES[0], Kind::Gauge, ValueType::Number, "1", 5 * SECOND)
        .unwrap();
    let v = m
        .define_with(
            NAMES[VERSIONED],
            Kind::Gauge,
            ValueType::Number,
            "1",
            5 * SECOND,
            SeriesOptions::versioned(ChangedAtKind::Version),
        )
        .unwrap();
    (m, [s, v])
}

/// Applies a step that is not an emit.
fn apply_dup_step(m: &mut kvik_model::engine::Model, op: &DupOp) {
    match op {
        DupOp::Emit { .. } => unreachable!("emits are checked by the caller"),
        DupOp::Seal => m.seal_active(),
        DupOp::Compact => {
            let _ = m.compact(None);
        }
        DupOp::Upload => {
            m.upload();
        }
        DupOp::CopyCold => {
            for f in m.disk.hot.clone() {
                if m.disk.files[&f].sealed {
                    m.copy_to_cold(f);
                }
            }
        }
        DupOp::Evict => {
            m.advance(100 * kvik_model::types::DAY);
            let _ = m.housekeeping();
        }
        DupOp::Restart => m.restart(),
    }
}

/// The reading an emit step submits.
fn dup_reading(ids: [kvik_model::types::SeriesId; 2], op: &DupOp) -> kvik_model::types::Reading {
    let DupOp::Emit {
        versioned,
        key,
        value,
        quality,
        changed_at,
        ..
    } = op
    else {
        unreachable!("an emit")
    };
    kvik_model::types::Reading {
        quality: DUP_QUALITIES[*quality],
        changed_at: versioned.then_some(*changed_at),
        ..kvik_model::types::Reading::num(
            ids[usize::from(*versioned)],
            DUP_KEYS[*key],
            DUP_VALUES[*value],
        )
    }
}

proptest! {
    #![proptest_config(cases(256))]

    /// 15. An arriving fact is dropped as a duplicate iff it is identical
    /// (§2.4: value, compared bitwise, "no value" for a non-finite one,
    /// adapter-supplied quality and, in a versioned series, `changed_at`) to
    /// a fact in the history of its `(series, t)` and lacks `revise`, except
    /// `duplicates_unchecked`: when the current fact of its key is on no
    /// local tier, or in a versioned series when a fact with its
    /// `changed_at` is on no local tier and none local is identical, it is
    /// recorded and counted. A versioned fact whose `changed_at` is above
    /// every one of its key's needs no check. The test keeps its own history
    /// from the disk.
    #[test]
    fn inv15_dropped_iff_identical_without_revise(ops in proptest::collection::vec(arb_dup_op(), 1..80)) {
        let (mut m, ids) = dup_machine();
        for op in &ops {
            let DupOp::Emit { revise, .. } = op else {
                apply_dup_step(&mut m, op);
                continue;
            };
            let r = dup_reading(ids, op);
            let identity = r.value.as_f64().filter(|v| v.is_finite()).map_or(Value::Absent, Value::Num);
            let history: Vec<_> = durable(&m).into_values().filter(|f| f.series == r.series && f.t == r.t).collect();
            let local = m.disk.local_positions();
            let same = |f: &kvik_model::types::Fact| f.value.identical(&identity) && f.supplied_quality == r.quality;
            let (seen, off) = match r.changed_at {
                None => (
                    history.iter().any(same),
                    history.iter().max_by_key(|f| f.position).is_some_and(|c| !local.contains(&c.position)),
                ),
                Some(c) => {
                    let equal: Vec<_> = history.iter().filter(|f| f.changed_at == Some(c)).collect();
                    let seen = equal.iter().any(|f| local.contains(&f.position) && same(f));
                    (seen, !seen && equal.iter().any(|f| !local.contains(&f.position)))
                }
            };
            let dropped = !*revise && !off && seen;
            let unchecked = m.counters.duplicates_unchecked;
            if *revise { m.emit_revise(r).unwrap() } else { m.emit(r).unwrap() }
            let c = m.commit(None).unwrap();
            prop_assert_eq!(c.duplicates, usize::from(dropped), "{:?}", op);
            prop_assert_eq!(c.positions.len(), usize::from(!dropped));
            prop_assert_eq!(m.counters.duplicates_unchecked - unchecked, u64::from(!*revise && off));
        }
    }

    /// 19. "For a source whose `changed_at` rises with every revision,
    /// re-submitting readings already recorded on a versioned series never
    /// changes the value, adapter-supplied quality or `changed_at` of any
    /// current fact while the version index is local, whether or not the
    /// duplicate check could run." Revisions take the next `changed_at` of
    /// their key; re-submissions repeat a recorded fact exactly, also while
    /// its detail is off every local tier.
    #[test]
    fn inv19_resubmission_never_changes_a_current_fact(
        steps in proptest::collection::vec(prop_oneof![
            6 => (0usize..3, 0usize..DUP_VALUES.len(), 0usize..DUP_QUALITIES.len())
                .prop_map(|(key, value, quality)| (Some((key, value, quality)), None, None)),
            6 => any::<proptest::sample::Index>().prop_map(|i| (None, Some(i), None)),
            6 => arb_dup_step().prop_map(|s| (None, None, Some(s))),
        ], 1..80),
    ) {
        use kvik_model::types::Reading;
        let (mut m, ids) = dup_machine();
        let v = ids[1];
        let identity = |m: &kvik_model::engine::Model| -> BTreeMap<Nanos, (String, kvik_model::types::Quality, Option<u64>)> {
            m.current().unwrap().iter().filter(|((s, _), _)| *s == v)
                .map(|((_, t), f)| (*t, (format!("{:?}", f.value), f.supplied_quality, f.changed_at))).collect()
        };
        for step in &steps {
            match step {
                (Some((key, value, quality)), _, _) => {
                    let t = DUP_KEYS[*key];
                    let next = durable(&m).values().filter(|f| f.series == v && f.t == t)
                        .filter_map(|f| f.changed_at).max().map_or(0, |c| c + 1);
                    m.ingest(vec![Reading { quality: DUP_QUALITIES[*quality], ..Reading::versioned(v, t, DUP_VALUES[*value], next) }]).unwrap();
                }
                (_, Some(i), _) => {
                    let recorded: Vec<_> = durable(&m).into_values().filter(|f| f.series == v).collect();
                    if recorded.is_empty() {
                        continue;
                    }
                    let f = i.get(&recorded);
                    let value = f.value.as_f64().unwrap_or(f64::NAN);
                    let before = identity(&m);
                    m.ingest(vec![Reading { quality: f.supplied_quality, ..Reading::versioned(v, f.t, value, f.changed_at.unwrap()) }]).unwrap();
                    prop_assert_eq!(identity(&m), before, "re-submitting {:?}", f);
                }
                (_, _, Some(s)) => apply_dup_step(&mut m, s),
                _ => unreachable!(),
            }
        }
    }
}

proptest! {
    #![proptest_config(cases(256))]

    /// 15 and 19, and row 2a, across a restore (§8 Restore step 2): history
    /// on a gauge and a versioned gauge before a tree checkpoint, versioned
    /// revisions after it (each with the next `changed_at` of its key), then
    /// a restore that has brought back only some files. Re-submitting
    /// recorded readings is either dropped or recorded and counted
    /// `duplicates_unchecked`, never silently recorded; a replay of an
    /// earlier value of a key revised before the checkpoint is dropped (the
    /// checkpoint carries the revised-key index); and once the restore ends,
    /// no current fact's value, adapter-supplied quality or `changed_at` has
    /// changed.
    #[test]
    fn inv19_resubmission_across_a_restore(
        before in proptest::collection::vec((any::<bool>(), 0usize..3, 0usize..DUP_VALUES.len(), 0usize..DUP_QUALITIES.len()), 1..20),
        after in proptest::collection::vec((0usize..3, 0usize..DUP_VALUES.len()), 0..6),
        replays in proptest::collection::vec(any::<proptest::sample::Index>(), 1..10),
        steps in 0usize..4,
    ) {
        use kvik_model::engine::{Config, Model};
        use kvik_model::registry::{ChangedAtKind, SeriesOptions};
        use kvik_model::types::{HOUR, Kind, Reading, ValueType};
        let mut m = Model::new(Config::default(), vec![], T0 + HOUR);
        let s = m.define(NAMES[0], Kind::Gauge, ValueType::Number, "1", 5 * SECOND).unwrap();
        let v = m
            .define_with(NAMES[VERSIONED], Kind::Gauge, ValueType::Number, "1", 5 * SECOND,
                SeriesOptions::versioned(ChangedAtKind::Version))
            .unwrap();
        let next = |m: &Model, t: Nanos| durable(m).values().filter(|f| f.series == v && f.t == t)
            .filter_map(|f| f.changed_at).max().map_or(0, |c| c + 1);
        for (versioned, key, value, quality) in &before {
            let t = DUP_KEYS[*key];
            let r = if *versioned {
                Reading::versioned(v, t, DUP_VALUES[*value], next(&m, t))
            } else {
                Reading::num(s, t, DUP_VALUES[*value])
            };
            m.ingest(vec![Reading { quality: DUP_QUALITIES[*quality], ..r }]).unwrap();
        }
        m.seal_active();
        m.compact(None).unwrap();
        m.checkpoint().unwrap();
        for (key, value) in &after {
            let t = DUP_KEYS[*key];
            m.ingest(vec![Reading::versioned(v, t, DUP_VALUES[*value], next(&m, t))]).unwrap();
        }
        m.seal_active();
        prop_assert_eq!(m.upload_checkpoints(), 1);
        let identity = |m: &Model| -> BTreeMap<_, _> {
            m.current().unwrap().iter()
                .map(|(k, f)| (*k, (format!("{:?}", f.value), f.supplied_quality, f.changed_at)))
                .collect()
        };
        let want = identity(&m);
        let recorded: Vec<_> = durable(&m).into_values().collect();
        let revised: BTreeSet<_> = {
            let mut seen = BTreeSet::new();
            recorded.iter().filter(|f| f.series == s).filter(|f| !seen.insert((f.series, f.t)))
                .map(|f| (f.series, f.t)).collect()
        };
        let mut r = restore_in_progress(&mut m, &BTreeMap::new());
        for _ in 0..steps {
            r.restore_step().unwrap();
        }
        for i in &replays {
            let f = i.get(&recorded);
            let unchecked = r.counters.duplicates_unchecked;
            let value = f.value.as_f64().unwrap_or(f64::NAN);
            let c = r.ingest(vec![Reading { quality: f.supplied_quality, changed_at: f.changed_at, ..Reading::num(f.series, f.t, value) }]).unwrap();
            let counted = r.counters.duplicates_unchecked - unchecked;
            prop_assert!(c.duplicates == 1 || (c.positions.len() == 1 && counted == 1), "replay of {:?}", f);
            if revised.contains(&(f.series, f.t)) {
                prop_assert_eq!(c.duplicates, 1, "row 2a: replay of a revised key {:?}", f);
            }
        }
        r.restore_rest().unwrap();
        prop_assert_eq!(identity(&r), want);
    }
}

/// Guards the duplicate machine: its histories must reach identical repeats,
/// `revise`, repeats whose current fact is off every local tier and
/// outranked facts.
#[test]
fn dup_generator_coverage() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let strat = proptest::collection::vec(arb_dup_op(), 60..80);
    let (mut dropped, mut unchecked, mut revised, mut outranked) = (0u64, 0u64, 0usize, 0u64);
    for _ in 0..200 {
        let ops = strat.new_tree(&mut runner).unwrap().current();
        let (mut m, ids) = dup_machine();
        for op in &ops {
            match op {
                DupOp::Emit { revise, .. } => {
                    let r = dup_reading(ids, op);
                    if *revise {
                        m.emit_revise(r).unwrap();
                        revised += 1;
                    } else {
                        m.emit(r).unwrap();
                    }
                    m.commit(None).unwrap();
                }
                other => apply_dup_step(&mut m, other),
            }
        }
        dropped += m.counters.duplicates_dropped;
        unchecked += m.counters.duplicates_unchecked;
        outranked += m.counters.outranked_on_arrival;
    }
    eprintln!("dropped {dropped} unchecked {unchecked} revised {revised} outranked {outranked}");
    assert!(dropped > 500 && unchecked > 100 && revised > 100 && outranked > 300);
}
