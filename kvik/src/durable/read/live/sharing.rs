//! One decode per set of series a round's sessions take: sessions whose `Tail`s select the same series of a round
//! share its facts, and a row limit met inside such a round still stops the batch there.

use std::collections::BTreeMap;
use std::time::Duration;

use super::tests::{Held, TICK, World, apply, gauge};
use crate::durable::tests::block_on;
use crate::read::{LiveId, LiveOptions, Query};
use crate::types::{Reading, SeriesId, Value};
use crate::{LiveSession, Reader, Select};

fn tail(
    r: &Reader,
    ids: &[SeriesId],
    o: LiveOptions,
) -> (LiveSession, LiveId, BTreeMap<LiveId, Held>) {
    let mut s = r.session();
    let t = s
        .open(
            Query::Tail {
                sel: Select::Ids(ids.to_vec()).into(),
            },
            o,
        )
        .unwrap();
    let mut held = BTreeMap::new();
    apply(&mut held, &s.next(Duration::ZERO).unwrap().unwrap());
    (s, t, held)
}

fn emit(w: &World, s: &crate::Store, rs: &[(SeriesId, f64)]) {
    let at = w.clock.wall();
    let rs: Vec<Reading> = rs
        .iter()
        .enumerate()
        .map(|(k, (id, v))| Reading::new(*id, crate::Timestamp(at.0 - k as i64), Value::F64(*v)))
        .collect();
    block_on(s.emit(&rs)).unwrap();
}

#[test]
fn sessions_taking_the_same_series_of_a_round_share_one_decode_of_its_facts() {
    let w = World::new("shared-batch");
    let s = w.open();
    let a = block_on(s.define_series(gauge("a/b/power"))).unwrap();
    let b = block_on(s.define_series(gauge("c/d/power"))).unwrap();
    let r = w.reader();
    let hub = || r.durable().unwrap().unwrap().hub().work().1;
    let sels: [&[SeriesId]; 5] = [&[a], &[a], &[a], &[a, b], &[b]];
    let mut subs: Vec<_> = sels
        .iter()
        .map(|ids| tail(&r, ids, LiveOptions::default()))
        .collect();
    let mut owed = |want: [usize; 5], decodes: u64| {
        let before = hub();
        w.clock.advance(TICK);
        for (k, (session, t, held)) in subs.iter_mut().enumerate() {
            if let Some(batch) = session.next(Duration::ZERO).unwrap() {
                apply(held, &batch);
            }
            assert_eq!(held[t].tail.len(), want[k], "session {k}");
        }
        assert_eq!(hub() - before, decodes);
    };
    emit(&w, &s, &[(a, 1.0)]);
    owed([1, 1, 1, 1, 0], 1);
    emit(&w, &s, &[(b, 2.0)]);
    owed([1, 1, 1, 2, 1], 1);
    emit(&w, &s, &[(a, 3.0), (b, 4.0)]);
    owed([2, 2, 2, 4, 2], 3);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_row_limit_met_inside_a_shared_round_stops_that_session_alone() {
    let w = World::new("shared-stop");
    let s = w.open();
    let a = block_on(s.define_series(gauge("a/b/power"))).unwrap();
    let r = w.reader();
    let (mut small, st, mut small_held) = tail(&r, &[a], LiveOptions::default().max_rows(1));
    let (mut wide, wt, mut wide_held) = tail(&r, &[a], LiveOptions::default());
    emit(&w, &s, &[(a, 1.0), (a, 2.0)]);
    w.clock.advance(TICK);
    apply(&mut wide_held, &wide.next(Duration::ZERO).unwrap().unwrap());
    assert_eq!(wide_held[&wt].tail.len(), 2);
    let first = small.next(Duration::ZERO).unwrap().unwrap();
    apply(&mut small_held, &first);
    assert_eq!(small_held[&st].tail.len(), 1);
    w.clock.advance(TICK);
    let second = small.next(Duration::ZERO).unwrap().unwrap();
    apply(&mut small_held, &second);
    assert_eq!(small_held[&st].tail, wide_held[&wt].tail);
    assert!(first.durable < second.durable);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn the_rest_of_a_round_a_batch_stopped_inside_is_not_delivered_again() {
    let w = World::new("shared-rest");
    let s = w.open();
    let a = block_on(s.define_series(gauge("a/b/power"))).unwrap();
    let r = w.reader();
    let (mut small, st, mut held) = tail(&r, &[a], LiveOptions::default().max_rows(2));
    emit(&w, &s, &[(a, 1.0)]);
    r.refresh().unwrap();
    emit(&w, &s, &[(a, 2.0), (a, 3.0)]);
    r.refresh().unwrap();
    for _ in 0..3 {
        w.clock.advance(TICK);
        if let Some(b) = small.next(Duration::ZERO).unwrap() {
            apply(&mut held, &b);
        }
    }
    let values: Vec<_> = held[&st].tail.iter().map(|f| f.value.clone()).collect();
    assert_eq!(values, [Value::F64(1.0), Value::F64(2.0), Value::F64(3.0)]);
    block_on(s.shutdown()).unwrap();
}
