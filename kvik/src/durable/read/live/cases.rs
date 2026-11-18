use std::collections::BTreeMap;
use std::time::Duration;

use super::tests::{Held, TICK, World, apply, emit, gauge, kinds};
use crate::config::{LogConfig, ReaderConfig};
use crate::durable::tests::block_on;
use crate::error::QueryError;
use crate::read::{
    Answer, CloseReason, Live, LiveEvent, LiveId, LiveOptions, Query, Selection, SessionOptions,
    UpdateBatch, Updated, View,
};
use crate::series::{Kind, SeriesSpec, Timing};
use crate::time::Timestamp;
use crate::types::{Fact, Position, Reading, SeriesId, Value};
use crate::{LiveSession, Reader, Select, Store};

const S: i64 = 1_000_000_000;

fn note(w: &World, s: &Store, id: SeriesId, secs: i64, text: &str) -> Position {
    let r = Reading::new(
        id,
        Timestamp(w.clock.wall().0 + secs * S),
        Value::Text(text.into()),
    );
    block_on(s.emit(&[r])).unwrap().last().unwrap()
}

fn tail_all() -> Query {
    Query::Tail {
        sel: Selection::all(),
    }
}

/// A session with a `Latest` and a `Tail` of every series, opened and polled once.
fn watched(r: &Reader, tail: LiveOptions) -> (LiveSession, LiveId, LiveId, BTreeMap<LiveId, Held>) {
    let mut s = r.session();
    let l = s
        .open(Query::Latest(Selection::all()), LiveOptions::default())
        .unwrap();
    let t = s.open(tail_all(), tail).unwrap();
    let mut held = BTreeMap::new();
    apply(&mut held, &s.next(Duration::ZERO).unwrap().unwrap());
    (s, l, t, held)
}

fn poll(w: &World, s: &mut LiveSession, held: &mut BTreeMap<LiveId, Held>) -> Option<UpdateBatch> {
    w.clock.advance(TICK);
    let b = s.next(Duration::ZERO).unwrap()?;
    apply(held, &b);
    Some(b)
}

/// The facts a `Tail` opened at `open` must hold at `cut`: the history's, less the spans it was told of.
fn owed(w: &World, open: Position, cut: Position, h: &Held) -> Vec<Fact> {
    let skipped = |p: Position| {
        h.lagged
            .iter()
            .chain(&h.gaps)
            .any(|(a, b)| *a <= p && p <= *b)
    };
    w.history()
        .into_iter()
        .filter(|f| f.position > open && f.position <= cut && !skipped(f.position))
        .collect()
}

fn latest_is_static(r: &Reader, h: &Held) {
    let snap = r.snapshot().unwrap();
    let want: Vec<(SeriesId, Live)> = snap
        .select(&Selection::all())
        .unwrap()
        .into_iter()
        .map(|id| (id, snap.live(id).unwrap()))
        .collect();
    assert_eq!(h.latest.as_ref().unwrap(), &want);
}

