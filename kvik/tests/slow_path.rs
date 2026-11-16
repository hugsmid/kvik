//! The decision's and the deliverer's slow path (roadmap 11.0, spec §2.4): readings and facts outside the recent
//! window are judged from the local log, across seals, restarts and damage, and what cannot be read is flagged
//! `unchecked`, counted and reported in health (Q-750).

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use common::block_on;
use common::world::{World, drain, gauge, num};
use kvik::{
    ChangedAtKind, DeliveryKind, Funnel, Health, Outcome, Reading, Reason, Select, Start, Store,
};
use kvik_log::fs::FaultOp;

/// The reason `LocalDataUnreadable` in a store's health, if present.
fn unreadable(s: &Store) -> Option<(u64, u64)> {
    match s.health() {
        Health::Degraded { reasons } => reasons.into_iter().find_map(|r| match r {
            Reason::LocalDataUnreadable {
                readings,
                deliveries,
            } => Some((readings, deliveries)),
            _ => None,
        }),
        _ => None,
    }
}

/// A segment's file id, from its name (`format.md` A.12).
fn file_id(p: &Path) -> u64 {
    let name = p.file_stem().unwrap().to_string_lossy();
    name.rsplit('-').next().unwrap().parse().unwrap()
}

/// Waits, on the real clock the log's timers use, until the store's log has `n` sealed segments.
fn sealed(w: &World, n: usize) -> Vec<PathBuf> {
    for _ in 0..4_000 {
        let v: Vec<_> =
            w.fs.paths()
                .into_iter()
                .filter(|p| {
                    p.starts_with(Path::new(&w.dir).join("log"))
                        && p.extension().is_some_and(|e| e == "kvl")
                })
                .collect();
        if v.len() >= n {
            return v;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("fewer than {n} sealed segments");
}

#[test]
fn a_reading_outside_the_recent_window_is_checked_against_the_log_across_seals_restarts_and_damage()
{
    let w = World::new("slow-path");
    let mut config = w.config();
    config.log.seal_interval = Duration::from_millis(20);
    let open = || block_on(Store::open(config.clone())).unwrap();
    let s = open();
    let g = block_on(s.define_series(gauge("site/meter/power"))).unwrap();
    sealed(&w, 1);
    block_on(s.emit(&[num(g, 0, 1.0), num(g, 1, 2.0)])).unwrap();
    let facts = sealed(&w, 2)
        .into_iter()
        .max_by_key(|p| file_id(p))
        .unwrap();
    // An hour later: the first readings leave the recent window, and the segment holding them is sealed.
    block_on(s.emit(&[num(g, 3_600, 3.0)])).unwrap();
    sealed(&w, 3);
    let ack = block_on(s.emit(&[num(g, 0, 1.0), num(g, 1, 9.0), num(g, 1, 2.0)])).unwrap();
    assert!(matches!(ack.outcomes[0], Outcome::Duplicate));
    assert!(matches!(
        ack.outcomes[1],
        Outcome::Recorded {
            unchecked: false,
            ..
        }
    ));
    assert!(matches!(ack.outcomes[2], Outcome::Duplicate));
    assert_eq!(s.stats().ingest.slow_path, 3);
    block_on(s.shutdown()).unwrap();
    drop(s);

    // After a restart the key revised outside the window is read from the log again.
    let s = open();
    let ack = block_on(s.emit(&[num(g, 1, 9.0), num(g, 0, 1.0), num(g, 2, 4.0)])).unwrap();
    assert!(matches!(ack.outcomes[0], Outcome::Duplicate));
    assert!(matches!(ack.outcomes[1], Outcome::Duplicate));
    assert!(matches!(
        ack.outcomes[2],
        Outcome::Recorded {
            unchecked: false,
            ..
        }
    ));
    assert_eq!(s.stats().ingest.slow_path, 3);
    // Damage while the store runs, in the batch of the first readings: never read as "no such key".
    let bytes = w.fs.read(&facts).unwrap();
    w.fs.poke(&facts, 100, &[bytes[100] ^ 0xFF]);
    let ack = block_on(s.emit(&[num(g, 0, 1.0)])).unwrap();
    assert!(
        matches!(
            ack.outcomes[0],
            Outcome::Recorded {
                unchecked: true,
                ..
            }
        ),
        "{:?}",
        ack.outcomes
    );
    assert_eq!(s.series_stats(g).unwrap().duplicates_unchecked, 1);
    assert_eq!(unreadable(&s), Some((1, 0)));
    // The unchecked reading is in the log now: a repeat is a duplicate of it.
    let ack = block_on(s.emit(&[num(g, 0, 1.0)])).unwrap();
    assert!(matches!(ack.outcomes[0], Outcome::Duplicate));
    block_on(s.shutdown()).unwrap();
    drop(s);

    // A start passes over the damaged region as lost (spec §3 Recovery rule 2): its facts are out of the log.
    let s = open();
    assert_eq!(unreadable(&s), None);
    let ack = block_on(s.emit(&[num(g, 1, 2.0)])).unwrap();
    assert!(
        matches!(
            ack.outcomes[0],
            Outcome::Recorded {
                unchecked: false,
                ..
            }
        ),
        "{:?}",
        ack.outcomes
    );
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_reading_that_leaves_the_window_before_its_call_is_acknowledged_is_still_found() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    let w = World::new("left-before-acknowledged");
    let s = w.open();
    let g = block_on(s.define_series(gauge("site/meter/power"))).unwrap();
    let armed = Arc::new(AtomicBool::new(false));
    let (entered_tx, entered) = mpsc::channel::<()>();
    let (release, release_rx) = mpsc::channel::<()>();
    let release_rx = std::sync::Mutex::new(release_rx);
    let hook_armed = armed.clone();
    w.fs.set_sync_hook(Some(Arc::new(move |p: &Path| {
        let in_use = p
            .to_str()
            .is_some_and(|p| p.ends_with("-0000000000000001.open"));
        if in_use && hook_armed.swap(false, Ordering::SeqCst) {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        }
    })));
    armed.store(true, Ordering::SeqCst);
    // One call whose second reading moves the window an hour past its first: the first leaves the window while the
    // call is not yet in the log.
    let original = s.try_emit(&[num(g, 0, 1.0), num(g, 3_600, 2.0)]).unwrap();
    entered
        .recv_timeout(Duration::from_secs(5))
        .expect("the original's sync started");
    let mut duplicate = std::pin::pin!(s.try_emit(&[num(g, 0, 1.0)]).unwrap());
    let woken = Arc::new(common::Flag::default());
    assert!(common::poll_once(duplicate.as_mut(), &woken).is_pending());
    release.send(()).unwrap();
    assert_eq!(block_on(original).unwrap().recorded(), 2);
    assert_eq!(
        block_on(duplicate.as_mut()).unwrap().outcomes,
        vec![Outcome::Duplicate]
    );
    w.fs.set_sync_hook(None);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_consumer_behind_stays_behind_while_the_log_cannot_be_read_again() {
    let w = World::new("behind-reread-fails");
    let s = w.open();
    let g = block_on(s.define_series(gauge("site/meter/power"))).unwrap();
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    block_on(c.route(Funnel::facts(Select::Ids(vec![g])))).unwrap();
    for i in 0..2 {
        block_on(s.emit(&[num(g, i, i as f64)])).unwrap();
    }
    drain(&mut c);
    let unconfirmed: Vec<_> = (2..5)
        .map(|i| {
            block_on(s.emit(&[num(g, i, i as f64)]))
                .unwrap()
                .first()
                .unwrap()
        })
        .collect();
    block_on(s.shutdown()).unwrap();
    drop((c, s));
    let s = w.open();
    // The consumer resumes behind: its records are read from the log again, with a reader whose lease cannot be
    // created this once.
    w.fs.fail_nth(FaultOp::Create, "leases", 0, std::io::ErrorKind::Other);
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    let first = drain(&mut c);
    assert!(
        first
            .iter()
            .all(|d| !matches!(d.kind, DeliveryKind::Fact(_))),
        "nothing replayed while the read fails: {first:?}"
    );
    assert!(s.stats().delivery.refresh_errors >= 1);
    let replayed: Vec<_> = drain(&mut c)
        .into_iter()
        .filter_map(|d| match d.kind {
            DeliveryKind::Fact(f) => Some(f.position),
            _ => None,
        })
        .collect();
    assert_eq!(replayed, unconfirmed);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_fact_whose_earlier_facts_cannot_be_read_is_delivered_unchecked() {
    let w = World::new("delivered-unchecked");
    let mut config = w.config();
    config.log.seal_interval = Duration::from_millis(20);
    let s = block_on(Store::open(config)).unwrap();
    let v = block_on(s.define_series(gauge("site/price/now").versioned(ChangedAtKind::Version)))
        .unwrap();
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    block_on(c.route(Funnel::facts(Select::Ids(vec![v])))).unwrap();
    sealed(&w, 1);
    let version = |secs, value, c| Reading {
        changed_at: Some(c),
        ..num(v, secs, value)
    };
    block_on(s.emit(&[version(0, 1.0, 5)])).unwrap();
    let facts = sealed(&w, 2)
        .into_iter()
        .max_by_key(|p| file_id(p))
        .unwrap();
    // An hour on, in event time and on the clock: the first key leaves both windows.
    w.clock.advance(Duration::from_secs(3_600));
    block_on(s.emit(&[version(3_600, 2.0, 5)])).unwrap();
    sealed(&w, 3);
    drain(&mut c);
    let bytes = w.fs.read(&facts).unwrap();
    w.fs.poke(&facts, 100, &[bytes[100] ^ 0xFF]);
    // A lower version of the first key: outranked, but the fact that outranks it cannot be read.
    let ack = block_on(s.emit(&[version(0, 3.0, 3)])).unwrap();
    assert!(matches!(
        ack.outcomes[0],
        Outcome::Recorded {
            unchecked: true,
            ..
        }
    ));
    let delivered: Vec<_> = drain(&mut c)
        .into_iter()
        .filter(|d| matches!(d.kind, DeliveryKind::Fact(_)))
        .collect();
    assert_eq!(delivered.len(), 1);
    assert!(delivered[0].flags.unchecked && !delivered[0].flags.outranked);
    assert_eq!(s.stats().delivery.unchecked, 1);
    assert_eq!(unreadable(&s), Some((1, 1)));
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_restart_across_several_parts_of_the_log_decides_as_before() {
    let w = World::new("restart-in-parts");
    let s = w.open();
    let ids: Vec<_> = (0..40)
        .map(|i| block_on(s.define_series(gauge(&format!("site/m{i}/power")))).unwrap())
        .collect();
    // More than two parts of 65,536 items, two hours of one reading a second per series.
    for chunk in (0..7_200).collect::<Vec<i64>>().chunks(400) {
        let readings: Vec<Reading> = chunk
            .iter()
            .flat_map(|&t| ids.iter().map(move |&id| num(id, t, t as f64)))
            .collect();
        block_on(s.emit(&readings)).unwrap();
    }
    let recorded = s.series_stats(ids[7]).unwrap().recorded;
    block_on(s.shutdown()).unwrap();
    drop(s);
    let s = w.open();
    assert_eq!(s.series_stats(ids[7]).unwrap().recorded, recorded);
    let probe = [
        num(ids[0], 10, 10.0),
        num(ids[1], 3_000, 3_000.0),
        num(ids[2], 7_000, 7_000.0),
        num(ids[3], 10, 11.0),
        num(ids[4], 7_199, 1.0),
    ];
    let ack = block_on(s.emit(&probe)).unwrap();
    let got: Vec<bool> = ack
        .outcomes
        .iter()
        .map(|o| matches!(o, Outcome::Duplicate))
        .collect();
    assert_eq!(got, [true, true, true, false, false]);
    assert_eq!(s.stats().ingest.slow_path, 3);
    block_on(s.shutdown()).unwrap();
}
