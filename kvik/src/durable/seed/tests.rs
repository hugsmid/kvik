//! A seeded start against the start from the whole log (`spec.md` §3 Order of a start): from one copy of the files
//! each, after randomised histories with restarts, crashes, power losses, damage, late and future readings,
//! revisions and consumers at every cursor, both give the same state, reads, deliveries and loss reports, and decide
//! later readings alike.

mod damage;
mod gives_way;
mod losses;
mod world;

use std::cell::RefCell;

use kvik_log::directory::Family;
use kvik_log::directory::series::SeriesDirectory;
use proptest::prelude::*;
use proptest::test_runner::TestRunner;

use crate::durable::scan::ScanOrder;
use crate::time::Timestamp;
use crate::types::{Reading, SeriesId, Value};
use world::*;

/// The compared starts of a run that met each case [`Reach`] names.
#[derive(Debug, Default)]
struct Tally {
    compared: u32,
    seeded: u32,
    sealed: u32,
    found_after: u32,
    found_before: u32,
    void: u32,
    below: u32,
    recoveries: u32,
}

impl Tally {
    fn add(&mut self, r: Reach) {
        self.compared += 1;
        for (n, hit) in [
            (&mut self.seeded, r.seeded),
            (&mut self.sealed, r.sealed),
            (&mut self.found_after, r.found_after),
            (&mut self.found_before, r.found_before),
            (&mut self.void, r.void),
            (&mut self.below, r.below),
            (&mut self.recoveries, r.recoveries),
        ] {
            *n += u32::from(hit);
        }
    }
}

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        // At most 2,000 histories: the deep job's 30-minute limit.
        .map_or(32, |n: u32| (n / 4).clamp(8, 2_000))
}

/// A history aimed at damage below a seed, which random histories meet in about one in a hundred: sealed rounds of
/// readings for routed consumers, one consumer's cursor file lost, an older batch damaged, and an end that writes
/// the series directory at the newest batch before a kill.
fn aimed() -> impl Strategy<Value = Vec<Step>> {
    let reading = (1..=SERIES, 0i64..3, 0u8..6, 0u8..4, Just(false));
    let round = prop::collection::vec(prop::collection::vec(reading.clone(), 1..4), 2..4);
    (
        (0u8..6, 0u8..6),
        prop::collection::vec(round, 2..4),
        (0usize..2, prop::collection::vec(reading, 1..3), 0usize..4),
    )
        .prop_map(|((r0, r1), rounds, (lost, last, damaged))| {
            let mut steps = vec![Step::Route(0, r0), Step::Route(1, r1)];
            for round in rounds {
                for rs in round {
                    steps.push(Step::Emit(rs, false));
                    steps.push(Step::Advance(1));
                }
                steps.push(Step::Seal);
            }
            steps.extend([
                Step::LoseCursor(lost),
                Step::Emit(last, false),
                Step::Damage(damaged),
            ]);
            steps
        })
}

