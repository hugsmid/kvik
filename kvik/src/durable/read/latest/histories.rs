use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};

use super::tests::{
    Losses, Rig, S, Step, T0, bridged, known_alike, positions_lost, recorded_ranges, seed, steps,
    write_directory,
};
use crate::Store;
use crate::time::Timestamp;
use crate::types::{Intent, Position, Reading, Value};

/// Runs a history and checks at its end.
pub(super) fn run(history: &[Step]) -> Result<[u64; 4], TestCaseError> {
    let mut rig = Rig::new("history");
    for s in history {
        rig.step(s)?;
    }
    rig.check()?;
    rig.shut();
    // Q-1104 rests on the store writing no series directory under the default seal interval.
    prop_assert_eq!(rig.over_damage, 0, "a check before a start skipped");
    Ok([rig.seeded, rig.searches, rig.after_damage, rig.set_aside])
}

#[test]
fn latest_values_equal_the_models_at_every_check() {
    // Each case opens two stores and several readers: a quarter of the configured cases.
    let cases = (ProptestConfig::default().cases / 4).max(16);
    let mut runner = TestRunner::new(ProptestConfig {
        cases,
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    let counts: [AtomicU64; 4] = Default::default();
    runner
        .run(&steps(), |history| {
            for (c, n) in counts.iter().zip(run(&history)?) {
                c.fetch_add(n, Ordering::Relaxed);
            }
            Ok(())
        })
        .unwrap();
    // The set-aside of Q-1071 is reached by about three histories in 256, too few to assert here: the history
    // below pins it.
    let [seeded, searches, damaged, _] = counts.map(AtomicU64::into_inner);
    assert!(seeded > 0, "some reader continues a series directory");
    assert!(searches > 0, "some live value is found in the log");
    assert!(damaged > 0, "some history compares losses after damage");
}

#[test]
fn a_history_damaging_the_batch_just_above_a_directory_sets_aside_the_commit_time_before() {
    let emit = |t| Step::Emit(vec![(0, t, 1, 0, 0), (1, t, 2, 0, 0)], false);
    // The directory is through the last definition and the first emit's batch lies just above it; an even draw
    // aims there, where the cycle would take the third batch above.
    let history = [
        emit(-30),
        Step::Directory(6),
        emit(-4),
        emit(-2),
        Step::Damage(2, true),
    ];
    let [seeded, _, damaged, set_aside] = run(&history).unwrap();
    assert!(seeded > 0 && damaged > 0);
    assert!(
        set_aside > 0,
        "the continuing reader's loss just above its directory is reached"
    );
}

#[test]
fn only_a_directory_the_store_wrote_counts_as_the_store_s_own() {
    let mut rig = Rig::new("own-directory");
    rig.step(&Step::Emit(vec![(0, -30, 1, 0, 0)], false))
        .unwrap();
    rig.step(&Step::Directory(6)).unwrap();
    let r = rig.reader();
    r.live(rig.ids[0]).unwrap();
    let t = seed(&r).expect("the rig's directory");
    assert!(!rig.store_wrote(t), "the rig wrote it");
    let none = Position {
        sequence: t.sequence + 100,
        ..t
    };
    assert!(!rig.store_wrote(none), "no directory through it");
    rig.shut();
}

#[test]
fn a_reader_seeded_from_a_directory_knows_no_commit_time_before_a_loss_just_above_it() {
    let mut rig = Rig::new("seeded-loss");
    let id = rig.ids[2];
    let note = |rig: &Rig, secs: i64, text: &str| {
        Reading::new(
            rig.ids[2],
            Timestamp(rig.clock.wall().0 + secs * S),
            Value::Text(text.into()),
        )
    };
    rig.emit_readings(&[note(&rig, -30, "NOTE-AAA")], Intent::Normal);
    rig.long.live(id).unwrap();
    rig.shut();
    write_directory(&rig.env, &rig.dir, usize::MAX).expect("a batch");
    rig.reopen();
    rig.clock.advance(Duration::from_secs(5));
    rig.emit_readings(&[note(&rig, -4, "NOTE-BBB")], Intent::Normal);
    rig.emit(&[(0, -2, 0, 0, 0)], false);
    rig.shut();
    for p in rig.fs.paths() {
        if p.starts_with(rig.dir.join("log"))
            && let Some(bytes) = rig.fs.read(&p)
            && let Some(at) = bytes.windows(8).position(|w| w == b"NOTE-BBB")
        {
            rig.fs.poke(&p, at, b"X");
        }
    }
    let seeded = rig.reader();
    seeded.live(id).unwrap();
    assert!(seeded.durable().unwrap().unwrap().seeded());
    let whole = &rig.long;
    assert!(!whole.durable().unwrap().unwrap().seeded());
    let strip = |mut l: Vec<crate::health::LossReport>| {
        for r in &mut l {
            r.commit_time_before = None;
        }
        l
    };
    let (known, unknown) = (whole.losses(), seeded.losses());
    assert_eq!(known.len(), 1);
    assert!(!known[0].recorded);
    assert_eq!(known[0].commit_time_before, Some(Timestamp(T0)));
    assert_eq!(unknown[0].commit_time_before, None);
    assert_eq!(
        strip(unknown.clone()),
        strip(known.clone()),
        "the same loss otherwise"
    );
    let at = seed(&seeded);
    assert!(at.is_some());
    let (a, b) = known_alike((unknown.clone(), at), (known.clone(), None));
    assert_eq!(a, b);
    let recorded = |mut l: Losses| {
        for r in &mut l {
            r.recorded = true;
        }
        l
    };
    let (a, b) = known_alike(
        (recorded(unknown.clone()), at),
        (recorded(known.clone()), None),
    );
    assert_ne!(a, b, "a recorded report keeps the commit time compared");
    let below = at.map(|t| Position {
        sequence: t.sequence - 1,
        ..t
    });
    let (a, b) = known_alike((unknown.clone(), below), (known.clone(), None));
    assert_ne!(
        a, b,
        "a loss not just above the directory keeps the commit time compared"
    );
    let (a, b) = known_alike((unknown, None), (known, None));
    assert_ne!(
        a, b,
        "a reader that read the whole log keeps the commit time compared"
    );
}

/// Pins the engine as built until the owner rules on Q-1102: the third damage bridges two recorded losses, the start
/// records the bridge under the first one's identity, and the writer and a reader of the whole log report it apart.
#[test]
fn a_start_s_range_across_two_recorded_losses_is_reported_apart_by_the_writer_and_a_whole_reader() {
    let emit = |q, revise| Step::Emit(vec![(0, -60, 0, q, 0)], revise);
    let history = [
        emit(0, false),
        emit(250, false),
        emit(0, true),
        Step::Restart,
        Step::Damage(2876, false),
        Step::Damage(13022, false),
        Step::Damage(0, false),
    ];
    let mut rig = Rig::new("bridge");
    for s in &history {
        rig.step(s).unwrap();
    }
    let whole = rig.reader();
    let records = recorded_ranges(&whole);
    assert_eq!(
        records,
        [
            (at(10), at(10), at(10)),
            (at(8), at(8), at(8)),
            (at(10), at(8), at(10))
        ],
        "(identity, first, last) of each lost-range record"
    );
    assert_eq!(bridged(&records), [(at(8), at(10))]);
    let brief = |l: Losses| -> Vec<(Position, Option<Position>, Option<Position>, bool)> {
        l.into_iter()
            .map(|r| (r.identity, r.first, r.last, r.recorded))
            .collect()
    };
    let own = rig.store.as_ref().map(Store::reader).unwrap();
    assert_eq!(
        brief(own.losses()),
        [(at(10), Some(at(8)), Some(at(10)), true)]
    );
    assert_eq!(
        brief(whole.losses()),
        [
            (at(8), Some(at(8)), Some(at(10)), false),
            (at(10), Some(at(8)), Some(at(10)), true)
        ]
    );
    for l in [own.losses(), whole.losses()] {
        assert_eq!(
            positions_lost(&l, &bridged(&records)),
            (vec![], vec![(at(8), at(10))]),
            "what the property compares"
        );
    }
    rig.check().unwrap();
    assert_eq!(
        rig.bridged, 1,
        "the property compares this history as Q-1102 sets aside"
    );
    rig.shut();
}

fn at(sequence: u64) -> Position {
    Position {
        generation: 0,
        sequence,
    }
}

fn data_lost(identity: u64, first: Option<u64>, last: Option<u64>) -> crate::health::LossReport {
    use crate::health::{LossCause, LossKind, LossReport, MarkState};
    LossReport {
        identity: at(identity),
        kind: LossKind::DataLost,
        cause: LossCause::Corruption,
        first: first.map(at),
        last: last.map(at),
        file_id: None,
        bytes: 0,
        readable_commit_blocks: 0,
        mark_state: MarkState::Readable,
        boot_id: None,
        commit_time_before: None,
        commit_time_after: None,
        cursors_missing: Vec::new(),
        recorded: true,
        acknowledged: false,
    }
}

#[test]
fn only_records_of_two_identities_that_overlap_bridge() {
    let disjoint = [(at(4), at(4), at(5)), (at(8), at(8), at(9))];
    assert_eq!(bridged(&disjoint), [], "two losses apart");
    let touching = [(at(4), at(4), at(5)), (at(6), at(6), at(7))];
    assert_eq!(bridged(&touching), [], "two losses side by side");
    let one = [(at(4), at(4), at(4)), (at(4), at(3), at(5))];
    assert_eq!(bridged(&one), [], "one loss extended");
    let apart = [
        (at(2), at(2), at(2)),
        (at(4), at(4), at(4)),
        (at(4), at(4), at(6)),
        (at(9), at(9), at(9)),
    ];
    assert_eq!(bridged(&apart), [], "a bridge needs two identities");
    let bridge = [
        (at(2), at(2), at(2)),
        (at(4), at(4), at(4)),
        (at(2), at(2), at(4)),
        (at(9), at(9), at(9)),
    ];
    assert_eq!(bridged(&bridge), [(at(2), at(4))], "the bridge alone");
}

#[test]
fn positions_lost_compares_whole_what_lies_outside_a_bridge_or_has_no_position() {
    let bridges = [(at(10), at(20))];
    let inside = [
        data_lost(10, Some(10), Some(11)),
        data_lost(14, Some(14), Some(15)),
    ];
    assert_eq!(
        positions_lost(&inside, &bridges),
        (vec![], vec![(at(10), at(11)), (at(14), at(15))]),
        "separate spans stay apart"
    );
    let touching = [
        data_lost(10, Some(10), Some(11)),
        data_lost(12, Some(12), Some(13)),
    ];
    assert_eq!(
        positions_lost(&touching, &bridges).1,
        [(at(10), at(11)), (at(12), at(13))],
        "spans that only touch stay apart"
    );
    let overlapping = [
        data_lost(10, Some(10), Some(12)),
        data_lost(11, Some(11), Some(13)),
    ];
    assert_eq!(positions_lost(&overlapping, &bridges).1, [(at(10), at(13))]);
    let outside = data_lost(30, Some(30), Some(31));
    let unplaced = data_lost(12, None, Some(12));
    let l = [inside[0].clone(), outside.clone(), unplaced.clone()];
    assert_eq!(
        positions_lost(&l, &bridges),
        (vec![outside.clone(), unplaced], vec![(at(10), at(11))])
    );
    assert_ne!(
        positions_lost(&[data_lost(12, None, None)], &bridges),
        positions_lost(&[data_lost(12, Some(12), Some(12))], &bridges),
        "a report without positions differs from one with them"
    );
    assert_eq!(
        positions_lost(&[outside.clone()], &[]),
        (vec![outside], vec![]),
        "without a bridge every report is compared whole"
    );
}
