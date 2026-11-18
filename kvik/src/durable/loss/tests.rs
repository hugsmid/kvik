use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use kvik_log::content::{BatchContent, Item};
use kvik_log::facts::{FactFlags, FactRow, Quality, Value as LogValue};
use kvik_log::reader::{ReadBatch, ReadEvent};
use kvik_log::scan::{InvalidRegion, LossCause as ScanCause};
use kvik_log::writer::Position as LogPos;

use super::*;
use crate::config::StoreConfig;
use crate::durable::reader::DurableReader;
use crate::durable::start::log_reader;
use crate::durable::{Emitted, Writer};
use crate::env::{Env, ManualClock, MemFs};
use crate::health::LossKind;
use crate::series::{Kind, SeriesSpec};
use crate::time::Timestamp;
use crate::types::{Intent, Outcome, Position, Reading, Value};

const MARK: LogPos = LogPos {
    generation: 0,
    sequence: 100,
};

fn at(sequence: u64) -> LogPos {
    LogPos {
        generation: 0,
        sequence,
    }
}

fn batch(first: u64, n: u64) -> ReadEvent {
    let items = (0..n)
        .map(|i| {
            Item::Fact(FactRow {
                series_id: 1,
                t: (first + i) as i64,
                quality: Quality::Good,
                flags: FactFlags::default(),
                value: LogValue::F64(1.0f64.to_bits()),
                changed_at: None,
            })
        })
        .collect();
    ReadEvent::Batch(ReadBatch {
        file_id: 1,
        offset: 0,
        first: at(first),
        last: at(first + n - 1),
        commit_time: first as i64,
        content: BatchContent {
            first_sequence: first,
            received_at: first as i64,
            items,
        },
    })
}

fn region(start: u64, before: Option<u64>, after: Option<u64>) -> ReadEvent {
    ReadEvent::Corruption {
        file_id: 1,
        region: InvalidRegion {
            start,
            end: start + 64,
            last_sequence_before: before,
            first_sequence_after: after,
            cause: ScanCause::Corruption,
        },
    }
}

#[test]
fn an_open_ended_region_is_closed_at_the_next_batch_of_the_part_and_its_report_kept_wide() {
    let mut f = Finder::default();
    let found = f.apply(
        &[batch(1, 2), region(100, Some(2), None), batch(5, 1)],
        MARK,
    );
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].closed, (at(3), at(4)));
    assert_eq!(found[0].report.last, Some(Position::new(0, MARK.sequence)));
    assert_eq!((found[0].file_id, found[0].span), (1, (100, 164)));
}

#[test]
fn an_open_ended_region_at_the_tail_of_what_was_read_is_closed_at_the_mark() {
    let mut f = Finder::default();
    let found = f.apply(&[batch(1, 2), region(100, Some(2), None)], MARK);
    assert_eq!(found[0].closed, (at(3), MARK));
    let found = f.apply(&[region(100, Some(2), Some(7)), batch(7, 1)], MARK);
    assert_eq!(
        found.len(),
        0,
        "the same region, no longer open, adds nothing"
    );
}

#[test]
fn a_region_after_a_void_past_the_commits_kept_starts_after_the_void() {
    let mut f = Finder::default();
    let mut events: Vec<ReadEvent> = (0..100).map(|i| batch(1 + 2 * i, 2)).collect();
    events.push(ReadEvent::Void {
        above: at(20),
        cause: kvik_log::reader::VoidCause::Rewritten,
    });
    events.push(region(100, None, Some(30)));
    events.push(batch(30, 1));
    let found = f.apply(&events, MARK);
    assert_eq!(found[0].closed, (at(21), at(29)));
    assert_eq!(found[0].report.commit_time_before, None);
}

#[test]
fn a_region_after_a_void_takes_the_commit_time_of_the_batch_before_it() {
    let mut f = Finder::default();
    let found = f.apply(
        &[
            batch(1, 2),
            batch(3, 2),
            ReadEvent::Void {
                above: at(2),
                cause: kvik_log::reader::VoidCause::Rewritten,
            },
            region(100, None, Some(5)),
            batch(5, 1),
        ],
        MARK,
    );
    assert_eq!(found[0].closed, (at(3), at(4)));
    assert_eq!(found[0].report.commit_time_before, Some(Timestamp(1)));
}

