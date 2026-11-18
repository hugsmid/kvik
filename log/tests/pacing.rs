//! Batch-sync pacing (spec §3 Group commit, Q-980 to Q-985): the synchronous writer core driven with explicit times,
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
use kvik_log::writer::{Clock, Log, LogWriter, MarkSync, SYNC_BURST, SystemClock, WriterConfig};
use proptest::prelude::*;

const BURST: usize = SYNC_BURST as usize;

/// The batch sync each call's covering start names, by index into `starts`.
fn batch_of(s: &Schedule, call: usize) -> usize {
    s.starts
        .iter()
        .rposition(|t| *t == s.covered[call])
        .unwrap()
}

/// Whether any `count` consecutive batch syncs of `starts` span less than `count - BURST` intervals.
fn over_budget(starts: &[Duration], interval: Duration) -> Option<(usize, usize)> {
    for i in 0..starts.len() {
        for j in i + BURST..starts.len() {
            if starts[j] - starts[i] < interval * (j + 1 - i - BURST) as u32 {
                return Some((i, j));
            }
        }
    }
    None
}

/// A sync time, and a floor and ceiling with the floor at most the ceiling and the ceiling above zero.
fn arb_device() -> impl Strategy<Value = (Duration, Duration, Duration)> {
    (arb_sync(), 0u64..=20, 1u64..=20)
        .prop_map(|(d, f, c)| (d, US * (500 * f.min(2 * c)) as u32, MS * c as u32))
}

