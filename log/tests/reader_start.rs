//! A reader that starts after a position (a series directory's `through`, `format.md` A.18) and one brought to
//! another reader's cut: what each returns against a reader that reads the whole log.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use kvik_log::content::Item;
use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
use kvik_log::fs::{Fs, MemFs};
use kvik_log::reader::{LogReader, ReadBatch, ReadEvent, ReaderConfig, Wanted};
use kvik_log::store::{NewStore, StoreDir};
use kvik_log::writer::{Log, LogWriter, Position, SystemClock, WriterConfig};

const T0: i64 = 1_767_225_600_000_000_000;

fn new_store() -> NewStore {
    NewStore {
        store_id: [4; 16],
        created_at: T0,
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: "kvik test".into(),
    }
}

fn start(fs: &MemFs) -> Log {
    start_on(fs, 1)
}

fn start_on(fs: &MemFs, boot: u8) -> Log {
    let fs: Arc<dyn Fs> = Arc::new(fs.clone());
    let store = StoreDir::open(fs, Path::new("s"), Some(&new_store())).unwrap();
    let cfg = WriterConfig {
        segment_size: 16 << 10,
        max_batch_bytes: 4 << 10,
        disk_critical: 0,
        disk_low_bytes: 0,
        relaxed_limits: true,
        ..WriterConfig::default()
    };
    let w = LogWriter::open(store, cfg, Some([boot; 16]), Duration::ZERO, T0).unwrap();
    Log::start(w, Arc::new(SystemClock::default())).unwrap()
}

fn fact(series: u32, t: i64) -> Item {
    Item::Fact(FactRow {
        series_id: series,
        t: T0 + t,
        quality: Quality::Good,
        flags: FactFlags::default(),
        value: Value::F64(t as u64),
        changed_at: None,
    })
}

/// A log of `calls` calls of 1 to 7 facts over several sealed segments.
fn written(calls: i64) -> MemFs {
    let fs = MemFs::new();
    let log = start(&fs);
    for k in 0..calls {
        let items = (0..1 + k % 7).map(|j| fact(j as u32, k * 10 + j)).collect();
        log.submit(items).unwrap().wait().unwrap();
    }
    log.shutdown().unwrap();
    fs
}

fn reader(fs: &MemFs, id: u8) -> LogReader {
    let clock = Arc::new(SystemClock::default());
    LogReader::open(ReaderConfig::new(
        "s",
        Arc::new(fs.clone()),
        clock,
        Some([1; 16]),
        [id; 16],
    ))
    .unwrap()
}

fn batches(events: &[ReadEvent]) -> Vec<ReadBatch> {
    events
        .iter()
        .filter_map(|e| match e {
            ReadEvent::Batch(b) => Some(b.clone()),
            _ => None,
        })
        .collect()
}

fn segments(fs: &MemFs) -> usize {
    fs.paths()
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "kvl" || e == "open"))
        .count()
}

#[test]
fn a_reader_started_after_a_position_returns_exactly_the_batches_above_it() {
    let fs = written(400);
    assert!(segments(&fs) >= 4, "{} segments", segments(&fs));
    let whole = batches(&reader(&fs, 9).refresh().unwrap().events);
    let mut afters: Vec<Position> = whole.iter().step_by(13).map(|b| b.last).collect();
    afters.push(whole.last().unwrap().last);
    for (k, after) in afters.into_iter().enumerate() {
        let mut r = reader(&fs, 10 + k as u8);
        assert!(r.start_after(after).unwrap(), "{after:?}");
        let u = r.refresh().unwrap();
        assert!(
            u.events.iter().all(|e| matches!(e, ReadEvent::Batch(_))),
            "{:?}",
            u.events
        );
        let want: Vec<ReadBatch> = whole.iter().filter(|b| b.last > after).cloned().collect();
        assert_eq!(batches(&u.events), want, "after {after:?}");
        assert_eq!(u.state.durable, whole.last().unwrap().last);
    }
}

#[test]
fn a_started_reader_reads_only_the_segments_from_the_one_holding_the_next_position() {
    let fs = written(400);
    let whole = batches(&reader(&fs, 9).refresh().unwrap().events);
    let last = whole.last().unwrap();
    // The last batch's own segment is the only one read for anything above the batch before it.
    let before = whole[whole.len() - 2].last;
    let mut r = reader(&fs, 10);
    assert!(r.start_after(before).unwrap());
    let u = r.refresh().unwrap();
    assert_eq!(batches(&u.events), vec![last.clone()]);
    assert!(
        r.kept_bytes() <= 32 << 10,
        "{} bytes kept, at most one segment",
        r.kept_bytes()
    );
}

#[test]
fn a_start_after_is_refused_after_a_refresh_and_for_another_generation_or_a_position_above_the_mark()
 {
    let fs = written(30);
    let mut r = reader(&fs, 9);
    let durable = r.state().durable;
    let above = Position {
        sequence: durable.sequence + 1,
        ..durable
    };
    assert!(!r.start_after(above).unwrap());
    let other = Position {
        generation: durable.generation + 1,
        sequence: 1,
    };
    assert!(!r.start_after(other).unwrap());
    let all = batches(&r.refresh().unwrap().events);
    assert_eq!(all.last().unwrap().last, durable);
    assert!(!r.start_after(all[0].last).unwrap());
    // A reader whose start was refused reads the whole log.
    let mut fresh = reader(&fs, 10);
    assert!(!fresh.start_after(above).unwrap());
    assert_eq!(batches(&fresh.refresh().unwrap().events), all);
}

