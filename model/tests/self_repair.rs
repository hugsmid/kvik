//! Self-repair (spec §8 Self-repair): the loss life cycle, closing records,
//! the alert on every corruption, and the scheduled check of the bucket.

mod common;

use common::T0;
use kvik_model::engine::{
    Alert, Config, ConsumerDecl, ConsumerId, Corruption, Error, FileClass, FileId, Level,
    ManifestOp, ManifestRecord, MarkSync, Model, QuarantineCause, Reason, RepairConfig, Selection,
};
use kvik_model::recovery::{LossCause, LossKind, LossState, MarkState, Outcome};
use kvik_model::types::{DAY, HOUR, Kind, MINUTE, Position, Reading, SECOND, SeriesId, ValueType};

fn store(repair: RepairConfig) -> (Model, SeriesId) {
    let config = Config {
        repair,
        ..Config::default()
    };
    let mut m = Model::new(config, vec![], T0 + HOUR);
    let s = m
        .define(
            "plant/line-1/power",
            Kind::Gauge,
            ValueType::Number,
            "kW",
            10 * SECOND,
        )
        .unwrap();
    m.disk.bucket.reachable = true;
    // The definition in a segment of its own, so that only facts are lost.
    m.seal_active();
    (m, s)
}

/// A sealed segment holding three facts, and their positions.
fn sealed_facts(m: &mut Model, s: SeriesId) -> (FileId, Vec<Position>) {
    m.advance(MINUTE);
    let now = m.wall;
    let readings = (0..3)
        .map(|i| Reading::num(s, now + i * SECOND, 1.0))
        .collect();
    let positions = m.ingest(readings).unwrap().positions;
    m.seal_active();
    let f = *m
        .disk
        .hot
        .iter()
        .find(|f| {
            m.disk.files[f].class == FileClass::Segment
                && m.disk.files[f]
                    .entries()
                    .any(|e| e.position == positions[0])
        })
        .unwrap();
    (f, positions)
}

fn held(m: &Model, p: &[Position]) -> bool {
    let all = m.disk.all_entries();
    p.iter().all(|p| all.contains_key(p))
}

/// The latest finding of `f` as a corruption.
fn finding(m: &Model, f: FileId) -> Corruption {
    m.corruptions()
        .into_iter()
        .map(|c| c.id)
        .filter_map(|id| match id {
            // One no record names yet is the latest.
            Corruption::File(g, e) if g == f => Some((e.unwrap_or(u64::MAX), id)),
            _ => None,
        })
        .max()
        .expect("a finding")
        .1
}

fn only_loss(m: &Model) -> kvik_model::recovery::Loss {
    let losses: Vec<_> = m
        .losses()
        .into_iter()
        .filter(|l| l.kind == LossKind::DataLost)
        .collect();
    assert_eq!(losses.len(), 1, "{losses:?}");
    losses[0].clone()
}

/// A corrupt sealed segment with no copy: its loss is recorded at once.
fn lost_without_copy(repair: RepairConfig) -> (Model, FileId, Vec<Position>) {
    let (mut m, s) = store(repair);
    let (f, p) = sealed_facts(&mut m, s);
    m.corrupt_hot(f);
    assert_eq!(m.scrub(), vec![f]);
    (m, f, p)
}

#[test]
fn s8_self_repair_closes_a_loss_when_the_resend_wait_passes_and_health_returns_to_ok() {
    let (mut m, f, p) = lost_without_copy(RepairConfig::default());
    let recorded = m.wall;
    let l = only_loss(&m);
    let until = recorded + 24 * HOUR;
    assert_eq!(l.state, Some(LossState::AwaitingResend { until }));
    assert_eq!((l.first, l.last), (Some(p[0]), Some(p[2])));
    let requests = m.resend_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        (
            requests[0].identity,
            requests[0].first,
            requests[0].last,
            requests[0].until
        ),
        (p[0], p[0], p[2], until)
    );
    assert_eq!(m.health().level, Level::Degraded);
    m.advance(24 * HOUR - SECOND);
    assert_eq!(m.repair(), Ok(0), "the wait has not passed");
    m.advance(SECOND);
    assert!(m.resend_requests().is_empty());
    assert_eq!(m.repair(), Ok(1));
    let l = only_loss(&m);
    assert_eq!(l.state, Some(LossState::Closed(Outcome::ResendEnded)));
    assert!(l.acknowledged && !l.degrades());
    assert_eq!(m.health().level, Level::Ok);
    assert_eq!(m.attention(), vec![Alert::Corruption(finding(&m, f))]);
    assert_eq!(m.repair(), Ok(0), "closed once");
    m.restart();
    assert_eq!(
        only_loss(&m).state,
        Some(LossState::Closed(Outcome::ResendEnded))
    );
    assert_eq!(m.health().level, Level::Ok);
    assert_eq!(m.attention(), vec![Alert::Corruption(finding(&m, f))]);
}

