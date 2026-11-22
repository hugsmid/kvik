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
