//! The checks of `docs/format-tree.md` T.6 and the nodes `encode` refuses (T.1).

use kvik_tree::{
    Body, DecodeError, Kind, NotStorable, Params, Value, decode, encode, exact, fold,
    with_stretches,
};

const SECOND: i64 = 1_000_000_000;

fn gauge() -> (Params, Vec<(i64, Value)>) {
    let p = Params::new(Kind::Gauge, Some(10 * SECOND));
    let r = vec![(SECOND, Value::F64(2.5)), (5 * SECOND, Value::F64(4.0))];
    (p, r)
}

fn invalid(b: &[u8]) -> bool {
    matches!(decode(b), Err(DecodeError::Invalid { .. }))
}

#[test]
fn trailing_and_missing_bytes_are_refused() {
    let (p, r) = gauge();
    let b = encode(&fold(&p, 0, 60 * SECOND, &r)).unwrap();
    let mut long = b.clone();
    long.push(0);
    assert_eq!(decode(&long), Err(DecodeError::Trailing(1)));
    assert!(matches!(
        decode(&b[..b.len() - 1]),
        Err(DecodeError::Truncated(_))
    ));
}

#[test]
fn an_integer_not_in_its_shortest_form_is_refused() {
    let b = encode(&kvik_tree::Node::empty(Kind::Text, 0, 5)).unwrap();
    assert_eq!(b, [4, 0, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0]);
    let padded = [4, 0, 0, 0, 0, 0, 0, 0, 0, 0x85, 0x00, 0, 0];
    assert!(invalid(&padded));
}

#[test]
fn an_even_mantissa_or_a_long_one_is_refused() {
    let p = Params::new(Kind::Gauge, None);
    let n = fold(&p, 0, 10, &[(0, Value::F64(3.0))]);
    let b = encode(&n).unwrap();
    // form, start, width, count, coverage, first dt, first value, then the sum: sign, exponent, length, mantissa.
    let sum = 1 + 8 + 1 + 1 + 1 + 1 + 8;
    assert_eq!(&b[sum..sum + 5], &[1, 0, 0, 1, 3]);
    let mut even = b.clone();
    even[sum + 4] = 6;
    assert!(invalid(&even));
    let mut long = b.clone();
    long[sum + 3] = 2;
    long.insert(sum + 5, 0);
    assert!(invalid(&long));
}

#[test]
fn window_evaluations_are_not_storable() {
    let (p, r) = gauge();
    let held = with_stretches(
        &p,
        fold(&p, 10 * SECOND, 20 * SECOND, &[]),
        r.last(),
        20 * SECOND,
    );
    assert!(held.last.is_some() && held.count == 0);
    assert!(encode(&held).is_err());
    let stretched = with_stretches(
        &p,
        fold(&p, 3 * SECOND, 9 * SECOND, &r[1..]),
        r.first(),
        9 * SECOND,
    );
    assert!(encode(&stretched).is_err());
    let iv = Params::interval(4 * SECOND);
    let mut part = kvik_tree::Node::empty(Kind::Interval, SECOND, 3 * SECOND);
    kvik_tree::credit_intervals(&iv, &mut part, &[(0, Value::F64(1.0))], SECOND, 3 * SECOND);
    assert!(part.apportioned());
    assert!(encode(&part).is_err());
}

#[test]
fn a_sum_that_is_not_dyadic_is_not_storable() {
    let p = Params::new(Kind::Gauge, None);
    let mut n = fold(&p, 0, 10, &[(0, Value::F64(3.0))]);
    if let Body::Gauge { sum, .. } = &mut n.body {
        *sum = exact(1.0) / exact(3.0);
    }
    assert_eq!(
        encode(&n),
        Err(NotStorable("an exact field that is not dyadic"))
    );
}
