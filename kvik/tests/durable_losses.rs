//! Losses on the durable backend: found while the store is open or by a start that could not write, reported once
//! in every process, and loss reports that cannot be read (spec §3 Recovery rule 2, §11 Health; Q-200, Q-201).

mod common;

use std::path::Path;

use common::block_on;
use common::durable::{World, damage, data_lost, gauge, history, note, num};
use kvik::{Health, Kind, LossKind, Position, Reason, SeriesSpec, Store};

#[test]
fn a_batch_damaged_while_the_store_is_open_is_reported_at_once_and_its_replay_recorded() {
    let w = World::new("damaged-open");
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let mut lost_at = Position::ORIGIN;
    for (i, t) in ["NOTE-ONE", "NOTE-TWO", "NOTE-THREE"].iter().enumerate() {
        let ack = block_on(s.emit(&[note(id, i as i64, t)])).unwrap();
        if i == 1 {
            lost_at = ack.first().unwrap();
        }
    }
    damage(&w, b"NOTE-TWO");
    let found = vec![(lost_at, Some(lost_at), Some(lost_at), false)];

    // A reader process and the store's own process each report the loss when their reads find it.
    let r = w.reader();
    assert_eq!(history(&r, id).len(), 2);
    assert_eq!(data_lost(&r.losses()), found);
    assert!(matches!(r.health(), Health::Degraded { .. }));
    assert_eq!(history(&s.reader(), id).len(), 2);
    assert_eq!(data_lost(&s.losses()), found);
    assert!(
        matches!(&s.health(), Health::Degraded { reasons } if matches!(&reasons[..], [Reason::DataLost(l)] if l.identity == lost_at))
    );

    // The lost reading is no duplicate: its replay is recorded.
    let ack = block_on(s.emit(&[note(id, 1, "NOTE-TWO")])).unwrap();
    assert_eq!(ack.recorded(), 1, "{:?}", ack.outcomes);
    assert_eq!(history(&s.reader(), id).len(), 3);
    // Latched: still reported, until acknowledged.
    assert_eq!(data_lost(&s.losses()), found);
    block_on(s.shutdown()).unwrap();
    drop(s);

    // The next start records the loss under the same identity: one report, in every process.
    let s = w.open();
    let recorded = vec![(lost_at, Some(lost_at), Some(lost_at), true)];
    assert_eq!(data_lost(&s.losses()), recorded);
    assert_eq!(data_lost(&r.losses()), recorded);
    assert_eq!(data_lost(&w.reader().losses()), recorded);
    assert_eq!(history(&s.reader(), id).len(), 3);
    let ack = block_on(s.emit(&[note(id, 1, "NOTE-TWO")])).unwrap();
    assert_eq!(ack.duplicates_dropped(), 1);
    block_on(s.acknowledge_losses(&[lost_at])).unwrap();
    assert!(matches!(s.health(), Health::Ok));
    block_on(s.shutdown()).unwrap();
}

