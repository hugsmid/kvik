//! Self-repair (spec §8 Self-repair): the loss life cycle, closing records,
//! the alert on every corruption, and the scheduled check of the bucket.

mod common;

use common::T0;
use kvik_model::engine::{
    Alert, Config, Corruption, Error, FileClass, FileId, Level, ManifestOp, Model, QuarantineCause,
    Reason, RepairConfig,
};
use kvik_model::recovery::{LossCause, LossKind, LossState, Outcome};
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
    assert_eq!(m.attention(), vec![Alert::Corruption(Corruption::File(f))]);
    assert_eq!(m.repair(), Ok(0), "closed once");
    m.restart();
    assert_eq!(
        only_loss(&m).state,
        Some(LossState::Closed(Outcome::ResendEnded))
    );
    assert_eq!(m.health().level, Level::Ok);
    assert_eq!(m.attention(), vec![Alert::Corruption(Corruption::File(f))]);
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
    assert_eq!(m.attention(), vec![Alert::Corruption(Corruption::File(f))]);
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
    let alert = Alert::Corruption(Corruption::File(f));
    assert_eq!(m.attention(), vec![alert]);
    m.restart();
    m.advance(40 * DAY);
    let _ = m.health();
    assert_eq!(
        m.attention(),
        vec![alert],
        "a restart, time and a read clear nothing"
    );
    m.acknowledge_corruptions(&[Corruption::File(f)]).unwrap();
    assert!(m.attention().is_empty());
    let records = m.disk.manifest.len();
    m.acknowledge_corruptions(&[Corruption::File(f)]).unwrap();
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
    m.acknowledge_corruptions(&[Corruption::File(f)]).unwrap();
    assert!(m.attention().is_empty(), "one corruption is not repeated");
    m.advance(29 * DAY);
    m.corrupt_hot(g);
    m.scrub();
    m.restore_file(g);
    m.acknowledge_corruptions(&[Corruption::File(g)]).unwrap();
    assert_eq!(m.attention(), vec![Alert::StorageSuspect(2)]);
    m.advance(2 * DAY);
    assert!(m.attention().is_empty(), "the first is older than 30 days");
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
    assert_eq!(m.attention(), vec![Alert::Corruption(Corruption::File(f))]);
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
    assert_eq!(m.attention(), vec![Alert::Corruption(Corruption::File(f))]);
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
fn s8_self_repair_calls_are_refused_while_read_only() {
    let (mut m, _, p) = lost_without_copy(RepairConfig::default());
    m.disk.drill = true;
    m.restart();
    assert_eq!(m.repair(), Err(Error::ReadOnly));
    assert_eq!(m.repair_done(p[0]), Err(Error::ReadOnly));
    assert_eq!(m.check_bucket(), Err(Error::ReadOnly));
    assert_eq!(m.acknowledge_corruptions(&[]), Err(Error::ReadOnly));
}
