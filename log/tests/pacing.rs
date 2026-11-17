//! Batch-sync pacing (spec §3 Group commit, Q-980 to Q-984): the synchronous writer core driven with explicit times,
//! a manual clock that each sync advances by a scripted duration from the file system's sync hook, and calls that
//! arrive by that clock while a sync is in flight, against a reference schedule written out on its own.

#[path = "pacing/harness.rs"]
mod harness;

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use harness::*;
use kvik_log::content::Item;
use kvik_log::entries::Entry;
use kvik_log::fs::{FaultOp, MemFs};
use kvik_log::store::{MARK, NewStore, StoreDir};
use kvik_log::writer::{Clock, Log, LogWriter, MarkSync, WriterConfig};
use proptest::prelude::*;

/// The batch sync each call's covering start names, by index into `starts`.
fn batch_of(s: &Schedule, call: usize) -> usize {
    s.starts
        .iter()
        .rposition(|t| *t == s.covered[call])
        .unwrap()
}

proptest! {
    #[test]
    fn the_writer_agrees_with_the_reference_schedule((floor, ceiling) in arb_bounds(), h in arb_history()) {
        let setup = Setup { floor, ceiling, ..Setup::base() };
        let (_, got) = run(setup, &h);
        prop_assert_eq!(got, reference(&h, setup.spacing()));
    }

    #[test]
    fn no_call_is_acknowledged_before_the_sync_covering_it_returns(
        (floor, ceiling) in arb_bounds(),
        h in arb_history(),
    ) {
        let (_, got) = run(Setup { floor, ceiling, ..Setup::base() }, &h);
        for (i, (c, a)) in got.covered.iter().zip(&got.acked).enumerate() {
            prop_assert!(*a >= *c + h.sync(batch_of(&got, i)), "call {} covered at {:?}, acknowledged at {:?}", i, c, a);
            prop_assert!(*c >= h.calls[i].0, "call {} covered before it arrived", i);
        }
    }

    #[test]
    fn under_load_batch_syncs_start_at_least_the_spacing_apart(
        (floor, ceiling) in arb_bounds(),
        h in arb_history(),
    ) {
        let setup = Setup { floor, ceiling, ..Setup::base() };
        let (s, _) = run(setup, &h);
        for k in 1..s.starts.len() {
            if s.waited[k - 1] {
                let gap = s.starts[k] - s.starts[k - 1];
                prop_assert!(gap >= setup.spacing(), "syncs {} and {} are {:?} apart", k - 1, k, gap);
            }
        }
    }

    #[test]
    fn a_batch_sync_after_one_that_returned_with_nothing_waiting_starts_at_the_first_turn_that_sees_its_call(
        (floor, ceiling) in arb_bounds(),
        h in arb_history(),
    ) {
        let (s, got) = run(Setup { floor, ceiling, ..Setup::base() }, &h);
        for k in 0..s.starts.len() {
            if k > 0 && s.waited[k - 1] {
                continue;
            }
            let first = got.covered.iter().position(|c| *c == s.starts[k]).unwrap();
            let seen = s.turns.iter().find(|t| **t >= h.calls[first].0).unwrap();
            prop_assert_eq!(s.starts[k], *seen, "sync {}", k);
        }
    }

    #[test]
    fn one_batch_sync_covers_every_call_that_arrived_while_the_previous_one_was_in_flight(
        (floor, ceiling) in arb_bounds(),
        h in arb_history(),
    ) {
        let (s, got) = run(Setup { floor, ceiling, ..Setup::base() }, &h);
        for k in 0..s.starts.len().saturating_sub(1) {
            for (i, &(t, _)) in h.calls.iter().enumerate() {
                if t > s.starts[k] && t <= s.ends[k] {
                    prop_assert_eq!(got.covered[i], s.starts[k + 1], "call {} arrived during sync {}", i, k);
                }
            }
        }
    }

    #[test]
    fn the_smoothed_sync_time_stays_between_the_last_sample_and_the_largest(
        syncs in prop::collection::vec(arb_sync(), 1..60),
    ) {
        let mut s = Sim::new(Setup::base());
        let mut at = START;
        let mut largest = Duration::ZERO;
        for (k, &d) in syncs.iter().enumerate() {
            s.push();
            s.turn(at, d);
            largest = largest.max(d);
            let est = s.w.stats().sync_time.unwrap();
            prop_assert!(d <= est && est <= largest, "after sync {}: {:?} of {:?}", k, est, d);
            prop_assert_eq!(s.w.stats().sync_interval, FLOOR);
            at += Duration::from_secs(1);
        }
    }

    #[test]
    fn after_a_peak_the_estimate_falls_at_least_an_eighth_of_the_way_per_sync(
        peak_us in 0u64..50_000,
        level_us in 0u64..50_000,
        m in 0usize..60,
    ) {
        let (peak, level) = (peak_us.max(level_us), peak_us.min(level_us));
        let mut s = Sim::new(Setup::base());
        let mut at = START;
        for d in std::iter::once(peak).chain(std::iter::repeat_n(level, m)) {
            s.push();
            s.turn(at, US * d as u32);
            at += Duration::from_secs(1);
        }
        let est = s.w.stats().sync_time.unwrap().as_nanos() as f64;
        let bound = 1e3 * (level as f64 + (peak - level) as f64 * (7.0f64 / 8.0).powi(m as i32)) + m as f64;
        prop_assert!(est <= bound, "{} ns above {} ns", est, bound);
    }
}