#[test]
fn a_recovery_entry_naming_a_tail_is_a_possible_loss_until_a_void_takes_it() {
    use kvik_log::entries::{Entry, MarkState, Recovery};
    let recovery = |tail_bytes| {
        Item::from(Entry::Recovery(Recovery {
            after: 1,
            stamp: None,
            boot_id: [0; 16],
            tail_bytes,
            readable_commits: 0,
            mark_state: MarkState::Readable,
            id_floor: 0,
            consumers: Vec::new(),
        }))
    };
    let entry = |first: u64, item| {
        ReadEvent::Batch(ReadBatch {
            file_id: 1,
            offset: 0,
            first: at(first),
            last: at(first),
            commit_time: first as i64,
            content: BatchContent {
                first_sequence: first,
                received_at: first as i64,
                items: vec![item],
            },
        })
    };
    let mut f = Finder::default();
    f.apply(
        &[batch(1, 1), entry(2, recovery(0)), entry(3, recovery(40))],
        MARK,
    );
    let losses = f.losses(&[]);
    assert_eq!(losses.len(), 1, "{losses:?}");
    assert_eq!(
        (losses[0].kind, losses[0].identity, losses[0].bytes),
        (LossKind::PossibleLoss, Position::new(0, 3), 40)
    );
    f.apply(
        &[ReadEvent::Void {
            above: at(2),
            cause: kvik_log::reader::VoidCause::Rewritten,
        }],
        MARK,
    );
    assert!(f.losses(&[]).is_empty());
}

fn find(first: u64, last: u64) -> Find {
    let mut f = Finder::default();
    let found = f.apply(
        &[
            batch(1, 1),
            region(first * 10, Some(first - 1), Some(last + 1)),
            batch(last + 1, 1),
        ],
        MARK,
    );
    found.into_iter().next().unwrap()
}

#[test]
fn the_latch_holds_one_report_per_loss_in_order_and_a_bridged_pair_becomes_one() {
    let latch = Latch::default();
    latch.note(&[find(20, 22), find(5, 6), find(40, 41)]);
    let ids = |l: &Latch| -> Vec<(Position, Option<Position>, Option<Position>)> {
        l.found()
            .iter()
            .map(|r| (r.identity, r.first, r.last))
            .collect()
    };
    let p = |s| Position::new(0, s);
    assert_eq!(
        ids(&latch),
        vec![
            (p(5), Some(p(5)), Some(p(6))),
            (p(20), Some(p(20)), Some(p(22))),
            (p(40), Some(p(40)), Some(p(41)))
        ]
    );
    latch.note(&[find(21, 23)]);
    assert_eq!(ids(&latch)[1], (p(20), Some(p(20)), Some(p(23))));
    latch.note(&[find(23, 40)]);
    assert_eq!(
        ids(&latch),
        vec![
            (p(5), Some(p(5)), Some(p(6))),
            (p(20), Some(p(20)), Some(p(41)))
        ]
    );
    assert_eq!(latch.take_fresh().len(), 5);
    assert!(latch.take_fresh().is_empty());
}

/// A closed range is handed out once per reader and only when its region is met for the first time: a region met
/// again, by any reader, holds no fact a reader still took.
#[test]
fn closed_ranges_are_handed_out_once_per_reader_and_an_empty_one_or_a_region_met_again_never() {
    let latch = Latch::default();
    let (mut a, mut b) = (0, 0);
    latch.note(&[find(5, 6)]);
    assert_eq!(latch.closed_since(&mut a), vec![(at(5), at(6))]);
    let mut f = Finder::default();
    let empty = f.apply(
        &[batch(1, 2), region(200, Some(2), None), batch(3, 1)],
        MARK,
    );
    assert!(empty[0].closed.0 > empty[0].closed.1);
    latch.note(&empty);
    latch.note(&[find(9, 9)]);
    assert_eq!(latch.closed_since(&mut a), vec![(at(9), at(9))]);
    latch.note(&[find(9, 9)]);
    latch.note(&[find(5, 6)]);
    assert!(latch.closed_since(&mut a).is_empty());
    assert_eq!(
        latch.closed_since(&mut b),
        vec![(at(5), at(6)), (at(9), at(9))]
    );
    assert!(latch.closed_since(&mut a).is_empty());
    assert!(latch.covers(1, 50, 114) && latch.covers(1, 90, 154));
    assert!(!latch.covers(1, 50, 170));
}