#[test]
fn s8_self_repair_repair_done_ends_the_resend_stage() {
    let (mut m, _, p) = lost_without_copy(RepairConfig::default());
    m.repair_done(Position::new(0, 999)).unwrap();
    assert_eq!(
        m.resend_requests().len(),
        1,
        "an unknown loss writes nothing"
    );
    m.repair_done(p[0]).unwrap();
    assert_eq!(
        only_loss(&m).state,
        Some(LossState::Closed(Outcome::ResendEnded))
    );
    assert_eq!(m.health().level, Level::Ok);
    let records = m.disk.manifest.len();
    m.repair_done(p[0]).unwrap();
    assert_eq!(
        m.disk.manifest.len(),
        records,
        "a closed loss writes nothing"
    );
}

#[test]
fn s8_self_repair_closes_at_once_when_nothing_listens() {
    let (mut m, _, _) = lost_without_copy(RepairConfig {
        resend_wait: 0,
        ..RepairConfig::default()
    });
    assert!(m.resend_requests().is_empty(), "no request is offered");
    assert_eq!(m.repair(), Ok(1));
    assert_eq!(
        only_loss(&m).state,
        Some(LossState::Closed(Outcome::ResendEnded))
    );
}

#[test]
fn s8_self_repair_manual_waits_for_an_operator() {
    let (mut m, f, p) = lost_without_copy(RepairConfig {
        manual: true,
        ..RepairConfig::default()
    });
    m.repair_done(p[0]).unwrap();
    assert_eq!(only_loss(&m).state, Some(LossState::AwaitingOperator));
    assert_eq!(m.repair(), Ok(0));
    assert_eq!(m.health().level, Level::Degraded);
    // Held in memory only (Q-243).
    m.restart();
    assert!(matches!(
        only_loss(&m).state,
        Some(LossState::AwaitingResend { .. })
    ));
    m.advance(DAY);
    assert_eq!(only_loss(&m).state, Some(LossState::AwaitingOperator));
    assert_eq!(m.repair(), Ok(0));
    assert_eq!(m.health().level, Level::Degraded);
    m.acknowledge_losses(&[p[0]]).unwrap();
    assert_eq!(
        only_loss(&m).state,
        Some(LossState::Closed(Outcome::Operator))
    );
    assert_eq!(m.health().level, Level::Ok);
    assert!(
        m.attention().is_empty(),
        "acknowledging the loss names {f:?} seen"
    );
}

#[test]
fn s8_self_repair_restores_a_recorded_loss_in_full_from_its_copy() {
    let (mut m, s) = store(RepairConfig::default());
    let (f, p) = sealed_facts(&mut m, s);
    assert!(m.upload() > 0);
    // Found while the store cannot write: no copy can be swapped in.
    m.crash();
    m.corrupt_hot(f);
    assert_eq!(m.scrub(), vec![f]);
    assert_eq!(only_loss(&m).state, Some(LossState::Found));
    m.restart();
    let l = only_loss(&m);
    assert_eq!(l.state, Some(LossState::Copying));
    assert!(!held(&m, &p), "a recorded loss holds nothing");
    assert!(m.resend_requests().is_empty(), "a copy is tried first");
    assert_eq!(m.repair(), Ok(0));
    let h = m.health();
    assert_eq!(h.level, Level::Fatal);
    assert!(h.reasons.contains(&Reason::Quarantined(f)));
    assert!(m.restore_file(f));
    let l = only_loss(&m);
    assert_eq!(l.state, Some(LossState::Closed(Outcome::Restored(3))));
    assert!(held(&m, &p), "the positions hold the same facts again");
    assert_eq!(m.health().level, Level::Ok);
    assert_eq!(m.attention(), vec![Alert::Corruption(finding(&m, f))]);
    m.restart();
    let facts: Vec<Position> = m.facts().unwrap().iter().map(|f| f.position).collect();
    assert_eq!(facts, p);
}

