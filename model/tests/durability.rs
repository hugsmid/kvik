//! Durability levels (spec §2.1 `durability`, §3 Batches, Commit, Recovery
//! rules 1 and 3; Q-170, Q-251): flagged batches acknowledged and delivered
//! once written, kept by a start on the same boot, and reported when a power
//! loss removes them.

mod common;

use common::T0;
use kvik_model::engine::{
    Config, ConsumerDecl, ConsumerId, CrashPoint, Envelope, Fault, MarkSync, Model, Selection,
    WriteOutcome,
};
use kvik_model::recovery::{LossKind, MarkState};
use kvik_model::registry::{Durability, SeriesOptions};
use kvik_model::types::{HOUR, Kind, Position, Reading, SECOND, SeriesId, ValueType};

const C: ConsumerId = ConsumerId(1);

/// A store with the timed mark sync, one consumer, a series at the
/// `written` level and one at the default level.
fn levels_model() -> (Model, SeriesId, SeriesId) {
    let consumer = ConsumerDecl {
        id: C,
        selection: Selection::All,
        capacity: 100,
    };
    let cfg = Config {
        mark_sync: MarkSync::Timed,
        seal_after_batches: 64,
        ..Config::default()
    };
    let mut m = Model::new(cfg, vec![consumer], T0 + HOUR);
    let fast = m
        .define_with(
            "market/feed/price",
            Kind::Gauge,
            ValueType::Number,
            "1",
            10 * SECOND,
            SeriesOptions::durability(Durability::Written),
        )
        .unwrap();
    let slow = m
        .define(
            "site/meter/energy",
            Kind::Gauge,
            ValueType::Number,
            "kWh",
            10 * SECOND,
        )
        .unwrap();
    m.drain().unwrap();
    (m, fast, slow)
}

fn commit(m: &mut Model, s: SeriesId, t: i64) -> Position {
    m.ingest(vec![Reading::num(s, T0 + t, 1.0)])
        .unwrap()
        .positions[0]
}

fn mark(m: &Model) -> Position {
    m.disk.mark.unwrap().last
}

#[test]
fn s3_flagged_batch_is_acknowledged_and_delivered_before_its_sync() {
    let (mut m, fast, _) = levels_model();
    let before = mark(&m);
    let p = commit(&mut m, fast, 1);
    assert_eq!(mark(&m), before, "no sync covers it yet");
    assert_eq!(m.poll(C, 10).unwrap().last().unwrap().position, p);
    m.sync_log();
    assert_eq!(mark(&m), p);
}

#[test]
fn s3_mixed_batch_is_acknowledged_at_the_default_level() {
    let (mut m, fast, slow) = levels_model();
    let _ = m.emit(Reading::num(fast, T0 + 1, 1.0));
    let _ = m.emit(Reading::num(slow, T0 + 1, 1.0));
    let last = *m.commit(None).unwrap().positions.last().unwrap();
    assert_eq!(mark(&m), last, "synced before it is acknowledged");
    assert!(m.disk.flagged.is_empty());
}

#[test]
fn s3_process_crash_keeps_flagged_batches_beyond_the_mark_with_no_reset_point() {
    let (mut m, fast, _) = levels_model();
    let k = mark(&m);
    let ps: Vec<Position> = (1..=3).map(|t| commit(&mut m, fast, t)).collect();
    assert_eq!(mark(&m), k);
    m.crash();
    m.restart();
    assert!(m.recovery_entries().is_empty(), "no reset point");
    assert!(m.losses().is_empty());
    let live = m.disk.live_entries();
    assert!(ps.iter().all(|p| live.contains_key(p)));
    assert_eq!(mark(&m), ps[2]);
    assert_eq!(
        m.disk.synced_mark.unwrap().last,
        ps[2],
        "the start syncs it"
    );
}

#[test]
fn s3_process_crash_keeps_flagged_batches_and_resets_after_an_unflagged_one() {
    let (mut m, fast, slow) = levels_model();
    let kept = commit(&mut m, fast, 1);
    let _ = m.emit(Reading::num(slow, T0 + 2, 1.0));
    let fault = Fault::Crash(CrashPoint::AfterWrite(WriteOutcome::Survived));
    assert!(m.commit(Some(fault)).is_err());
    m.restart();
    let (at, e) = m.recovery_entries().into_iter().next_back().unwrap();
    assert_eq!(e.after, kept, "K is the last kept flagged batch");
    assert_eq!(e.tail.batches, 1);
    assert!(m.disk.live_entries().contains_key(&kept));
    assert!(
        m.losses()
            .iter()
            .any(|l| l.kind == LossKind::PossibleLoss && l.identity == at)
    );
}

