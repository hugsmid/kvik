//! The adaptive batch-sync interval (spec §3 Group commit, Q-310, Q-712): the synchronous writer core driven with
//! explicit times, as in `cadence.rs`, and a manual clock that each sync advances by a scripted duration from the
//! file system's sync hook, against a reference schedule written out on its own.

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
use kvik_log::store::{NewStore, StoreDir};
use kvik_log::writer::{Log, LogWriter, MarkSync, WriterConfig};
use proptest::prelude::*;

proptest! {
    #[test]
    fn the_writer_agrees_with_the_reference_schedule((floor, ceiling) in arb_bounds(), h in arb_history()) {
        let setup = Setup { floor, ceiling, ..Setup::adaptive() };
        let (_, got) = run(setup, &h);
        prop_assert_eq!(got, reference(&h, floor, ceiling, true));
    }

    #[test]
    fn any_n_plus_1_batch_syncs_span_at_least_n_floors_less_one_ceiling(
        (floor, ceiling) in arb_bounds(),
        h in arb_history(),
    ) {
        let (_, got) = run(Setup { floor, ceiling, ..Setup::adaptive() }, &h);
        let st = &got.starts;
        for i in 0..st.len() {
            for j in i + 1..st.len() {
                let bound = (floor.min(ceiling) * (j - i) as u32).saturating_sub(ceiling);
                prop_assert!(st[j] - st[i] >= bound, "syncs {} to {} span {:?}", i, j, st[j] - st[i]);
            }
        }
    }

    #[test]
    fn no_batch_sync_starts_within_half_the_floor_of_a_log_sync_a_seal_included(
        (floor, ceiling) in arb_bounds(),
        seal_ms in 30u64..400,
        h in arb_history(),
    ) {
        let seal = MS * seal_ms as u32;
        let (s, _) = run(Setup { floor, ceiling, seal, ..Setup::adaptive() }, &h);
        prop_assert!(s.w.stats().sealed_segments == 0, "seal jobs are not run here");
        for pair in s.log_syncs.windows(2) {
            if s.starts.contains(&pair[1]) {
                prop_assert!(pair[1] - pair[0] >= floor.min(ceiling) / 2, "log syncs at {:?}", pair);
            }
        }
    }

    #[test]
    fn a_queued_batch_waits_at_most_a_ceiling_after_the_last_batch_sync_or_half_one_after_the_last_log_sync(
        (floor, ceiling) in arb_bounds(),
        seal_ms in 30u64..400,
        h in arb_history(),
    ) {
        let seal = MS * seal_ms as u32;
        let (s, _) = run(Setup { floor, ceiling, seal, ..Setup::adaptive() }, &h);
        prop_assert!(s.ceiling_breaks.is_empty(), "{:?}", s.ceiling_breaks);
    }

    #[test]
    fn with_the_floor_at_the_ceiling_or_no_clock_the_schedule_is_the_fixed_one(
        ceiling_ms in 0u64..=20,
        above in 0u64..20,
        below in 0u64..20,
        h in arb_history(),
    ) {
        let ceiling = MS * ceiling_ms as u32;
        let fixed = reference(&h, ceiling, ceiling, false);
        let at_or_above = Setup { floor: ceiling + MS * above as u32, ceiling, ..Setup::adaptive() };
        prop_assert_eq!(&run(at_or_above, &h).1, &fixed);
        let no_clock = Setup { floor: MS * below as u32, ceiling, ..Setup::fixed() };
        prop_assert_eq!(&run(no_clock, &h).1, &fixed);
    }

    #[test]
    fn the_smoothed_sync_time_stays_between_the_last_sample_and_the_largest(
        syncs in prop::collection::vec(arb_sync(), 1..60),
    ) {
        let mut s = Sim::new(Setup::adaptive());
        let mut at = START;
        let mut largest = Duration::ZERO;
        for (k, &d) in syncs.iter().enumerate() {
            s.push();
            s.turn(at, d);
            largest = largest.max(d);
            let est = s.w.stats().sync_time.unwrap();
            prop_assert!(d <= est && est <= largest, "after sync {}: {:?} of {:?}", k, est, d);
            let i = s.w.stats().sync_interval;
            prop_assert_eq!(i, s.w.sync_interval());
            prop_assert!(FLOOR <= i && i <= CEILING, "interval {:?}", i);
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
        let mut s = Sim::new(Setup::adaptive());
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

#[test]
fn a_fast_device_at_one_call_per_5_ms_syncs_every_call_at_once() {
    let (s, waits) = periodic(Setup::adaptive(), 200, 5 * MS, FAST);
    assert!(waits.iter().all(|w| *w <= WAKE), "{:?}", waits.iter().max());
    assert_eq!(s.w.stats().sync_interval, FLOOR);
    assert_eq!(s.w.stats().sync_time, Some(FAST));
    let (_, waits) = periodic(Setup::fixed(), 200, 5 * MS, FAST);
    assert!(
        waits.iter().filter(|w| **w > WAKE).count() >= 90,
        "the fixed interval delays every other call"
    );
}

#[test]
fn a_20_ms_device_runs_the_fixed_schedule() {
    let slow = 20 * MS;
    for period in [3 * MS, 10 * MS, 25 * MS] {
        let (a, wa) = periodic(Setup::adaptive(), 150, period, slow);
        let (f, wf) = periodic(Setup::fixed(), 150, period, slow);
        assert_eq!(
            (&a.starts, &wa),
            (&f.starts, &wf),
            "one call every {period:?}"
        );
        assert!(a.intervals.iter().all(|i| *i == CEILING));
    }
}

#[test]
fn a_device_of_half_a_millisecond_runs_at_eight_times_its_sync_time() {
    let s = saturated(Setup::adaptive(), 50, |_| 500 * US);
    assert_eq!(s.w.stats().sync_interval, 4 * MS);
    let gaps: Vec<_> = s.starts[10..].windows(2).map(|p| p[1] - p[0]).collect();
    assert!(
        gaps.iter().all(|g| *g >= 4 * MS && *g < 4 * MS + 100 * US),
        "{gaps:?}"
    );
}

#[test]
fn a_single_2_ms_sync_holds_the_ceiling_for_four_syncs() {
    let s = saturated(
        Setup::adaptive(),
        80,
        |k| if k == 40 { 2 * MS } else { FAST },
    );
    assert_eq!(s.intervals[39], FLOOR);
    assert_eq!(&s.intervals[40..44], &[CEILING; 4]);
    assert!(s.intervals[44] < CEILING && s.intervals[44] > FLOOR);
    let back = s.intervals[40..].iter().position(|i| *i == FLOOR).unwrap();
    assert_eq!(back, 23);
}

#[test]
fn a_single_25_ms_stall_holds_the_ceiling_for_24_syncs_and_the_floor_returns_after_43() {
    let s = saturated(
        Setup::adaptive(),
        120,
        |k| if k == 40 { 25 * MS } else { FAST },
    );
    assert_eq!(s.intervals[39], FLOOR);
    let at_ceiling = s.intervals[40..]
        .iter()
        .take_while(|i| **i == CEILING)
        .count();
    assert_eq!(at_ceiling, 24);
    let back = s.intervals[40..].iter().position(|i| *i == FLOOR).unwrap();
    assert_eq!(back, 43);
    assert_eq!(
        s.starts[41] - s.starts[40],
        25 * MS,
        "the next batch starts as the stall returns"
    );
    for (k, p) in s.starts[41..40 + at_ceiling].windows(2).enumerate() {
        assert!(
            p[1] - p[0] >= CEILING / 2,
            "syncs {} and {} after the stall",
            k + 1,
            k + 2
        );
    }
    let held = &s.starts[41..=40 + at_ceiling];
    for i in 0..held.len() {
        for j in i + 1..held.len() {
            assert!(
                held[j] - held[i] >= CEILING * (j - i - 1) as u32,
                "the fixed schedule's rate bound"
            );
        }
    }
}

#[test]
fn a_failed_sync_leaves_the_estimate_alone() {
    let mut s = saturated(Setup::adaptive(), 30, |_| FAST);
    assert_eq!(s.w.stats().sync_time, Some(FAST));
    s.fs.fail_nth(FaultOp::Sync, ".open", 0, io::ErrorKind::Other);
    s.push();
    let at = *s.starts.last().unwrap() + CEILING;
    s.turn(at, 25 * MS);
    assert!(s.w.fault().is_some(), "the sync failed");
    assert_eq!(s.w.stats().sync_time, Some(FAST));
    assert_eq!(s.w.sync_interval(), FLOOR);
}

#[test]
fn only_the_batch_sync_is_timed_not_the_batch_write_or_the_mark_update() {
    let mut s = Sim::new(Setup::adaptive());
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
    assert_eq!(s.w.stats().sync_interval, FLOOR);
}

#[test]
fn a_mark_sync_leaves_the_estimate_alone() {
    for mark in [NO_MARK, MarkSync::EveryCommit] {
        let mut s = Sim::new(Setup {
            mark,
            ..Setup::adaptive()
        });
        let mut at = START;
        for k in 0..20 {
            if k % 2 == 1 || mark == MarkSync::EveryCommit {
                s.push();
            } else {
                // An alias is not a fact: its batch forces a mark sync.
                s.push_items(vec![Item::Entry(Entry::Alias {
                    id: k,
                    name: format!("a{k}"),
                })]);
            }
            s.turn_with(at, FAST, 25 * MS);
            at += Duration::from_secs(1);
        }
        assert!(s.w.stats().mark_syncs >= 10, "{mark:?}");
        assert_eq!(s.w.stats().sync_time, Some(FAST), "{mark:?}");
        assert_eq!(s.w.stats().sync_interval, FLOOR, "{mark:?}");
    }
}

#[test]
fn a_seal_sync_spaces_the_next_batch_by_half_the_interval_and_leaves_the_estimate_alone() {
    let mut s = Sim::new(Setup {
        seal: Duration::from_secs(2),
        ..Setup::adaptive()
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
    s.turn(seal_at, 25 * MS);
    assert_eq!(
        s.w.stats().syncs,
        syncs + 1,
        "the seal synced the next segment"
    );
    assert_eq!(s.w.stats().sync_time, Some(FAST));
    s.push();
    let (next, _) = s.turn(seal_at + 100 * US, FAST);
    assert_eq!(next, Some(seal_at + FLOOR / 2));
}

#[test]
fn the_interval_is_the_ceiling_after_an_open_until_the_first_batch_sync() {
    let s = saturated(Setup::adaptive(), 20, |_| FAST);
    assert_eq!(s.w.stats().sync_interval, FLOOR);
    let at = *s.starts.last().unwrap() + Duration::from_secs(1);
    let mut s = s.reopen(at);
    assert_eq!(s.w.sync_interval(), CEILING);
    assert_eq!(s.w.stats().sync_interval, CEILING);
    assert_eq!(s.w.stats().sync_time, None);
    s.push();
    s.turn(at, FAST);
    assert_eq!(s.w.stats().sync_interval, FLOOR);
}

#[test]
fn a_floor_above_the_ceiling_fixes_the_interval_at_the_ceiling() {
    let s = saturated(
        Setup {
            floor: 20 * MS,
            ..Setup::adaptive()
        },
        20,
        |_| FAST,
    );
    assert!(s.intervals.iter().all(|i| *i == CEILING));
    assert_eq!(s.w.stats().sync_time, Some(FAST));
}

#[test]
fn a_zero_ceiling_disables_the_bound_whatever_the_floor() {
    let s = saturated(
        Setup {
            ceiling: Duration::ZERO,
            ..Setup::adaptive()
        },
        20,
        |_| FAST,
    );
    assert!(s.intervals.iter().all(|i| i.is_zero()));
    assert!(
        s.starts.windows(2).all(|p| p[1] - p[0] == FAST),
        "each sync starts as the last returns"
    );
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
        Setup::adaptive().config(),
        Some([9; 16]),
        Duration::ZERO,
        T0,
    )
    .unwrap();
    let log = Log::start(w, clock).unwrap();
    log.submit(vec![fact(T0)]).unwrap().wait().unwrap();
    let stats = log.stats();
    assert_eq!(stats.sync_time, Some(500 * US));
    assert_eq!(stats.sync_interval, 4 * MS);
    log.shutdown().unwrap();
}

#[test]
fn the_default_floor_is_2_ms_under_a_10_ms_ceiling() {
    let cfg = WriterConfig::default();
    assert_eq!(cfg.sync_interval_floor, FLOOR);
    assert_eq!(cfg.min_sync_interval, CEILING);
}

#[test]
fn a_ceiling_near_the_largest_duration_schedules_without_overflow() {
    let run = |clock| {
        let mut s = Sim::new(Setup {
            ceiling: Duration::MAX,
            clock,
            ..Setup::adaptive()
        });
        s.push();
        s.turn(START, FAST);
        s.push();
        s.turn(START + MS, FAST);
        s
    };
    let fixed = run(false);
    assert_eq!(fixed.starts, [START]);
    assert_eq!(fixed.w.sync_ready_at(), Duration::from_nanos(u64::MAX));
    let adaptive = run(true);
    assert_eq!(adaptive.starts, [START, START + MS]);
    assert_eq!(adaptive.w.sync_interval(), FLOOR);
}
