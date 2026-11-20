//! Property tests of series labels (spec §2.1 Labels, §2.3 Label views): order of
//! calls, as-of cuts, withdrawals, history and replay under lowered limits.

use std::collections::{BTreeMap, BTreeSet};

use kvik_model::labels::{
    Effective, LabelChange, LabelEntry, LabelError, LabelItem, LabelLimits, LabelLog, LabelView,
    StoreLimit,
};
use kvik_model::types::{Position, SeriesId};
use proptest::prelude::*;

use super::{Call, S1, S2, Store, snapshot};

fn arb_from() -> impl Strategy<Value = Effective> {
    prop_oneof![
        1 => Just(Effective::Always),
        4 => (0i64..4).prop_map(|k| Effective::At(k * 10)),
    ]
}

fn arb_item() -> impl Strategy<Value = LabelItem> {
    prop_oneof![
        3 => prop_oneof![Just("x"), Just("y")].prop_map(|v| LabelItem::Set(v.to_string())),
        1 => Just(LabelItem::Remove),
        2 => Just(LabelItem::Withdraw),
    ]
}

fn arb_change() -> impl Strategy<Value = LabelChange> {
    prop::collection::btree_map(
        prop_oneof![Just("a"), Just("b"), Just("c")],
        arb_item(),
        1..=3,
    )
    .prop_map(|m| {
        m.into_iter()
            .fold(LabelChange::new(), |c, (k, i)| c.item(k, i))
    })
}

fn arb_call() -> impl Strategy<Value = Call> {
    (prop_oneof![Just(S1), Just(S2)], arb_from(), arb_change())
}

fn triples(c: &Call) -> BTreeSet<(SeriesId, String, Effective)> {
    c.2.items()
        .iter()
        .map(|(k, _)| (c.0, k.clone(), c.1))
        .collect()
}

/// A reordering of `calls` that keeps the order of every two calls sharing a
/// `(series, key, from)`, chosen by `priority`.
fn reorder(calls: &[Call], priority: &[u32]) -> Vec<Call> {
    let mut left: Vec<usize> = (0..calls.len()).collect();
    let mut out = Vec::new();
    while !left.is_empty() {
        let free = left
            .iter()
            .copied()
            .filter(|&i| {
                left.iter()
                    .all(|&j| j >= i || triples(&calls[i]).is_disjoint(&triples(&calls[j])))
            })
            .min_by_key(|&i| (priority[i % priority.len()], i))
            .expect("the earliest call left is always free");
        left.retain(|&i| i != free);
        out.push(calls[free].clone());
    }
    out
}

fn run(calls: &[Call]) -> Store {
    let mut st = Store::new(2);
    for c in calls {
        st.many(std::slice::from_ref(c)).unwrap();
    }
    st
}

