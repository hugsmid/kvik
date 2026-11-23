//! Calls of `next` over time: idle close, waiting without spinning, the shared tail of a session not called, a view
//! whose lease is gone, and a refresh of live staleness after the batch that carried its fact.

use std::time::{Duration, Instant};

use super::tests::{TICK, World, apply, emit, gauge, watched};
use crate::config::ReaderConfig;
use crate::durable::tests::block_on;
use crate::read::{Live, LiveOptions, SessionOptions};
use crate::{Query, Reader, Selection};

#[test]
fn a_session_polled_through_a_spell_longer_than_the_cut_age_is_not_closed_in_either_process() {
    let w = World::new("quiet-spell");
    let s = w.open();
    let id = block_on(s.define_series(gauge("a/b/power"))).unwrap();
    let readers = [("far", w.reader()), ("own", s.reader())];
    let mut sessions: Vec<_> = readers
        .iter()
        .map(|(_, r)| watched(r, LiveOptions::default()))
        .collect();
    for _ in 0..40 {
        w.clock.advance(Duration::from_secs(60));
        for (session, _, _, held) in &mut sessions {
            if let Some(b) = session.next(Duration::ZERO).unwrap() {
                apply(held, &b);
            }
        }
    }
    emit(&w, &s, id, 2.0);
    w.clock.advance(TICK);
    for ((what, _), (session, _, t, held)) in readers.iter().zip(&mut sessions) {
        apply(held, &session.next(Duration::ZERO).unwrap().unwrap());
        assert!(!held[t].closed, "{what}");
        assert_eq!(held[t].tail.len(), 1, "{what}");
    }
    block_on(s.shutdown()).unwrap();
}

#[test]
fn next_sleeps_out_the_least_batch_interval_and_a_session_with_no_live_read() {
    let w = World::new("no-spin");
    let s = w.open();
    let id = block_on(s.define_series(gauge("a/b/power"))).unwrap();
    let r = s.reader();
    let (mut session, l, t, mut held) = watched(&r, LiveOptions::default());
    w.clock.advance(TICK);
    emit(&w, &s, id, 2.0);
    apply(&mut held, &session.next(Duration::ZERO).unwrap().unwrap());
    // A reading inside the interval: the clock stands still, so the interval never passes while `next` waits.
    emit(&w, &s, id, 3.0);
    let before = session.turns();
    assert!(session.next(Duration::from_millis(150)).unwrap().is_none());
    assert!(session.turns() - before < 5, "{}", session.turns() - before);
    session.cancel(l);
    session.cancel(t);
    emit(&w, &s, id, 4.0);
    let before = session.turns();
    assert!(session.next(Duration::from_millis(150)).unwrap().is_none());
    assert!(session.turns() - before < 5, "{}", session.turns() - before);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_waiting_next_in_the_writer_s_process_returns_soon_after_a_reading() {
    let w = World::new("wake");
    let s = w.open();
    let id = block_on(s.define_series(gauge("a/b/power"))).unwrap();
    let r = s.reader();
    let (mut session, _, t, mut held) = watched(&r, LiveOptions::default());
    w.clock.advance(TICK);
    let started = Instant::now();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(40));
            emit(&w, &s, id, 2.0);
        });
        let b = session.next(Duration::from_secs(20)).unwrap().unwrap();
        apply(&mut held, &b);
    });
    assert_eq!(held[&t].tail.len(), 1);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    block_on(s.shutdown()).unwrap();
}