#[test]
fn s8_self_repair_alerts_on_a_repaired_corruption_until_it_is_seen() {
    let (mut m, s) = store(RepairConfig::default());
    let (f, _) = sealed_facts(&mut m, s);
    m.upload();
    m.corrupt_hot(f);
    m.scrub();
    assert!(m.restore_file(f));
    assert!(
        m.losses().is_empty(),
        "a copy is tried before anything is recorded"
    );
    assert_eq!(m.health().level, Level::Ok);
    let alert = Alert::Corruption(finding(&m, f));
    assert_eq!(m.attention(), vec![alert]);
    m.restart();
    m.advance(40 * DAY);
    let _ = m.health();
    assert_eq!(
        m.attention(),
        vec![alert],
        "a restart, time and a read clear nothing"
    );
    m.acknowledge_corruptions(&[finding(&m, f)]).unwrap();
    assert!(m.attention().is_empty());
    let records = m.disk.manifest.len();
    m.acknowledge_corruptions(&[finding(&m, f)]).unwrap();
    assert_eq!(m.disk.manifest.len(), records, "seen once");
}

#[test]
fn s8_self_repair_repeated_corruption_makes_the_storage_suspect() {
    let (mut m, s) = store(RepairConfig::default());
    let (f, _) = sealed_facts(&mut m, s);
    let (g, _) = sealed_facts(&mut m, s);
    m.upload();
    m.corrupt_hot(f);
    m.scrub();
    m.restore_file(f);
    m.acknowledge_corruptions(&[finding(&m, f)]).unwrap();
    assert!(m.attention().is_empty(), "one corruption is not repeated");
    m.advance(29 * DAY);
    m.corrupt_hot(g);
    m.scrub();
    m.restore_file(g);
    m.acknowledge_corruptions(&[finding(&m, g)]).unwrap();
    assert_eq!(m.attention(), vec![Alert::StorageSuspect(2)]);
    let h = m.health();
    assert_eq!(h.level, Level::Degraded, "seen or not");
    assert_eq!(h.reasons, vec![Reason::StorageSuspect(2)]);
    m.advance(2 * DAY);
    assert!(m.attention().is_empty(), "the first is older than 30 days");
    assert_eq!(m.health().level, Level::Ok);
}

#[test]
fn s8_self_repair_a_second_corruption_of_the_same_file_alerts_and_counts() {
    let (mut m, s) = store(RepairConfig::default());
    let (f, p) = sealed_facts(&mut m, s);
    m.upload();
    m.corrupt_hot(f);
    m.scrub();
    assert!(m.restore_file(f));
    let first = finding(&m, f);
    m.acknowledge_corruptions(&[first]).unwrap();
    assert!(m.attention().is_empty());
    m.advance(7 * DAY);
    m.corrupt_hot(f);
    m.scrub();
    assert!(m.restore_file(f));
    let second = finding(&m, f);
    assert_ne!(first, second, "a new finding is a new corruption");
    assert_eq!(m.corruptions().len(), 2);
    let want = vec![Alert::Corruption(second), Alert::StorageSuspect(2)];
    assert_eq!(m.attention(), want);
    assert_eq!(m.health().reasons, vec![Reason::StorageSuspect(2)]);
    assert!(held(&m, &p));
    m.restart();
    assert_eq!(m.attention(), want);
    m.acknowledge_corruptions(&[second]).unwrap();
    assert_eq!(m.attention(), vec![Alert::StorageSuspect(2)]);
}

