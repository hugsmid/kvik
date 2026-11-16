//! The first start of a restored generation and the starts after it (Q-260, Q-262, Q-264): which mark is read as
//! no mark, the id floor the first start takes, and a crash at every operation of `init_generation` and that start.

use std::io;
use std::time::Duration;

use kvik_log::content::Item;
use kvik_log::cursor::{CursorState, append};
use kvik_log::fs::{FaultOp, Fs, MemFs};
use kvik_log::mark::ResetStamp;
use kvik_log::setaside::{RestoredEnd, SetAside};
use kvik_log::writer::{
    GenerationStart, LossKind, MarkSync, Position, StartReport, StartRule, WriterConfig,
};

use super::at;
use super::rig::*;

/// A mark synced only once an hour: the generation's first mark stays the durable one while batches are acknowledged.
fn lazy() -> WriterConfig {
    WriterConfig {
        mark_sync: MarkSync::Interval(Duration::from_secs(3600)),
        ..cfg()
    }
}

/// A power loss, then every unsealed segment of `generation` removed, or zeroed whole.
fn lose_unsealed(r: &mut Rig, generation: u32, zeroed: bool) {
    r.w = None;
    r.fs.power_loss();
    for (n, p) in r.segments() {
        if n.generation == generation && !n.sealed {
            if zeroed {
                let len = r.fs.read(&p).unwrap().len();
                r.fs.poke(&p, 0, &vec![0; len]);
            } else {
                r.fs.remove_file(&p).unwrap();
            }
        }
    }
    r.fs.restart();
}

/// Three batches acknowledged under the first mark, a power loss, the only segment lost, a reboot, one more batch:
/// the start's report, the next position acknowledged and whether the stamp changed.
fn lose_the_only_segment(
    mut r: Rig,
    generation: u32,
    zeroed: bool,
) -> (StartReport, Position, bool) {
    r.open();
    for k in 0..3 {
        r.submit(vec![fact(5, k)]);
        r.drain();
    }
    let before = r.mark().stamp;
    lose_unsealed(&mut r, generation, zeroed);
    r.boot += 1;
    let rep = r.open();
    r.acked.clear();
    r.submit(vec![fact(5, 9)]);
    r.drain();
    (rep, r.acked[0], r.mark().stamp != before)
}

/// Q-260 as refined by Q-125: once the first start has written its mark, losing the only segment before the mark
/// moves is a reset with a new stamp, as in generation 0, and (1, 1) is never acknowledged again under the restore's
/// stamp.
#[test]
fn losing_the_only_segment_before_the_first_mark_moves_is_a_reset_as_in_generation_0() {
    for zeroed in [false, true] {
        let (rep, next, restamped) = lose_the_only_segment(restored(lazy()).0, 1, zeroed);
        let (rep0, next0, restamped0) =
            lose_the_only_segment(Rig::on(MemFs::new(), lazy(), None), 0, zeroed);
        assert_eq!(
            rep.rule,
            StartRule::Reboot(kvik_log::entries::MarkState::Readable)
        );
        assert_eq!(rep.rule, rep0.rule, "zeroed {zeroed}");
        let e = rep.entry.as_ref().expect("a recovery entry");
        assert_eq!((e.entry.after, e.sequence), (0, 1), "zeroed {zeroed}");
        assert_ne!(e.entry.stamp, Some(stamp()));
        assert!(restamped && restamped0, "zeroed {zeroed}");
        assert_eq!((next.generation, next.sequence), (1, 2), "zeroed {zeroed}");
        assert_eq!(next.sequence, next0.sequence);
        for r in [&rep, &rep0] {
            assert!(
                r.losses.iter().all(|l| l.kind == LossKind::PossibleLoss),
                "zeroed {zeroed}: {:?}",
                r.losses
            );
        }
    }
}

/// Q-264 with Q-125: a cursor file naming a position the lost segment held makes the loss a recorded `data_lost`
/// from the generation's first position, in both generations.
#[test]
fn a_cursor_past_the_lost_only_segment_gives_data_lost_from_the_generations_first_position() {
    for generation in [0, 1] {
        let mut r = if generation == 0 {
            Rig::on(MemFs::new(), lazy(), None)
        } else {
            restored(lazy()).0
        };
        r.open();
        for k in 0..3 {
            r.submit(vec![fact(5, k)]);
            r.drain();
        }
        let state = CursorState {
            consumer: "c".into(),
            position: at(generation, 3),
            latches: Vec::new(),
            reset: None,
        };
        append(&r.fs, &dir(), STORE_ID, generation, T0, &state, None).unwrap();
        lose_unsealed(&mut r, generation, false);
        r.boot += 1;
        let rep = r.open();
        let e = rep.entry.as_ref().expect("a recovery entry");
        assert_eq!(
            (e.entry.after, e.sequence),
            (0, 4),
            "generation {generation}"
        );
        assert!(
            rep.losses
                .iter()
                .any(|l| l.kind == LossKind::DataLost && (l.first, l.last) == (Some(1), Some(3))),
            "generation {generation}: {:?}",
            rep.losses
        );
        let lost: Vec<_> = r
            .lost_ranges()
            .into_iter()
            .map(|(g, l)| (g, l.first, l.last))
            .collect();
        assert_eq!(
            lost,
            vec![(generation, (generation, 1), (generation, 3))],
            "generation {generation}"
        );
    }
}

