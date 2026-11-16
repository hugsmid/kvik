//! The steps of a recovery history and their generator.

use proptest::prelude::*;

/// One step of a recovery history: commits on a store with the timed mark
/// sync, consumers taking and confirming deliveries, and every stop and fault
/// a start must recover from (§3 Recovery rules 1–3).
#[derive(Clone, Debug)]
pub enum ROp {
    /// Readings committed in one batch, with a fault or none.
    Ingest(u8, Option<kvik_model::engine::Fault>),
    /// A definition: a batch that is not only facts forces a mark sync.
    Define,
    /// The timed mark sync fires.
    SyncMark,
    Take {
        c: usize,
        n: usize,
    },
    Confirm {
        c: usize,
    },
    CatchUp {
        c: usize,
    },
    Reconnect {
        c: usize,
    },
    Crash,
    /// A power loss losing up to this many acknowledged batches above the
    /// last synced mark.
    PowerLoss(usize),
    Remount,
    LoseMark(bool),
    /// The mark is lost, then a power loss loses up to this many batches,
    /// below positions a cursor or a derived file may name.
    LoseMarkAndBatches(usize),
    LoseCursor {
        c: usize,
    },
    Corrupt(usize),
    /// Restart; with `true` the start's sync fails.
    Restart(bool),
    Acknowledge,
    Seal,
    Compact,
    Merge,
    Housekeeping,
    Checkpoint,
    /// Every closed segment is uploaded to the bucket.
    Upload,
    /// The clocks move on this many days, so tier moves take log segments
    /// off the local tiers and cursors fall behind the oldest local segment.
    Advance(u16),
    /// The clocks move on this many minutes.
    Minutes(u16),
    /// The hot copy of a sealed segment is damaged and a scrub runs, with
    /// the engine up or stopped.
    CorruptSealed(usize),
    /// Fill pressure moves every sealed segment verified in the bucket off
    /// the fast disk.
    Evict,
    /// Every quarantined file is swapped for its copy where one exists.
    RestoreFiles,
    /// The bucket loses the object of an uploaded segment.
    LoseObject(usize),
    /// The scheduled check of the bucket, this many times, 5 minutes apart.
    CheckBucket(u8),
    /// The self-repair step closes every loss whose re-send stage ended.
    Repair,
    /// The application ends the re-send stage of the first open request.
    RepairDone,
    /// Every corruption is named as seen.
    AcknowledgeCorruptions,
    /// An uploaded segment leaves the fast disk, the bucket loses it, and
    /// the scheduled check runs three times 5 minutes apart.
    LoseEvicted(usize),
    /// An uploaded segment is damaged while the engine is stopped, a start
    /// records the range found, and the copy is swapped in.
    DamageStoppedRestore(usize),
}

pub fn arb_rop() -> impl Strategy<Value = ROp> {
    use kvik_model::engine::{CrashPoint, Fault, WriteOutcome};
    let outcome = prop_oneof![
        Just(WriteOutcome::Lost),
        Just(WriteOutcome::Torn),
        Just(WriteOutcome::Survived)
    ];
    let fault = prop_oneof![
        outcome
            .clone()
            .prop_map(|o| Fault::Crash(CrashPoint::AfterWrite(o))),
        Just(Fault::Crash(CrashPoint::AfterSyncBeforeMark)),
        Just(Fault::Crash(CrashPoint::BeforeWrite)),
        outcome.clone().prop_map(Fault::PowerLoss),
        outcome.prop_map(Fault::SyncError),
    ];
    prop_oneof![
        8 => (1u8..4, proptest::option::weighted(0.15, fault)).prop_map(|(n, f)| ROp::Ingest(n, f)),
        1 => Just(ROp::Define),
        1 => Just(ROp::SyncMark),
        12 => (0usize..2, 4usize..16).prop_map(|(c, n)| ROp::Take { c, n }),
        10 => (0usize..2).prop_map(|c| ROp::Confirm { c }),
        2 => (0usize..2).prop_map(|c| ROp::CatchUp { c }),
        1 => (0usize..2).prop_map(|c| ROp::Reconnect { c }),
        1 => Just(ROp::Crash),
        2 => (0usize..4).prop_map(ROp::PowerLoss),
        2 => Just(ROp::Remount),
        1 => any::<bool>().prop_map(ROp::LoseMark),
        1 => (1usize..4).prop_map(ROp::LoseMarkAndBatches),
        1 => (0usize..2).prop_map(|c| ROp::LoseCursor { c }),
        1 => (0usize..8).prop_map(ROp::Corrupt),
        1 => proptest::bool::weighted(0.1).prop_map(ROp::Restart),
        1 => Just(ROp::Acknowledge),
        1 => Just(ROp::Seal),
        1 => Just(ROp::Compact),
        1 => Just(ROp::Merge),
        1 => Just(ROp::Housekeeping),
        1 => Just(ROp::Checkpoint),
        1 => Just(ROp::Upload),
        1 => prop_oneof![Just(1u16), Just(100)].prop_map(ROp::Advance),
        4 => prop_oneof![
            1 => prop_oneof![Just(5u16), Just(30)].prop_map(ROp::Minutes),
            2 => (0usize..4).prop_map(ROp::CorruptSealed),
            1 => Just(ROp::Evict),
            1 => Just(ROp::RestoreFiles),
            1 => (0usize..4).prop_map(ROp::LoseObject),
            2 => (1u8..4).prop_map(ROp::CheckBucket),
            1 => Just(ROp::Repair),
            1 => Just(ROp::RepairDone),
            1 => Just(ROp::AcknowledgeCorruptions),
        ],
        1 => prop_oneof![
            (0usize..4).prop_map(ROp::LoseEvicted),
            (0usize..4).prop_map(ROp::DamageStoppedRestore),
        ],
    ]
}
