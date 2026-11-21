use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};

use super::tests::{Losses, Rig, S, Step, T0, known_alike, seed, steps, write_directory};
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