#[test]
fn refreshes_that_read_nothing_keep_one_round_for_a_session_not_called() {
    let w = World::new("empty-rounds");
    let s = w.open();
    let id = block_on(s.define_series(gauge("a/b/power"))).unwrap();
    let r = w.reader();
    let (mut session, _, t, mut held) = watched(&r, LiveOptions::default());
    let d = r.durable().unwrap().unwrap();
    for _ in 0..10_000 {
        r.refresh().unwrap();
    }
    // An entry the writer appends on its own may stand between two runs.
    assert!(d.hub().held().2 <= 3, "{:?}", d.hub().held());
    emit(&w, &s, id, 2.0);
    r.refresh().unwrap();
    for _ in 0..100 {
        r.refresh().unwrap();
    }
    assert!(d.hub().held().2 <= 5, "{:?}", d.hub().held());
    w.clock.advance(TICK);
    apply(&mut held, &session.next(Duration::ZERO).unwrap().unwrap());
    assert_eq!(held[&t].tail.len(), 1);
    assert!(held[&t].lagged.is_empty());
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_view_another_call_published_whose_lease_is_gone_is_never_a_batch_s_cut() {
    let w = World::new("lease");
    let s = w.open();
    let id = block_on(s.define_series(gauge("a/b/power"))).unwrap();
    let r = Reader::open(
        ReaderConfig::new(&w.dir)
            .env(w.env.clone())
            .lease_refresh(Duration::from_secs(2)),
    )
    .unwrap();
    let opts = SessionOptions::new(Duration::from_millis(200), Duration::from_secs(10));
    let mut session = r.session_with(opts).unwrap();
    let t = session
        .open(
            Query::Tail {
                sel: Selection::all(),
            },
            LiveOptions::default(),
        )
        .unwrap();
    let mut held = std::collections::BTreeMap::new();
    apply(&mut held, &session.next(Duration::ZERO).unwrap().unwrap());
    emit(&w, &s, id, 2.0);
    r.refresh().unwrap();
    for e in crate::env::Fs::read_dir(&*w.env.fs, &w.dir.join("leases")).unwrap() {
        if e.name.ends_with(".lease") {
            crate::env::Fs::remove_file(&*w.env.fs, &w.dir.join("leases").join(&e.name)).unwrap();
        }
    }
    // Older than half the lease refresh, inside the poll interval: the session would take that view.
    w.clock.advance(Duration::from_millis(1_500));
    let got = session.next(Duration::ZERO);
    assert!(matches!(got, Ok(None)), "{got:?}");
    w.clock.advance(Duration::from_secs(10));
    apply(&mut held, &session.next(Duration::ZERO).unwrap().unwrap());
    assert_eq!(held[&t].tail.len(), 1);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn an_acknowledgement_after_the_batch_that_carried_its_fact_makes_latest_fresh_again() {
    let w = World::new("ack-later");
    let s = w.open();
    let g =
        block_on(s.define_series(gauge("a/b/power").staleness(Duration::from_secs(30)))).unwrap();
    emit(&w, &s, g, 1.0);
    let r = s.reader();
    let (mut session, l, _, mut held) = watched(&r, LiveOptions::default());
    w.clock.advance(Duration::from_secs(40));
    apply(&mut held, &session.next(Duration::ZERO).unwrap().unwrap());
    let stale = |h: &super::tests::Held| matches!(&h.latest.as_ref().unwrap()[0].1, Live::Value(v) if v.stale);
    assert!(stale(&held[&l]));
    // The acknowledgement of a call whose fact a batch already carried refreshes the staleness after it.
    r.durable().unwrap().unwrap().clocks().refresh(&[g.0]);
    w.clock.advance(TICK);
    apply(&mut held, &session.next(Duration::ZERO).unwrap().unwrap());
    assert!(!stale(&held[&l]));
    // Measured since the acknowledgement in this process alone: its own reader is the static read to match.
    let snap = r.snapshot().unwrap();
    assert_eq!(
        held[&l].latest.as_ref().unwrap()[0].1,
        snap.live(g).unwrap()
    );
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_ring_inside_the_batch_interval_is_taken_once_the_interval_passes() {
    let w = World::new("ring-in-interval");
    let s = w.open();
    let id = block_on(s.define_series(gauge("a/b/power"))).unwrap();
    let r = s.reader();
    // A `Tail` alone: no refresh of live staleness rings it a second time.
    let mut session = r.session();
    let t = session
        .open(super::tests::tail_all(), LiveOptions::default())
        .unwrap();
    let mut held = std::collections::BTreeMap::new();
    apply(&mut held, &session.next(Duration::ZERO).unwrap().unwrap());
    let rung = |s: &mut crate::LiveSession| s.durable_mut().and_then(|d| d.rung());
    let before = rung(&mut session);
    emit(&w, &s, id, 2.0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while rung(&mut session) == before {
        assert!(
            Instant::now() < deadline,
            "the fan-out thread never rang the session"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(rung(&mut session), before.map(|b| b + 1));
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            w.clock.advance(TICK);
        });
        let started = Instant::now();
        let b = session.next(Duration::from_secs(3)).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the ring was lost inside the interval"
        );
        apply(&mut held, &b.unwrap());
    });
    assert_eq!(held[&t].tail.len(), 1);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_refresh_rings_a_session_with_a_latest_of_the_series_and_not_a_tail_alone() {
    use crate::durable::read::fanout;
    let w = World::new("refresh-ring");
    let s = w.open();
    let g =
        block_on(s.define_series(gauge("a/b/power").staleness(Duration::from_secs(30)))).unwrap();
    emit(&w, &s, g, 1.0);
    let r = s.reader();
    let d = r.durable().unwrap().unwrap();
    // The test runs the fan-out thread's pass itself.
    d.fanout.store(true, std::sync::atomic::Ordering::SeqCst);
    d.clocks().on_refresh(std::task::Waker::noop().clone());
    let (mut latest, ..) = watched(&r, LiveOptions::default());
    let mut tail = r.session();
    tail.open(super::tests::tail_all(), LiveOptions::default())
        .unwrap();
    w.clock.advance(TICK);
    tail.next(Duration::ZERO).unwrap();
    for s in [&mut latest, &mut tail] {
        w.clock.advance(TICK);
        while s.next(Duration::ZERO).unwrap().is_some() {
            w.clock.advance(TICK);
        }
    }
    let rung = |s: &mut crate::LiveSession| s.durable_mut().and_then(|d| d.rung()).unwrap();
    let (l0, t0) = (rung(&mut latest), rung(&mut tail));
    d.clocks().refresh(&[g.0]);
    fanout::pass(&d);
    assert!(rung(&mut latest) > l0);
    assert_eq!(rung(&mut tail), t0);
    block_on(s.shutdown()).unwrap();
}