fn segments(fs: &MemFs) -> Vec<PathBuf> {
    fs.paths()
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "open"))
        .collect()
}

#[test]
fn no_call_is_acknowledged_before_its_batch_is_on_stable_storage() {
    let sync = 700 * US;
    let mut s = Sim::new(Setup::base());
    s.push();
    for k in 0..40u32 {
        s.arrive(START + 150 * US * k);
    }
    let mut at = START;
    let mut first = true;
    while s.calls_waiting() || !s.pending.is_empty() {
        let acked = s.acked.len();
        let (n, busy) = s.turn(at, sync);
        if s.acked.len() > acked {
            for p in segments(&s.fs) {
                assert_eq!(s.fs.read(&p), s.fs.read_durable(&p), "{p:?} at {at:?}");
            }
            if first {
                let mark = Path::new("store/log").join(MARK);
                assert_eq!(s.fs.read(&mark), s.fs.read_durable(&mark), "Q-630");
                first = false;
            }
            assert!(busy >= sync, "acknowledged {busy:?} into the turn");
        }
        let free = at + busy;
        let by_call = s.pending.iter().map(|(t, _)| *t).find(|t| *t > free);
        at = [timed(n, free, Duration::ZERO), by_call]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(free)
            .max(free);
    }
    assert_eq!(s.acked.len(), 41);
    assert!(s.w.stats().batches < 41, "calls shared batch syncs");
}

#[test]
fn the_calls_that_arrive_during_a_sync_form_the_next_batch_at_the_floor_or_as_it_returns() {
    for (floor, sync, second) in [
        (Duration::ZERO, MS, START + MS),
        (FLOOR, MS, START + FLOOR),
        (FLOOR, 3 * MS, START + 3 * MS),
    ] {
        let mut s = Sim::new(Setup {
            floor,
            ..Setup::base()
        });
        let a = s.push();
        for k in 1..=5u32 {
            s.arrive(START + 100 * US * k);
        }
        let (next, busy) = s.turn(START, sync);
        assert_eq!(s.acked[&a], START + sync);
        assert_eq!(timed(next, START + busy, Duration::ZERO), Some(second));
        s.turn(second, sync);
        assert_eq!(s.w.stats().batches, 2, "floor {floor:?}, sync {sync:?}");
        assert!(s.ids[1..].iter().all(|id| s.covered[id] == second));
    }
}

#[test]
fn under_load_batch_syncs_start_the_floor_apart_or_as_the_previous_one_returns() {
    let gaps = |s: &Sim| {
        s.starts[5..]
            .windows(2)
            .map(|p| p[1] - p[0])
            .collect::<Vec<_>>()
    };
    let at_floor = saturated(Setup::base(), 40, |_| FAST);
    assert!(
        gaps(&at_floor).iter().all(|g| *g == FLOOR + 50 * US),
        "a timed wake 50 µs late: {:?}",
        gaps(&at_floor)
    );
    let slow = saturated(Setup::base(), 40, |_| 3 * MS);
    assert!(
        gaps(&slow).iter().all(|g| *g == 3 * MS),
        "{:?}",
        gaps(&slow)
    );
}