proptest! {
    #![proptest_config(ProptestConfig { max_shrink_time: 20_000, ..ProptestConfig::default() })]

    #[test]
    fn the_writer_agrees_with_the_reference_schedule((floor, ceiling) in arb_bounds(), h in arb_history()) {
        let setup = Setup { floor, ceiling, ..Setup::base() };
        let (_, got) = run(setup, &h);
        prop_assert_eq!(got, reference(&h, setup, true));
    }

    #[test]
    fn without_a_clock_the_writer_agrees_with_the_reference_at_the_ceiling((floor, ceiling) in arb_bounds(), h in arb_history()) {
        let setup = Setup { floor, ceiling, ..Setup::unclocked() };
        let (_, got) = run(setup, &h);
        prop_assert_eq!(got, reference(&h, setup, false));
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
    fn batch_syncs_in_any_window_stay_within_the_least_interval_and_the_burst(
        (floor, ceiling) in arb_bounds(),
        h in arb_history(),
    ) {
        let setup = Setup { floor, ceiling, ..Setup::base() };
        let (s, _) = run(setup, &h);
        prop_assert_eq!(over_budget(&s.starts, setup.least_interval()), None);
    }

    #[test]
    fn on_a_steady_device_batch_syncs_in_any_window_stay_within_its_interval_and_the_burst(
        (d, floor, ceiling) in arb_device(),
        h in arb_history(),
    ) {
        let setup = Setup { floor, ceiling, ..Setup::base() };
        let h = History { syncs: vec![d], ..h };
        let (s, _) = run(setup, &h);
        prop_assert_eq!(over_budget(&s.starts, setup.interval(Some(d))), None);
    }

    #[test]
    fn a_lone_writer_under_budget_is_never_delayed(
        (d, floor, ceiling) in arb_device(),
        extra in 0u64..3_000,
        n in 1usize..60,
    ) {
        let setup = Setup { floor, ceiling, ..Setup::base() };
        let turnaround = setup.interval(Some(d)).saturating_sub(d) + US * extra as u32;
        let s = lone_writer(setup, n, d, turnaround);
        let mut at = START;
        for (k, id) in s.ids.iter().enumerate() {
            prop_assert_eq!(s.covered[id], at, "call {} waited", k);
            at = s.acked[id] + turnaround;
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
            let i = s.w.stats().sync_interval;
            prop_assert_eq!(i, s.w.sync_interval());
            prop_assert_eq!(i, Setup::base().interval(Some(est)));
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
        assert!(s.turns.len() < 1_000, "the calls do not drain");
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

/// Batch syncs a second for `per_sec` evenly staggered calls of one reading, none waiting for another, each batch
/// sync taking `sync`, over one second.
fn staggered(per_sec: u32, sync: Duration) -> (usize, Vec<Duration>) {
    let period = Duration::from_secs(1) / per_sec;
    let (s, waits) = periodic(Setup::base(), per_sec as usize, period, sync);
    (s.starts.len(), waits)
}

#[test]
fn staggered_callers_get_a_sync_each_under_budget_and_share_syncs_at_its_rate_above_it() {
    let (syncs, waits) = staggered(400, 130 * US);
    assert_eq!(syncs, 400, "under 1/floor every call is synced on its own");
    assert!(waits.iter().all(|w| *w == WAKE));
    for (per_sec, sync, budget) in [
        (1_000, 130 * US, 500),
        (4_000, 130 * US, 500),
        (10_000, 130 * US, 500),
        (800, 1_100 * US, 114),
        (2_000, 1_100 * US, 114),
        (150, 5 * MS, 100),
        (1_000, 5 * MS, 100),
    ] {
        let (syncs, waits) = staggered(per_sec, sync);
        assert!(
            syncs <= budget + BURST && syncs + 2 >= budget,
            "{per_sec} calls/s, sync {sync:?}: {syncs} batch syncs"
        );
        assert!(
            waits
                .iter()
                .all(|w| *w <= WAKE + Setup::base().interval(Some(sync))),
            "{per_sec} calls/s, sync {sync:?}: {:?}",
            waits.iter().max()
        );
    }
}

#[test]
fn slow_devices_keep_the_ceiling_of_the_fixed_schedule() {
    for d in [1_250 * US, 5 * MS, 10 * MS] {
        let s = saturated(Setup::base(), 40, |_| d);
        assert_eq!(s.w.sync_interval(), CEILING, "sync {d:?}");
        let gaps: Vec<_> = s.starts[BURST + 2..]
            .windows(2)
            .map(|p| p[1] - p[0])
            .collect();
        assert!(
            gaps.iter()
                .all(|g| *g >= CEILING && *g <= CEILING + 50 * US),
            "sync {d:?}: {gaps:?}"
        );
    }
}

#[test]
fn the_calls_that_arrive_during_a_sync_form_one_batch_as_it_returns_while_the_budget_holds() {
    for (floor, sync) in [(Duration::ZERO, MS), (FLOOR, MS), (FLOOR, 3 * MS)] {
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
        let second = START + sync;
        assert_eq!(timed(next, START + busy, Duration::ZERO), Some(second));
        s.turn(second, sync);
        assert_eq!(s.w.stats().batches, 2, "floor {floor:?}, sync {sync:?}");
        assert!(s.ids[1..].iter().all(|id| s.covered[id] == second));
    }
}

#[test]
fn a_spent_budget_holds_arrivals_for_the_next_refill_and_they_form_one_batch() {
    let mut s = Sim::new(Setup::base());
    let mut at = START;
    for _ in 0..BURST {
        s.arrive(at);
        let (_, busy) = s.turn(at, FAST);
        at += busy;
    }
    assert_eq!(s.starts.len(), BURST);
    let interval = s.w.sync_interval();
    assert_eq!(interval, FLOOR);
    let ready = s.w.sync_ready_at();
    assert!(ready > at && ready <= START + interval, "{ready:?}");
    for k in 0..5u32 {
        s.arrive(at + 100 * US * k);
        let (next, _) = s.turn(at + 100 * US * k, FAST);
        assert_eq!(next, Some(ready));
    }
    s.turn(ready, FAST);
    assert_eq!(s.starts.len(), BURST + 1);
    assert!(s.ids[BURST..].iter().all(|id| s.covered[id] == ready));
}

#[test]
fn a_lone_writer_above_budget_runs_at_one_sync_per_interval_after_the_burst() {
    let s = lone_writer(Setup::base(), 40, FAST, Duration::ZERO);
    for (k, id) in s.ids.iter().take(BURST).enumerate() {
        assert_eq!(
            s.covered[id],
            START + FAST * k as u32,
            "call {k} of the burst"
        );
    }
    let gaps: Vec<_> = s.starts[BURST + 1..]
        .windows(2)
        .map(|p| p[1] - p[0])
        .collect();
    assert!(gaps.iter().all(|g| *g == FLOOR), "{gaps:?}");
}

#[test]
fn under_load_batch_syncs_run_at_the_interval_or_as_the_previous_one_returns() {
    let gaps = |s: &Sim| {
        s.starts[BURST + 2..]
            .windows(2)
            .map(|p| p[1] - p[0])
            .collect::<Vec<_>>()
    };
    let fast = saturated(Setup::base(), 40, |_| FAST);
    assert!(
        gaps(&fast).iter().all(|g| *g == FLOOR),
        "a timed wake 50 µs late is caught up from the budget: {:?}",
        gaps(&fast)
    );
    let zero_floor = saturated(
        Setup {
            floor: Duration::ZERO,
            ..Setup::base()
        },
        40,
        |_| FAST,
    );
    assert_eq!(zero_floor.w.sync_interval(), 8 * FAST);
    assert!(gaps(&zero_floor).iter().all(|g| *g == 8 * FAST));
    let slow = saturated(Setup::base(), 40, |_| 20 * MS);
    assert!(
        gaps(&slow).iter().all(|g| *g == 20 * MS),
        "{:?}",
        gaps(&slow)
    );
}

#[test]
fn a_zero_ceiling_starts_each_batch_sync_under_load_as_the_previous_one_returns() {
    for floor in [Duration::ZERO, FLOOR] {
        let s = saturated(
            Setup {
                floor,
                ceiling: Duration::ZERO,
                ..Setup::base()
            },
            40,
            |_| FAST,
        );
        assert!(
            s.starts.windows(2).all(|p| p[1] - p[0] == FAST),
            "{floor:?}"
        );
        assert_eq!(s.w.stats().sync_interval, Duration::ZERO);
    }
}

#[test]
fn a_floor_above_the_ceiling_fixes_the_interval_at_the_ceiling() {
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
        s.starts[BURST + 1..]
            .windows(2)
            .all(|p| p[1] - p[0] == CEILING)
    );
}

#[test]
fn calls_further_apart_than_the_interval_are_each_synced_at_once_clocked_or_not() {
    for setup in [Setup::base(), Setup::unclocked()] {
        for (period, sync) in [(10 * MS, FAST), (10 * MS, 2 * MS), (25 * MS, 20 * MS)] {
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
fn a_25_ms_stall_holds_the_ceiling_until_the_estimate_falls_and_the_floor_returns_within_45_syncs()
{
    let s = saturated(Setup::base(), 120, |k| if k == 40 { 25 * MS } else { FAST });
    assert_eq!(
        s.starts[41] - s.starts[40],
        25 * MS,
        "the next batch starts as the stall returns"
    );
    assert_eq!(over_budget(&s.starts, FLOOR), None);
    assert!(
        s.starts[42..]
            .windows(2)
            .take(20)
            .all(|p| p[1] - p[0] > FLOOR)
    );
    let back = s.starts[41..]
        .windows(2)
        .position(|p| p[1] - p[0] == FLOOR)
        .unwrap();
    assert!(back <= 45, "{back}");
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
fn a_seal_sync_spends_nothing_of_the_budget_and_leaves_the_estimate_alone() {
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
    let mut t = seal_at + FAST + 10 * US;
    for k in 0..BURST {
        s.arrive(t);
        s.turn(t, FAST);
        assert_eq!(s.starts.last(), Some(&t), "sync {k} after the seal");
        t += FAST;
    }
}

#[test]
fn after_an_open_the_first_call_is_synced_at_once_and_the_interval_is_the_ceiling_until_a_sample() {
    let s = saturated(Setup::base(), 20, |_| FAST);
    let at = s.clock.monotonic() + Duration::from_secs(1);
    let mut s = s.reopen(at);
    assert_eq!(s.w.stats().sync_interval, CEILING);
    assert_eq!(s.w.stats().sync_time, None);
    s.push();
    s.turn(at, FAST);
    assert_eq!(s.starts, [at]);
    assert_eq!(s.w.stats().sync_time, Some(FAST));
    assert_eq!(s.w.stats().sync_interval, FLOOR);
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
fn an_interval_near_the_largest_duration_schedules_without_overflow() {
    let top = Duration::from_nanos(u64::MAX);
    let s = saturated(
        Setup {
            floor: Duration::MAX,
            ceiling: Duration::MAX,
            ..Setup::base()
        },
        BURST,
        |_| FAST,
    );
    assert_eq!(s.w.stats().sync_interval, top);
    assert!(s.w.sync_ready_at() > START + top / 2);
    let mut s = Sim::new(Setup {
        ceiling: Duration::MAX,
        ..Setup::base()
    });
    assert_eq!(s.w.stats().sync_interval, top);
    s.push();
    s.turn(START, FAST);
    assert_eq!(s.w.stats().sync_interval, FLOOR);
    s.arrive(START + MS);
    s.turn(START + MS, FAST);
    assert_eq!(s.starts, [START, START + MS]);
}

#[test]
fn the_writer_thread_syncs_a_lone_writer_at_once_within_the_budget_and_holds_the_next_for_its_refill()
 {
    let fs = MemFs::new();
    let new = NewStore {
        store_id: [7; 16],
        created_at: T0,
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: "kvik test".into(),
    };
    let store = StoreDir::open(Arc::new(fs.clone()), &PathBuf::from("store"), Some(&new)).unwrap();
    let interval = Duration::from_millis(400);
    let cfg = Setup {
        floor: interval,
        ceiling: interval,
        ..Setup::base()
    }
    .config();
    let clock = Arc::new(SystemClock::default());
    let w = LogWriter::open(store, cfg, Some([9; 16]), clock.monotonic(), T0).unwrap();
    let log = Log::start(w, clock).unwrap();
    let waits: Vec<_> = (0..=BURST)
        .map(|k| {
            let t = std::time::Instant::now();
            log.submit(vec![fact(T0 + k as i64)])
                .unwrap()
                .wait()
                .unwrap();
            t.elapsed()
        })
        .collect();
    log.shutdown().unwrap();
    assert!(
        waits[..BURST].iter().all(|w| *w < interval / 4),
        "the burst waited: {waits:?}"
    );
    assert!(
        waits[BURST] >= interval / 2,
        "the call past the burst: {waits:?}"
    );
}