#[test]
fn s8_self_repair_a_start_records_a_second_finding_made_while_stopped() {
    let (mut m, s) = store(RepairConfig::default());
    let (f, _) = sealed_facts(&mut m, s);
    m.upload();
    m.corrupt_hot(f);
    m.scrub();
    assert!(m.restore_file(f));
    let first = finding(&m, f);
    m.acknowledge_corruptions(&[first]).unwrap();
    m.crash();
    m.corrupt_hot(f);
    m.scrub();
    assert_eq!(finding(&m, f), Corruption::File(f, None));
    m.restart();
    let second = finding(&m, f);
    assert!(matches!(second, Corruption::File(_, Some(_))) && second != first);
    assert_eq!(m.corruptions().len(), 2);
    assert!(m.attention().contains(&Alert::Corruption(second)));
}

#[test]
fn s8_self_repair_a_start_quarantines_by_record_what_was_found_while_stopped() {
    let (mut m, s) = store(RepairConfig::default());
    let (f, _) = sealed_facts(&mut m, s);
    m.crash();
    m.corrupt_hot(f);
    m.scrub();
    assert_eq!(m.corruptions()[0].at, None);
    m.restart();
    let quarantined = m.disk.manifest.iter().any(|r| {
        matches!(r.op, ManifestOp::Quarantine { file, cause: QuarantineCause::Damaged } if file == f)
    });
    assert!(quarantined);
    assert!(m.corruptions()[0].at.is_some());
}

/// An uploaded segment no longer on a local tier, lost by the bucket.
fn lost_in_bucket(local: bool) -> (Model, FileId, Vec<Position>) {
    let (mut m, s) = store(RepairConfig::default());
    let (f, p) = sealed_facts(&mut m, s);
    assert!(m.upload() > 0);
    if !local {
        m.disk.hot.remove(&f);
    }
    m.lose_object(f);
    (m, f, p)
}

#[test]
fn s8_self_repair_lost_object_needs_three_lookups_over_ten_minutes_while_the_bucket_answers() {
    let (mut m, f, _) = lost_in_bucket(false);
    // An unreachable bucket counts nothing.
    m.disk.bucket.reachable = false;
    for _ in 0..4 {
        assert_eq!(m.check_bucket(), Ok(vec![]));
        m.advance(10 * MINUTE);
    }
    m.disk.bucket.reachable = true;
    // Three lookups within a minute do not span ten.
    for _ in 0..3 {
        assert_eq!(m.check_bucket(), Ok(vec![]));
        m.advance(20 * SECOND);
    }
    assert!(m.losses().is_empty());
    m.restart();
    // Two lookups spanning ten minutes are not three.
    assert_eq!(m.check_bucket(), Ok(vec![]));
    m.advance(10 * MINUTE);
    assert_eq!(m.check_bucket(), Ok(vec![]));
    m.advance(MINUTE);
    assert_eq!(m.check_bucket(), Ok(vec![f]));
    let l = only_loss(&m);
    assert_eq!(l.cause, LossCause::LostInBucket);
    assert!(matches!(l.state, Some(LossState::AwaitingResend { .. })));
    assert_eq!(m.health().level, Level::Degraded);
    assert_eq!(m.attention(), vec![Alert::Corruption(finding(&m, f))]);
    assert_eq!(m.check_bucket(), Ok(vec![]), "judged once");
}

#[test]
fn s8_self_repair_a_lost_object_with_a_local_copy_is_uploaded_again() {
    let (mut m, f, p) = lost_in_bucket(true);
    for _ in 0..2 {
        m.check_bucket().unwrap();
        m.advance(5 * MINUTE);
    }
    assert_eq!(m.check_bucket(), Ok(vec![f]));
    assert!(m.losses().is_empty(), "a corruption repaired, no loss");
    assert!(m.disk.in_bucket(f));
    assert!(held(&m, &p));
    assert_eq!(m.health().level, Level::Ok);
    assert_eq!(m.attention(), vec![Alert::Corruption(finding(&m, f))]);
}

#[test]
fn s8_self_repair_a_closed_loss_stays_closed_when_its_copy_appears() {
    let (mut m, f, p) = lost_in_bucket(false);
    for _ in 0..2 {
        m.check_bucket().unwrap();
        m.advance(5 * MINUTE);
    }
    assert_eq!(m.check_bucket(), Ok(vec![f]));
    m.advance(DAY);
    assert_eq!(m.repair(), Ok(1));
    m.find_object(f);
    assert!(m.disk.in_bucket(f));
    assert!(!held(&m, &p), "a closed loss is not opened again (Q-246)");
    assert_eq!(
        only_loss(&m).state,
        Some(LossState::Closed(Outcome::ResendEnded))
    );
    m.restart();
    assert!(m.facts().unwrap().is_empty());
    assert_eq!(m.health().level, Level::Ok);
}