/// The anchor of a generation starting above 1: after its first start, a start on the same boot or after a reboot
/// with nothing written reports nothing lost below the first position.
#[test]
fn a_generation_starting_above_1_loses_nothing_below_its_first_position_after_its_first_start() {
    let (files, end) = source();
    let start = GenerationStart {
        stamp: ResetStamp {
            sequence: 5,
            nonce: 3,
        },
        ..generation_start(end)
    };
    let mut r = restored_from(&files, start, cfg());
    assert_eq!(r.open().rule, StartRule::FirstStart);
    r.kill();
    let rep = r.open();
    assert_eq!(rep.rule, StartRule::SameBoot);
    assert!(rep.entry.is_none() && rep.losses.is_empty(), "{rep:?}");
    r.kill();
    r.boot += 1;
    let rep = r.open();
    let e = rep.entry.as_ref().expect("rule 3 writes an entry");
    assert_eq!((e.entry.after, e.sequence), (4, 5));
    assert!(
        rep.losses.iter().all(|l| l.kind == LossKind::PossibleLoss),
        "{:?}",
        rep.losses
    );
    assert!(r.lost_ranges().is_empty());
    r.submit(vec![fact(5, 0)]);
    r.drain();
    assert_eq!(r.acked, vec![at(1, 6)]);
}

/// `is_first_mark` compares the stamp: a first mark whose stamp is not the start's is read as a mark.
#[test]
fn a_first_mark_with_another_stamp_is_read_as_a_mark() {
    let (mut r, end) = restored(cfg());
    r.start = Some(GenerationStart {
        stamp: ResetStamp {
            sequence: 1,
            nonce: 78,
        },
        ..generation_start(end)
    });
    let rep = r.open();
    assert_ne!(rep.rule, StartRule::FirstStart);
    assert!(rep.entry.is_some());
    assert_ne!(r.mark().stamp, stamp());
}

/// Q-262: the first start takes the sealed log's id floor into its mark.
#[test]
fn the_first_start_writes_the_id_floor_in_its_mark() {
    let (mut r, _) = restored(cfg());
    assert_eq!(r.open().rule, StartRule::FirstStart);
    assert_eq!(r.mark().max_series_id, 9);
}

/// Generation 0's log, a sealed segment defining series 5 and then `last` alone in a second one, restored to the end
/// of the first, so that `last` is set aside.
fn restored_with(last: Vec<Item>) -> Rig {
    let mut r = Rig::on(MemFs::new(), cfg(), None);
    r.open();
    r.submit(vec![interval_series(5)]);
    r.drain();
    r.timed_seal();
    let end = r.acked.last().unwrap().sequence;
    r.submit(last);
    r.drain();
    r.timed_seal();
    let start = GenerationStart {
        set_aside: SetAside::new(vec![RestoredEnd {
            generation: 1,
            end: (0, end),
        }]),
        ..generation_start(end)
    };
    restored_from(&sealed_files(&r), start, cfg())
}

/// Q-262: a fact in a set-aside batch reserves its id, with no definition beside it.
#[test]
fn a_fact_in_a_set_aside_batch_reserves_its_id() {
    let mut r = restored_with(vec![fact(20, 0)]);
    assert_eq!(r.open().rule, StartRule::FirstStart);
    assert_eq!(r.mark().max_series_id, 20);
}

/// The id floor a rule 1 recovery entry carries (an id held only by the batch it truncated) counts in the sealed
/// scan, which is the only source at a restored generation's first start.
#[test]
fn a_recovery_entrys_id_floor_in_the_sealed_log_counts_at_the_first_start() {
    let mut r = Rig::on(MemFs::new(), cfg(), None);
    r.open();
    r.submit(vec![interval_series(5)]);
    r.drain();
    r.fs.fail_nth(FaultOp::Write, "MARK", 0, io::ErrorKind::Other);
    r.submit(vec![interval_series(12)]);
    r.step();
    r.kill();
    let e = r
        .open()
        .entry
        .expect("rule 1 truncates the batch beyond the mark");
    assert!(e.entry.id_floor >= 12);
    r.submit(vec![fact(5, 0)]);
    r.drain();
    r.timed_seal();
    let end = r.acked.last().unwrap().sequence;
    let mut g1 = restored_from(&sealed_files(&r), generation_start(end), cfg());
    assert_eq!(g1.open().rule, StartRule::FirstStart);
    assert_eq!(g1.mark().max_series_id, 12);
}
