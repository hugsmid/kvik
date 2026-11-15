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
