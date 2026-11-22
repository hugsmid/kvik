//! A loss folded into a lower identity keeps what its records gave (Q-1102, Q-1108).

use kvik_model::engine::{Alert, Corruption, ManifestOp, ManifestRecord, Model, RepairConfig};
use kvik_model::recovery::{LossCause, LossKind};
use kvik_model::types::Position;

use super::store;

/// Appends a lost-range record, as a start that found the range would.
fn lost(m: &mut Model, identity: u64, from: u64, to: u64) {
    let epoch = m.disk.latest_epoch() + 1;
    m.disk.manifest.push(ManifestRecord {
        epoch,
        at: m.wall,
        op: ManifestOp::LostRange {
            from: Position::new(0, from),
            to: Position::new(0, to),
            identity: Position::new(0, identity),
            cause: LossCause::Corruption,
            stamp: None,
        },
    });
}

#[test]
fn a_fold_keeps_each_absorbed_loss_s_corruption_and_the_count() {
    let (mut m, _) = store(RepairConfig::default());
    lost(&mut m, 8, 8, 9);
    lost(&mut m, 10, 10, 11);
    m.restart();
    let before = m.corruptions();
    assert_eq!(before.len(), 2);
    assert!(m.attention().contains(&Alert::StorageSuspect(2)));
    lost(&mut m, 8, 8, 11);
    m.restart();
    let data: Vec<_> = m
        .losses()
        .into_iter()
        .filter(|l| l.kind == LossKind::DataLost)
        .collect();
    let p = |s| Position::new(0, s);
    assert_eq!(data.len(), 1, "{data:?}");
    assert_eq!(
        (data[0].identity, data[0].absorbed.as_slice()),
        (p(8), &[p(10)][..])
    );
    let after = m.corruptions();
    let ids: Vec<_> = after.iter().map(|c| (c.id, c.loss)).collect();
    assert_eq!(
        ids,
        [
            (Corruption::Range(p(8)), Some(p(8))),
            (Corruption::Range(p(10)), Some(p(8)))
        ]
    );
    assert!(after.iter().zip(&before).all(|(a, b)| a.at == b.at));
    assert!(m.attention().contains(&Alert::StorageSuspect(2)));
    m.acknowledge_losses(&[p(8)]).unwrap();
    assert!(m.corruptions().iter().all(|c| c.seen));
    assert_eq!(m.attention(), [Alert::StorageSuspect(2)]);
}

/// The one `data_lost` loss's identity, what it absorbed and whether it is acknowledged.
fn one(m: &Model) -> (Position, Vec<Position>, bool) {
    let data: Vec<_> = m
        .losses()
        .into_iter()
        .filter(|l| l.kind == LossKind::DataLost)
        .collect();
    assert_eq!(data.len(), 1, "{data:?}");
    (
        data[0].identity,
        data[0].absorbed.clone(),
        data[0].acknowledged,
    )
}

/// The closing records written, as the identities each names.
fn closings(m: &Model) -> Vec<Vec<Position>> {
    m.disk
        .manifest
        .iter()
        .filter_map(|r| match &r.op {
            ManifestOp::Acknowledge { losses, .. } => {
                Some(losses.iter().map(|a| a.identity).collect())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn acknowledgements_of_two_losses_carry_over_a_bridge_that_adds_no_position() {
    let p = |s| Position::new(0, s);
    let (mut m, _) = store(RepairConfig::default());
    lost(&mut m, 8, 8, 9);
    lost(&mut m, 10, 10, 11);
    m.restart();
    m.acknowledge_losses(&[p(8), p(10)]).unwrap();
    lost(&mut m, 8, 8, 11);
    m.restart();
    assert_eq!(one(&m), (p(8), vec![p(10)], true));
    let before = closings(&m);
    m.acknowledge_losses(&[p(10)]).unwrap();
    assert_eq!(closings(&m), before);
    assert_eq!(one(&m), (p(8), vec![p(10)], true));
}

#[test]
fn an_absorbed_identity_given_to_acknowledge_losses_acknowledges_nothing() {
    let p = |s| Position::new(0, s);
    let (mut m, _) = store(RepairConfig::default());
    lost(&mut m, 8, 8, 9);
    lost(&mut m, 10, 10, 11);
    m.restart();
    m.acknowledge_losses(&[p(8)]).unwrap();
    lost(&mut m, 8, 8, 11);
    m.restart();
    assert_eq!(one(&m), (p(8), vec![p(10)], false));
    let before = closings(&m);
    m.acknowledge_losses(&[p(10)]).unwrap();
    let named: Vec<_> = closings(&m).into_iter().filter(|c| !c.is_empty()).collect();
    assert_eq!(
        named, before,
        "at most the absorbed loss's corruption is seen"
    );
    assert_eq!(one(&m), (p(8), vec![p(10)], false));
    m.acknowledge_losses(&[p(8)]).unwrap();
    assert_eq!(one(&m), (p(8), vec![p(10)], true));
}
