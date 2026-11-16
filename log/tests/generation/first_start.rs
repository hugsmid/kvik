//! The first start of a restored generation and the starts after it (Q-260, Q-262, Q-264): which mark is read as
//! no mark, the id floor the first start takes, and a crash at every operation of `init_generation` and that start.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use kvik_log::content::Item;
use kvik_log::cursor::{CursorState, append};
use kvik_log::entries::MarkState;
use kvik_log::fs::{FaultOp, Fs, MemFs};
use kvik_log::mark::{MarkSlot, ResetStamp, read_mark};
use kvik_log::setaside::{RestoredEnd, SetAside};
use kvik_log::store::StoreDir;
use kvik_log::writer::{
    GenerationStart, LogWriter, LossKind, MarkSync, Position, StartOptions, StartReport, StartRule,
    WriterConfig, init_generation,
};

use super::rig::*;
use super::{at, read_all};

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
        assert_eq!(rep.rule, StartRule::Reboot(MarkState::Readable));
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

fn read_mark_of(fs: &MemFs) -> Option<MarkSlot> {
    let b = fs.read(&log_path("MARK"))?;
    read_mark(&b, &STORE_ID).ok().flatten().map(|(_, m)| m)
}

/// How a stopped process comes back.
#[derive(Clone, Copy, Debug)]
enum Stop {
    /// Killed and started again on the same boot, the page cache kept.
    Kill,
    PowerLoss,
    PartialPowerLoss(u64),
}

/// A crash at every operation of `init_generation`, the first start and its first commit, then a kill, a whole or a
/// partial power loss and `init_generation` and a start again. Only the mark `init_generation` wrote is read as no
/// mark (Q-260); nothing acknowledged is lost or acknowledged again; no `data_lost`.
#[test]
fn a_crash_at_every_operation_of_init_generation_and_the_first_start() {
    let (files, end) = source();
    let start = generation_start(end);
    let stops = [
        Stop::Kill,
        Stop::PowerLoss,
        Stop::PartialPowerLoss(1),
        Stop::PartialPowerLoss(2),
        Stop::PartialPowerLoss(3),
        Stop::PartialPowerLoss(4),
        Stop::PartialPowerLoss(5),
    ];
    for config in [cfg(), lazy()] {
        for stop in stops {
            let mut points = 0;
            for n in 0.. {
                let ctx = format!("{:?}, {stop:?}, crash after {n}", config.mark_sync);
                let fs = installed(&files);
                fs.crash_after(n);
                let mut r = Rig::on(fs.clone(), config.clone(), Some(start.clone()));
                let init = StoreDir::open(Arc::new(fs.clone()), &dir(), None)
                    .ok()
                    .map(|s| init_generation(&s, &start, T0));
                if matches!(init, Some(Ok(()))) && !fs.crashed() {
                    let opts = StartOptions {
                        generation: Some(start.clone()),
                        ..StartOptions::default()
                    };
                    if let Ok(store) = StoreDir::open(Arc::new(fs.clone()), &dir(), None)
                        && let Ok(w) = LogWriter::open_with(
                            store,
                            config.clone(),
                            Some([1; 16]),
                            r.now,
                            r.wall,
                            opts,
                        )
                        && w.fault().is_none()
                    {
                        r.w = Some(w);
                        r.maint();
                        r.submit(vec![fact(5, 0)]);
                        r.drain();
                    }
                }
                let acked = std::mem::take(&mut r.acked);
                let crashed = fs.crashed();
                r.w = None;
                match stop {
                    Stop::Kill => {}
                    Stop::PowerLoss => fs.power_loss(),
                    Stop::PartialPowerLoss(s) => fs.power_loss_partial(s * 1000 + n as u64),
                }
                fs.restart();
                let store = StoreDir::open(Arc::new(fs.clone()), &dir(), None).unwrap();
                init_generation(&store, &start, T0).unwrap_or_else(|e| panic!("{ctx}: {e:?}"));
                drop(store);
                let before = read_mark_of(&fs).unwrap_or_else(|| panic!("{ctx}: no mark"));
                r.boot = if matches!(stop, Stop::Kill) { 1 } else { 2 };
                let rep = r.try_open().unwrap_or_else(|e| panic!("{ctx}: {e:?}"));
                let m = r.mark();
                assert_eq!((m.generation, m.boot_id), (1, [r.boot; 16]), "{ctx}");
                assert!(
                    rep.losses.iter().all(|l| l.kind != LossKind::DataLost),
                    "{ctx}: {rep:?}"
                );
                if before.boot_id == [0; 16] {
                    assert!(acked.is_empty(), "{ctx}: acknowledged under the first mark");
                    assert_eq!(rep.rule, StartRule::FirstStart, "{ctx}");
                    assert!(
                        rep.entry.is_none() && rep.losses.is_empty(),
                        "{ctx}: {rep:?}"
                    );
                    assert_eq!((m.stamp, m.max_series_id), (stamp(), 9), "{ctx}");
                } else {
                    assert_ne!(rep.rule, StartRule::FirstStart, "{ctx}");
                    if matches!(rep.rule, StartRule::Reboot(_)) {
                        assert!(rep.entry.is_some(), "{ctx}");
                    }
                    assert_eq!(m.stamp == stamp(), rep.entry.is_none(), "{ctx}");
                }
                let kept = read_all(&r, start.set_aside.clone());
                for p in &acked {
                    assert!(
                        kept.iter().any(|(f, l)| f <= p && p <= l),
                        "{ctx}: {p:?} lost from {kept:?}"
                    );
                }
                r.submit(vec![fact(5, 1)]);
                r.drain();
                let next = *r.acked.last().unwrap_or_else(|| panic!("{ctx}: no ack"));
                let top = kept
                    .iter()
                    .filter(|(_, l)| l.generation == 1)
                    .map(|(_, l)| l.sequence)
                    .max();
                let expected = match &rep.entry {
                    Some(e) => e.sequence + 1,
                    None => top.map_or(1, |t| t + 1),
                };
                assert_eq!(next, at(1, expected), "{ctx}");
                assert!(acked.iter().all(|p| p.sequence < next.sequence), "{ctx}");
                if !crashed {
                    break;
                }
                points += 1;
            }
            assert!(
                points >= 20,
                "{:?}, {stop:?}: {points} crash points",
                config.mark_sync
            );
        }
    }
}
