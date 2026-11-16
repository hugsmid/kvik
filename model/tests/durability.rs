//! Durability levels (spec §2.1 `durability`, §3 Batches, Commit, Recovery
//! rules 1 and 3; Q-170, Q-251): flagged batches acknowledged and delivered
//! once written, kept by a start on the same boot, and reported when a power
//! loss removes them.

mod common;

use std::collections::BTreeMap;

use common::{T0, restore_in_progress};
use kvik_model::condition::Condition;
use kvik_model::engine::{
    Config, ConsumerDecl, ConsumerId, CrashPoint, Envelope, Fault, Funnel, FunnelId, MarkSync,
    Model, Selection, WriteOutcome,
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

fn last_written(m: &Model) -> Position {
    m.disk.files[&m.disk.active.unwrap()]
        .batches
        .last()
        .unwrap()[0]
        .position
}

/// Whether the kept log holds a fact at `p`: a lost position is given
/// again, first to the recovery entry (§3 Recovery rule 3).
fn fact_at(m: &Model, p: Position) -> bool {
    m.disk
        .live_entries()
        .get(&p)
        .is_some_and(|e| e.fact().is_some())
}

/// Two flagged batches, then a batch of `slow` whose sync fails with
/// `outcome`, then a power loss keeping `keep` batches not yet synced.
fn power_loss_after_a_failed_sync(
    outcome: WriteOutcome,
    keep: usize,
) -> (Model, [Position; 2], Position) {
    let (mut m, fast, slow) = levels_model();
    let flagged = [commit(&mut m, fast, 1), commit(&mut m, fast, 2)];
    let _ = m.emit(Reading::num(slow, T0 + 3, 1.0));
    assert!(m.commit(Some(Fault::SyncError(outcome))).is_err());
    let failed = last_written(&m);
    m.power_loss_keeping(keep);
    m.restart();
    (m, flagged, failed)
}

/// §3 Batches: every flag names a batch holding only facts of the
/// `written` series (none once damaged).
fn flags_name_flagged_batches(m: &Model, fast: SeriesId) -> bool {
    m.disk.flagged.iter().all(|(f, b)| {
        m.disk.files[f].batches.get(*b).is_some_and(|x| {
            x.iter()
                .all(|e| e.fact().is_some_and(|ft| ft.series == fast))
        })
    })
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

#[test]
fn s3_a_second_power_loss_before_a_start_drops_no_synced_batch() {
    let (mut m, fast, slow) = levels_model();
    let ps = [
        commit(&mut m, slow, 1),
        commit(&mut m, fast, 2),
        commit(&mut m, slow, 3),
    ];
    assert!(
        m.disk.synced_mark.unwrap().last < ps[0],
        "the mark file lags"
    );
    m.power_loss();
    m.power_loss();
    m.restart();
    let live = m.disk.live_entries();
    assert!(ps.iter().all(|p| live.contains_key(p)));
    let (_, e) = m.recovery_entries().into_iter().next_back().unwrap();
    assert_eq!(e.after, ps[2]);
    assert!(m.losses().iter().all(|l| l.kind != LossKind::DataLost));
}

#[test]
fn s3_power_loss_after_a_failed_sync_keeps_flagged_batches_but_not_a_lost_write() {
    let (m, flagged, failed) = power_loss_after_a_failed_sync(WriteOutcome::Lost, 3);
    let live = m.disk.live_entries();
    assert!(flagged.iter().all(|p| live.contains_key(p)));
    assert!(!fact_at(&m, failed), "its write was lost");
}

#[test]
fn s3_a_torn_write_is_a_torn_tail_only_when_the_flagged_batches_before_it_reached_the_disk() {
    let (m, flagged, _) = power_loss_after_a_failed_sync(WriteOutcome::Torn, 2);
    let (_, e) = m.recovery_entries().into_iter().next_back().unwrap();
    assert_eq!(e.after, flagged[1]);
    assert!(e.tail.torn);
    let (m, flagged, _) = power_loss_after_a_failed_sync(WriteOutcome::Torn, 0);
    let (_, e) = m.recovery_entries().into_iter().next_back().unwrap();
    assert!(e.after < flagged[0]);
    assert!(
        !e.tail.torn,
        "written back in order: nothing of it reached the disk"
    );
}

#[test]
fn s3_power_loss_with_the_mark_lost_drops_a_lost_write() {
    let (mut m, _, slow) = levels_model();
    let kept = commit(&mut m, slow, 1);
    let _ = m.emit(Reading::num(slow, T0 + 2, 1.0));
    let fault = Fault::Crash(CrashPoint::AfterWrite(WriteOutcome::Lost));
    assert!(m.commit(Some(fault)).is_err());
    let lost = last_written(&m);
    m.lose_mark(MarkState::Missing).unwrap();
    m.power_loss();
    m.restart();
    let live = m.disk.live_entries();
    assert!(live.contains_key(&kept));
    assert!(!fact_at(&m, lost));
}

#[test]
fn s3_power_loss_drops_flagged_batches_of_a_segment_the_mark_has_not_reached() {
    let (mut m, fast, _) = levels_model();
    let sealed = commit(&mut m, fast, 1);
    m.seal_active();
    let p = commit(&mut m, fast, 2);
    assert_eq!(mark(&m), sealed);
    m.power_loss();
    m.restart();
    let live = m.disk.live_entries();
    assert!(live.contains_key(&sealed), "the seal synced it");
    assert!(!fact_at(&m, p));
}

#[test]
fn s3_a_power_loss_after_a_start_drops_flagged_batches_written_since() {
    let (mut m, fast, _) = levels_model();
    m.power_loss();
    m.restart();
    let p = commit(&mut m, fast, 1);
    m.power_loss();
    m.restart();
    assert!(!fact_at(&m, p));
    let (_, e) = m.recovery_entries().into_iter().next_back().unwrap();
    assert!(e.after < p);
}

#[test]
fn s3_a_start_whose_sync_fails_moves_no_mark_past_flagged_batches() {
    let (mut m, fast, _) = levels_model();
    let k = mark(&m);
    let p = commit(&mut m, fast, 1);
    m.crash();
    m.restart_with(true);
    assert!(m.is_read_only());
    assert_eq!(mark(&m), k);
    assert!(m.disk.synced_mark.unwrap().last < p);
    m.sync_log();
    assert_eq!(mark(&m), k, "a read-only engine syncs nothing");
    m.crash();
    m.restart();
    assert_eq!(mark(&m), p);
    assert!(m.recovery_entries().is_empty());
}

#[test]
fn s3_a_read_only_engine_writes_no_latch_for_a_flagged_position_not_synced() {
    let (mut m, fast, slow) = levels_model();
    let on_change = Funnel::condition(fast, None, Condition::OnChange { deadband: 0.0 });
    m.add_route(C, FunnelId(1), on_change).unwrap();
    m.drain().unwrap();
    let p = commit(&mut m, fast, 1);
    let _ = m.emit(Reading::num(slow, T0 + 2, 1.0));
    assert!(
        m.commit(Some(Fault::SyncError(WriteOutcome::Lost)))
            .is_err()
    );
    let got = m.receive(C, usize::MAX).unwrap();
    assert!(got.iter().any(|d| d.trigger().is_some() && d.position == p));
    m.confirm_delivery(C, got.last().unwrap().id()).unwrap();
    let synced = m.disk.synced_mark.unwrap().last;
    assert!(synced < p);
    assert!(m.disk.cursors[&C] <= synced);
    assert!(m.disk.latches.values().all(|l| l.position <= synced));
}

#[test]
fn s3_a_written_series_known_only_from_a_restored_checkpoint_makes_the_entry_a_possible_loss() {
    let (mut m, fast, _) = levels_model();
    commit(&mut m, fast, 1);
    m.seal_active();
    m.compact(None).unwrap();
    m.checkpoint().unwrap();
    commit(&mut m, fast, 2);
    m.seal_active();
    assert_eq!(m.upload_checkpoints(), 1);
    let mut r = restore_in_progress(&mut m, &BTreeMap::new());
    commit(&mut r, fast, 3);
    r.power_loss();
    r.restart();
    let (_, e) = r.recovery_entries().into_iter().next_back().unwrap();
    assert!(e.written_level && e.possible_loss());
}

#[test]
fn s3_no_flag_outlives_a_flagged_batch_that_a_power_loss_or_a_start_removes() {
    let index = |m: &Model| m.disk.files[&m.disk.active.unwrap()].batches.len() - 1;
    // Dropped from the page cache.
    let (mut m, fast, _) = levels_model();
    commit(&mut m, fast, 1);
    commit(&mut m, fast, 2);
    m.power_loss_keeping(0);
    m.restart();
    assert!(flags_name_flagged_batches(&m, fast), "dropped");
    // Lost by a power loss after their sync.
    let (mut m, fast, _) = levels_model();
    commit(&mut m, fast, 1);
    commit(&mut m, fast, 2);
    m.sync_log();
    assert_eq!(m.power_loss_losing(2), 2);
    m.restart();
    assert!(flags_name_flagged_batches(&m, fast), "lost");
    // Damaged at the tail, then a reboot.
    let (mut m, fast, _) = levels_model();
    commit(&mut m, fast, 1);
    let first = index(&m);
    commit(&mut m, fast, 2);
    m.sync_log();
    m.crash();
    m.corrupt_open_batch(first).unwrap();
    m.corrupt_open_batch(first + 1).unwrap();
    m.power_loss();
    m.restart();
    assert!(flags_name_flagged_batches(&m, fast), "torn tail");
    // Damaged after a kept batch beyond a same-boot mark.
    let (mut m, fast, _) = levels_model();
    let kept = commit(&mut m, fast, 1);
    commit(&mut m, fast, 2);
    let second = index(&m);
    commit(&mut m, fast, 3);
    m.sync_log();
    m.crash();
    m.corrupt_open_batch(second).unwrap();
    m.corrupt_open_batch(second + 1).unwrap();
    m.remount();
    m.restart();
    assert!(m.disk.live_entries().contains_key(&kept));
    assert!(flags_name_flagged_batches(&m, fast), "truncated by rule 1");
}
