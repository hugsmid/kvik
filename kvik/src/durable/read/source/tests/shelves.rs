//! Streams whose shelves are emptied: where they resume and that they progress (Q-1067, Q-1096).

use std::sync::Arc;
use std::time::Duration;

use super::{all, fact_bytes, reader_with};
use crate::Intent;
use crate::durable::read::cache::{LEAST_MEMORY, Sizes};
use crate::durable::read::latest::tests::Rig;
use crate::durable::reader::facts::{at, facts};
use crate::read::View;
use crate::time::Timestamp;
use crate::types::{Fact, Reading, Value};

/// One pass at a time keeping `rows` bytes, and a shelf of more than a byte leaves less than the least grant free.
fn one_pass_of(rows: u64) -> Sizes {
    Sizes {
        cap: 400_000,
        block: 300_000 - rows,
        min_rows: rows - 1,
        lookups: 0,
    }
}

#[test]
fn a_stream_whose_rows_another_read_needed_reads_them_again_and_yields_each_once() {
    let mut rig = Rig::new("shelf-emptied");
    let id = rig.ids[0];
    let now = rig.clock.wall().0;
    let rs: Vec<Reading> = (0..300)
        .map(|k| Reading::new(id, Timestamp(now - 1_000 + k), Value::F64(k as f64)))
        .collect();
    rig.emit_readings(&rs, Intent::Normal);
    let oracle = reader_with(&rig.env, &rig.dir, Sizes::of(LEAST_MEMORY));
    let want = oracle
        .with(|i, _| i.facts(id, all(), View::History))
        .unwrap();
    let r = reader_with(&rig.env, &rig.dir, one_pass_of(50_000));
    let mut s = facts(r.clone(), id, all(), View::History).unwrap();
    let mut got: Vec<Fact> = s.by_ref().take(10).collect::<Result<_, _>>().unwrap();
    let t = Timestamp(now - 1_000 + 150);
    assert_eq!(at(&r, id, t, View::History).unwrap().map(|f| f.t), Some(t));
    assert_eq!(r.memory().emptied(), 1);
    got.extend(s.collect::<Result<Vec<_>, _>>().unwrap());
    assert_eq!(got, want);
    rig.shut();
}

/// A shelf holds at most 60 rows here, so the 201st row taken is on the fourth shelf or later; each time holds two
/// facts, and in history the 201st is the first of its time's two.
#[test]
fn a_stream_whose_later_shelf_is_emptied_yields_each_row_once_in_both_views() {
    let mut rig = Rig::new("later-shelf-emptied");
    let id = rig.ids[0];
    let now = rig.clock.wall().0;
    for round in 0..2 {
        let rs: Vec<Reading> = (0..300)
            .map(|k| {
                Reading::new(
                    id,
                    Timestamp(now - 1_000 + k),
                    Value::F64((k + round) as f64),
                )
            })
            .collect();
        rig.emit_readings(&rs, Intent::Normal);
    }
    let oracle = reader_with(&rig.env, &rig.dir, Sizes::of(LEAST_MEMORY));
    for (view, len) in [(View::History, 600), (View::Current, 300)] {
        let want = oracle.with(|i, _| i.facts(id, all(), view)).unwrap();
        assert_eq!(want.len(), len);
        let r = reader_with(&rig.env, &rig.dir, one_pass_of(60 * fact_bytes()));
        let mut s = facts(r.clone(), id, all(), view).unwrap();
        let mut got: Vec<Fact> = s.by_ref().take(201).collect::<Result<_, _>>().unwrap();
        let t = Timestamp(now - 1_000 + 20);
        assert_eq!(at(&r, id, t, view).unwrap().map(|f| f.t), Some(t));
        assert_eq!(r.memory().emptied(), 1);
        got.extend(s.collect::<Result<Vec<_>, _>>().unwrap());
        assert_eq!(got, want, "{view:?}");
    }
    rig.shut();
}

/// Damage found between a stream's two reads of a part makes the second shorter; the stream passes over rows by the
/// last it yielded, so none after it is lost (Q-1096).
#[test]
fn a_stream_whose_part_lost_a_batch_before_it_was_read_again_yields_every_row_after_its_last() {
    let mut rig = Rig::new("shelf-damaged");
    let id = rig.ids[0];
    let now = rig.clock.wall().0;
    for b in 0..30i64 {
        let rs: Vec<Reading> = (0..10)
            .map(|k| {
                let i = b * 10 + k;
                Reading::new(id, Timestamp(now - 1_000 + i), Value::F64(i as f64))
            })
            .collect();
        rig.emit_readings(&rs, Intent::Normal);
    }
    let r = reader_with(&rig.env, &rig.dir, one_pass_of(50_000));
    let mut s = facts(r.clone(), id, all(), View::History).unwrap();
    let got: Vec<Fact> = s.by_ref().take(15).collect::<Result<_, _>>().unwrap();
    assert!(rig.damage_batch(0));
    let t = Timestamp(now - 1_000 + 150);
    assert_eq!(at(&r, id, t, View::History).unwrap().map(|f| f.t), Some(t));
    assert_eq!(r.memory().emptied(), 1);
    let rest = s.collect::<Result<Vec<_>, _>>().unwrap();
    let oracle = reader_with(&rig.env, &rig.dir, Sizes::of(LEAST_MEMORY));
    let want = oracle
        .with(|i, _| i.facts(id, all(), View::History))
        .unwrap();
    assert_eq!(want.len(), 290);
    let last = got.last().unwrap().t;
    assert_eq!(last, Timestamp(now - 1_000 + 14));
    assert_eq!(
        rest,
        want.into_iter().filter(|f| f.t > last).collect::<Vec<_>>()
    );
    rig.shut();
}