#[test]
fn s8_self_repair_a_closed_loss_is_not_restored_from_a_copy_found_later() {
    let (mut m, s) = store(RepairConfig::default());
    let (f, p) = sealed_facts(&mut m, s);
    m.upload();
    m.lose_object(f);
    m.corrupt_hot(f);
    m.scrub();
    assert_eq!(judge(&mut m), vec![f]);
    assert_eq!(only_loss(&m).cause, LossCause::LostInBucket);
    // The hot finding and the lost object are two corruptions (Q-247).
    let suspect = Reason::StorageSuspect(2);
    let h = m.health();
    assert_eq!(h.level, Level::Degraded, "no swap can happen");
    assert!(!h.reasons.contains(&Reason::Quarantined(f)));
    m.advance(DAY);
    assert_eq!(m.repair(), Ok(1));
    assert_eq!(m.health().reasons, vec![suspect.clone()]);
    m.find_object(f);
    m.check_bucket().unwrap();
    assert!(!m.restore_file(f), "Q-246");
    assert!(!held(&m, &p));
    assert_eq!(
        only_loss(&m).state,
        Some(LossState::Closed(Outcome::ResendEnded))
    );
    assert_eq!(
        m.health().reasons,
        vec![suspect],
        "the late copy holds no error"
    );
    m.crash();
    m.restart();
    m.advance(40 * DAY);
    assert_eq!(m.health().level, Level::Ok, "never fatal again");
}

#[test]
fn s8_self_repair_stage_1_waits_for_the_lost_object_rule_before_recording() {
    let (mut m, s) = store(RepairConfig::default());
    let (f, p) = sealed_facts(&mut m, s);
    m.upload();
    m.lose_object(f);
    m.corrupt_hot(f);
    m.scrub();
    m.check_bucket().unwrap();
    assert!(m.losses().is_empty(), "one absent lookup records nothing");
    assert!(m.disk.lost_ranges().is_empty());
    let h = m.health();
    assert_eq!(h.level, Level::Fatal);
    assert!(h.reasons.contains(&Reason::Quarantined(f)));
    m.find_object(f);
    assert!(m.restore_file(f));
    assert!(m.losses().is_empty(), "no loss, no reset point");
    assert!(held(&m, &p));
    assert_eq!(m.health().level, Level::Ok);
}

#[test]
fn s8_self_repair_no_copy_is_swapped_in_while_the_store_cannot_write() {
    let (mut m, s) = store(RepairConfig::default());
    let (f, p) = sealed_facts(&mut m, s);
    m.upload();
    m.crash();
    m.corrupt_hot(f);
    m.scrub();
    assert!(!m.restore_file(f), "a swap is a manifest record");
    m.restart();
    assert_eq!(only_loss(&m).state, Some(LossState::Copying));
    assert!(m.restore_file(f));
    assert_eq!(
        only_loss(&m).state,
        Some(LossState::Closed(Outcome::Restored(3)))
    );
    assert!(held(&m, &p));
    m.advance(DAY);
    assert_eq!(m.repair(), Ok(0));
    assert_eq!(m.health().level, Level::Ok);
}

