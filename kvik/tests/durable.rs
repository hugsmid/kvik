//! The durable backend: what reaches the log survives crashes and power losses, a reader in another handle sees
//! what is committed, the series registry is recovered at every start, ids are never assigned twice, and calls
//! waiting for room in the ingest queue hold up no other call.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use common::durable::{World, gauge, history, note, num};
use common::{at, block_on};
use kvik::{
    ChangedAtKind, DefineError, IntervalLength, Kind, LogConfig, LossKind, Outcome, Position,
    Reader, ReaderConfig, Reading, Refusal, RetentionClass, SeriesId, SeriesSpec, StateValues,
    Store, StoreConfig, Timestamp, Value, ValueType, View, ZoneName,
};

#[test]
fn acknowledged_readings_survive_a_power_loss() {
    let mut w = World::new("power-loss");
    let s = w.open();
    let g = block_on(s.define_series(gauge("site/meter/power"))).unwrap();
    let mut acked: Vec<(Position, f64)> = Vec::new();
    for i in 0..50 {
        let ack = block_on(s.emit(&[num(g, i, i as f64), num(g, i, i as f64)])).unwrap();
        assert_eq!(ack.duplicates_dropped(), 1);
        acked.push((ack.first().unwrap(), i as f64));
    }
    // A call that waits on no one: queued, never awaited, so never acknowledged.
    drop(s.try_emit(&[num(g, 1_000, 1.0)]).unwrap());
    w.power_loss(s);

    let s = w.open();
    let r = s.reader();
    let kept = history(&r, g);
    for (p, v) in &acked {
        assert!(
            kept.iter()
                .any(|f| f.position == *p && f.number() == Some(*v)),
            "acknowledged reading at {p} lost"
        );
    }
    // The registry file the start after the power loss writes holds the definition and the start's recovery entry,
    // up to the mark it synced.
    let f = registry_file(&w);
    let kinds: Vec<_> = f
        .entries
        .iter()
        .map(|(_, _, e)| matches!(e, kvik_log::entries::Entry::Recovery(_)))
        .collect();
    assert_eq!(kinds, vec![false, true]);
    assert_eq!(f.max_series_id, 1);
    // The registry was recovered: the series is defined, under its name, and defining it again is idempotent.
    assert_eq!(s.series("site/meter/power").unwrap().id, g);
    assert_eq!(
        block_on(s.define_series(gauge("site/meter/power"))).unwrap(),
        g
    );
    // A replay of what was acknowledged is a duplicate.
    let replay: Vec<_> = acked.iter().map(|(_, v)| num(g, *v as i64, *v)).collect();
    let ack = block_on(s.emit(&replay)).unwrap();
    assert_eq!(ack.recorded(), 0, "{:?}", ack.outcomes);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_duplicate_of_a_queued_reading_waits_for_the_original() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    let mut w = World::new("queued-duplicate");
    let s = w.open();
    let g = block_on(s.define_series(gauge("site/meter/power"))).unwrap();
    // Hold the next sync of the segment in use, then fail it: the original call's outcome is unknown, and it is not
    // stored.
    let armed = Arc::new(AtomicBool::new(false));
    let (entered_tx, entered) = mpsc::channel::<()>();
    let (release, release_rx) = mpsc::channel::<()>();
    let release_rx = std::sync::Mutex::new(release_rx);
    let fs = w.fs.clone();
    let hook_armed = armed.clone();
    w.fs.set_sync_hook(Some(Arc::new(move |p: &Path| {
        // The segment in use (file id 1), not the one created ahead.
        let in_use = p
            .to_str()
            .is_some_and(|p| p.ends_with("-0000000000000001.open"));
        if in_use && hook_armed.swap(false, Ordering::SeqCst) {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
            fs.fail_syncs(true);
        }
    })));
    armed.store(true, Ordering::SeqCst);
    let original = s.try_emit(&[num(g, 5, 1.0)]).unwrap();
    entered
        .recv_timeout(Duration::from_secs(5))
        .expect("the original's sync started");
    // The same reading again while the original is queued but not synced: a duplicate, not yet final.
    let mut duplicate = std::pin::pin!(s.try_emit(&[num(g, 5, 1.0)]).unwrap());
    let woken = Arc::new(common::Flag::default());
    assert!(common::poll_once(duplicate.as_mut(), &woken).is_pending());
    release.send(()).unwrap();
    // The original's sync failed: the duplicate is not acknowledged either.
    let r = block_on(duplicate.as_mut());
    assert!(
        matches!(r, Err(kvik::EmitError::OutcomeUnknown(_))),
        "{r:?}"
    );
    assert!(matches!(
        block_on(original),
        Err(kvik::EmitError::OutcomeUnknown(_))
    ));
    w.fs.set_sync_hook(None);
    w.fs.fail_syncs(false);
    w.power_loss(s);
    let s = w.open();
    assert!(
        history(&s.reader(), g).is_empty(),
        "the reading was never stored"
    );
    // Acknowledged duplicates of acknowledged readings resolve at once.
    block_on(s.emit(&[num(g, 5, 1.0)])).unwrap();
    let ack = block_on(s.emit(&[num(g, 5, 1.0)])).unwrap();
    assert_eq!(ack.outcomes, vec![Outcome::Duplicate]);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_reader_process_sees_committed_facts() {
    let w = World::new("reader-process");
    let s = w.open();
    // Another handle on the same files, as another process would open it.
    let r = w.reader();
    let g = block_on(s.define_series(gauge("site/room/temperature"))).unwrap();
    assert_eq!(r.series("site/room/temperature").unwrap().id, g);
    let first = block_on(s.emit(&[num(g, 0, 20.5)])).unwrap();
    let state = r.refresh().unwrap();
    assert!(state.durable >= first.last().unwrap());
    assert_eq!(history(&r, g).len(), 1);
    block_on(s.emit(&[num(g, 1, 21.0), num(g, 0, 20.0)])).unwrap();
    block_on(s.alias(g, "site/room/temp")).unwrap();
    // Every read call reads what was appended since the last.
    let cur: Vec<_> = r
        .facts(g, at(0)..at(2), View::Current)
        .unwrap()
        .map(|f| f.unwrap().number().unwrap())
        .collect();
    assert_eq!(cur, vec![20.0, 21.0]);
    let then: Vec<_> = r
        .facts(g, at(0)..at(2), View::AsOf(first.last().unwrap()))
        .unwrap()
        .map(|f| f.unwrap().number().unwrap())
        .collect();
    assert_eq!(then, vec![20.5]);
    let info = r.series("site/room/temp").unwrap();
    assert_eq!(info.names, vec!["site/room/temperature", "site/room/temp"]);
    assert_eq!(info.latest_t, Some(at(1)));
    // The reader keeps reading after the writer has gone.
    block_on(s.shutdown()).unwrap();
    assert_eq!(history(&r, g).len(), 3);
    // Live state from the facts the reader holds: the current fact with the greatest `t` not after `now`.
    match r.live(g).unwrap() {
        kvik::Live::Value(v) => assert_eq!((v.fact.number(), v.stale), (Some(20.0), false)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_registry_is_recovered_after_crashes() {
    let mut w = World::new("registry-recovery");
    let s = w.open();
    let a = block_on(s.define_series(gauge("site/a/power"))).unwrap();
    let mode = block_on(s.define_series(SeriesSpec::new(
        "site/a/mode",
        Kind::State {
            values: StateValues::Named(vec!["off".into(), "on".into()]),
        },
        "",
    )))
    .unwrap();
    let price =
        block_on(s.define_series(gauge("grid/area/price").versioned(ChangedAtKind::Version)))
            .unwrap();
    let energy = block_on(s.define_series(SeriesSpec::new(
        "site/a/energy",
        Kind::Interval {
            value: ValueType::F64,
            length: IntervalLength::Fixed {
                d: Duration::from_secs(1_800),
                phase: Duration::ZERO,
            },
        },
        "kWh",
    )))
    .unwrap();
    block_on(s.alias(a, "site/a/p")).unwrap();
    block_on(s.retire(mode, at(100))).unwrap();
    block_on(s.set_parameters(
        a,
        at(0),
        kvik::ParameterChange::default().retention(RetentionClass::Diagnostic {
            period: Duration::from_secs(3_600),
        }),
    ))
    .unwrap();
    block_on(s.set_site_zone(&ZoneName::new("Europe/Oslo"), at(86_400))).unwrap();
    let before: Vec<_> = [a, mode, price, energy]
        .iter()
        .map(|id| s.series(*id).unwrap())
        .collect();
    w.power_loss(s);

    // The zone in force is the one the entry named.
    assert!(matches!(
        block_on(Store::open(w.config())),
        Err(kvik::OpenError::SiteZoneMismatch { .. })
    ));
    let mut oslo = w.config();
    oslo.time_zone = "Europe/Oslo".into();
    let s = block_on(Store::open(oslo)).unwrap();
    let after: Vec<_> = [a, mode, price, energy]
        .iter()
        .map(|id| s.series(*id).unwrap())
        .collect();
    assert_eq!(after, before);
    assert_eq!(s.series("site/a/p").unwrap().id, a);
    assert!(matches!(
        s.series("site/a/mode"),
        Err(kvik::QueryError::UnknownSeries)
    ));
    // Rules decided by the recovered registry.
    let ack = block_on(s.emit(&[
        Reading::new(mode, at(100), Value::State(1)),
        Reading::new(mode, at(99), Value::State(1)),
        num(price, 0, 1.0),
        Reading::new(energy, at(900), Value::F64(1.0)),
        Reading::new(energy, at(1_800), Value::F64(1.0)),
    ]))
    .unwrap();
    assert_eq!(
        ack.refused().collect::<Vec<_>>(),
        vec![
            (0, Refusal::Retired),
            (2, Refusal::ChangedAtMissing),
            (3, Refusal::OffPhase)
        ]
    );
    assert!(matches!(
        block_on(s.define_series(gauge("site/a/power").expected_interval(Duration::from_secs(5)))),
        Err(DefineError::Conflict {
            field: "expected_interval"
        })
    ));
    // A retired name can be defined anew: a new series with a new id.
    let mode2 = block_on(s.define_series(SeriesSpec::new("site/a/mode", Kind::Text, ""))).unwrap();
    assert!(mode2.0 > energy.0);
}

#[test]
fn ids_start_at_one_and_never_reuse_a_floor() {
    let w = World::new("id-floor");
    let s = w.open();
    let a = block_on(s.define_series(gauge("x/y/a"))).unwrap();
    assert_eq!(a, SeriesId(1), "ids start at 1 (Q-121)");
    let b = block_on(s.define_series(gauge("x/y/lost"))).unwrap();
    assert_eq!(b, SeriesId(2));
    // Facts after the definition, so that its batch lies below the mark, between complete batches.
    for i in 0..3 {
        block_on(s.emit(&[num(a, i, 1.0)])).unwrap();
    }
    block_on(s.shutdown()).unwrap();
    drop(s);
    // Damage the batch that holds the second definition: synced bytes lost on the disk.
    let (path, bytes) = w
        .segments()
        .into_iter()
        .find(|(_, b)| b.windows(8).any(|x| x == b"x/y/lost"))
        .expect("a segment holding the definition");
    let at_name = bytes.windows(8).position(|x| x == b"x/y/lost").unwrap();
    w.fs.poke(&path, at_name, b"X");

    let s = w.open();
    let losses = s.losses();
    assert!(
        losses.iter().any(|l| l.kind == LossKind::DataLost),
        "{losses:?}"
    );
    assert!(matches!(s.series(b), Err(kvik::QueryError::UnknownSeries)));
    // The lost definition's id stays assigned: defined again, the series gets a new id.
    let again = block_on(s.define_series(gauge("x/y/lost"))).unwrap();
    assert_eq!(again, SeriesId(3));
    block_on(s.shutdown()).unwrap();
}

/// The store's registry file, read as a start reads it.
fn registry_file(w: &World) -> kvik_log::registry::RegistryFile {
    let store_id = kvik_log::store::read_descriptor(&*w.fs, Path::new(&w.dir))
        .unwrap()
        .store_id;
    kvik_log::registry::read_latest(&*w.fs, Path::new(&w.dir), &store_id)
        .unwrap()
        .expect("a registry file")
        .1
}

#[test]
fn the_registry_file_follows_the_kept_log() {
    let w = World::new("registry-file");
    let s = w.open();
    // Written by the first start, empty.
    assert!(registry_file(&w).entries.is_empty());
    let a = block_on(s.define_series(gauge("x/y/a"))).unwrap();
    block_on(s.alias(a, "x/y/b")).unwrap();
    block_on(s.emit(&[num(a, 0, 1.0)])).unwrap();
    block_on(s.shutdown()).unwrap();
    drop(s);
    // Rewritten at the clean shutdown, up to the mark it synced: the definition and the alias with their positions.
    let f = registry_file(&w);
    assert_eq!(f.entries.len(), 2);
    assert_eq!(f.max_series_id, 1);
    assert!(f.through >= 3);

    // A file that is not a function of the kept log (here it claims a definition at a position that holds a fact,
    // as one from a lost range would) is replaced at the next start; the ids it names stay assigned.
    let mut planted = f.clone();
    let mut def = match &f.entries[0].2 {
        kvik_log::entries::Entry::Definition(d) => (**d).clone(),
        other => panic!("{other:?}"),
    };
    def.id = 50;
    def.name = "x/y/ghost".into();
    planted.entries.push((
        f.through,
        0,
        kvik_log::entries::Entry::Definition(Box::new(def)),
    ));
    planted.max_series_id = 50;
    let store_id = kvik_log::store::read_descriptor(&*w.fs, Path::new(&w.dir))
        .unwrap()
        .store_id;
    kvik_log::registry::write(&*w.fs, Path::new(&w.dir), store_id, 0, &planted).unwrap();
    let s = w.open();
    let rebuilt = registry_file(&w);
    assert_eq!(rebuilt.entries, f.entries, "rebuilt from the kept log");
    assert_eq!(rebuilt.max_series_id, 50, "the floor never goes down");
    assert!(matches!(
        s.series("x/y/ghost"),
        Err(kvik::QueryError::UnknownSeries)
    ));
    assert_eq!(
        block_on(s.define_series(gauge("x/y/c"))).unwrap(),
        SeriesId(51)
    );
    block_on(s.shutdown()).unwrap();
}

#[test]
fn the_registry_file_carries_the_id_floor_when_the_mark_is_lost() {
    let w = World::new("registry-floor");
    let s = w.open();
    let a = block_on(s.define_series(gauge("x/y/a"))).unwrap();
    block_on(s.define_series(gauge("x/y/lost"))).unwrap();
    for i in 0..3 {
        block_on(s.emit(&[num(a, i, 1.0)])).unwrap();
    }
    block_on(s.shutdown()).unwrap();
    drop(s);
    // The definition of id 2 is damaged and the mark is gone: only the registry file still knows id 2.
    let (path, bytes) = w
        .segments()
        .into_iter()
        .find(|(_, b)| b.windows(8).any(|x| x == b"x/y/lost"))
        .unwrap();
    let at_name = bytes.windows(8).position(|x| x == b"x/y/lost").unwrap();
    w.fs.poke(&path, at_name, b"X");
    kvik::env::Fs::remove_file(&*w.fs, &Path::new(&w.dir).join("log").join("MARK")).unwrap();
    let s = w.open();
    assert_eq!(
        block_on(s.define_series(gauge("x/y/lost"))).unwrap(),
        SeriesId(3),
        "the id floor of the registry file"
    );
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_clean_shutdown_writes_the_registry_file_under_the_stores_lock() {
    use kvik::env::{Fs, OpenMode};
    use std::sync::Mutex;
    let w = World::new("shutdown-lock");
    let s = w.open();
    block_on(s.define_series(gauge("a/b/c"))).unwrap();
    let seen: Arc<Mutex<Vec<bool>>> = Arc::default();
    let (fs, lock_path, seen2) = (w.fs.clone(), Path::new(&w.dir).join("LOCK"), seen.clone());
    w.fs.set_sync_hook(Some(Arc::new(move |p: &Path| {
        if p.components().any(|c| c.as_os_str() == "registry") {
            let f = fs.open(&lock_path, OpenMode::ReadWrite).unwrap();
            seen2.lock().unwrap().push(!f.try_lock_exclusive().unwrap());
        }
    })));
    block_on(s.shutdown()).unwrap();
    w.fs.set_sync_hook(None);
    let seen = seen.lock().unwrap().clone();
    assert!(
        !seen.is_empty() && seen.iter().all(|held| *held),
        "{seen:?}"
    );
}

#[test]
fn orphaned_facts_stay_out_of_views_and_are_exported() {
    let w = World::new("orphaned");
    let s = w.open();
    let a = block_on(s.define_series(gauge("x/y/a"))).unwrap();
    let b = block_on(s.define_series(gauge("x/y/orphan"))).unwrap();
    block_on(s.emit(&[num(b, 0, 7.0), num(b, 1, 8.0)])).unwrap();
    for i in 0..3 {
        block_on(s.emit(&[num(a, i, 1.0)])).unwrap();
    }
    block_on(s.shutdown()).unwrap();
    drop(s);
    let (path, bytes) = w
        .segments()
        .into_iter()
        .find(|(_, x)| x.windows(10).any(|n| n == b"x/y/orphan"))
        .unwrap();
    let at_name = bytes.windows(10).position(|n| n == b"x/y/orphan").unwrap();
    w.fs.poke(&path, at_name, b"X");

    let s = w.open();
    // Kept in the log, outside views (spec §2.1): the id is unknown to every call.
    assert!(matches!(s.series(b), Err(kvik::QueryError::UnknownSeries)));
    let r = s.reader();
    assert!(matches!(
        r.facts(b, at(0)..at(2), View::Current),
        Err(kvik::QueryError::UnknownSeries)
    ));
    let ack = block_on(s.emit(&[num(b, 2, 9.0)])).unwrap();
    assert_eq!(
        ack.refused().collect::<Vec<_>>(),
        vec![(0, Refusal::UnknownSeries)]
    );
    // Exported for re-ingest, in position order.
    let orphans = s
        .export_orphaned(Position::ORIGIN..=Position::new(0, u64::MAX))
        .unwrap();
    let values: Vec<_> = orphans.iter().map(|f| (f.series, f.number())).collect();
    assert_eq!(values, vec![(b, Some(7.0)), (b, Some(8.0))]);
    // New ids go above every series id in a kept fact.
    assert_eq!(
        block_on(s.define_series(gauge("x/y/new"))).unwrap(),
        SeriesId(3)
    );
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_store_on_the_system_environment() {
    // The default environment: the operating system's file system, clocks, boot identity and tzdata.
    let dir = std::env::temp_dir().join(format!("kvik-durable-system-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut log = LogConfig::default();
    log.segment_size = 16 << 20;
    let config = || {
        StoreConfig::new(&dir, "Europe/London")
            .create_if_missing(true)
            .log(log.clone())
    };
    let s = block_on(Store::open(config())).unwrap();
    assert!(s.is_durable());
    let g = block_on(s.define_series(gauge("a/b/c"))).unwrap();
    let now = s.now();
    let ack = block_on(s.emit(&[Reading::new(g, now, Value::F64(1.5))])).unwrap();
    assert_eq!(ack.recorded(), 1);
    block_on(s.shutdown()).unwrap();
    drop(s);
    let s = block_on(Store::open(config())).unwrap();
    let r = Reader::open(ReaderConfig::new(&dir)).unwrap();
    let fs: Vec<_> = r
        .facts(g, now..Timestamp(now.0 + 1), View::Current)
        .unwrap()
        .map(|f| f.unwrap().number())
        .collect();
    assert_eq!(fs, vec![Some(1.5)]);
    assert_eq!(r.series("a/b/c").unwrap().id, g);
    drop(r);
    block_on(s.shutdown()).unwrap();
    drop(s);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn writers_on_several_threads() {
    let w = World::new("threads");
    let s = w.open();
    let g = block_on(s.define_series(gauge("a/b/c"))).unwrap();
    let writers: Vec<_> = (0..4)
        .map(|k| {
            let s = s.clone();
            thread::spawn(move || {
                (0..25)
                    .map(|i| {
                        block_on(s.emit(&[num(g, k * 100 + i, i as f64)]))
                            .unwrap()
                            .first()
                            .unwrap()
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut positions: Vec<Position> = writers
        .into_iter()
        .flat_map(|t| t.join().unwrap())
        .collect();
    positions.sort();
    positions.dedup();
    assert_eq!(positions.len(), 100);
    assert_eq!(history(&s.reader(), g).len(), 100);
    assert_eq!(s.series_stats(g).unwrap().recorded, 100);
    block_on(s.shutdown()).unwrap();
}

/// Runs `f` on another thread; whether it returned within `limit`.
fn returns_within(limit: Duration, f: impl FnOnce() + Send + 'static) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        f();
        let _ = tx.send(());
    });
    rx.recv_timeout(limit).is_ok()
}

#[test]
fn with_the_queue_full_try_emit_is_busy_at_once_while_other_calls_wait_for_room() {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    let w = World::new("queue-full");
    let mut limits = kvik::Limits::default();
    limits.ingest_queue = 1024;
    let s = block_on(Store::open(w.config().limits(limits))).unwrap();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    // The next log sync is held, so nothing leaves the queue.
    let (release, gate) = std::sync::mpsc::channel::<()>();
    let gate = Mutex::new(gate);
    let armed = Arc::new(AtomicBool::new(true));
    let a = armed.clone();
    w.fs.set_sync_hook(Some(Arc::new(move |_: &Path| {
        if thread::current().name() == Some("kvik-writer") && a.swap(false, Ordering::SeqCst) {
            gate.lock().unwrap().recv().unwrap();
        }
    })));
    let batch = |n: i64| -> Vec<Reading> {
        (0..256)
            .map(|k| note(id, n * 1_000 + k, &format!("v{n}-{k}")))
            .collect()
    };
    let mut queued = Vec::new();
    let mut n = 0;
    let full = loop {
        match s.try_emit(&batch(n)) {
            Ok(p) => queued.push(p),
            Err(e) => break e,
        }
        n += 1;
        assert!(n < 100, "the queue never filled");
        thread::sleep(Duration::from_millis(10));
    };
    assert!(
        matches!(full, kvik::EmitError::Busy(kvik::Wait::QueueFull)),
        "{full:?}"
    );

    // A waiting emit and a definition wait for room without holding up other calls.
    let s2 = s.clone();
    let waiting = thread::spawn(move || block_on(s2.emit(&[note(id, 900_000, "w")])).map(|_| ()));
    let s3 = s.clone();
    let defining =
        thread::spawn(move || block_on(s3.define_series(gauge("a/b/later"))).map(|_| ()));
    thread::sleep(Duration::from_millis(200));
    let s4 = s.clone();
    assert!(returns_within(Duration::from_secs(1), move || {
        let r = s4.try_emit(&[note(id, 900_001, "t")]);
        assert!(
            matches!(r, Err(kvik::EmitError::Busy(kvik::Wait::QueueFull))),
            "{:?}",
            r.map(|_| ())
        );
    }));
    let s5 = s.clone();
    assert!(returns_within(Duration::from_secs(1), move || {
        s5.series_stats(id).unwrap();
        s5.health();
        s5.stats();
    }));
    assert!(!waiting.is_finished() && !defining.is_finished());

    release.send(()).unwrap();
    waiting.join().unwrap().unwrap();
    defining.join().unwrap().unwrap();
    for p in queued {
        block_on(p).unwrap();
    }
    w.fs.set_sync_hook(None);
    block_on(s.shutdown()).unwrap();
}