#[test]
fn a_seeded_start_gives_what_the_start_from_the_whole_log_gives() {
    let mut runner = TestRunner::new(ProptestConfig {
        cases: cases(),
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    let tally = RefCell::new(Tally::default());
    let histories = (
        prop_oneof![
            3 => (prop::collection::vec(step(), 1..30), end()),
            1 => aimed().prop_map(|steps| (steps, End::KillAt(usize::MAX))),
        ],
        any::<bool>(),
        readings(),
        0i64..3_000,
    )
        .prop_map(|((steps, end), before, later, start_back)| {
            (steps, end, before, later, start_back)
        });
    let run = runner.run(&histories, |(steps, end, before, later, start_back)| {
        let order = if before {
            ScanOrder::BeforeOpen
        } else {
            ScanOrder::Background
        };
        tally
            .borrow_mut()
            .add(compare(&steps, end, order, &later, start_back)?);
        Ok(())
    });
    if let Err(e) = run {
        panic!("{e}");
    }
    let t = tally.into_inner();
    eprintln!("{t:?}");
    // The reach a run must show: a third seeded, a third of those over sealed segments, and damage below a seed
    // found after and before the open and recovery entries at or below it. Voids and regions below a seed do not
    // reach the pass through `Seed::find` (`gives_way.rs`).
    if t.compared >= 64 {
        assert!(t.seeded * 10 >= t.compared * 3, "{t:?}");
        assert!(t.sealed * 10 >= t.seeded * 3, "{t:?}");
        for n in [t.found_after, t.found_before, t.recoveries] {
            assert!(n > 0, "{t:?}");
        }
    }
}

#[test]
fn a_store_with_a_directory_starts_seeded_and_gives_what_the_whole_log_gives() {
    let mut steps = vec![Step::Route(0, 0), Step::Route(1, 3)];
    for k in 0..20 {
        steps.push(Step::Emit(
            vec![(1, 0, k % 6, 0, false), (2, -5, 3, k % 4, false)],
            false,
        ));
        steps.push(Step::Advance(2));
        if k == 8 {
            steps.push(Step::Drain(0, 3));
        }
    }
    steps.push(Step::Directory(usize::MAX - 3));
    steps.push(Step::Emit(vec![(3, 0, 1, 0, false)], false));
    let later = [
        (1, 0, 4, 0, false),
        (2, -5, 3, 1, false),
        (2, -5, 3, 3, false),
    ];
    for order in [ScanOrder::Background, ScanOrder::BeforeOpen] {
        for end in [End::Shutdown, End::Kill, End::PowerLoss] {
            // Derived files are never synced (A.18): a power loss may take the directory, and the start reads the
            // whole log.
            let seeded = compare(&steps, end, order, &later, 30).unwrap().seeded;
            assert!(seeded || end == End::PowerLoss, "{order:?} {end:?}");
        }
    }
}

#[test]
fn a_late_revision_behind_a_fact_that_moves_the_window_is_checked_against_its_whole_key() {
    let e = |id, dt, v| Step::Emit(vec![(id, dt, v, 0, false)], false);
    let steps = vec![
        e(1, 0, 0),
        e(1, 0, 0),
        e(2, 0, 0),
        e(1, 0, 1),
        e(3, 0, 0),
        e(1, 1, 0),
        e(1, 0, 2),
        Step::Route(0, 0),
        Step::Directory(738048058492601894),
        Step::Advance(290),
        Step::Emit(vec![(2, 0, 0, 0, false), (2, -901, 0, 0, false)], false),
    ];
    compare(
        &steps,
        End::Shutdown,
        ScanOrder::Background,
        &[(1, 0, 0, 0, false)],
        0,
    )
    .unwrap();
}

#[test]
fn a_consumer_behind_the_seed_with_no_batch_after_it_is_replayed_from_the_log() {
    let e = |id, v, revise| Step::Emit(vec![(id, 0, v, 0, false)], revise);
    let steps = vec![
        Step::Route(0, 0),
        e(3, 0, false),
        e(1, 0, false),
        e(1, 1, false),
        e(1, 0, true),
        Step::Directory(usize::MAX),
    ];
    for order in [ScanOrder::Background, ScanOrder::BeforeOpen] {
        assert!(
            compare(&steps, End::Shutdown, order, &[(1, 0, 0, 0, false)], 0)
                .unwrap()
                .seeded
        );
    }
}

#[test]
fn a_clean_shutdown_leaves_a_series_directory_through_the_mark() {
    let mut w = World::new();
    let n = kvik_log::directory::cadence::SHUTDOWN_POSITIONS as i64 + 10;
    let readings: Vec<Reading> = (0..n)
        .map(|k| Reading::new(SeriesId(1), Timestamp(T0 - n * S + k * S), Value::F64(1.0)))
        .collect();
    for chunk in readings.chunks(20_000) {
        block_on(w.store().emit(chunk)).unwrap();
    }
    w.shut();
    let store_id = kvik_log::store::read_descriptor(&*w.env.fs, &w.dir)
        .unwrap()
        .store_id;
    let (_, d) = kvik_log::directory::read_latest(&*w.env.fs, &w.dir, Family::Series, |b| {
        SeriesDirectory::decode(b, &store_id)
    })
    .unwrap()
    .unwrap();
    let mut log = reader(&w.env, &w.dir);
    let mark = log.refresh().unwrap().state.durable;
    let _ = log.close();
    assert_eq!(d.through, mark);
    assert!(d.entries.iter().all(|e| e.counts.is_some()));
}

#[test]
fn a_stale_for_condition_on_an_outranked_last_fact_resumes_from_the_last_arrival_that_counts() {
    let steps = vec![
        Step::Route(0, 5),
        Step::Emit(vec![(2, 0, 3, 3, false)], false),
        Step::Advance(40),
        Step::Emit(vec![(2, -40, 1, 1, false)], false),
        Step::Directory(usize::MAX),
    ];
    for order in [ScanOrder::Background, ScanOrder::BeforeOpen] {
        assert!(
            compare(&steps, End::Shutdown, order, &[(1, 0, 0, 0, false)], 0)
                .unwrap()
                .seeded
        );
    }
}

#[test]
fn a_sealed_segment_damaged_above_the_seed_before_a_kill_gives_the_same_reports_with_the_scan_before_the_open()
 {
    let e = |r: Vec<(u32, i64, u8, u8, bool)>, revise| Step::Emit(r, revise);
    let steps = vec![
        e(
            vec![
                (1, 77, 2, 0, false),
                (2, 0, 2, 3, false),
                (3, -1058, 0, 1, false),
            ],
            false,
        ),
        Step::Damage(4605220851183485209),
        e(vec![(3, 1, 4, 0, false)], true),
        Step::Seal,
        Step::Route(1, 3),
        e(
            vec![
                (2, -469, 2, 0, false),
                (3, 1, 3, 2, false),
                (3, 2, 2, 1, false),
                (2, 0, 4, 1, false),
            ],
            false,
        ),
        Step::Directory(9625873109082309730),
        Step::Drain(0, 5),
        Step::Advance(383),
        Step::Restart,
        Step::Route(1, 0),
        e(vec![(1, 2, 1, 0, false)], false),
        Step::Damage(6279550178163669897),
    ];
    let later = [
        (2, 1, 5, 2, false),
        (1, 0, 5, 2, false),
        (2, 253, 0, 2, false),
    ];
    compare(&steps, End::Kill, ScanOrder::BeforeOpen, &later, 2350).unwrap();
}

#[test]
fn a_stale_for_condition_on_a_last_fact_tied_with_an_earlier_one_resumes_from_the_tied_arrival() {
    for revise in [false, true] {
        let steps = vec![
            Step::Route(0, 5),
            Step::Emit(vec![(2, 0, 3, 3, false)], false),
            Step::Advance(40),
            Step::Emit(vec![(2, -40, 1, 3, false)], revise),
            Step::Directory(usize::MAX),
        ];
        for order in [ScanOrder::Background, ScanOrder::BeforeOpen] {
            let r = compare(&steps, End::Shutdown, order, &[(1, 0, 0, 0, false)], 0).unwrap();
            assert!(r.seeded, "{revise} {order:?}");
        }
    }
}

#[test]
fn a_reset_point_resync_below_the_seed_carries_the_time_of_the_call_that_delivers_it() {
    let e = |r: Vec<(u32, i64, u8, u8, bool)>, revise| Step::Emit(r, revise);
    let steps = vec![
        Step::PowerLoss,
        Step::Kill,
        Step::Damage(9907959249914154488),
        Step::Kill,
        Step::Directory(1412234892553971929),
        e(vec![(2, 2, 4, 2, false)], false),
        e(
            vec![
                (1, 0, 0, 2, true),
                (2, -521, 2, 2, false),
                (3, 1, 5, 3, false),
                (2, -1012, 2, 1, false),
            ],
            false,
        ),
        Step::Directory(1465176635907732360),
        Step::PowerLoss,
        e(vec![(2, 405, 2, 1, false)], true),
        Step::Advance(3),
        e(vec![(1, 1, 4, 1, true), (1, -96, 2, 2, false)], false),
        Step::Damage(8747610490528319193),
        Step::Advance(319),
        e(
            vec![
                (2, 2, 1, 0, false),
                (1, -305, 2, 1, false),
                (3, 1, 0, 1, false),
                (2, 2, 1, 1, false),
            ],
            true,
        ),
        Step::Damage(13283164618690287806),
        e(vec![(3, 2, 3, 2, false), (3, 0, 4, 0, false)], false),
        e(
            vec![
                (1, 2, 2, 0, false),
                (3, -1057, 5, 3, false),
                (3, 2, 3, 2, false),
            ],
            false,
        ),
    ];
    // With sealing switched on: a seal due before each emit in turn.
    for k in 0..steps.len() {
        let mut s = steps.clone();
        s.insert(k, Step::Seal);
        compare(
            &s,
            End::Shutdown,
            ScanOrder::Background,
            &[(1, 0, 0, 3, false)],
            2655,
        )
        .unwrap();
    }
}

#[test]
fn a_consumer_starting_at_the_commit_time_of_batches_below_the_seed_starts_after_the_last_of_them()
{
    let steps = vec![
        Step::Route(0, 0),
        Step::Emit(vec![(1, 0, 1, 0, false)], false),
        Step::Emit(vec![(3, 0, 2, 0, false)], false),
        Step::Advance(10),
        Step::Emit(vec![(1, 0, 3, 0, false)], false),
        Step::Directory(usize::MAX),
    ];
    for order in [ScanOrder::Background, ScanOrder::BeforeOpen] {
        let r = compare(&steps, End::Shutdown, order, &[(1, 0, 4, 0, false)], 10).unwrap();
        assert!(r.seeded, "{order:?}");
    }
}

#[test]
fn damage_the_scan_finds_below_a_loss_found_at_the_open_grows_it_under_the_identity_found_first() {
    let e = |r: Vec<(u32, i64, u8, u8, bool)>, revise| Step::Emit(r, revise);
    let steps = vec![
        Step::Seal,
        e(vec![(2, -1019, 1, 2, false), (3, 2, 3, 2, false)], true),
        Step::Route(1, 1),
        Step::Route(0, 4),
        Step::PowerLoss,
        Step::Restart,
        Step::Advance(2),
        e(vec![(3, 0, 3, 2, false), (3, 1, 2, 1, false)], false),
        Step::Route(0, 3),
        Step::Damage(15369219847121119798),
        Step::Drain(1, 4),
        e(vec![(3, 1, 5, 0, false), (1, 0, 1, 1, false)], false),
        Step::Restart,
        e(vec![(1, 0, 3, 2, false), (2, 1, 5, 1, false)], false),
        e(vec![(2, 2, 2, 1, false)], false),
        Step::Directory(3499691690462183775),
        Step::LoseCursor(0),
        Step::Seal,
        e(vec![(3, 0, 5, 3, false)], true),
        Step::LoseCursor(0),
        Step::Damage(2772374668308363000),
    ];
    let later = [
        (3, 512, 3, 3, false),
        (3, 388, 5, 2, false),
        (2, 2, 4, 2, false),
        (3, 1, 0, 3, false),
    ];
    compare(
        &steps,
        End::KillAt(2656987907825394109),
        ScanOrder::Background,
        &later,
        330,
    )
    .unwrap();
}

#[test]
fn a_consumer_starting_below_the_seed_passes_over_a_recorded_loss_as_the_whole_log_does() {
    let e = |r: Vec<(u32, i64, u8, u8, bool)>, revise| Step::Emit(r, revise);
    let steps = vec![
        e(
            vec![
                (3, 2, 3, 1, false),
                (2, 1, 0, 0, false),
                (2, 0, 3, 1, false),
                (2, 0, 5, 0, false),
            ],
            false,
        ),
        Step::Directory(4176864351485358171),
        e(
            vec![
                (3, 2, 5, 1, false),
                (3, 0, 1, 3, false),
                (2, 0, 5, 2, false),
                (3, 1, 5, 2, false),
            ],
            true,
        ),
        Step::Advance(1194),
        Step::Damage(5611314754272779672),
        e(
            vec![
                (3, 2, 1, 1, false),
                (2, 2, 4, 1, false),
                (2, 1, 2, 1, false),
            ],
            false,
        ),
        Step::Seal,
        e(
            vec![
                (1, 0, 4, 2, false),
                (1, 547, 0, 2, false),
                (3, 564, 5, 3, false),
            ],
            false,
        ),
        Step::Directory(14052021757065914289),
        e(vec![(2, 1, 1, 1, false), (2, -1008, 3, 3, false)], false),
        e(
            vec![(2, 0, 1, 2, false), (3, 0, 0, 0, true), (1, 1, 3, 2, false)],
            false,
        ),
    ];
    let later = [(2, 218, 1, 1, false), (1, 1, 2, 2, false)];
    for _ in 0..4 {
        compare(&steps, End::Kill, ScanOrder::Background, &later, 2230).unwrap();
    }
}

#[test]
fn a_replay_below_the_seed_over_a_recorded_loss_is_decided_as_the_whole_log_decides_it() {
    let steps = vec![
        Step::Emit(vec![(1, 0, 1, 0, false)], false),
        Step::Emit(vec![(2, 0, 1, 0, false)], false),
        Step::Damage(0),
        Step::Emit(vec![(3, 0, 1, 0, false)], false),
        Step::Directory(usize::MAX),
    ];
    for order in [ScanOrder::Background, ScanOrder::BeforeOpen] {
        let r = compare(&steps, End::Shutdown, order, &[(1, 0, 2, 0, false)], 0).unwrap();
        assert!(r.seeded, "{order:?}");
    }
}