/// The first fact at one time, of the highest version, is lost to damage after the stream yielded it and before its
/// part is read again; the lower version left at a later position is the same `(t, series)` and is not yielded again
/// (Q-1096).
#[test]
fn a_versioned_key_whose_yielded_fact_is_lost_before_its_part_is_read_again_is_not_yielded_twice() {
    let mut rig = Rig::new("current-key-damaged");
    let id = rig.ids[3];
    let now = rig.clock.wall().0;
    let version = |t: i64, v: u64| {
        let mut r = Reading::new(id, Timestamp(t), Value::F64(v as f64));
        r.changed_at = Some(v);
        r
    };
    let t0 = now - 1_000;
    rig.emit_readings(&[version(t0, 9)], Intent::Normal);
    rig.emit_readings(&[version(t0, 1)], Intent::Normal);
    let later: Vec<Reading> = (1..100).map(|k| version(t0 + k, 1)).collect();
    rig.emit_readings(&later, Intent::Normal);
    let r = reader_with(&rig.env, &rig.dir, one_pass_of(50_000));
    let mut s = facts(r.clone(), id, all(), View::Current).unwrap();
    let first = s.next().unwrap().unwrap();
    assert_eq!((first.t, first.changed_at), (Timestamp(t0), Some(9)));
    assert!(rig.damage_batch(0));
    let t = Timestamp(t0 + 50);
    assert_eq!(at(&r, id, t, View::Current).unwrap().map(|f| f.t), Some(t));
    assert_eq!(r.memory().emptied(), 1);
    let rest = s.collect::<Result<Vec<_>, _>>().unwrap();
    let oracle = reader_with(&rig.env, &rig.dir, Sizes::of(LEAST_MEMORY));
    let want = oracle
        .with(|i, _| i.facts(id, all(), View::Current))
        .unwrap();
    assert_eq!((want[0].t, want[0].changed_at), (Timestamp(t0), Some(1)));
    assert_eq!(rest, want[1..]);
    rig.shut();
}

/// 300 facts in parts of at most 60 rows, every shelf emptied as soon as it is made, as another thread's admission
/// may: each read of a part still yields its first new row, so the stream ends with every row once (Q-1067).
#[test]
fn a_stream_whose_every_shelf_is_emptied_as_it_is_made_yields_every_row_once_in_both_views() {
    let mut rig = Rig::new("shelf-raced");
    let id = rig.ids[0];
    let now = rig.clock.wall().0;
    let rs: Vec<Reading> = (0..300)
        .map(|k| Reading::new(id, Timestamp(now - 1_000 + k), Value::F64(k as f64)))
        .collect();
    rig.emit_readings(&rs, Intent::Normal);
    let oracle = reader_with(&rig.env, &rig.dir, Sizes::of(LEAST_MEMORY));
    for view in [View::History, View::Current] {
        let want = oracle.with(|i, _| i.facts(id, all(), view)).unwrap();
        let r = reader_with(&rig.env, &rig.dir, one_pass_of(60 * fact_bytes()));
        *r.memory().after_shelve.0.lock().unwrap() = Some(Box::new(|shelf| {
            shelf.lock().unwrap().take();
        }));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let s = facts(r, id, all(), view).unwrap();
            let _ = tx.send(s.collect::<Result<Vec<_>, _>>());
        });
        let got = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("the stream ends")
            .unwrap();
        assert_eq!(got, want, "{view:?}");
    }
    rig.shut();
}

/// Two threads stream 2,000 facts from one reader whose passes cannot run beside a held shelf, so each admission
/// empties the other's shelf; both end with every row once (Q-1067).
#[test]
fn two_threads_streaming_from_one_reader_that_empty_each_others_shelves_both_end_with_every_row() {
    let mut rig = Rig::new("shelves-two-threads");
    let id = rig.ids[0];
    let now = rig.clock.wall().0;
    let rs: Vec<Reading> = (0..2_000)
        .map(|k| Reading::new(id, Timestamp(now - 10_000 + k), Value::F64(k as f64)))
        .collect();
    rig.emit_readings(&rs, Intent::Normal);
    let oracle = reader_with(&rig.env, &rig.dir, Sizes::of(LEAST_MEMORY));
    let want = oracle
        .with(|i, _| i.facts(id, all(), View::History))
        .unwrap();
    let r = reader_with(&rig.env, &rig.dir, one_pass_of(60 * fact_bytes()));
    let (tx, rx) = std::sync::mpsc::channel();
    let gate = Arc::new(std::sync::Barrier::new(2));
    for _ in 0..2 {
        let (r, tx, gate) = (r.clone(), tx.clone(), gate.clone());
        std::thread::spawn(move || {
            let mut s = facts(r, id, all(), View::History).unwrap();
            let mut got: Vec<Fact> = s.by_ref().take(1).collect::<Result<_, _>>().unwrap();
            gate.wait();
            got.extend(s.collect::<Result<Vec<_>, _>>().unwrap());
            let _ = tx.send(got);
        });
    }
    for _ in 0..2 {
        let got = rx
            .recv_timeout(Duration::from_secs(120))
            .expect("both streams end");
        assert_eq!(got, want);
    }
    assert!(r.memory().emptied() > 0);
    rig.shut();
}
