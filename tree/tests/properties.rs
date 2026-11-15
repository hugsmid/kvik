//! Merge laws (spec §5.3, invariant 6), agreement with the reference model, and the byte layout's round trip
//! (`docs/format-tree.md`), over generated readings of every kind.

mod support;

use kvik_model as km;
use kvik_tree::{
    Body, Node, Value, credit_intervals, decode, encode, fold, merge, merge_all, with_stretches,
};
use proptest::prelude::*;
use support::*;

fn bytes(n: &Node) -> Vec<u8> {
    encode(n).expect("a fold or merge of readings is storable")
}

/// Min and max by bit pattern, so −0.0 and +0.0 differ.
fn extremes(n: &Node) -> Option<(u64, u64)> {
    match n.body {
        Body::Gauge { min, max, .. } | Body::Interval { min, max, .. } => {
            Some((min?.to_bits(), max?.to_bits()))
        }
        _ => None,
    }
}

/// Min and max of readings in IEEE total order (Q-181), by bit pattern.
fn total_order_extremes(r: &[(i64, Value)]) -> Option<(u64, u64)> {
    let mut x = r.iter().filter_map(|(_, v)| v.as_f64());
    let first = x.next()?;
    let (lo, hi) = x.fold((first, first), |(lo, hi), v| {
        (
            if v.total_cmp(&lo).is_lt() { v } else { lo },
            if v.total_cmp(&hi).is_gt() { v } else { hi },
        )
    });
    Some((lo.to_bits(), hi.to_bits()))
}

fn parts(c: &Case, edges: &[i64]) -> Vec<Node> {
    edges
        .windows(2)
        .map(|w| fold(&c.p, w[0], w[1], c.within(w[0], w[1])))
        .collect()
}

proptest! {
    #[test]
    fn folding_any_split_equals_folding_all(
        c in case(),
        cuts in prop::collection::vec(0u16..=1000, 0..8),
    ) {
        let all = fold(&c.p, c.start, c.end, &c.readings);
        let merged = merge_all(&c.p, c.start, &parts(&c, &c.edges(&cuts)));
        prop_assert_eq!(&merged, &all);
        prop_assert_eq!(extremes(&merged), extremes(&all));
        prop_assert_eq!(bytes(&merged), bytes(&all));
    }

    #[test]
    fn merge_is_associative(c in case(), cuts in prop::collection::vec(0u16..=1000, 2)) {
        let n = parts(&c, &c.edges(&cuts));
        let (a, b, d) = (&n[0], &n[1], &n[2]);
        let left = merge(&c.p, &merge(&c.p, a, b), d);
        let right = merge(&c.p, a, &merge(&c.p, b, d));
        prop_assert_eq!(&left, &right);
        prop_assert_eq!(extremes(&left), extremes(&right));
        prop_assert_eq!(bytes(&left), bytes(&right));
    }

    #[test]
    fn the_empty_node_is_the_identity_on_either_side(c in case(), cut in 0u16..=1000) {
        let e = c.edges(&[cut]);
        let n = fold(&c.p, e[0], e[1], c.within(e[0], e[1]));
        let before = Node::empty(c.p.kind, e[0] - 7, e[0]);
        let after = Node::empty(c.p.kind, e[1], e[1] + 7);
        let l = merge(&c.p, &before, &n);
        let r = merge(&c.p, &n, &after);
        prop_assert_eq!((l.start, l.end, r.start, r.end), (e[0] - 7, e[1], e[0], e[1] + 7));
        prop_assert_eq!(Node { start: e[0], ..l }, n.clone());
        prop_assert_eq!(Node { end: e[1], ..r }, n);
    }

    #[test]
    fn fold_and_merge_equal_the_model(c in case(), cut in 0u16..=1000) {
        let mp = model_params(&c.p);
        let e = c.edges(&[cut]);
        let (a, b) = (
            fold(&c.p, e[0], e[1], c.within(e[0], e[1])),
            fold(&c.p, e[1], e[2], c.within(e[1], e[2])),
        );
        let (ma, mb) = (
            km::summary::direct(&mp, e[0], e[1], &model_readings(c.within(e[0], e[1]))),
            km::summary::direct(&mp, e[1], e[2], &model_readings(c.within(e[1], e[2]))),
        );
        prop_assert_eq!(model_node(&a), ma.clone());
        prop_assert_eq!(model_node(&b), mb.clone());
        let ab = merge(&c.p, &a, &b);
        prop_assert_eq!(model_node(&ab), km::summary::merge(&mp, &ma, &mb));
        if matches!(ab.body, Body::Gauge { .. } | Body::Interval { .. }) {
            prop_assert_eq!(extremes(&a), total_order_extremes(c.within(e[0], e[1])));
            prop_assert_eq!(extremes(&ab), total_order_extremes(c.within(e[0], e[2])));
        }
    }

    #[test]
    fn window_credit_equals_the_model(c in case(), cuts in prop::collection::vec(0u16..=1000, 2)) {
        let mp = model_params(&c.p);
        let e = c.edges(&cuts);
        let (start, until) = (e[1], e[2]);
        let inside = c.within(start, until);
        let got = if c.p.interval.is_some() {
            let d = c.p.interval.unwrap();
            let mut n = Node::empty(c.p.kind, start, until);
            credit_intervals(&c.p, &mut n, c.within(start - d + 1, until), start, until);
            n
        } else {
            let prior = c.within(c.start, start).last();
            with_stretches(&c.p, fold(&c.p, start, until, inside), prior, until)
        };
        let want = if let Some(d) = c.p.interval {
            let mut n = km::summary::Node::empty(mp.kind, start, until);
            let r = model_readings(c.within(start - d + 1, until));
            km::summary::credit_intervals(&mp, &mut n, &r, start, until);
            n
        } else {
            let prior = c.within(c.start, start).last().map(|(t, v)| (*t, model_value(v)));
            let core = km::summary::direct(&mp, start, until, &model_readings(inside));
            km::summary::with_stretches(&mp, core, prior.as_ref(), until)
        };
        prop_assert_eq!(model_node(&got), want);
    }

    #[test]
    fn every_folded_or_merged_node_round_trips_through_its_bytes(
        c in case(),
        cuts in prop::collection::vec(0u16..=1000, 0..4),
    ) {
        let mut nodes = parts(&c, &c.edges(&cuts));
        nodes.push(merge_all(&c.p, c.start, &nodes.clone()));
        for n in nodes {
            let b = bytes(&n);
            let back = decode(&b).unwrap();
            prop_assert_eq!(&back, &n);
            prop_assert_eq!(extremes(&back), extremes(&n));
            prop_assert_eq!(bytes(&back), b);
        }
    }
}

#[test]
fn signed_zeros_give_the_same_bytes_in_any_order_of_folding() {
    let p = kvik_tree::Params::new(kvik_tree::Kind::Gauge, None);
    let r = |a: f64, b: f64| vec![(0, Value::F64(a)), (SECOND, Value::F64(b))];
    let one = fold(&p, 0, 2 * SECOND, &r(0.0, -0.0));
    let other = fold(&p, 0, 2 * SECOND, &r(-0.0, 0.0));
    let split = merge(
        &p,
        &fold(&p, 0, SECOND, &r(0.0, -0.0)[..1]),
        &fold(&p, SECOND, 2 * SECOND, &r(0.0, -0.0)[1..]),
    );
    for n in [&one, &other, &split] {
        assert_eq!(extremes(n), Some(((-0.0f64).to_bits(), 0.0f64.to_bits())));
    }
    assert_eq!(bytes(&one), bytes(&split));
}
