//! Losses on the durable backend: found while the store is open or by a start that could not write, reported once
//! in every process, and loss reports that cannot be read (spec §3 Recovery rule 2, §11 Health; Q-200, Q-201).

mod common;

use std::path::Path;

use common::block_on;
use common::durable::{World, damage, data_lost, gauge, history, note, num};
use kvik::{
    Health, Kind, LossKind, Position, Reason, Select, SeriesId, SeriesSpec, Start, Store, Value,
};

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

/// The deliverer as the only finder: nothing polls `losses()` or `health()`, nothing queries, before the replay.
#[test]
fn the_deliverer_alone_finds_damage_to_the_newest_batch_and_its_replay_is_recorded() {
    for consumer in [true, false] {
        let w = World::new(&format!("deliverer-finds-{consumer}"));
        let s = w.open();
        let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
        let c = consumer.then(|| block_on(s.consumer("c", Start::Now, Select::None)).unwrap());
        block_on(s.emit(&[note(id, 0, "DV-ONE")])).unwrap();
        block_on(s.emit(&[note(id, 1, "DV-TWO")])).unwrap();
        damage(&w, b"DV-TWO");
        block_on(s.emit(&[note(id, 2, "DV-THREE")])).unwrap();
        // With no consumer the `kvik-deliver` thread takes the batch, woken by the writer's progress.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let ack = block_on(s.emit(&[note(id, 1, "DV-TWO")])).unwrap();
        assert!(
            matches!(
                ack.outcomes[..],
                [kvik::Outcome::Recorded {
                    unchecked: false,
                    ..
                }]
            ),
            "consumer {consumer}: {:?}",
            ack.outcomes
        );
        assert_eq!(data_lost(&s.losses()).len(), 1);
        drop(c);
        block_on(s.shutdown()).unwrap();
    }
}

/// The kept log of three notes, written by one run; the store opened again and the second note's batch damaged.
fn damaged_kept_log(name: &str) -> (World, Store, SeriesId) {
    let w = World::new(name);
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    for (i, t) in ["KEPT-ONE", "KEPT-TWO", "KEPT-THREE"].iter().enumerate() {
        block_on(s.emit(&[note(id, i as i64, t)])).unwrap();
    }
    block_on(s.shutdown()).unwrap();
    drop(s);
    let s = w.open();
    damage(&w, b"KEPT-TWO");
    (w, s, id)
}

/// Q-928 as proposed (the switch in `Latch::note`): the store's own reader reads the kept log at its first query and
/// finds the damage as any scanning reader does. With the switch off, `losses()` stays empty and the replay is
/// dropped.
#[test]
fn damage_to_the_kept_log_is_found_by_the_stores_first_read_of_it() {
    let (w, s, id) = damaged_kept_log("damaged-kept-query");
    assert_eq!(history(&s.reader(), id).len(), 2);
    let lost = data_lost(&s.losses());
    assert_eq!(lost.len(), 1);
    assert!(!lost[0].3);
    assert_eq!(data_lost(&w.reader().losses()), lost);
    let ack = block_on(s.emit(&[note(id, 1, "KEPT-TWO")])).unwrap();
    assert_eq!(ack.recorded(), 1, "{:?}", ack.outcomes);
    assert_eq!(history(&s.reader(), id).len(), 3);
    block_on(s.shutdown()).unwrap();
    drop(s);
    let s = w.open();
    let lost = data_lost(&s.losses());
    assert_eq!(lost.len(), 1);
    assert!(lost[0].3);
    assert_eq!(history(&s.reader(), id).len(), 3);
    block_on(s.shutdown()).unwrap();
}

/// Q-928: with no query, the store reads the kept log only at its start (Q-200), so `losses()` does not find the
/// damage, a reader process does, and a replay of a fact whose key is in the recent window is dropped until the next
/// start records the loss.
#[test]
fn a_replay_of_a_fact_of_the_damaged_kept_log_is_dropped_while_its_key_is_in_the_window() {
    let (w, s, id) = damaged_kept_log("damaged-kept-window");
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
    let mut at = Vec::new();
    for (i, t) in ["DOWN-ONE", "DOWN-TWO", "DOWN-THREE"].iter().enumerate() {
        w.clock.advance(std::time::Duration::from_secs(10));
        at.push(
            block_on(s.emit(&[note(id, i as i64, t)]))
                .unwrap()
                .first()
                .unwrap(),
        );
    }
    let (two, three) = (at[1], at[2]);
    damage(&w, b"DOWN-THREE");
    let r = w.reader();
    assert_eq!(data_lost(&r.losses()), data_lost(&s.losses()));
    let before_three = s.losses()[0].commit_time_before;
    damage(&w, b"DOWN-TWO");
    let grown = vec![(three, Some(two), Some(three), false)];
    assert_eq!(data_lost(&s.losses()), grown);
    assert_eq!(data_lost(&r.losses()), grown);
    assert_eq!(history(&s.reader(), id).len(), 1);
    assert_eq!(history(&r, id).len(), 1);
    let ack = block_on(s.emit(&[note(id, 1, "DOWN-TWO")])).unwrap();
    assert_eq!(ack.recorded(), 1, "{:?}", ack.outcomes);
    let open_side = s.losses()[0].clone();
    assert_ne!(open_side.commit_time_before, before_three);
    block_on(s.shutdown()).unwrap();
    drop(s);

    // The next start records the grown range under its own first position, not the first-reported identity
    // (Q-203).
    let s = w.open();
    let recorded = vec![(two, Some(two), Some(three), true)];
    assert_eq!(data_lost(&s.losses()), recorded);
    assert_eq!(data_lost(&r.losses()), recorded);
    assert_eq!(
        s.losses()[0].commit_time_before,
        open_side.commit_time_before
    );
    block_on(s.shutdown()).unwrap();
}

