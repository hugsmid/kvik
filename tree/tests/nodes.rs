//! Node-level rules the generated properties reach rarely: the identity of merge, the end of a held value, and
//! readings far apart.

mod support;

use kvik_model as km;
use kvik_tree::{
    Body, Kind, Node, Params, Value, credit_intervals, decode, encode, exact, fold, merge,
    with_stretches,
};
use support::*;

#[test]
fn an_apportioned_window_part_with_no_reading_is_not_the_identity() {
    let p = Params::interval(4 * SECOND);
    let mut part = Node::empty(Kind::Interval, SECOND, 3 * SECOND);
    credit_intervals(&p, &mut part, &[(0, Value::F64(2.0))], SECOND, 3 * SECOND);
    assert_eq!((part.count, part.coverage), (0, 2 * i128::from(SECOND)));
    let next = fold(&p, 3 * SECOND, 8 * SECOND, &[(4 * SECOND, Value::F64(1.0))]);
    for n in [
        merge(&p, &part, &next),
        merge(&p, &Node::empty(Kind::Interval, 0, SECOND), &part),
    ] {
        assert_eq!(
            n.coverage,
            part.coverage
                + if n.end == 8 * SECOND {
                    next.coverage
                } else {
                    0
                }
        );
    }
    let Body::Interval { sum, .. } = merge(&p, &part, &next).body else {
        unreachable!()
    };
    assert_eq!(sum, exact(2.0));
}

#[test]
fn a_held_value_ends_where_its_validity_ends() {
    let p = Params::new(Kind::Gauge, Some(10 * SECOND));
    let prior = (0, Value::F64(3.0));
    let mp = model_params(&p);
    let mprior = (0, km::types::Value::Num(3.0));
    for (start, held) in [(10 * SECOND, false), (10 * SECOND - 1, true)] {
        let end = start + SECOND;
        let n = with_stretches(&p, fold(&p, start, end, &[]), Some(&prior), end);
        assert_eq!(n.last.is_some(), held, "window from {start}");
        let m = km::summary::direct(&mp, start, end, &[]);
        assert_eq!(
            model_node(&n),
            km::summary::with_stretches(&mp, m, Some(&mprior), end)
        );
    }
}

#[test]
fn readings_further_apart_than_i64_nanoseconds_neither_wrap_nor_panic() {
    let p = Params::new(Kind::Counter, Some(SECOND));
    let r = [
        (i64::MIN + 1, Value::F64(1.0)),
        (i64::MAX - 1, Value::F64(4.0)),
    ];
    let n = fold(&p, i64::MIN, i64::MAX, &r);
    let Body::Counter {
        elapsed,
        long_steps,
        ..
    } = n.body
    else {
        unreachable!()
    };
    assert_eq!(
        elapsed,
        i128::from(i64::MAX) - 1 - (i128::from(i64::MIN) + 1)
    );
    assert_eq!(long_steps, 1);
    assert_eq!(decode(&encode(&n).unwrap()).unwrap(), n);
    let mp = model_params(&p);
    assert_eq!(
        model_node(&n),
        km::summary::direct(&mp, i64::MIN, i64::MAX, &model_readings(&r))
    );
}