#[test]
fn a_reset_voids_what_the_tail_holds_above_it_and_opens_latest_again() {
    let mut w = World::new("reset");
    w.log = LogConfig {
        segment_size: 16 << 20,
        mark_sync: crate::config::MarkSync::Interval(Duration::from_secs(3_600)),
        ..LogConfig::default()
    };
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let r = w.reader();
    let (mut session, l, t, mut held) = watched(&r, LiveOptions::default());
    let open = r.snapshot().unwrap().state().durable;
    for k in 0..40 {
        note(&w, &s, id, k - 40, &format!("RS-{k:02}"));
        if k == 19 {
            poll(&w, &mut session, &mut held).unwrap();
        }
        // The shared tail keeps for the session what other calls read: the next batch brings these facts and the
        // reset together.
        r.refresh().unwrap();
    }
    assert_eq!(held[&t].tail.len(), 20);
    // The power goes with the mark unsynced and the newest batch torn: the start truncates the tail there and
    // assigns its positions again (spec §3 Recovery rule 3).
    w.fs.crash_after(0);
    drop(s);
    w.fs.power_loss();
    w.boot.set(Some([2; 16]));
    w.damage(b"RS-39");
    let s = w.open();
    for k in 0..3 {
        note(&w, &s, id, k, &format!("RS-NEW-{k}"));
    }
    let b = poll(&w, &mut session, &mut held).unwrap();
    let above = held[&t].resets.first().copied().expect("a reset");
    assert_eq!(held[&l].resets, [above]);
    assert!(
        b.events
            .iter()
            .any(|(k, e)| *k == l && matches!(e, LiveEvent::Opened(_)))
    );
    let texts: Vec<String> = held[&t]
        .tail
        .iter()
        .map(|f| match &f.value {
            Value::Text(x) => x.clone(),
            _ => String::new(),
        })
        .collect();
    assert!(!texts.contains(&"RS-39".to_string()), "{texts:?}");
    assert!(texts.ends_with(&["RS-NEW-0".into(), "RS-NEW-1".into(), "RS-NEW-2".into()]));
    assert_eq!(held[&t].tail, owed(&w, open, b.durable, &held[&t]));
    latest_is_static(&r, &held[&l]);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_loss_the_start_records_is_a_gap_and_its_fact_leaves_the_tail() {
    let mut w = World::new("recorded");
    w.log = LogConfig {
        segment_size: 16 << 20,
        mark_sync: crate::config::MarkSync::EveryCommit,
        ..LogConfig::default()
    };
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let r = w.reader();
    let (mut session, l, t, mut held) = watched(&r, LiveOptions::default());
    let open = r.snapshot().unwrap().state().durable;
    let mut lost = Position::ORIGIN;
    for k in 0..10 {
        let p = note(&w, &s, id, k - 10, &format!("RC-{k:02}"));
        if k == 5 {
            lost = p;
        }
    }
    poll(&w, &mut session, &mut held).unwrap();
    assert_eq!(held[&t].tail.len(), 10);
    // The power goes and a batch below the synced mark is damaged: the start records the range lost.
    w.fs.crash_after(0);
    drop(s);
    w.fs.power_loss();
    w.boot.set(Some([2; 16]));
    w.damage(b"RC-05");
    let s = w.open();
    note(&w, &s, id, 1, "RC-NEW");
    let b = poll(&w, &mut session, &mut held).unwrap();
    assert_eq!(held[&t].gaps, [(lost, lost)]);
    assert_eq!(held[&l].gaps, [(lost, lost)]);
    assert_eq!(held[&t].tail.len(), 10);
    assert!(held[&t].tail.iter().all(|f| f.position != lost));
    assert_eq!(held[&t].tail, owed(&w, open, b.durable, &held[&t]));
    latest_is_static(&r, &held[&l]);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn damage_a_tail_meets_is_a_gap_and_its_fact_is_never_shown() {
    let w = World::new("gap");
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let r = w.reader();
    let (mut session, l, t, mut held) = watched(&r, LiveOptions::default());
    let open = r.snapshot().unwrap().state().durable;
    note(&w, &s, id, -3, "GAP-ONE");
    let lost = note(&w, &s, id, -2, "GAP-TWO");
    note(&w, &s, id, -1, "GAP-THREE");
    w.damage(b"GAP-TWO");
    let b = poll(&w, &mut session, &mut held).unwrap();
    assert!(
        held[&t].gaps.iter().any(|(a, z)| *a <= lost && lost <= *z),
        "{b:?}"
    );
    assert!(held[&l].gaps == held[&t].gaps);
    assert_eq!(held[&t].tail.len(), 2);
    assert!(held[&t].tail.iter().all(|f| f.position != lost));
    assert_eq!(held[&t].tail, owed(&w, open, b.durable, &held[&t]));
    latest_is_static(&r, &held[&l]);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_loss_another_read_finds_after_delivery_is_a_gap_that_takes_the_fact_back() {
    let w = World::new("gap-later");
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let r = w.reader();
    let (mut session, l, t, mut held) = watched(&r, LiveOptions::default());
    let (mut later, _, lt, mut later_held) = watched(&r, LiveOptions::default());
    note(&w, &s, id, -3, "LATE-ONE");
    let lost = note(&w, &s, id, -2, "LATE-TWO");
    // The newest batch is checked again at every refresh and would be a reset; the one before it is not.
    note(&w, &s, id, -1, "LATE-THREE");
    poll(&w, &mut session, &mut held).unwrap();
    assert_eq!(held[&t].tail.len(), 3);
    w.damage(b"LATE-TWO");
    // A call that reads facts builds the fact index from the whole log and meets the damage (Q-938).
    let read: Vec<Fact> = r
        .facts(id, Timestamp(i64::MIN)..Timestamp(i64::MAX), View::History)
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(read.len(), 2);
    let b = poll(&w, &mut session, &mut held).unwrap();
    assert!(held[&t].resets.is_empty(), "{b:?}");
    assert!(
        held[&t].gaps.iter().any(|(a, z)| *a <= lost && lost <= *z),
        "{b:?}"
    );
    assert_eq!(held[&t].tail.len(), 2);
    assert!(
        b.events
            .iter()
            .any(|(k, e)| *k == l && matches!(e, LiveEvent::Opened(_)))
    );
    latest_is_static(&r, &held[&l]);
    // A session that takes the fact and the loss in one batch is never sent the fact.
    let b = later.next(Duration::ZERO).unwrap().unwrap();
    apply(&mut later_held, &b);
    assert_eq!(later_held[&lt].tail.len(), 2, "{b:?}");
    assert!(later_held[&lt].tail.iter().all(|f| f.position != lost));
    block_on(s.shutdown()).unwrap();
}

/// Q-951: a reader whose newest batch lies in a segment removed with no record fails every call until reopened;
/// its live session returns the error at every `next`, never silence.
#[test]
fn a_missing_segment_under_the_tail_is_an_error_at_every_next() {
    let mut w = World::new("missing");
    w.log.seal_interval = Duration::from_millis(20);
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let r = w.reader();
    let (mut session, _, _, mut held) = watched(&r, LiveOptions::default());
    let sealed = |text: &[u8]| {
        for _ in 0..500 {
            if w.segments().iter().any(|(p, b)| {
                p.extension().is_some_and(|e| e == "kvl")
                    && b.windows(text.len()).any(|x| x == text)
            }) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("not sealed");
    };
    note(&w, &s, id, 0, "MS-ZERO");
    sealed(b"MS-ZERO");
    note(&w, &s, id, 1, "MS-ONE");
    sealed(b"MS-ONE");
    poll(&w, &mut session, &mut held).unwrap();
    note(&w, &s, id, 2, "MS-TWO");
    sealed(b"MS-TWO");
    let (path, _) = w
        .segments()
        .into_iter()
        .find(|(_, b)| b.windows(6).any(|x| x == b"MS-ONE"))
        .unwrap();
    crate::env::Fs::remove_file(&*w.env.fs, &path).unwrap();
    for _ in 0..3 {
        w.clock.advance(TICK);
        let got = session.next(Duration::ZERO);
        assert!(
            matches!(got, Err(QueryError::NotAvailable { .. })),
            "{got:?}"
        );
    }
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_slow_tail_gets_lagged_to_the_mark_and_the_shared_tail_stays_within_its_backlog() {
    let w = World::new("lagged");
    let s = w.open();
    let id = block_on(s.define_series(gauge("a/b/power"))).unwrap();
    let r = w.reader();
    let small = LiveOptions::default().max_backlog(50, 1 << 30);
    let (mut session, l, t, mut held) = watched(&r, small);
    let d = r.durable().unwrap().unwrap();
    let mut most = 0;
    for k in 0..200 {
        emit(&w, &s, id, f64::from(k));
        // Other calls on the reader keep reading the tail for every session on it.
        r.refresh().unwrap();
        most = most.max(d.hub().held().0);
    }
    assert!(most <= 51, "{most} facts held");
    let b = poll(&w, &mut session, &mut held).unwrap();
    let (from, to) = held[&t].lagged[0];
    assert_eq!(to, b.durable);
    assert!(from.sequence < to.sequence - 150, "{from:?} {to:?}");
    assert!(held[&t].tail.is_empty());
    latest_is_static(&r, &held[&l]);
    emit(&w, &s, id, 1000.0);
    poll(&w, &mut session, &mut held).unwrap();
    assert_eq!(held[&t].tail.len(), 1);
    // Read by its own poll alone, a backlog past the limit is not held at all.
    for k in 0..200 {
        emit(&w, &s, id, f64::from(k));
    }
    poll(&w, &mut session, &mut held).unwrap();
    assert_eq!(held[&t].lagged.len(), 2);
    assert_eq!(held[&t].tail.len(), 1);
    assert!(
        d.hub().peak() <= 51,
        "{} facts held at once",
        d.hub().peak()
    );
    // Caught up, the reader keeps nothing.
    assert_eq!(d.hub().held(), (0, 0, 0));
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_tail_lags_by_its_own_limit_while_another_session_keeps_the_shared_tail() {
    let w = World::new("lagged-own");
    let s = w.open();
    let id = block_on(s.define_series(gauge("a/b/power"))).unwrap();
    let r = w.reader();
    let (mut slow, _, t, mut held) = watched(&r, LiveOptions::default().max_backlog(50, 1 << 30));
    let (mut patient, _, pt, mut patient_held) = watched(&r, LiveOptions::default());
    for k in 0..200 {
        emit(&w, &s, id, f64::from(k));
        r.refresh().unwrap();
    }
    assert!(r.durable().unwrap().unwrap().hub().held().0 >= 200);
    poll(&w, &mut patient, &mut patient_held).unwrap();
    assert_eq!(patient_held[&pt].tail.len(), 200);
    poll(&w, &mut slow, &mut held).unwrap();
    assert_eq!(held[&t].lagged.len(), 1);
    assert!(held[&t].tail.is_empty());
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_row_limit_stops_every_live_read_of_the_session_at_one_cut_inside_the_read() {
    let w = World::new("rows");
    let s = w.open();
    let id = block_on(s.define_series(gauge("a/b/power"))).unwrap();
    let r = w.reader();
    let (mut session, l, t, mut held) = watched(&r, LiveOptions::default().max_rows(10));
    let open = r.snapshot().unwrap().state().durable;
    let readings: Vec<Reading> = (0..35i32)
        .map(|k| {
            Reading::new(
                id,
                Timestamp(w.clock.wall().0 - i64::from(35 - k) * S),
                Value::F64(f64::from(k)),
            )
        })
        .collect();
    block_on(s.emit(&readings)).unwrap();
    let mut sizes = Vec::new();
    while let Some(b) = poll(&w, &mut session, &mut held) {
        let n = held[&t].tail.len();
        sizes.push(n);
        // The `Latest` stands at the same cut: the fact in force then.
        let want = r.at(id, Timestamp(b.now.0), View::AsOf(b.durable)).unwrap();
        let shown = match &held[&l].latest.as_ref().unwrap()[0].1 {
            Live::Value(v) => Some(v.fact.clone()),
            _ => None,
        };
        assert_eq!(shown.map(|f| f.value), want.map(|f| f.value), "{b:?}");
        assert_eq!(held[&t].tail, owed(&w, open, b.durable, &held[&t]));
    }
    assert_eq!(sizes, [10, 20, 30, 35]);
    latest_is_static(&r, &held[&l]);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn staleness_and_a_value_falling_due_update_latest_with_no_new_fact() {
    let w = World::new("due");
    let s = w.open();
    let g =
        block_on(s.define_series(gauge("a/b/power").staleness(Duration::from_secs(30)))).unwrap();
    let p = block_on(s.define_series(gauge("a/b/price").timing(Timing::Scheduled))).unwrap();
    let q = block_on(s.define_series(gauge("a/b/other"))).unwrap();
    emit(&w, &s, g, 1.0);
    emit(&w, &s, p, 5.0);
    emit(&w, &s, q, 7.0);
    let at = Timestamp(w.clock.wall().0 + 10 * S);
    block_on(s.emit(&[Reading::new(p, at, Value::F64(6.0))])).unwrap();
    let readers = [s.reader(), w.reader()];
    let mut sessions: Vec<_> = readers
        .iter()
        .map(|r| (watched(r, LiveOptions::default()), Vec::new()))
        .collect();
    for _ in 0..200 {
        w.clock.advance(TICK);
        for (r, ((session, l, _, held), updates)) in readers.iter().zip(&mut sessions) {
            if let Some(b) = session.next(Duration::ZERO).unwrap() {
                apply(held, &b);
                for (_, e) in &b.events {
                    if let LiveEvent::Updated(Updated::Live { series, .. }) = e {
                        updates.push(*series);
                    }
                }
            }
            latest_is_static(r, &held[l]);
        }
    }
    // One update when the scheduled value falls due, one when the gauge goes stale; none for the third.
    for ((session, ..), updates) in &mut sessions {
        assert_eq!(*updates, [p, g]);
        assert!(session.next(Duration::ZERO).unwrap().is_none());
    }
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_session_not_polled_for_the_cut_age_is_closed_as_idle() {
    let w = World::new("idle");
    let s = w.open();
    let id = block_on(s.define_series(gauge("a/b/power"))).unwrap();
    let r = Reader::open(
        ReaderConfig::new(&w.dir)
            .env(w.env.clone())
            .cut_max_age(Duration::from_secs(60)),
    )
    .unwrap();
    let (mut session, l, t, mut held) = watched(&r, LiveOptions::default());
    w.clock.advance(Duration::from_secs(61));
    emit(&w, &s, id, 1.0);
    r.refresh().unwrap();
    let b = session.next(Duration::ZERO).unwrap().unwrap();
    assert_eq!(kinds(&b), ["closed", "closed"]);
    assert!(
        b.events
            .iter()
            .all(|(_, e)| *e == LiveEvent::Closed(CloseReason::Idle))
    );
    assert!(held.values().all(|h| !h.closed));
    apply(&mut held, &b);
    assert!(held[&l].closed && held[&t].closed);
    assert_eq!(r.durable().unwrap().unwrap().hub().held(), (0, 0, 0));
    // A live read opened afterwards starts afresh.
    let again = session.open(tail_all(), LiveOptions::default()).unwrap();
    let b = session.next(Duration::ZERO).unwrap().unwrap();
    assert_eq!(b.events, [(again, LiveEvent::Opened(Answer::Tail))]);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_reading_is_in_a_batch_within_the_poll_interval_in_a_reader_process_and_at_once_in_the_writers()
{
    let w = World::new("fresh");
    let s = w.open();
    let id = block_on(s.define_series(gauge("a/b/power"))).unwrap();
    let opts = SessionOptions::new(Duration::from_millis(20), Duration::from_millis(100));
    let far = w.reader();
    let mut sessions: Vec<LiveSession> = [s.reader(), far]
        .iter()
        .map(|r| {
            let mut x = r.session_with(opts).unwrap();
            x.open(tail_all(), LiveOptions::default()).unwrap();
            x.next(Duration::ZERO).unwrap().unwrap();
            x
        })
        .collect();
    w.clock.advance(Duration::from_millis(30));
    emit(&w, &s, id, 1.0);
    // The writer's process is woken by the writer's progress: no poll to wait for.
    assert!(sessions[0].next(Duration::ZERO).unwrap().is_some());
    // A reader process reads the mark once a poll interval on the injected clock, so not yet.
    assert!(sessions[1].next(Duration::ZERO).unwrap().is_none());
    w.clock.advance(Duration::from_millis(69));
    assert!(sessions[1].next(Duration::ZERO).unwrap().is_none());
    w.clock.advance(Duration::from_millis(1));
    let b = sessions[1].next(Duration::ZERO).unwrap().unwrap();
    assert_eq!(kinds(&b), ["appended"]);
    // Batches are at least the batch interval apart: a reading just after one waits for it.
    emit(&w, &s, id, 2.0);
    assert!(sessions[0].next(Duration::ZERO).unwrap().is_some());
    emit(&w, &s, id, 3.0);
    assert!(sessions[0].next(Duration::ZERO).unwrap().is_none());
    w.clock.advance(Duration::from_millis(20));
    assert!(sessions[0].next(Duration::ZERO).unwrap().is_some());
    w.clock.advance(Duration::from_millis(500));
    assert!(sessions[1].next(Duration::ZERO).unwrap().is_some());
    // Quiet: no batch, however often it is called.
    for _ in 0..3 {
        w.clock.advance(Duration::from_millis(500));
        for x in &mut sessions {
            assert!(x.next(Duration::ZERO).unwrap().is_none());
        }
    }
    block_on(s.shutdown()).unwrap();
}

#[test]
fn intervals_below_their_least_and_a_pattern_outside_the_grammar_are_refused() {
    let w = World::new("invalid");
    let s = w.open();
    let r = s.reader();
    let low = SessionOptions::new(Duration::from_millis(19), Duration::from_millis(100));
    let poll = SessionOptions::new(Duration::from_millis(20), Duration::from_millis(9));
    let bad = r.session().open(
        Query::Tail {
            sel: Select::Names("a/b".into()).into(),
        },
        LiveOptions::default(),
    );
    assert!(matches!(
        r.session_with(low),
        Err(QueryError::Invalid { .. })
    ));
    assert!(matches!(
        r.session_with(poll),
        Err(QueryError::Invalid { .. })
    ));
    assert!(matches!(bad, Err(QueryError::Invalid { .. })));
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_maximal_batch_reaches_the_tail_whole_in_row_limited_batches() {
    let mut w = World::new("maximal");
    w.log = LogConfig {
        max_batch_bytes: 8 << 20,
        ..LogConfig::default()
    };
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let r = w.reader();
    let (mut session, l, t, mut held) = watched(&r, LiveOptions::default());
    let open = r.snapshot().unwrap().state().durable;
    let n = 26_000i64;
    let readings: Vec<Reading> = (0..n)
        .map(|k| {
            let t = Timestamp(w.clock.wall().0 - (n - k) * 1_000_000);
            Reading::new(id, t, Value::Text(format!("{k:0>255}")))
        })
        .collect();
    block_on(s.emit(&readings)).unwrap();
    let largest = w.segments().iter().map(|(_, b)| b.len()).max().unwrap();
    assert!(largest > 7 << 20, "{largest} bytes");
    let mut batches = 0;
    let mut cut = open;
    while let Some(b) = poll(&w, &mut session, &mut held) {
        batches += 1;
        cut = b.durable;
    }
    assert_eq!(batches, 3);
    assert_eq!(held[&t].tail.len(), 26_000);
    assert_eq!(held[&t].tail, owed(&w, open, cut, &held[&t]));
    latest_is_static(&r, &held[&l]);
    block_on(s.shutdown()).unwrap();
}