#[test]
fn acknowledging_an_identity_that_has_no_loss_covers_no_later_loss_under_it() {
    let w = World::new("ack-none");
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let three = emit_notes(&s, id, &["NONE-ONE", "NONE-TWO", "NONE-THREE"])[2];
    assert!(s.losses().is_empty());
    block_on(s.acknowledge_losses(&[three])).unwrap();

    damage(&w, b"NONE-THREE");
    assert_eq!(
        data_lost(&s.losses()),
        vec![(three, Some(three), Some(three), false)]
    );
    assert!(!s.losses()[0].acknowledged);
    assert!(degraded_by(&s.health(), three), "{:?}", s.health());

    let four = block_on(s.emit(&[note(id, 3, "NONE-FOUR")]))
        .unwrap()
        .first()
        .unwrap();
    damage(&w, b"NONE-FOUR");
    assert_eq!(
        data_lost(&s.losses()),
        vec![(three, Some(three), Some(four), false)]
    );
    assert!(!s.losses()[0].acknowledged);
    assert!(degraded_by(&s.health(), three), "{:?}", s.health());

    block_on(s.acknowledge_losses(&[three])).unwrap();
    assert!(matches!(s.health(), Health::Ok), "{:?}", s.health());
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_fact_lost_after_the_stores_reader_read_it_is_shown_once_with_its_replay() {
    let w = World::new("lost-after-read");
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let two = emit_notes(&s, id, &["SEEN-ONE", "SEEN-TWO", "SEEN-THREE"])[1];
    assert_eq!(history(&s.reader(), id).len(), 3);
    damage(&w, b"SEEN-TWO");
    assert_eq!(
        data_lost(&s.losses()),
        vec![(two, Some(two), Some(two), false)]
    );
    let ack = block_on(s.emit(&[note(id, 1, "SEEN-TWO")])).unwrap();
    assert_eq!(ack.recorded(), 1, "{:?}", ack.outcomes);
    let shown = history(&s.reader(), id);
    assert_eq!(shown.len(), 3, "{shown:?}");
    assert!(shown.iter().all(|f| f.position != two));
    assert_eq!(history(&w.reader(), id), shown);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn two_damaged_neighbours_are_one_loss_as_a_reader_process_reports_it_and_each_replay_is_recorded()
{
    let w = World::new("neighbours");
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let at = emit_notes(&s, id, &["NB-ONE", "NB-TWO", "NB-THREE", "NB-FOUR"]);
    damage(&w, b"NB-TWO");
    damage(&w, b"NB-THREE");
    let one = vec![(at[1], Some(at[1]), Some(at[2]), false)];
    assert_eq!(data_lost(&s.losses()), one);
    assert_eq!(data_lost(&w.reader().losses()), one);
    for (i, t) in [(1, "NB-TWO"), (2, "NB-THREE")] {
        let ack = block_on(s.emit(&[note(id, i, t)])).unwrap();
        assert_eq!(ack.recorded(), 1, "{t}: {:?}", ack.outcomes);
    }
    assert_eq!(data_lost(&s.losses()), one);
    assert_eq!(history(&s.reader(), id).len(), 4);
    let ack = block_on(s.emit(&[note(id, 3, "NB-FOUR")])).unwrap();
    assert_eq!(ack.duplicates_dropped(), 1);
    block_on(s.shutdown()).unwrap();
}

/// Waits until the segment holding `text` is sealed.
fn sealed_with(w: &World, text: &[u8]) {
    for _ in 0..4_000 {
        let held: Vec<_> = w
            .segments()
            .into_iter()
            .filter(|(_, b)| b.windows(text.len()).any(|x| x == text))
            .collect();
        if !held.is_empty()
            && held
                .iter()
                .all(|(p, _)| p.extension().is_some_and(|e| e == "kvl"))
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("the segment holding the text was not sealed");
}

#[test]
fn damage_inside_a_loss_reported_too_wide_reaches_the_stores_reader_and_its_replay_is_recorded() {
    let w = World::new("inside-wide");
    let mut config = w.config();
    config.log.seal_interval = std::time::Duration::from_millis(20);
    let s = block_on(Store::open(config)).unwrap();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let two = emit_notes(&s, id, &["WIDE-ONE", "WIDE-TWO"])[1];
    sealed_with(&w, b"WIDE-TWO");
    for (i, t) in [(2, "WIDE-THREE"), (3, "WIDE-FOUR")] {
        block_on(s.emit(&[note(id, i, t)])).unwrap();
    }
    sealed_with(&w, b"WIDE-FOUR");
    assert_eq!(history(&s.reader(), id).len(), 4);
    // Both are the last batch of a sealed segment: the loss watch meets TWO's region first and reports it to the
    // mark (Q-937), then FOUR's inside that report.
    damage(&w, b"WIDE-TWO");
    damage(&w, b"WIDE-FOUR");
    let lost = data_lost(&s.losses());
    assert_eq!(lost.len(), 1);
    assert!(
        lost[0].2 > Some(two),
        "reported to the mark (Q-937): {lost:?}"
    );
    let ack = block_on(s.emit(&[note(id, 3, "WIDE-FOUR")])).unwrap();
    assert_eq!(ack.recorded(), 1, "{:?}", ack.outcomes);
    let shown: Vec<_> = history(&s.reader(), id)
        .into_iter()
        .map(|f| f.value)
        .collect();
    assert_eq!(shown.len(), 3, "{shown:?}");
    block_on(s.shutdown()).unwrap();
}

/// A loss watch not polled across the removal of the segment it stands in: the log reader passes over a segment no
/// listing holds and voids what it took from it, so the watch reads on from the segments present and still finds
/// damage written after them (Q-927, Q-950). The rule is for a removal a manifest record names, as retention and
/// compaction will write; none removes a segment yet. This removal has no record, and today no reader reports its
/// positions lost (Q-951, for the owner).
#[test]
fn an_unpolled_watch_reads_on_past_the_removal_of_the_segment_it_stands_in() {
    let w = World::new("unpolled-watch");
    let mut config = w.config();
    config.log.seal_interval = std::time::Duration::from_millis(20);
    let s = block_on(Store::open(config)).unwrap();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    block_on(s.emit(&[note(id, 0, "UW-ZERO")])).unwrap();
    sealed_with(&w, b"UW-ZERO");
    block_on(s.emit(&[note(id, 1, "UW-ONE")])).unwrap();
    sealed_with(&w, b"UW-ONE");
    assert!(data_lost(&s.losses()).is_empty());
    block_on(s.emit(&[note(id, 2, "UW-TWO")])).unwrap();
    sealed_with(&w, b"UW-TWO");
    block_on(s.emit(&[note(id, 3, "UW-THREE")])).unwrap();
    let (path, _) = w
        .segments()
        .into_iter()
        .find(|(_, b)| b.windows(6).any(|x| x == b"UW-ONE"))
        .unwrap();
    kvik::env::Fs::remove_file(&*w.fs, &path).unwrap();
    for _ in 0..2 {
        assert!(s.losses().is_empty());
        assert!(matches!(s.health(), Health::Ok), "{:?}", s.health());
    }
    block_on(s.emit(&[note(id, 4, "UW-FOUR")])).unwrap();
    // A seal writes the segment from the writer's own bytes: the damage is made once it is sealed.
    sealed_with(&w, b"UW-FOUR");
    damage(&w, b"UW-FOUR");
    assert_eq!(data_lost(&s.losses()).len(), 1);
    block_on(s.shutdown()).unwrap();
}

/// After the void of a removed segment, a damaged region at the head of the next segment takes its first position
/// from the void, so the loss is reported from the removed segment's first position; with no batch after it in its
/// segment it runs to the mark read, past the intact batch that follows. Too wide, never too narrow (Q-951).
#[test]
fn a_region_after_a_removed_segment_is_reported_from_the_removed_positions() {
    let w = World::new("removed-then-region");
    let mut config = w.config();
    config.log.seal_interval = std::time::Duration::from_millis(20);
    let s = block_on(Store::open(config)).unwrap();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let mut at = Vec::new();
    for (k, t) in ["RR-ZERO", "RR-ONE", "RR-TWO"].iter().enumerate() {
        at.push(
            block_on(s.emit(&[note(id, k as i64, t)]))
                .unwrap()
                .first()
                .unwrap(),
        );
        sealed_with(&w, t.as_bytes());
        if k == 1 {
            assert!(data_lost(&s.losses()).is_empty());
        }
    }
    let three = block_on(s.emit(&[note(id, 3, "RR-THREE")]))
        .unwrap()
        .first()
        .unwrap();
    let (path, _) = w
        .segments()
        .into_iter()
        .find(|(_, b)| b.windows(6).any(|x| x == b"RR-ONE"))
        .unwrap();
    kvik::env::Fs::remove_file(&*w.fs, &path).unwrap();
    damage(&w, b"RR-TWO");
    assert_eq!(
        data_lost(&s.losses()),
        vec![(at[1], Some(at[1]), Some(three), false)]
    );
    let texts: Vec<_> = history(&s.reader(), id)
        .into_iter()
        .map(|f| f.value)
        .collect();
    assert_eq!(
        texts,
        [
            Value::Text("RR-ZERO".into()),
            Value::Text("RR-THREE".into())
        ]
    );
    block_on(s.shutdown()).unwrap();
}