#[test]
fn a_zero_floor_starts_each_batch_sync_under_load_as_the_previous_one_returns() {
    for (floor, ceiling) in [(Duration::ZERO, CEILING), (FLOOR, Duration::ZERO)] {
        let s = saturated(
            Setup {
                floor,
                ceiling,
                ..Setup::base()
            },
            40,
            |_| FAST,
        );
        assert!(s.waited.iter().all(|w| *w));
        assert!(
            s.starts.windows(2).all(|p| p[1] - p[0] == FAST),
            "{floor:?} under {ceiling:?}"
        );
        assert_eq!(s.w.stats().sync_interval, Duration::ZERO);
    }
}

#[test]
fn a_lone_writer_waits_for_no_timer() {
    for turnaround in [Duration::ZERO, 10 * US] {
        let s = lone_writer(Setup::base(), 50, FAST, turnaround);
        assert_eq!(s.starts.len(), 50);
        for (k, id) in s.ids.iter().enumerate() {
            let arrival = START + (FAST + turnaround) * k as u32;
            assert_eq!(s.covered[id], arrival, "call {k} waited");
            assert_eq!(s.acked[id], arrival + FAST, "call {k}");
        }
        assert!(
            s.waited.iter().all(|w| !w),
            "nothing waits as a lone writer's sync returns"
        );
    }
}

#[test]
fn the_floor_does_not_apply_after_a_batch_sync_that_returned_with_nothing_waiting() {
    let mut s = Sim::new(Setup::base());
    s.push();
    s.turn(START, FAST);
    let at = START + FAST + 10 * US;
    assert!(at < START + FLOOR);
    s.arrive(at);
    s.turn(at, FAST);
    assert_eq!(s.starts, [START, at]);
}

#[test]
fn calls_arriving_after_a_sync_returned_with_others_waiting_join_them_at_the_floor() {
    let mut s = Sim::new(Setup::base());
    s.push();
    for t in [50 * US, 100 * US, 500 * US, MS] {
        s.arrive(START + t);
    }
    let (next, _) = s.turn(START, FAST);
    assert_eq!(next, Some(START + FLOOR));
    s.turn(START + 500 * US, FAST);
    assert_eq!(s.starts, [START], "not before the floor");
    s.turn(START + FLOOR, FAST);
    assert_eq!(s.starts, [START, START + FLOOR]);
    assert!(s.ids[1..].iter().all(|id| s.covered[id] == START + FLOOR));
}

#[test]
fn a_floor_above_the_ceiling_spaces_batch_syncs_at_the_ceiling() {
    let s = saturated(
        Setup {
            floor: 20 * MS,
            ..Setup::base()
        },
        20,
        |_| FAST,
    );
    assert_eq!(s.w.stats().sync_interval, CEILING);
    assert!(
        s.starts[1..]
            .windows(2)
            .all(|p| p[1] - p[0] == CEILING + 50 * US)
    );
}

#[test]
fn calls_further_apart_than_a_sync_are_each_synced_at_once_clocked_or_not() {
    for setup in [Setup::base(), Setup::unclocked()] {
        for (period, sync) in [(5 * MS, FAST), (10 * MS, 2 * MS), (MS, 900 * US)] {
            let (_, waits) = periodic(setup, 200, period, sync);
            assert!(
                waits.iter().all(|w| *w == WAKE),
                "one call every {period:?}: {:?}",
                waits.iter().max()
            );
        }
    }
}

#[test]
fn late_wakes_and_stalled_turns_delay_no_later_call() {
    for stall in [3 * MS, 8 * MS, 30 * MS] {
        let calls = (0..300u32)
            .map(|k| {
                let wake = match k {
                    20 => stall,
                    k if k % 50 == 1 => 300 * US,
                    _ => WAKE,
                };
                (START + CEILING * k, wake)
            })
            .collect::<Vec<_>>();
        let h = History {
            calls: calls.clone(),
            late: 100 * US,
            syncs: vec![2 * MS],
        };
        let (_, out) = run(Setup::base(), &h);
        let waits: Vec<_> = out
            .covered
            .iter()
            .zip(&calls)
            .map(|(c, a)| *c - a.0)
            .collect();
        assert!(
            waits[25..].iter().all(|w| *w <= 300 * US),
            "after a {stall:?} stall: {:?}",
            waits[25..].iter().max()
        );
    }
}