#[test]
fn s3_flagged_batches_do_not_make_a_same_boot_mark_stale() {
    let (mut m, fast, slow) = levels_model();
    let ps: Vec<Position> = (1..=3).map(|t| commit(&mut m, fast, t)).collect();
    let _ = m.emit(Reading::num(slow, T0 + 4, 1.0));
    let fault = Fault::Crash(CrashPoint::AfterWrite(WriteOutcome::Survived));
    assert!(m.commit(Some(fault)).is_err());
    m.restart();
    let (_, e) = m.recovery_entries().into_iter().next_back().unwrap();
    assert_eq!(
        e.mark_state,
        MarkState::Readable,
        "four batches beyond, one unflagged"
    );
    assert_eq!(e.after, ps[2]);
}

#[test]
fn s3_power_loss_removing_flagged_batches_is_a_reset_point_and_possible_loss() {
    let (mut m, fast, _) = levels_model();
    let ps: Vec<Position> = (1..=3).map(|t| commit(&mut m, fast, t)).collect();
    let got = m.receive(C, usize::MAX).unwrap();
    assert_eq!(
        got.last().unwrap().position,
        ps[2],
        "delivered at its write"
    );
    m.power_loss_keeping(1);
    m.restart();
    let (at, e) = m.recovery_entries().into_iter().next_back().unwrap();
    assert_eq!(e.after, ps[0], "the first batch reached the disk");
    assert!(e.tail.is_empty(), "the others left no byte");
    assert!(e.written_level);
    assert!(
        m.losses()
            .iter()
            .any(|l| l.kind == LossKind::PossibleLoss && l.identity == at)
    );
    let p = commit(&mut m, fast, 9);
    assert!(
        p > at && p <= ps[2],
        "a position delivered before is given again"
    );
    let mut got = Vec::new();
    for _ in 0..10 {
        m.catch_up(C, 8).unwrap();
        got.extend(m.receive(C, usize::MAX).unwrap());
    }
    let r = got
        .iter()
        .find_map(Envelope::resync)
        .expect("the reset point");
    assert_eq!(r.forget_above, Some(ps[0]));
}

#[test]
fn s3_a_reboot_entry_is_a_possible_loss_while_a_written_series_is_defined() {
    let entry_after_reboot = |written: bool| {
        let (mut m, fast, slow) = levels_model();
        let s = if written { fast } else { slow };
        commit(&mut m, s, 1);
        m.sync_log();
        m.sync_mark_file();
        let mut m = if written {
            m
        } else {
            let consumer = ConsumerDecl {
                id: C,
                selection: Selection::All,
                capacity: 100,
            };
            let cfg = Config {
                mark_sync: MarkSync::Timed,
                ..Config::default()
            };
            let mut plain = Model::new(cfg, vec![consumer], T0 + HOUR);
            let s = plain
                .define(
                    "site/meter/energy",
                    Kind::Gauge,
                    ValueType::Number,
                    "kWh",
                    10 * SECOND,
                )
                .unwrap();
            commit(&mut plain, s, 1);
            plain.sync_mark_file();
            plain
        };
        m.power_loss();
        m.restart();
        m.recovery_entries().into_iter().next_back().unwrap().1
    };
    let e = entry_after_reboot(true);
    assert!(e.tail.is_empty() && e.mark_state == MarkState::Readable);
    assert!(e.written_level && e.possible_loss());
    assert!(!entry_after_reboot(false).possible_loss());
}

#[test]
fn s3_a_confirmed_flagged_position_is_synced_before_the_cursor_names_it() {
    let (mut m, fast, _) = levels_model();
    let p = commit(&mut m, fast, 1);
    let got = m.receive(C, usize::MAX).unwrap();
    m.confirm_delivery(C, got.last().unwrap().id()).unwrap();
    assert_eq!(m.disk.cursors[&C], p);
    assert!(m.disk.synced_mark.unwrap().last >= p);
}

#[test]
fn s3_a_seal_syncs_flagged_batches_first() {
    let (mut m, fast, _) = levels_model();
    let p = commit(&mut m, fast, 1);
    let open = m.disk.active.unwrap();
    m.seal_active();
    assert!(m.disk.files[&open].sealed);
    assert_eq!(mark(&m), p);
}
