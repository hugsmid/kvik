//! A batch stopped by a row limit inside what the session took, and the losses around its stop.

use super::tests::{World, latest_is_static, note, poll, tail_is_owed, watched};
use crate::durable::tests::block_on;
use crate::read::{LiveOptions, View};
use crate::series::{Kind, SeriesSpec};
use crate::time::Timestamp;

#[test]
fn a_row_limited_batch_never_carries_a_fact_the_reader_already_reports_lost() {
    let w = World::new("stop-known");
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let r = w.reader();
    let (mut session, l, t, mut held) = watched(&r, LiveOptions::default().max_rows(2));
    let open = r.snapshot().unwrap().state().durable;
    note(&w, &s, id, -4, "PF-ONE");
    let lost = note(&w, &s, id, -3, "PF-TWO");
    note(&w, &s, id, -2, "PF-THREE");
    note(&w, &s, id, -1, "PF-FOUR");
    r.refresh().unwrap();
    w.damage(b"PF-TWO");
    let read = r
        .facts(id, Timestamp(i64::MIN)..Timestamp(i64::MAX), View::History)
        .unwrap();
    assert_eq!(read.count(), 3);
    let mut batches = 0;
    while let Some(b) = poll(&w, &mut session, &mut held) {
        batches += 1;
        assert!(held[&t].tail.iter().all(|f| f.position != lost), "{b:?}");
        tail_is_owed(&w, &s, open, b.durable, &held[&t]);
    }
    assert_eq!(held[&t].tail.len(), 3);
    assert_eq!(batches, 2);
    latest_is_static(&w, &held[&l]);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_gap_after_a_row_limit_stop_is_named_in_that_batch_and_survives_the_round_s_trim() {
    let w = World::new("stop-trimmed");
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let r = w.reader();
    let opts = LiveOptions::default().max_rows(2).max_backlog(5, 1 << 30);
    let (mut session, l, t, mut held) = watched(&r, opts);
    let open = r.snapshot().unwrap().state().durable;
    note(&w, &s, id, -9, "PG-ONE");
    let lost = note(&w, &s, id, -8, "PG-TWO");
    note(&w, &s, id, -7, "PG-THREE");
    note(&w, &s, id, -6, "PG-FOUR");
    w.damage(b"PG-TWO");
    let b = poll(&w, &mut session, &mut held).unwrap();
    assert!(
        held[&t].gaps.iter().any(|(a, z)| *a <= lost && lost <= *z),
        "{b:?}"
    );
    tail_is_owed(&w, &s, open, b.durable, &held[&t]);
    for k in 0..4 {
        note(&w, &s, id, -5 + k, &format!("PG-MORE-{k}"));
    }
    // Another call reads past the backlog: the round the session stopped inside keeps only its gap.
    r.refresh().unwrap();
    while let Some(b) = poll(&w, &mut session, &mut held) {
        tail_is_owed(&w, &s, open, b.durable, &held[&t]);
    }
    assert_eq!(held[&t].gaps.len(), 1, "{:?}", held[&t]);
    assert_eq!(held[&t].lagged.len(), 1, "{:?}", held[&t]);
    latest_is_static(&w, &held[&l]);
    block_on(s.shutdown()).unwrap();
}

/// A tail that could never hold a row is refused at `open`.
#[test]
fn a_tail_with_no_rows_allowed_is_refused() {
    let w = World::new("zero-rows");
    let s = w.open();
    let r = s.reader();
    let got = r.session().open(super::tests::tail_all(), LiveOptions::default().max_rows(0));
    assert!(matches!(got, Err(crate::QueryError::Invalid { .. })), "{got:?}");
    block_on(s.shutdown()).unwrap();
}

/// A timeout no instant can hold means no deadline.
#[test]
fn next_with_the_longest_timeout_returns_what_is_ready() {
    let w = World::new("max-timeout");
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let r = s.reader();
    let (mut session, ..) = watched(&r, LiveOptions::default());
    note(&w, &s, id, 0, "MT-ONE");
    w.clock.advance(super::tests::TICK);
    let got = session.next(std::time::Duration::MAX).unwrap();
    assert!(got.is_some());
    block_on(s.shutdown()).unwrap();
}