#[test]
fn a_timed_seal_delays_only_a_call_arriving_during_its_sync_and_by_no_more_than_it() {
    let seal_sync = 2 * MS;
    for phase in [3 * MS, 7 * MS, 9 * MS + 900 * US] {
        let seal = Duration::from_secs(2) + phase;
        let mut s = Sim::new(Setup {
            seal,
            ..Setup::base()
        });
        let calls = (0..300u32)
            .map(|k| (START + CEILING * k, WAKE))
            .collect::<Vec<_>>();
        let h = History {
            calls: calls.clone(),
            late: Duration::ZERO,
            syncs: vec![seal_sync],
        };
        let out = simulate(&mut s, &h);
        let stats = s.w.stats();
        assert_eq!(stats.syncs, stats.batches + 1, "one seal at {phase:?}");
        // A call that arrives during the seal's sync joins the turn that ran it.
        let late: Vec<_> = out
            .acked
            .iter()
            .zip(&calls)
            .map(|(k, a)| *k - a.0 - seal_sync)
            .filter(|w| *w > WAKE)
            .collect();
        assert!(late.len() <= 1, "a seal {phase:?} in: {late:?}");
        assert!(late.iter().all(|w| *w <= seal_sync), "{late:?}");
    }
}

#[test]
fn a_device_slower_than_the_floor_syncs_back_to_back_under_load() {
    let s = saturated(Setup::base(), 30, |_| 20 * MS);
    assert!(s.starts.windows(2).all(|p| p[1] - p[0] == 20 * MS));
}

#[test]
fn after_a_stall_the_next_batch_starts_as_it_returns_and_the_floor_holds_at_once() {
    let s = saturated(Setup::base(), 60, |k| if k == 40 { 25 * MS } else { FAST });
    assert_eq!(s.starts[41] - s.starts[40], 25 * MS);
    assert_eq!(s.starts[42] - s.starts[41], FLOOR + 50 * US);
    assert_eq!(s.w.stats().sync_time.map(|t| t > FAST), Some(true));
}

#[test]
fn a_failed_sync_leaves_the_estimate_alone() {
    let mut s = saturated(Setup::base(), 30, |_| FAST);
    assert_eq!(s.w.stats().sync_time, Some(FAST));
    s.fs.fail_nth(FaultOp::Sync, ".open", 0, io::ErrorKind::Other);
    s.push();
    let at = s.clock.monotonic() + CEILING;
    s.turn(at, 25 * MS);
    assert!(s.w.fault().is_some(), "the sync failed");
    assert_eq!(s.w.stats().sync_time, Some(FAST));
}

#[test]
fn only_the_batch_sync_is_timed_not_the_batch_write_or_the_mark_update() {
    let mut s = Sim::new(Setup::base());
    s.write = 25 * MS;
    let mut at = START;
    for _ in 0..10 {
        s.push();
        let (_, busy) = s.turn(at, FAST);
        assert!(
            busy >= 50 * MS + FAST,
            "the batch write and the mark update were timed: {busy:?}"
        );
        at += Duration::from_secs(1);
    }
    assert_eq!(s.w.stats().sync_time, Some(FAST));
}

#[test]
fn a_mark_sync_leaves_the_estimate_alone() {
    for mark in [NO_MARK, MarkSync::EveryCommit] {
        let mut s = Sim::new(Setup {
            mark,
            ..Setup::base()
        });
        let mut at = START;
        for k in 0..20 {
            if k % 2 == 1 || mark == MarkSync::EveryCommit {
                s.push();
            } else {
                // An alias is not a fact: its batch forces a mark sync.
                s.push_items(vec![Item::from(Entry::Alias {
                    id: k,
                    name: format!("a{k}"),
                })]);
            }
            s.turn_with(at, FAST, 25 * MS);
            at += Duration::from_secs(1);
        }
        assert!(s.w.stats().mark_syncs >= 10, "{mark:?}");
        assert_eq!(s.w.stats().sync_time, Some(FAST), "{mark:?}");
    }
}