#[test]
fn s8_self_repair_a_loss_restored_from_deep_and_damaged_again_with_deep_gone_is_open_again() {
    let (mut m, s) = store(RepairConfig::default());
    let (f, p) = sealed_facts(&mut m, s);
    m.upload();
    m.crash();
    m.corrupt_hot(f);
    m.scrub();
    m.restart();
    assert!(m.restore_file(f));
    let identity = only_loss(&m).identity;
    m.acknowledge_corruptions(&[finding(&m, f)]).unwrap();
    m.advance(7 * DAY);
    m.lose_object(f);
    m.corrupt_hot(f);
    m.scrub();
    assert_eq!(
        only_loss(&m).state,
        Some(LossState::Closed(Outcome::Restored(3))),
        "waits for the lost-object rule"
    );
    assert_eq!(m.health().level, Level::Fatal);
    assert_eq!(judge(&mut m), vec![f]);
    let l = only_loss(&m);
    assert_eq!(l.identity, identity);
    assert!(matches!(l.state, Some(LossState::AwaitingResend { .. })));
    assert_eq!(m.health().level, Level::Degraded);
    let unseen = m
        .attention()
        .iter()
        .filter(|a| matches!(a, Alert::Corruption(_)))
        .count();
    assert_eq!(unseen, 2, "the hot finding and the lost object");
    m.crash();
    m.restart();
    assert!(m.facts().unwrap().is_empty());
    assert!(!held(&m, &p));
}

#[test]
fn s8_self_repair_an_object_that_answers_again_restores_its_open_loss_in_full() {
    let (mut m, f, p) = lost_in_bucket(false);
    for _ in 0..2 {
        m.check_bucket().unwrap();
        m.advance(5 * MINUTE);
    }
    assert_eq!(m.check_bucket(), Ok(vec![f]));
    assert!(!held(&m, &p));
    m.find_object(f);
    assert_eq!(m.check_bucket(), Ok(vec![]));
    assert_eq!(
        only_loss(&m).state,
        Some(LossState::Closed(Outcome::Restored(3)))
    );
    assert!(held(&m, &p));
    assert_eq!(m.health().level, Level::Ok);
    assert_eq!(m.attention(), vec![Alert::Corruption(finding(&m, f))]);
}

/// Three lookups 5 minutes apart: the object is judged at the third.
fn judge(m: &mut Model) -> Vec<FileId> {
    let mut judged = Vec::new();
    for i in 0..3 {
        if i > 0 {
            m.advance(5 * MINUTE);
        }
        judged.extend(m.check_bucket().unwrap());
    }
    judged
}

#[test]
fn s8_self_repair_a_loss_restored_in_full_and_lost_again_is_open_again() {
    let (mut m, f, p) = lost_in_bucket(false);
    assert_eq!(judge(&mut m), vec![f]);
    let identity = only_loss(&m).identity;
    m.find_object(f);
    m.check_bucket().unwrap();
    assert_eq!(
        only_loss(&m).state,
        Some(LossState::Closed(Outcome::Restored(3)))
    );
    let first = finding(&m, f);
    m.acknowledge_corruptions(&[first]).unwrap();
    m.advance(7 * DAY);
    m.lose_object(f);
    assert_eq!(judge(&mut m), vec![f]);
    let l = only_loss(&m);
    assert_eq!(l.identity, identity, "the same loss");
    assert!(matches!(l.state, Some(LossState::AwaitingResend { .. })));
    assert!(!held(&m, &p));
    assert_eq!(m.health().level, Level::Degraded);
    let second = finding(&m, f);
    assert_ne!(second, first);
    assert!(m.attention().contains(&Alert::Corruption(second)));
    m.crash();
    m.restart();
    assert!(m.facts().unwrap().is_empty());
    assert!(only_loss(&m).degrades());
    m.find_object(f);
    m.check_bucket().unwrap();
    assert_eq!(
        only_loss(&m).state,
        Some(LossState::Closed(Outcome::Restored(3)))
    );
    assert!(held(&m, &p));
}

#[test]
fn s8_self_repair_an_operator_does_not_close_a_loss_whose_copy_is_coming() {
    let (mut m, s) = store(RepairConfig::default());
    let (f, p) = sealed_facts(&mut m, s);
    m.upload();
    m.crash();
    m.corrupt_hot(f);
    m.scrub();
    m.restart();
    assert_eq!(only_loss(&m).state, Some(LossState::Copying));
    m.acknowledge_losses(&[p[0]]).unwrap();
    assert_eq!(only_loss(&m).state, Some(LossState::Copying), "Q-248");
    assert!(m.attention().is_empty(), "its corruption is named seen");
    assert!(m.restore_file(f));
    assert_eq!(
        only_loss(&m).state,
        Some(LossState::Closed(Outcome::Restored(3)))
    );
    assert_eq!(m.health().level, Level::Ok);
}