proptest! {
    #[test]
    fn s2_1_labels_the_label_set_is_independent_of_the_order_of_calls(
        calls in prop::collection::vec(arb_call(), 0..24),
        priority in prop::collection::vec(any::<u32>(), 1..24),
    ) {
        let a = run(&calls);
        let b = run(&reorder(&calls, &priority));
        prop_assert_eq!(
            snapshot(&a.log, &a.reg, LabelView::Current),
            snapshot(&b.log, &b.reg, LabelView::Current)
        );
    }

    #[test]
    fn s2_3_label_views_as_of_p_equals_a_replay_cut_at_p(
        batches in prop::collection::vec(prop::collection::vec(arb_call(), 1..3), 0..16),
        pick in any::<prop::sample::Index>(),
        tail in prop::collection::vec(arb_call(), 0..6),
    ) {
        let mut st = Store::new(2);
        for b in &batches {
            st.many(b).unwrap();
        }
        let entries = st.log.entries().to_vec();
        let i = pick.index(entries.len() + 1);
        let p = if i == 0 { Position::ORIGIN } else { entries[i - 1].position };
        let cut = LabelLog::replay(entries[..i].iter().cloned());
        prop_assert_eq!(&st.log.cut(p), &cut);
        let want = snapshot(&cut, &st.reg, LabelView::Current);
        prop_assert_eq!(snapshot(&st.log, &st.reg, LabelView::AsOf(p)), want.clone());
        let t = if i == 0 { 0 } else { entries[i - 1].recorded_at };
        let by_time = if i < entries.len() && entries[i].recorded_at == t {
            None
        } else {
            Some(snapshot(&st.log, &st.reg, LabelView::AsOfTime(t)))
        };
        if let Some(by_time) = by_time {
            prop_assert_eq!(by_time, want.clone());
        }
        let mut restored = Store { log: cut, next: Position::first(1), ..Store::new(2) };
        for c in &tail {
            restored.many(std::slice::from_ref(c)).unwrap();
        }
        prop_assert_eq!(snapshot(&restored.log, &restored.reg, LabelView::AsOf(p)), want);
    }

    #[test]
    fn s2_1_labels_a_withdrawal_reads_as_no_change_whatever_is_corrected_before_it(
        calls in prop::collection::vec(arb_call(), 0..24),
    ) {
        let st = run(&calls);
        let mut last: BTreeMap<(SeriesId, String, Effective), LabelItem> = BTreeMap::new();
        for e in st.log.entries() {
            for (k, i) in &e.items {
                last.insert((e.series, k.clone(), e.from), i.clone());
            }
        }
        let withdrawn: BTreeSet<_> = last
            .into_iter()
            .filter(|(_, i)| *i == LabelItem::Withdraw)
            .map(|(t, _)| t)
            .collect();
        let stripped = st.log.entries().iter().filter_map(|e| {
            let items: Vec<_> = e
                .items
                .iter()
                .filter(|(k, _)| !withdrawn.contains(&(e.series, k.clone(), e.from)))
                .cloned()
                .collect();
            (!items.is_empty()).then(|| LabelEntry { items, ..e.clone() })
        });
        let without = LabelLog::replay(stripped);
        prop_assert_eq!(
            snapshot(&st.log, &st.reg, LabelView::Current),
            snapshot(&without, &st.reg, LabelView::Current)
        );
    }

    #[test]
    fn s2_1_labels_history_never_shrinks(
        ops in prop::collection::vec((0u8..7, prop::collection::vec(arb_call(), 1..3), 0i64..40), 0..24),
    ) {
        let mut st = Store::new(2);
        for (kind, calls, now) in ops {
            let before_log = st.log.clone();
            let before: Vec<_> = [S1, S2].iter().map(|s| st.log.history(&st.reg, *s)).collect();
            let r = match kind {
                0 | 1 => st.many(&calls),
                2 => st.declare(calls[0].0, &[("a", "x"), ("b", "y")], now),
                3 => st.many(&[(calls[0].0, calls[0].1, LabelChange::new().set("Bad", "x"))]),
                4 => st.many(&[(SeriesId(9), calls[0].1, calls[0].2.clone())]),
                5 => {
                    let limits = st.limits;
                    st.limits.versions = st.log.versions();
                    let r = st.many(&calls);
                    st.limits = limits;
                    r
                }
                _ => {
                    let limits = st.limits;
                    st.limits.bytes = st.log.bytes();
                    let r = st.many(&calls);
                    st.limits = limits;
                    r
                }
            };
            if r.is_err() {
                prop_assert_eq!(&st.log, &before_log);
            }
            for (s, old) in [S1, S2].iter().zip(before) {
                let new = st.log.history(&st.reg, *s);
                prop_assert!(new.len() >= old.len());
                for (o, n) in old.iter().zip(&new) {
                    prop_assert_eq!(
                        (&o.key, o.from, o.position, &o.item),
                        (&n.key, n.from, n.position, &n.item)
                    );
                    prop_assert!(!o.superseded || n.superseded);
                }
            }
        }
    }

    #[test]
    fn s2_1_labels_replay_under_a_lowered_limit_applies_every_entry(
        calls in prop::collection::vec(arb_call(), 1..24),
        cut in (0.0f64..1.0, 0.0f64..1.0, 0.0f64..1.0),
    ) {
        let st = run(&calls);
        let per_series = [S1, S2]
            .iter()
            .map(|s| st.log.history(&st.reg, *s).len() as u64)
            .max()
            .unwrap();
        let lowered = LabelLimits {
            versions_per_series: (per_series as f64 * cut.0) as u64,
            versions: (st.log.versions() as f64 * cut.1) as u64,
            bytes: (st.log.bytes() as f64 * cut.2) as u64,
        };
        let replayed = LabelLog::replay(st.log.entries().to_vec());
        prop_assert_eq!(&replayed, &st.log);
        prop_assert_eq!(
            snapshot(&replayed, &st.reg, LabelView::Current),
            snapshot(&st.log, &st.reg, LabelView::Current)
        );
        let n = replayed.notices(&st.reg, &lowered);
        prop_assert!(n.store.contains(&StoreLimit::Versions));
        prop_assert!(n.store.contains(&StoreLimit::Bytes));
        prop_assert!(!n.series.is_empty());

        let mut again = Store { log: replayed, limits: lowered, next: st.next, ..Store::new(2) };
        let s = st.log.entries()[0].series;
        let used = again.log.history(&again.reg, s).len() as u64;
        let want = if used + 1 > lowered.versions_per_series {
            LabelError::SeriesLimit { limit: lowered.versions_per_series }
        } else {
            LabelError::StoreLimit { which: StoreLimit::Versions, limit: lowered.versions }
        };
        let r = again.set(s, Effective::At(999), LabelChange::new().set("z", "v"));
        prop_assert_eq!(r, Err(want));
        let e = &st.log.entries()[st.log.entries().len() - 1];
        let (k, i) = e.items[0].clone();
        let o = again.set(e.series, e.from, LabelChange::new().item(&k, i)).unwrap();
        prop_assert_eq!((o.recorded, o.duplicates), (0, 1));
    }
}

#[test]
fn labels_generator_coverage() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let (mut reordered, mut withdrawn_over_earlier) = (0, 0);
    for _ in 0..500 {
        let calls = prop::collection::vec(arb_call(), 0..24)
            .new_tree(&mut runner)
            .unwrap()
            .current();
        let priority = prop::collection::vec(any::<u32>(), 1..24)
            .new_tree(&mut runner)
            .unwrap()
            .current();
        if reorder(&calls, &priority) != calls {
            reordered += 1;
        }
        let st = run(&calls);
        let h = st.log.history(&st.reg, S1);
        if h.iter().any(|w| {
            !w.superseded
                && w.item == LabelItem::Withdraw
                && h.iter()
                    .any(|e| e.key == w.key && e.from < w.from && e.position > w.position)
        }) {
            withdrawn_over_earlier += 1;
        }
    }
    assert!(reordered > 100, "{reordered}");
    assert!(withdrawn_over_earlier > 25, "{withdrawn_over_earlier}");
}