#[test]
fn a_seal_sync_neither_spaces_the_next_batch_nor_moves_the_estimate() {
    let mut s = Sim::new(Setup {
        seal: Duration::from_secs(2),
        ..Setup::base()
    });
    let mut at = START;
    while s.starts.len() < 30 {
        s.push();
        s.turn(at, FAST);
        at += 5 * MS;
    }
    let (seal_at, _) = s.turn(at, FAST);
    let seal_at = seal_at.unwrap();
    assert_eq!(seal_at, START + Duration::from_secs(2));
    let syncs = s.w.stats().syncs;
    s.turn(seal_at, FAST);
    assert_eq!(
        s.w.stats().syncs,
        syncs + 1,
        "the seal synced the next segment"
    );
    assert_eq!(s.w.stats().sync_time, Some(FAST));
    let after = seal_at + FAST + 10 * US;
    s.arrive(after);
    s.turn(after, FAST);
    assert_eq!(s.starts.last(), Some(&after));
}

#[test]
fn after_an_open_the_first_call_is_synced_at_once_and_no_sync_time_is_known() {
    let s = saturated(Setup::base(), 20, |_| FAST);
    let at = s.clock.monotonic() + Duration::from_secs(1);
    let mut s = s.reopen(at);
    assert_eq!(s.w.stats().sync_interval, FLOOR);
    assert_eq!(s.w.stats().sync_time, None);
    s.push();
    s.turn(at, FAST);
    assert_eq!(s.starts, [at]);
    assert_eq!(s.w.stats().sync_time, Some(FAST));
}

#[test]
fn log_start_times_batch_syncs_on_its_clock() {
    let fs = MemFs::new();
    let new = NewStore {
        store_id: [7; 16],
        created_at: T0,
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: "kvik test".into(),
    };
    let store = StoreDir::open(Arc::new(fs.clone()), &PathBuf::from("store"), Some(&new)).unwrap();
    let clock = Arc::new(ManualClock::default());
    // Creation syncs each segment once; a later sync of the same segment is a batch's.
    let seen = Arc::new(Mutex::new(BTreeMap::<PathBuf, u32>::new()));
    let (c, seen2) = (clock.clone(), seen.clone());
    fs.set_sync_hook(Some(Arc::new(move |p: &Path| {
        let mut seen = lock(&seen2);
        let n = seen.entry(p.to_path_buf()).or_insert(0);
        *n += 1;
        if *n >= 2 && p.extension().is_some_and(|e| e == "open") {
            *lock(&c.0) += 500 * US;
        }
    })));
    let w = LogWriter::open(
        store,
        Setup::base().config(),
        Some([9; 16]),
        Duration::ZERO,
        T0,
    )
    .unwrap();
    let log = Log::start(w, clock).unwrap();
    log.submit(vec![fact(T0)]).unwrap().wait().unwrap();
    let stats = log.stats();
    assert_eq!(stats.sync_time, Some(500 * US));
    assert_eq!(stats.sync_interval, FLOOR);
    log.shutdown().unwrap();
}

#[test]
fn the_default_floor_is_2_ms_under_a_10_ms_ceiling() {
    let cfg = WriterConfig::default();
    assert_eq!(cfg.sync_interval_floor, FLOOR);
    assert_eq!(cfg.min_sync_interval, CEILING);
}

#[test]
fn a_spacing_near_the_largest_duration_schedules_without_overflow() {
    let top = Duration::from_nanos(u64::MAX);
    let s = saturated(
        Setup {
            floor: Duration::MAX,
            ceiling: Duration::MAX,
            ..Setup::base()
        },
        1,
        |_| FAST,
    );
    assert_eq!(s.w.stats().sync_interval, top);
    assert_eq!(s.w.sync_ready_at(), START + top);
    let mut s = Sim::new(Setup {
        ceiling: Duration::MAX,
        ..Setup::base()
    });
    assert_eq!(s.w.stats().sync_interval, FLOOR);
    s.push();
    s.turn(START, FAST);
    s.arrive(START + MS);
    s.turn(START + MS, FAST);
    assert_eq!(s.starts, [START, START + MS]);
}