#[test]
fn a_started_reader_follows_what_is_written_after_it() {
    let fs = written(40);
    let whole = batches(&reader(&fs, 9).refresh().unwrap().events);
    let after = whole[whole.len() / 2].last;
    let mut r = reader(&fs, 10);
    assert!(r.start_after(after).unwrap());
    let mut got = batches(&r.refresh().unwrap().events);
    let log = start(&fs);
    for k in 0..40 {
        log.submit(vec![fact(1, 10_000 + k)])
            .unwrap()
            .wait()
            .unwrap();
        if k % 9 == 0 {
            got.extend(batches(&r.refresh().unwrap().events));
        }
    }
    log.shutdown().unwrap();
    got.extend(batches(&r.refresh().unwrap().events));
    let all = batches(&reader(&fs, 11).refresh().unwrap().events);
    let want: Vec<ReadBatch> = all.into_iter().filter(|b| b.last > after).collect();
    assert_eq!(got, want);
}

#[test]
fn a_refresh_upto_returns_nothing_above_it_and_resumes_there() {
    let fs = written(80);
    let whole = batches(&reader(&fs, 9).refresh().unwrap().events);
    let cut = whole[whole.len() / 3].last;
    let mut r = reader(&fs, 10);
    let mut got = Vec::new();
    loop {
        let (u, more) = r.refresh_upto(5, cut).unwrap();
        got.extend(batches(&u.events));
        if !more {
            break;
        }
    }
    let want: Vec<ReadBatch> = whole.iter().filter(|b| b.last <= cut).cloned().collect();
    assert_eq!(got, want);
    let rest = batches(&r.refresh().unwrap().events);
    let want: Vec<ReadBatch> = whole.iter().filter(|b| b.last > cut).cloned().collect();
    assert_eq!(rest, want);
}

#[test]
fn a_lookup_reports_the_marks_reset_stamp_and_position() {
    let fs = written(20);
    let mut r = reader(&fs, 9);
    let st = r.refresh().unwrap().state;
    let l = r
        .lookup(
            &[Wanted {
                series_id: 0,
                from: i64::MIN,
                to: i64::MAX,
            }],
            &|_, _, _| false,
        )
        .unwrap();
    assert_eq!((l.durable, l.stamp), (st.durable, st.stamp));
    assert_eq!(l.facts.len(), 20);
}

#[test]
fn a_recovery_entry_at_or_below_the_start_voids_nothing_the_start_passes_over() {
    let fs = written(20);
    // A new boot: the start writes a recovery entry at the head of a new segment.
    fs.power_loss();
    fs.restart();
    let log = start_on(&fs, 2);
    for k in 0..5 {
        log.submit(vec![fact(1, 1_000 + k)])
            .unwrap()
            .wait()
            .unwrap();
    }
    log.shutdown().unwrap();
    let whole = batches(&reader(&fs, 9).refresh().unwrap().events);
    let at = whole.iter().position(|b| b.recovery().is_some()).unwrap();
    assert!(whole[at].recovery().unwrap().after < whole[at].last.sequence);
    for after in [whole[at].last, whole[at + 2].last] {
        let mut r = reader(&fs, 10);
        assert!(r.start_after(after).unwrap());
        let u = r.refresh().unwrap();
        assert!(
            u.events.iter().all(|e| matches!(e, ReadEvent::Batch(_))),
            "after {after:?}: {:?}",
            u.events
        );
        let want: Vec<ReadBatch> = whole.iter().filter(|b| b.last > after).cloned().collect();
        assert_eq!(batches(&u.events), want);
    }
}

/// A reader never reads again a batch it returned: damage to one below its last that a later start records as a
/// lost range reaches a started reader, as it does one that read the log from its start, only through
/// [`LogReader::lost_ranges`], never as an event.
#[test]
fn a_range_a_restart_records_below_what_was_read_reaches_a_started_reader_only_as_a_lost_range() {
    let fs = written(20);
    let whole = batches(&reader(&fs, 9).refresh().unwrap().events);
    let mut full = reader(&fs, 10);
    let mut started = reader(&fs, 11);
    assert!(started.start_after(whole[whole.len() - 6].last).unwrap());
    full.refresh().unwrap();
    started.refresh().unwrap();
    let target = &whole[whole.len() - 3];
    fs.power_loss();
    let segment = fs
        .paths()
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "kvl" || e == "open"))
        .find(|p| {
            p.to_string_lossy()
                .contains(&format!("{:016x}", target.file_id))
        })
        .unwrap();
    fs.poke(&segment, target.offset as usize + 40, b"XXXXXXXX");
    fs.restart();
    start_on(&fs, 2).shutdown().unwrap();
    let f = full.refresh().unwrap().events;
    let s = started.refresh().unwrap().events;
    assert!(s.iter().all(|e| matches!(e, ReadEvent::Batch(_))), "{s:?}");
    assert_eq!(batches(&s), batches(&f));
    let lost = started.lost_ranges();
    assert_eq!(lost, full.lost_ranges());
    assert!(
        lost.len() == 1
            && lost[0].first <= (target.first.generation, target.first.sequence)
            && (target.last.generation, target.last.sequence) <= lost[0].last,
        "{lost:?}"
    );
}