#[test]
fn s8_self_repair_a_region_a_start_finds_alerts_by_its_loss_and_a_truncated_tail_does_not() {
    let (mut m, s) = store(RepairConfig::default());
    for i in 0..3 {
        m.ingest(vec![Reading::num(s, m.wall + i * SECOND, 1.0)])
            .unwrap();
    }
    m.crash();
    let active = m.disk.active.unwrap();
    let batch = m.disk.files[&active].batches.len() - 2;
    m.corrupt_open_batch(batch).unwrap();
    m.restart();
    let l = only_loss(&m);
    assert_eq!(l.cause, LossCause::Corruption);
    let alert = Alert::Corruption(Corruption::Range(l.identity));
    assert_eq!(m.attention(), vec![alert]);
    m.acknowledge_losses(&[l.identity]).unwrap();
    assert_eq!(
        only_loss(&m).state,
        Some(LossState::Closed(Outcome::Operator))
    );
    assert!(m.attention().is_empty());

    let consumer = ConsumerDecl {
        id: ConsumerId(1),
        selection: Selection::All,
        capacity: 16,
    };
    let config = Config {
        mark_sync: MarkSync::Timed,
        ..Config::default()
    };
    let mut m = Model::new(config, vec![consumer], T0 + HOUR);
    let s = m
        .define(
            "plant/line-1/power",
            Kind::Gauge,
            ValueType::Number,
            "kW",
            10 * SECOND,
        )
        .unwrap();
    for i in 0..3 {
        m.ingest(vec![Reading::num(s, T0 + i, 1.0)]).unwrap();
    }
    m.drain().unwrap();
    m.crash();
    m.lose_mark(MarkState::Missing).unwrap();
    assert_eq!(m.power_loss_losing(2), 2);
    m.restart();
    assert_eq!(only_loss(&m).cause, LossCause::TruncatedTail);
    assert!(m.corruptions().is_empty());
    assert!(m.attention().is_empty());
}

#[test]
fn s8_self_repair_corruptions_are_read_from_records_so_a_restart_or_growth_counts_once() {
    let (mut m, s) = store(RepairConfig::default());
    for i in 0..3 {
        m.ingest(vec![Reading::num(s, m.wall + i * SECOND, 1.0)])
            .unwrap();
    }
    m.crash();
    let active = m.disk.active.unwrap();
    let batch = m.disk.files[&active].batches.len() - 2;
    m.corrupt_open_batch(batch).unwrap();
    m.restart();
    let l = only_loss(&m);
    let alert = vec![Alert::Corruption(Corruption::Range(l.identity))];
    let before = m.corruptions();
    assert_eq!(before.len(), 1);
    m.restart();
    m.crash();
    m.restart();
    assert_eq!(m.corruptions(), before, "a restart reads the same records");
    assert_eq!(m.attention(), alert);
    let (first, last) = (l.first.unwrap(), l.last.unwrap());
    for (from, to) in [(first, last), (first, last.next())] {
        let epoch = m.disk.latest_epoch() + 1;
        m.disk.manifest.push(ManifestRecord {
            epoch,
            at: m.wall,
            op: ManifestOp::LostRange {
                from,
                to,
                identity: l.identity,
                cause: LossCause::Corruption,
                stamp: None,
            },
        });
        m.restart();
        assert_eq!(only_loss(&m).last, Some(to));
        assert_eq!(m.corruptions().len(), 1, "the same loss is one corruption");
        assert_eq!(m.attention(), alert, "no second alert, no repeated count");
    }
    m.acknowledge_corruptions(&[Corruption::Range(l.identity)])
        .unwrap();
    assert!(m.attention().is_empty());
}

#[test]
fn s8_self_repair_calls_are_refused_while_read_only() {
    let (mut m, _, p) = lost_without_copy(RepairConfig::default());
    m.disk.drill = true;
    m.restart();
    assert_eq!(m.repair(), Err(Error::ReadOnly));
    assert_eq!(m.repair_done(p[0]), Err(Error::ReadOnly));
    assert_eq!(m.check_bucket(), Err(Error::ReadOnly));
    assert_eq!(m.acknowledge_corruptions(&[]), Err(Error::ReadOnly));
}