#[test]
fn damage_to_a_batch_the_store_has_read_is_found_by_other_readers_and_recorded_at_the_next_start() {
    let w = World::new("damaged-kept");
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    for (i, t) in ["KEPT-ONE", "KEPT-TWO", "KEPT-THREE"].iter().enumerate() {
        block_on(s.emit(&[note(id, i as i64, t)])).unwrap();
    }
    block_on(s.shutdown()).unwrap();
    drop(s);
    let s = w.open();
    damage(&w, b"KEPT-TWO");
    // The store read the kept log at its start and does not read it again (Q-200): a reader process finds it.
    assert_eq!(history(&s.reader(), id).len(), 3);
    assert!(data_lost(&s.losses()).is_empty());
    assert_eq!(data_lost(&w.reader().losses()).len(), 1);
    assert_eq!(
        block_on(s.emit(&[note(id, 1, "KEPT-TWO")]))
            .unwrap()
            .duplicates_dropped(),
        1
    );
    block_on(s.shutdown()).unwrap();
    drop(s);
    let s = w.open();
    let lost = data_lost(&s.losses());
    assert_eq!(lost.len(), 1);
    assert!(lost[0].3);
    assert_eq!(history(&s.reader(), id).len(), 2);
    assert_eq!(
        block_on(s.emit(&[note(id, 1, "KEPT-TWO")]))
            .unwrap()
            .recorded(),
        1
    );
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_start_left_read_only_reports_the_losses_it_could_not_record() {
    let w = World::new("start-read-only");
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    for (i, t) in ["RO-ONE", "RO-TWO", "RO-THREE"].iter().enumerate() {
        block_on(s.emit(&[note(id, i as i64, t)])).unwrap();
    }
    block_on(s.shutdown()).unwrap();
    drop(s);
    damage(&w, b"RO-TWO");
    // The start's sync of the segment holding the lost range fails.
    w.fs.fail_nth(
        kvik_log::fs::FaultOp::Sync,
        ".open",
        0,
        std::io::ErrorKind::Other,
    );
    let s = w.open();
    let lost = s.losses();
    assert_eq!(lost.len(), 1, "{lost:?}");
    // The start's own report, with its boot identity, not yet recorded.
    assert_eq!(
        (lost[0].kind, lost[0].recorded, lost[0].boot_id),
        (LossKind::DataLost, false, Some([1; 16]))
    );
    assert!(
        matches!(&s.health(), Health::ReadOnly { reasons, .. } if matches!(&reasons[..], [Reason::DataLost(l)] if l.identity == lost[0].identity))
    );
    drop(s);
    let s = w.open();
    let again = s.losses();
    assert_eq!(again.len(), 1);
    assert_eq!(
        (again[0].identity, again[0].recorded),
        (lost[0].identity, true)
    );
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_start_that_could_not_write_a_readable_mark_is_an_open_error() {
    let w = World::new("no-mark");
    w.fs.fail_nth(
        kvik_log::fs::FaultOp::Write,
        "MARK",
        0,
        std::io::ErrorKind::Other,
    );
    let r = block_on(Store::open(w.config()));
    assert!(
        matches!(r, Err(kvik::OpenError::MarkUnreadable)),
        "{:?}",
        r.map(|_| ())
    );
}

#[test]
fn unreadable_loss_reports_degrade_health() {
    let w = World::new("losses-unreadable");
    let s = w.open();
    let id = block_on(s.define_series(gauge("a/b/c"))).unwrap();
    block_on(s.emit(&[num(id, 0, 1.0)])).unwrap();
    let r = w.reader();
    assert!(matches!(r.health(), Health::Ok));
    assert!(matches!(s.health(), Health::Ok));
    let mark = Path::new(&w.dir).join("log").join("MARK");
    let len = w.fs.read(&mark).unwrap().len();
    w.fs.poke(&mark, 0, &vec![0xA5; len]);
    let unknown = |h: Health| matches!(&h, Health::Degraded { reasons } if reasons.iter().any(|x| matches!(x, Reason::LossesUnknown { .. })));
    assert!(unknown(s.health()), "{:?}", s.health());
    assert!(unknown(s.reader().health()));
    assert!(unknown(r.health()));
}

fn degraded_by(h: &Health, identity: Position) -> bool {
    matches!(h, Health::Degraded { reasons } if reasons.iter().any(|r| matches!(r, Reason::DataLost(l) if l.identity == identity)))
}

#[test]
fn damage_next_to_a_found_loss_extends_it_and_needs_a_new_acknowledgement() {
    let w = World::new("extend-found");
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let mut three = Position::ORIGIN;
    for (i, t) in ["EXT-ONE", "EXT-TWO", "EXT-THREE"].iter().enumerate() {
        three = block_on(s.emit(&[note(id, i as i64, t)]))
            .unwrap()
            .first()
            .unwrap();
    }
    damage(&w, b"EXT-THREE");
    let r = w.reader();
    let first = vec![(three, Some(three), Some(three), false)];
    assert_eq!(data_lost(&r.losses()), first);
    assert_eq!(data_lost(&s.losses()), first);
    block_on(s.acknowledge_losses(&[three])).unwrap();
    assert!(matches!(s.health(), Health::Ok));

    let four = block_on(s.emit(&[note(id, 3, "EXT-FOUR")]))
        .unwrap()
        .first()
        .unwrap();
    damage(&w, b"EXT-FOUR");
    // One loss under its first identity, grown to hold FOUR: no longer acknowledged (spec §8 Acknowledging a loss).
    let grown = vec![(three, Some(three), Some(four), false)];
    assert_eq!(history(&s.reader(), id).len(), 2);
    assert_eq!(history(&r, id).len(), 2);
    assert_eq!(data_lost(&s.losses()), grown);
    assert_eq!(data_lost(&r.losses()), grown);
    assert!(degraded_by(&s.health(), three), "{:?}", s.health());
    assert!(degraded_by(&r.health(), three));
    let ack = block_on(s.emit(&[note(id, 3, "EXT-FOUR")])).unwrap();
    assert_eq!(ack.recorded(), 1, "{:?}", ack.outcomes);
    block_on(s.acknowledge_losses(&[three])).unwrap();
    assert!(matches!(s.health(), Health::Ok));
    block_on(s.shutdown()).unwrap();
    drop(s);

    let s = w.open();
    assert_eq!(
        data_lost(&s.losses()),
        vec![(three, Some(three), Some(four), true)]
    );
    block_on(s.shutdown()).unwrap();
}

#[test]
fn damage_to_a_batch_read_after_a_found_loss_extends_it_in_every_process() {
    let w = World::new("void-after-loss");
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let mut at = Vec::new();
    for (i, t) in ["VOID-ONE", "VOID-TWO", "VOID-THREE"].iter().enumerate() {
        at.push(
            block_on(s.emit(&[note(id, i as i64, t)]))
                .unwrap()
                .first()
                .unwrap(),
        );
    }
    let (two, three) = (at[1], at[2]);
    damage(&w, b"VOID-TWO");
    let r = w.reader();
    let first = vec![(two, Some(two), Some(two), false)];
    assert_eq!(data_lost(&r.losses()), first);
    assert_eq!(data_lost(&s.losses()), first);
    assert_eq!(history(&r, id).len(), 2);
    assert_eq!(history(&s.reader(), id).len(), 2);

    damage(&w, b"VOID-THREE");
    let grown = vec![(two, Some(two), Some(three), false)];
    for reader in [&r, &s.reader()] {
        assert_eq!(history(reader, id).len(), 1);
        let losses = reader.losses();
        assert_eq!(data_lost(&losses), grown);
        assert_eq!(losses[0].commit_time_after, None);
        assert!(degraded_by(&reader.health(), two), "{:?}", reader.health());
    }
    assert_eq!(data_lost(&s.losses()), grown);
    for (i, t) in [(1, "VOID-TWO"), (2, "VOID-THREE")] {
        let ack = block_on(s.emit(&[note(id, i, t)])).unwrap();
        assert_eq!(ack.recorded(), 1, "{t}: {:?}", ack.outcomes);
    }
    let ack = block_on(s.emit(&[note(id, 0, "VOID-ONE")])).unwrap();
    assert_eq!(ack.duplicates_dropped(), 1);
    block_on(s.shutdown()).unwrap();
    drop(s);

    let s = w.open();
    assert_eq!(
        data_lost(&s.losses()),
        vec![(two, Some(two), Some(three), true)]
    );
    block_on(s.shutdown()).unwrap();
}

fn emit_notes(s: &Store, id: kvik::SeriesId, texts: &[&str]) -> Vec<Position> {
    texts
        .iter()
        .enumerate()
        .map(|(i, t)| {
            block_on(s.emit(&[note(id, i as i64, t)]))
                .unwrap()
                .first()
                .unwrap()
        })
        .collect()
}

#[test]
fn damage_next_to_a_recorded_loss_grows_it_unrecorded_until_the_next_start() {
    let w = World::new("extend-recorded");
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let three = emit_notes(&s, id, &["REC-ONE", "REC-TWO", "REC-THREE"])[2];
    damage(&w, b"REC-THREE");
    block_on(s.shutdown()).unwrap();
    drop(s);

    let s = w.open();
    assert_eq!(
        data_lost(&s.losses()),
        vec![(three, Some(three), Some(three), true)]
    );
    block_on(s.acknowledge_losses(&[three])).unwrap();
    assert!(matches!(s.health(), Health::Ok));
    let four = block_on(s.emit(&[note(id, 3, "REC-FOUR")]))
        .unwrap()
        .first()
        .unwrap();
    damage(&w, b"REC-FOUR");
    let r = w.reader();
    let grown = vec![(three, Some(three), Some(four), false)];
    assert_eq!(data_lost(&s.losses()), grown);
    assert_eq!(data_lost(&r.losses()), grown);
    assert!(degraded_by(&s.health(), three), "{:?}", s.health());
    let ack = block_on(s.emit(&[note(id, 3, "REC-FOUR")])).unwrap();
    assert_eq!(ack.recorded(), 1, "{:?}", ack.outcomes);
    block_on(s.shutdown()).unwrap();
    drop(s);

    let s = w.open();
    let recorded = vec![(three, Some(three), Some(four), true)];
    assert_eq!(data_lost(&s.losses()), recorded);
    assert_eq!(data_lost(&r.losses()), recorded);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn losses_keep_the_grown_range_the_process_holds_when_the_reports_cannot_be_read() {
    let w = World::new("extend-unreadable");
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let three = emit_notes(&s, id, &["HELD-ONE", "HELD-TWO", "HELD-THREE"])[2];
    damage(&w, b"HELD-THREE");
    assert_eq!(s.losses().len(), 1);
    let four = block_on(s.emit(&[note(id, 3, "HELD-FOUR")]))
        .unwrap()
        .first()
        .unwrap();
    damage(&w, b"HELD-FOUR");
    let grown = vec![(three, Some(three), Some(four), false)];
    assert_eq!(data_lost(&s.losses()), grown);

    let mark = Path::new(&w.dir).join("log").join("MARK");
    let len = w.fs.read(&mark).unwrap().len();
    w.fs.poke(&mark, 0, &vec![0xA5; len]);
    assert_eq!(data_lost(&s.losses()), grown);
    assert!(
        matches!(&s.health(), Health::Degraded { reasons } if reasons.iter().any(|x| matches!(x, Reason::LossesUnknown { .. })) && reasons.iter().any(|x| matches!(x, Reason::DataLost(l) if l.last == Some(four))))
    );
}

#[test]
fn damage_below_a_found_loss_grows_it_downward_under_its_first_identity() {
    let w = World::new("extend-down");
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let at = emit_notes(&s, id, &["DOWN-ONE", "DOWN-TWO", "DOWN-THREE"]);
    let (two, three) = (at[1], at[2]);
    damage(&w, b"DOWN-THREE");
    let r = w.reader();
    assert_eq!(data_lost(&r.losses()), data_lost(&s.losses()));
    damage(&w, b"DOWN-TWO");
    let grown = vec![(three, Some(two), Some(three), false)];
    assert_eq!(data_lost(&s.losses()), grown);
    assert_eq!(data_lost(&r.losses()), grown);
    assert_eq!(history(&s.reader(), id).len(), 1);
    assert_eq!(history(&r, id).len(), 1);
    let ack = block_on(s.emit(&[note(id, 1, "DOWN-TWO")])).unwrap();
    assert_eq!(ack.recorded(), 1, "{:?}", ack.outcomes);
    // The identity the next start records is open (Q-203).
    block_on(s.shutdown()).unwrap();
}
