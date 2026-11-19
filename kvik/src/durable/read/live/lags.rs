//! Sessions the shared tail outran: what a trimmed round still tells them, and where their `Lagged` starts.

use super::tests::{World, emit, gauge, latest_is_static, note, poll, tail_is_owed, watched};
use crate::durable::tests::block_on;
use crate::read::{LiveOptions, View};
use crate::series::{Kind, SeriesSpec};
use crate::time::Timestamp;
use crate::types::Position;

fn after(p: Position) -> Position {
    Position {
        sequence: p.sequence + 1,
        ..p
    }
}

#[test]
fn a_loss_found_after_delivery_in_a_round_since_trimmed_is_still_named_to_a_lagging_tail() {
    let w = World::new("lag-gap");
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let r = w.reader();
    let (mut session, l, t, mut held) = watched(&r, LiveOptions::default().max_backlog(5, 1 << 30));
    let open = r.snapshot().unwrap().state().durable;
    note(&w, &s, id, -9, "LG-ONE");
    let lost = note(&w, &s, id, -8, "LG-TWO");
    note(&w, &s, id, -7, "LG-THREE");
    poll(&w, &mut session, &mut held).unwrap();
    assert_eq!(held[&t].tail.len(), 3);
    w.damage(b"LG-TWO");
    let read = r
        .facts(id, Timestamp(i64::MIN)..Timestamp(i64::MAX), View::History)
        .unwrap();
    assert_eq!(read.count(), 2);
    // The refresh hands the loss to the shared tail; the ones after it pass the backlog and trim its round.
    r.refresh().unwrap();
    for k in 0..8 {
        note(&w, &s, id, -6 + k, &format!("LG-MORE-{k}"));
        r.refresh().unwrap();
    }
    let b = poll(&w, &mut session, &mut held).unwrap();
    assert_eq!(held[&t].lagged.len(), 1, "{b:?}");
    assert!(
        held[&t].gaps.iter().any(|(a, z)| *a <= lost && lost <= *z),
        "{b:?}"
    );
    assert!(held[&t].tail.iter().all(|f| f.position != lost));
    tail_is_owed(&w, &s, open, b.durable, &held[&t]);
    latest_is_static(&w, &held[&l]);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_lagged_latest_judges_a_series_whose_only_new_fact_was_trimmed() {
    let w = World::new("lag-latest");
    let s = w.open();
    let a = block_on(s.define_series(gauge("a/b/power"))).unwrap();
    let b = block_on(s.define_series(gauge("a/b/other"))).unwrap();
    emit(&w, &s, a, 1.0);
    emit(&w, &s, b, 1.0);
    let r = w.reader();
    let (mut session, l, t, mut held) = watched(&r, LiveOptions::default().max_backlog(5, 1 << 30));
    w.clock.advance(std::time::Duration::from_secs(1));
    emit(&w, &s, b, 2.0);
    r.refresh().unwrap();
    for k in 0..10 {
        emit(&w, &s, a, f64::from(k));
        r.refresh().unwrap();
    }
    poll(&w, &mut session, &mut held).unwrap();
    assert_eq!(held[&t].lagged.len(), 1);
    latest_is_static(&w, &held[&l]);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_backlog_past_its_bytes_alone_lags_and_the_shared_tail_keeps_no_more() {
    let w = World::new("lag-bytes");
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let r = w.reader();
    let (mut session, _, t, mut held) =
        watched(&r, LiveOptions::default().max_backlog(1_000_000, 4_096));
    let d = r.durable().unwrap().unwrap();
    for k in 0..40 {
        note(&w, &s, id, k - 40, &format!("{k:02}{}", "B".repeat(200)));
        r.refresh().unwrap();
        assert!(d.hub().held().1 <= 4_096 + 512, "{:?}", d.hub().held());
    }
    poll(&w, &mut session, &mut held).unwrap();
    assert_eq!(held[&t].lagged.len(), 1);
    assert!(held[&t].tail.is_empty());
    block_on(s.shutdown()).unwrap();
}

#[test]
fn lagged_starts_at_the_position_after_the_last_fact_delivered() {
    let w = World::new("lag-from");
    let s = w.open();
    let id = block_on(s.define_series(gauge("a/b/power"))).unwrap();
    let r = w.reader();
    let (mut session, _, t, mut held) = watched(&r, LiveOptions::default().max_backlog(5, 1 << 30));
    let open = r.snapshot().unwrap().state().durable;
    for k in 0..10 {
        emit(&w, &s, id, f64::from(k));
        r.refresh().unwrap();
    }
    let b = poll(&w, &mut session, &mut held).unwrap();
    assert_eq!(held[&t].lagged, [(after(open), b.durable)]);
    emit(&w, &s, id, 100.0);
    poll(&w, &mut session, &mut held).unwrap();
    let last = held[&t].tail.last().unwrap().position;
    for k in 0..10 {
        emit(&w, &s, id, f64::from(k));
        r.refresh().unwrap();
    }
    let b = poll(&w, &mut session, &mut held).unwrap();
    assert_eq!(held[&t].lagged[1], (after(last), b.durable));
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_backlog_of_entries_past_its_bytes_lags_by_what_their_names_hold() {
    let w = World::new("lag-entries");
    let s = w.open();
    let r = w.reader();
    let piece = std::mem::size_of::<crate::durable::read::hub::Piece>();
    let cap = u64::try_from(40 * piece).unwrap();
    let (mut session, l, t, mut held) =
        watched(&r, LiveOptions::default().max_backlog(1_000_000, cap));
    for k in 0..30 {
        let name = format!("{}/{}/m{k:02}", "a".repeat(60), "b".repeat(60));
        block_on(s.define_series(gauge(&name))).unwrap();
        r.refresh().unwrap();
    }
    poll(&w, &mut session, &mut held).unwrap();
    assert_eq!(held[&t].lagged.len(), 1, "{:?}", held[&t]);
    latest_is_static(&w, &held[&l]);
    block_on(s.shutdown()).unwrap();
}