fn wait(e: Emitted) -> crate::types::Ack {
    match e {
        Emitted::Ready(a) => a,
        Emitted::Queued(mut q) => {
            let mut cx = Context::from_waker(Waker::noop());
            loop {
                if let Poll::Ready(r) = q.poll(&mut cx) {
                    return q.finish(r).unwrap();
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

fn damage(fs: &MemFs, dir: &Path, text: &[u8]) {
    let log = dir.join("log");
    let (path, bytes) = fs
        .paths()
        .into_iter()
        .filter(|p| p.starts_with(&log) && p.extension().is_some_and(|e| e == "open"))
        .filter_map(|p| fs.read(&p).map(|b| (p, b)))
        .find(|(_, b)| b.windows(text.len()).any(|x| x == text))
        .unwrap();
    let i = bytes.windows(text.len()).position(|x| x == text).unwrap();
    fs.poke(&path, i, b"X");
}

#[test]
fn the_watchs_reports_taken_in_parts_equal_a_full_reads_and_it_keeps_no_segment_bytes() {
    let fs = MemFs::new();
    let clock = ManualClock::new("2026-01-05T12:00:00Z".parse().unwrap());
    let mut env = Env::test(clock.clone());
    env.fs = Arc::new(fs.clone());
    let dir = PathBuf::from("durable-unit/watch");
    let config = StoreConfig::new(&dir, "Etc/UTC")
        .create_if_missing(true)
        .env(env.clone());
    let emit = |w: &Arc<Writer>, id, t: i64, text: &str| {
        let r = Reading::new(id, Timestamp(t), Value::Text(text.into()));
        wait(w.emit(&[r], Intent::Normal, true).unwrap())
    };
    let w = Writer::open(&config).unwrap();
    let id = w
        .define(&SeriesSpec::new("a/b/note", Kind::Text, ""))
        .unwrap();
    for (i, t) in ["W-ONE", "W-TWO", "W-THREE", "W-FOUR"].iter().enumerate() {
        emit(&w, id, i as i64, t);
    }
    w.shutdown().unwrap();
    drop(w);
    damage(&fs, &dir, b"W-TWO");
    damage(&fs, &dir, b"W-FOUR");
    let w = Writer::open(&config).unwrap();
    for (i, t) in ["W-FIVE", "W-SIX", "W-SEVEN"].iter().enumerate() {
        emit(&w, id, 10 + i as i64, t);
    }
    damage(&fs, &dir, b"W-SIX");

    let mut watch = Watch::with_part(log_reader(&dir, &env).unwrap(), Finder::default(), 1);
    watch.catch_up(&Latch::default()).unwrap();
    assert_eq!(watch.kept_bytes(), 0);
    let full = DurableReader::open(&dir, &env, Duration::from_secs(60), Duration::from_secs(60))
        .unwrap()
        .losses()
        .unwrap();
    assert_eq!(watch.reports(), full);
    assert!(
        full.iter()
            .any(|l| l.kind == LossKind::DataLost && l.recorded)
    );
    assert!(
        full.iter()
            .any(|l| l.kind == LossKind::DataLost && !l.recorded)
    );
    assert_eq!(w.losses().len(), full.len());
    w.shutdown().unwrap();
}

#[test]
fn a_region_inside_a_report_held_too_wide_still_reaches_the_latch_with_its_closed_range() {
    let mut f = Finder::default();
    let latch = Latch::default();
    let found = f.apply(
        &[
            batch(1, 2),
            region(100, Some(2), None),
            batch(5, 1),
            batch(6, 2),
        ],
        at(7),
    );
    latch.note(&found);
    assert_eq!(found[0].closed, (at(3), at(4)));
    assert_eq!(found[0].report.last, Some(Position::new(0, 7)));
    let tail = ReadEvent::Corruption {
        file_id: 2,
        region: InvalidRegion {
            start: 300,
            end: 364,
            last_sequence_before: Some(5),
            first_sequence_after: None,
            cause: ScanCause::Corruption,
        },
    };
    let void = ReadEvent::Void {
        above: at(5),
        cause: kvik_log::reader::VoidCause::Rewritten,
    };
    let found = f.apply(&[void, tail.clone()], at(7));
    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].report.last,
        Some(Position::new(0, 7)),
        "the held report, unchanged"
    );
    latch.note(&found);
    let mut seen = 1;
    assert_eq!(latch.closed_since(&mut seen), vec![(at(6), at(7))]);
    assert!(latch.covers(2, 300, 364));
    assert_eq!(latch.found().len(), 1);
    let fresh = latch.take_fresh();
    assert_eq!(
        fresh.iter().map(|f| f.grew).collect::<Vec<_>>(),
        vec![true, true]
    );
    assert!(f.apply(&[tail], at(7)).is_empty(), "met again, nothing new");
}

/// A plan never outlives a forgetting (Q-921, Q-943). A call that waits for room plans again, so a loss
/// latched during the wait is taken in: the replay of the lost fact, a duplicate when first planned, is recorded.
#[test]
fn a_loss_latched_while_a_call_waits_for_room_is_taken_in_when_it_plans_again() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    use crate::durable::pause::{Gate, Point};

    let fs = MemFs::new();
    let clock = ManualClock::new("2026-01-05T12:00:00Z".parse().unwrap());
    let mut env = Env::test(clock.clone());
    env.fs = Arc::new(fs.clone());
    let dir = PathBuf::from("durable-unit/plan-again");
    let mut config = StoreConfig::new(&dir, "Etc/UTC")
        .create_if_missing(true)
        .env(env);
    config.limits.ingest_queue = 1_024;
    let w = Writer::open(&config).unwrap();
    let id = w
        .define(&SeriesSpec::new("a/b/note", Kind::Text, ""))
        .unwrap();
    let other = w
        .define(&SeriesSpec::new("a/b/other", Kind::Text, ""))
        .unwrap();
    let note = move |t: i64, text: &str| Reading::new(id, Timestamp(t), Value::Text(text.into()));
    wait(w.emit(&[note(0, "I2-LOST")], Intent::Normal, true).unwrap());
    assert!(w.losses().is_empty());
    // The next batch sync is held, so the queue fills behind it.
    let (entered_tx, entered) = mpsc::channel::<()>();
    let (release, release_rx) = mpsc::channel::<()>();
    let release_rx = std::sync::Mutex::new(release_rx);
    let armed = Arc::new(AtomicBool::new(true));
    let hook_armed = armed.clone();
    fs.set_sync_hook(Some(Arc::new(move |p: &Path| {
        if p.extension().is_some_and(|e| e == "open") && hook_armed.swap(false, Ordering::SeqCst) {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        }
    })));
    let held = {
        let w = w.clone();
        std::thread::spawn(move || {
            wait(w.emit(&[note(1, "I2-HELD")], Intent::Normal, true).unwrap())
        })
    };
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    // Another series, so that the lost fact stays in its window and the call first plans it as a duplicate.
    let fill: Vec<Reading> = (0..1_024)
        .map(|k| Reading::new(other, Timestamp(100 + k), Value::Text("I2-FILL".into())))
        .collect();
    let filled = match w.emit(&fill, Intent::Normal, false) {
        Ok(Emitted::Queued(q)) => q,
        other => panic!("the queue takes the call: {:?}", other.map(|_| ()).err()),
    };
    let gate = Gate::new(&dir, Point::WaitingForRoom);
    let replay = {
        let w = w.clone();
        std::thread::spawn(move || {
            wait(
                w.emit(
                    &[note(0, "I2-LOST"), note(5_000, "I2-NEW")],
                    Intent::Normal,
                    true,
                )
                .unwrap(),
            )
        })
    };
    gate.reached();
    damage(&fs, &dir, b"I2-LOST");
    assert_eq!(w.losses().len(), 1);
    release.send(()).unwrap();
    gate.release();
    let ack = replay.join().unwrap();
    assert!(
        matches!(
            ack.outcomes[..],
            [Outcome::Recorded { .. }, Outcome::Recorded { .. }]
        ),
        "{:?}",
        ack.outcomes
    );
    held.join().unwrap();
    wait(Emitted::Queued(filled));
    fs.set_sync_hook(None);
}

/// Q-938 with published views: the round that publishes the next view forgets the ranges the latch took from the
/// index the view is cloned from, while a view a call still holds keeps what it was published with.
#[test]
fn a_latched_range_leaves_the_next_view_and_a_view_held_since_keeps_it() {
    let fs = MemFs::new();
    let clock = ManualClock::new("2026-01-05T12:00:00Z".parse().unwrap());
    let mut env = Env::test(clock.clone());
    env.fs = Arc::new(fs.clone());
    let dir = PathBuf::from("durable-unit/views");
    let config = StoreConfig::new(&dir, "Etc/UTC")
        .create_if_missing(true)
        .env(env);
    let w = Writer::open(&config).unwrap();
    let id = w
        .define(&SeriesSpec::new("a/b/note", Kind::Text, ""))
        .unwrap();
    for (i, t) in ["V-ONE", "V-TWO", "V-THREE"].iter().enumerate() {
        let r = Reading::new(id, Timestamp(i as i64), Value::Text((*t).into()));
        wait(w.emit(&[r], Intent::Normal, true).unwrap());
    }
    let r = w.reader().unwrap();
    let all = crate::time::TimeRange::new(Timestamp(i64::MIN), Timestamp(i64::MAX));
    let count = move |r: &DurableReader| {
        r.with(|i, _| i.facts(id, all, crate::read::View::History))
            .unwrap()
            .len()
    };
    let (entered, entered_rx) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel::<()>();
    let held = {
        let r = r.clone();
        std::thread::spawn(move || {
            r.with(|i, _| {
                entered.send(()).unwrap();
                release_rx.recv().unwrap();
                i.facts(id, all, crate::read::View::History)
            })
            .unwrap()
            .len()
        })
    };
    entered_rx.recv().unwrap();
    damage(&fs, &dir, b"V-TWO");
    assert_eq!(data_lost_count(&w.losses()), 1, "the loss watch finds it");
    assert_eq!(count(&r), 2, "the next view no longer shows it");
    release.send(()).unwrap();
    assert_eq!(held.join().unwrap(), 3, "the held view as published");
    w.shutdown().unwrap();
}

fn data_lost_count(losses: &[crate::health::LossReport]) -> usize {
    losses
        .iter()
        .filter(|l| l.kind == LossKind::DataLost)
        .count()
}
