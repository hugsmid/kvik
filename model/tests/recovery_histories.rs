//! Property tests over crash and restart histories on a store with the timed
//! mark sync (spec §3 Recovery, §4 Resync, §11 Health): invariants 25–27, one
//! test each, and invariant 1 at every step of every history. Steps commit,
//! take and confirm deliveries, crash, lose power (losing acknowledged
//! batches above the last synced mark), remount, lose the mark or a cursor
//! file, damage a batch, fail a start's sync, acknowledge losses, compact,
//! upload, move files off the local tiers and let time pass, and self-repair:
//! damage a sealed segment, lose an object in the bucket, swap copies in,
//! close losses and name corruptions seen.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::{T0, seqs};
use kvik_model::engine::ConsumerId;
use kvik_model::recovery::{Loss, LossCause, LossKind, LossState, Outcome};
use kvik_model::types::{DAY, Kind, Nanos, SECOND};
use proptest::prelude::*;

fn cases(n: u32) -> ProptestConfig {
    ProptestConfig {
        cases: std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(n),
        ..ProptestConfig::default()
    }
}

/// One step of a recovery history: commits on a store with the timed mark
/// sync, consumers taking and confirming deliveries, and every stop and fault
/// a start must recover from (§3 Recovery rules 1–3).
#[derive(Clone, Debug)]
enum ROp {
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
}

fn arb_rop() -> impl Strategy<Value = ROp> {
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
    ]
}

const RC: [ConsumerId; 2] = [ConsumerId(1), ConsumerId(2)];

/// The losses with their closing and state set aside: what invariant 27 says
/// acknowledging and closing never change.
fn strip(v: &[Loss]) -> Vec<Loss> {
    v.iter()
        .map(|l| {
            let mut l = l.clone();
            l.acknowledged = false;
            l.state = None;
            l
        })
        .collect()
}

/// What a recovery history saw, for the properties and their guard.
#[derive(Default)]
struct Seen {
    /// Every fact each consumer received, by position: a different fact at
    /// a position received before is a reassigned position.
    facts: BTreeMap<(usize, kvik_model::types::Position), kvik_model::types::Fact>,
    reassigned: usize,
    forget_resyncs: usize,
    behind_resyncs: usize,
    forced_syncs: usize,
    stale: usize,
    gaps: usize,
    rule_1_resets: usize,
    acknowledged: usize,
    possible: usize,
    data_lost: usize,
    closed_by_repair: usize,
    restored: usize,
    lost_in_bucket: usize,
    judged_lost: usize,
    corruptions_seen: usize,
    /// Every recorded corruption reported, with whether it was seen.
    corruptions: BTreeMap<kvik_model::engine::Corruption, bool>,
    /// The step names corruptions as seen.
    acknowledging: bool,
    /// Every loss listed, by identity and kind, with its range.
    losses: BTreeMap<
        (kvik_model::types::Position, bool),
        (
            Option<kvik_model::types::Position>,
            Option<kvik_model::types::Position>,
        ),
    >,
}

/// The invariants a history checks; invariant 1 is checked at every step of
/// every history.
#[derive(Clone, Copy)]
struct Checks {
    inv25: bool,
    inv26: bool,
    inv27: bool,
}

const ALL: Checks = Checks {
    inv25: true,
    inv26: true,
    inv27: true,
};

struct RWorld {
    m: kvik_model::engine::Model,
    s: kvik_model::types::SeriesId,
    t: Nanos,
    defs: usize,
    last: [Option<kvik_model::engine::DeliveryId>; 2],
    checks: Checks,
}

impl RWorld {
    fn new(checks: Checks) -> Self {
        use kvik_model::engine::{Config, ConsumerDecl, MarkSync, Model, Selection};
        let consumers = RC
            .iter()
            .map(|id| ConsumerDecl {
                id: *id,
                selection: Selection::All,
                capacity: 16,
            })
            .collect();
        let cfg = Config {
            mark_sync: MarkSync::Timed,
            seal_after_batches: 5,
            ..Config::default()
        };
        let mut m = Model::new(cfg, consumers, T0 + 3600 * SECOND);
        let s = m
            .define(
                "plant/line-1/temperature",
                Kind::Gauge,
                kvik_model::types::ValueType::Number,
                "degC",
                5 * SECOND,
            )
            .unwrap();
        RWorld {
            m,
            s,
            t: T0,
            defs: 0,
            last: [None, None],
            checks,
        }
    }

    /// A lost range recorded at run time is followed by a restart: the model
    /// sends no reset point for it in the session (Q-33 requirement 1, "the
    /// writer restarts recovery when it records one").
    fn restart_after_run_time_loss(&mut self, before: usize) {
        if self.m.disk.lost_ranges().len() > before {
            self.m.crash();
            self.m.restart();
            self.last = [None, None];
        }
    }

    fn down(&mut self) {
        if self.m.is_up() {
            self.m.crash();
        }
    }

    /// Applies one step and checks invariants 25 (at every delivery taken)
    /// and 26 and 27 (after every step).
    fn apply(&mut self, op: &ROp, seen: &mut Seen) -> Result<(), TestCaseError> {
        use kvik_model::recovery::MarkState;
        let entries = self.m.recovery_entries().len();
        let mark_boot = self.m.disk.mark.map(|m| m.boot);
        // A step that needs the engine starts it first, so histories spend
        // most of their time up.
        let needs_up = !matches!(
            op,
            ROp::SyncMark
                | ROp::Upload
                | ROp::Advance(_)
                | ROp::Minutes(_)
                | ROp::LoseObject(_)
                | ROp::CorruptSealed(_)
                | ROp::Evict
                | ROp::Crash
                | ROp::PowerLoss(_)
                | ROp::Remount
                | ROp::LoseMark(_)
                | ROp::LoseMarkAndBatches(_)
                | ROp::LoseCursor { .. }
                | ROp::Corrupt(_)
                | ROp::Restart(_)
        );
        if needs_up && !self.m.is_up() {
            self.m.restart();
            self.last = [None, None];
        }
        let up = self.m.is_up();
        match op {
            ROp::Ingest(n, fault) if up => {
                for _ in 0..*n {
                    self.t += SECOND;
                    let _ = self
                        .m
                        .emit(kvik_model::types::Reading::num(self.s, self.t, 1.0));
                }
                let _ = self.m.commit(*fault);
            }
            ROp::Define if up => {
                self.defs += 1;
                let _ = self.m.define(
                    &format!("plant/line-1/extra-{}", self.defs),
                    Kind::Gauge,
                    kvik_model::types::ValueType::Number,
                    "1",
                    5 * SECOND,
                );
            }
            ROp::SyncMark => self.m.sync_mark_file(),
            ROp::Take { c, n } if up => {
                // The engine's catch-up runs as the consumer takes.
                self.m.catch_up(RC[*c], 32).unwrap();
                let got = self.m.receive(RC[*c], *n).unwrap();
                let cursor = self.m.disk.cursors.get(&RC[*c]).copied();
                let points = self.m.reset_points();
                for d in &got {
                    if let Some(r) = d.resync()
                        && r.forget_above.is_some()
                    {
                        if r.replaced.is_some() {
                            seen.behind_resyncs += 1;
                        } else {
                            seen.forget_resyncs += 1;
                        }
                    }
                    let Some(f) = d.fact() else { continue };
                    // 25. Nothing above a reset point's `forget_above`
                    // before its `resync` is confirmed and the cursor naming
                    // its id is on disk.
                    let applies = |r: &&kvik_model::recovery::ResetPoint| {
                        self.checks.inv25 && r.forget_above < f.position
                    };
                    for rp in points.iter().filter(applies) {
                        prop_assert!(
                            cursor.is_some_and(|c| c >= rp.id),
                            "{:?} delivered {:?} past {:?} with cursor {:?}",
                            RC[*c],
                            f.position,
                            rp,
                            cursor
                        );
                    }
                    if let Some(old) = seen.facts.insert((*c, f.position), f.clone())
                        && old != *f
                    {
                        seen.reassigned += 1;
                    }
                }
                if let Some(d) = got.last() {
                    self.last[*c] = Some(d.id());
                }
            }
            ROp::Confirm { c } if up => {
                if let Some(id) = self.last[*c].take() {
                    let before = self.m.disk.synced_mark;
                    let r = self.m.confirm_delivery(RC[*c], id);
                    if r.is_ok() && self.m.disk.synced_mark != before {
                        seen.forced_syncs += 1;
                    }
                }
            }
            ROp::CatchUp { c } if up => self.m.catch_up(RC[*c], 32).unwrap(),
            ROp::Reconnect { c } if up => {
                self.m.reconnect(RC[*c]).unwrap();
                self.last[*c] = None;
            }
            ROp::Crash => self.m.crash(),
            ROp::PowerLoss(n) => {
                self.m.power_loss_losing(*n);
            }
            ROp::Remount => self.m.remount(),
            ROp::LoseMark(missing) => {
                self.down();
                let state = if *missing {
                    MarkState::Missing
                } else {
                    MarkState::Unreadable
                };
                self.m.lose_mark(state).unwrap();
            }
            ROp::LoseMarkAndBatches(n) => {
                self.down();
                self.m.lose_mark(MarkState::Missing).unwrap();
                self.m.power_loss_losing(*n);
            }
            ROp::LoseCursor { c } => {
                self.down();
                self.m.lose_cursor_file(RC[*c]).unwrap();
            }
            ROp::Corrupt(b) => {
                self.down();
                if let Some(f) = self.m.disk.active {
                    let n = self.m.disk.files[&f].batches.len().max(1);
                    let _ = self.m.corrupt_open_batch(b % n);
                }
            }
            ROp::Restart(fails) => {
                self.m.restart_with(*fails);
                self.last = [None, None];
            }
            ROp::Acknowledge if up => {
                let ids: Vec<_> = self.m.losses().iter().map(|l| l.identity).collect();
                let before = self.m.losses();
                seen.acknowledging = true;
                if self.m.acknowledge_losses(&ids).is_ok() && self.checks.inv27 {
                    let after = self.m.losses();
                    // 27. Acknowledging changes only the state and health.
                    prop_assert_eq!(strip(&before), strip(&after));
                    prop_assert!(after.iter().all(|l| !l.degrades()));
                    seen.acknowledged += after.iter().filter(|l| l.acknowledged).count();
                }
            }
            ROp::Repair if up => {
                let before = self.m.losses();
                if let Ok(n) = self.m.repair()
                    && self.checks.inv27
                {
                    let after = self.m.losses();
                    // 27. A loss closed by self-repair stays listed; closing
                    // changes only its state and health.
                    prop_assert_eq!(strip(&before), strip(&after));
                    let closed = |v: &[Loss]| v.iter().filter(|l| l.acknowledged).count();
                    prop_assert_eq!(closed(&after), closed(&before) + n);
                    seen.closed_by_repair += n;
                }
            }
            ROp::RepairDone if up => {
                if let Some(r) = self.m.resend_requests().first()
                    && !self.m.is_read_only()
                {
                    let before = self.m.losses();
                    self.m.repair_done(r.identity).unwrap();
                    let after = self.m.losses();
                    prop_assert_eq!(strip(&before), strip(&after));
                    let l = after.iter().find(|l| l.identity == r.identity).unwrap();
                    prop_assert_eq!(l.state, Some(LossState::Closed(Outcome::ResendEnded)));
                    seen.closed_by_repair += 1;
                }
            }
            ROp::AcknowledgeCorruptions if up => {
                let ids: Vec<_> = self.m.corruptions().iter().map(|c| c.id).collect();
                seen.acknowledging = true;
                let before = self.m.losses();
                if self.m.acknowledge_corruptions(&ids).is_ok() {
                    prop_assert_eq!(
                        before,
                        self.m.losses(),
                        "naming corruptions seen closes nothing"
                    );
                    seen.corruptions_seen += ids.len();
                }
            }
            ROp::Minutes(n) => self.m.advance(i64::from(*n) * 60 * SECOND),
            ROp::Evict => {
                let verified: Vec<_> = self
                    .m
                    .disk
                    .hot
                    .iter()
                    .copied()
                    .filter(|f| {
                        let file = &self.m.disk.files[f];
                        file.sealed
                            && file.class == kvik_model::engine::FileClass::Segment
                            && self.m.disk.in_bucket(*f)
                    })
                    .collect();
                for f in verified {
                    self.m.disk.hot.remove(&f);
                }
            }
            ROp::CorruptSealed(k) => {
                let mut sealed: Vec<_> = self
                    .m
                    .disk
                    .hot
                    .iter()
                    .copied()
                    .filter(|f| {
                        let file = &self.m.disk.files[f];
                        file.sealed && file.class == kvik_model::engine::FileClass::Segment
                    })
                    .collect();
                // Segments with a copy in the bucket first, so that stage 1
                // is reached often.
                sealed.sort_by_key(|f| !self.m.disk.in_bucket(*f));
                if let Some(f) = sealed.get(k % sealed.len().max(1)) {
                    let lost = self.m.disk.lost_ranges().len();
                    self.m.corrupt_hot(*f);
                    self.m.scrub();
                    self.restart_after_run_time_loss(lost);
                }
            }
            ROp::RestoreFiles if up => {
                let q: Vec<_> = self.m.disk.quarantined.iter().copied().collect();
                for f in q {
                    let before = self.m.losses();
                    self.m.restore_file(f);
                    let after = self.m.losses();
                    prop_assert_eq!(strip(&before), strip(&after));
                    seen.restored += after
                        .iter()
                        .filter(|l| {
                            matches!(l.state, Some(LossState::Closed(Outcome::Restored(_))))
                        })
                        .count();
                }
            }
            ROp::LoseObject(k) => {
                let objects: Vec<_> = self.m.disk.bucket.objects.keys().copied().collect();
                if let Some(f) = objects.get(k % objects.len().max(1)) {
                    self.m.lose_object(*f);
                }
            }
            ROp::CheckBucket(n) if up => {
                let lost = self.m.disk.lost_ranges().len();
                for i in 0..*n {
                    if i > 0 {
                        self.m.advance(5 * 60 * SECOND);
                    }
                    if let Ok(judged) = self.m.check_bucket() {
                        seen.judged_lost += judged.len();
                    }
                }
                self.restart_after_run_time_loss(lost);
                seen.lost_in_bucket = seen.lost_in_bucket.max(
                    self.m
                        .losses()
                        .iter()
                        .filter(|l| l.cause == LossCause::LostInBucket)
                        .count(),
                );
            }
            ROp::Seal if up => self.m.seal_active(),
            ROp::Compact if up => {
                let _ = self.m.compact(None);
            }
            ROp::Merge if up => {
                let _ = self.m.merge_chunks(None);
            }
            ROp::Housekeeping if up => {
                let _ = self.m.housekeeping();
            }
            ROp::Upload => {
                self.m.upload();
            }
            ROp::Advance(days) => self.m.advance(i64::from(*days) * DAY),
            ROp::Checkpoint if up => {
                let _ = self.m.checkpoint();
            }
            _ => {}
        }
        if !matches!(op, ROp::Take { .. }) {
            self.last = self.last.map(|l| l.filter(|_| self.m.is_up()));
        }
        let acknowledging = std::mem::take(&mut seen.acknowledging);
        if self.checks.inv27 && self.m.is_up() {
            // A corruption's alert ends only when a record names it as seen
            // (Q-240): never by a restart, time or its repair.
            let now: BTreeMap<_, _> = self
                .m
                .corruptions()
                .into_iter()
                .map(|c| (c.id, (c.seen, c.at.is_some())))
                .collect();
            for (id, was_seen) in &seen.corruptions {
                let Some((is_seen, _)) = now.get(id) else {
                    return Err(TestCaseError::fail(format!("corruption {id:?} lost")));
                };
                prop_assert!(!was_seen || *is_seen, "{:?} unseen again", id);
                prop_assert!(
                    *was_seen || !is_seen || acknowledging,
                    "{:?} seen by itself",
                    id
                );
            }
            let alerts = self.m.attention();
            for (id, (is_seen, recorded)) in now {
                prop_assert_eq!(
                    alerts.contains(&kvik_model::engine::Alert::Corruption(id)),
                    !is_seen
                );
                if recorded {
                    seen.corruptions.insert(id, is_seen);
                }
            }
        }
        for (p, e) in self.m.recovery_entries().into_iter().skip(entries) {
            seen.stale += usize::from(e.mark_state == MarkState::Stale);
            seen.gaps += usize::from(e.gap(p).is_some());
            seen.rule_1_resets +=
                usize::from(e.mark_state == MarkState::Readable && mark_boot == Some(e.boot));
        }
        self.check(seen)
    }

    fn check(&self, seen: &mut Seen) -> Result<(), TestCaseError> {
        let m = &self.m;
        if !m.is_up() {
            return Ok(());
        }
        // 26. No cursor file, derived file or manifest record (but a start's
        // lost-range record) names a position above the last synced mark.
        if let Some(synced) = m.disk.synced_mark.filter(|_| self.checks.inv26) {
            let g = m.disk.generation;
            let named = m
                .disk
                .cursors
                .values()
                .copied()
                .chain(m.disk.latches.values().map(|l| l.position))
                .chain(m.disk.checkpoints.iter().map(|c| c.position))
                .chain(
                    m.disk
                        .manifest
                        .iter()
                        .filter(|r| {
                            !matches!(r.op, kvik_model::engine::ManifestOp::LostRange { .. })
                        })
                        .flat_map(|r| m.disk.named(r)),
                );
            for p in named.filter(|p| p.generation == g) {
                prop_assert!(
                    p <= synced.last,
                    "{:?} named above the synced mark {:?}",
                    p,
                    synced
                );
            }
        }
        // 1. Gap-free except lost ranges, each recorded in the manifest or
        // described by a recovery entry (the positions between K and F),
        // checked at every step of every recovery history.
        // Every complete batch on disk, beyond the mark too (after a start
        // that could not write, with the mark lost).
        let mut held: BTreeSet<u64> = seqs(
            m.disk
                .files
                .values()
                .filter(|f| f.class == kvik_model::engine::FileClass::Segment)
                .flat_map(|f| f.entries().map(|e| e.position)),
        )
        .into_iter()
        .collect();
        // A range a start that could not write found is reported as found
        // and recorded by the next start that can.
        let found = m
            .disk
            .found_lost
            .iter()
            .copied()
            .chain(m.disk.damaged_runs());
        for (from, to) in m.disk.lost_ranges().into_iter().chain(found) {
            held.extend(from.sequence..=to.sequence);
        }
        let top = held.iter().next_back().copied().unwrap_or(0);
        prop_assert_eq!(held.len() as u64, top, "a gap that no lost range holds");
        // 27. The list of losses never loses a loss, each keeps one identity
        // and its range only grows; health is degraded by exactly the
        // unacknowledged `data_lost`.
        let losses = m.losses();
        let mut keys = BTreeSet::new();
        for l in losses.iter().filter(|_| self.checks.inv27) {
            let key = (l.identity, l.kind == LossKind::PossibleLoss);
            prop_assert!(keys.insert(key), "listed twice: {:?}", l);
            if let Some((first, last)) = seen.losses.get(&key) {
                prop_assert!(l.first <= *first || first.is_none() && l.first.is_none());
                prop_assert!(l.last >= *last);
            }
            // A range reported as found by a start that could not write is
            // in no file until a start records it, and a later start may
            // report it under its own entry (Q-124 (d)); a recorded loss is
            // never lost.
            if l.recorded {
                seen.losses.insert(key, (l.first, l.last));
            }
        }
        for key in seen.losses.keys().filter(|_| self.checks.inv27) {
            prop_assert!(keys.contains(key), "loss {:?} no longer listed", key);
        }
        seen.possible = seen.possible.max(
            losses
                .iter()
                .filter(|l| l.kind == LossKind::PossibleLoss)
                .count(),
        );
        seen.data_lost = seen.data_lost.max(
            losses
                .iter()
                .filter(|l| l.kind == LossKind::DataLost)
                .count(),
        );
        if self.checks.inv27 {
            let degraded = m
                .health()
                .reasons
                .iter()
                .filter(|r| matches!(r, kvik_model::engine::Reason::DataLost(..)))
                .count();
            prop_assert_eq!(degraded, losses.iter().filter(|l| l.degrades()).count());
            for l in &losses {
                let closed = matches!(l.state, Some(LossState::Closed(_)));
                prop_assert_eq!(closed, l.acknowledged);
                prop_assert_eq!(l.state.is_none(), l.kind == LossKind::PossibleLoss);
            }
        }
        Ok(())
    }
}

fn run_recovery(ops: &[ROp], seen: &mut Seen, checks: Checks) -> Result<(), TestCaseError> {
    let mut w = RWorld::new(checks);
    for op in ops {
        w.apply(op, seen)?;
    }
    // Every consumer can still drain to the end: a reset point waits for its
    // confirmation, never for ever.
    w.m.restart();
    w.m.drain()
        .map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
    let mut seen_end = Seen::default();
    w.check(&mut seen_end)
}

proptest! {
    #![proptest_config(cases(256))]

    /// 25. No position above a reset point's `forget_above` is delivered to a
    /// consumer whose position is below the reset point's id before the
    /// `resync` carrying it has been confirmed and its cursor synced (§4
    /// Resync), across crashes, power losses that lose acknowledged and
    /// delivered batches, remounts, lost marks and cursor files and
    /// corruption.
    #[test]
    fn inv25_nothing_above_forget_above_before_the_reset_point_is_confirmed(
        ops in proptest::collection::vec(arb_rop(), 1..120)
    ) {
        run_recovery(&ops, &mut Seen::default(), Checks { inv25: true, inv26: false, inv27: false })?;
    }

    /// 26. No cursor file or derived file is written naming a position above
    /// the last synced mark, nor any manifest record except a lost-range
    /// record written by a start (§3 Durable mark), under the timed mark
    /// sync.
    #[test]
    fn inv26_nothing_names_a_position_above_the_last_synced_mark(
        ops in proptest::collection::vec(arb_rop(), 1..120)
    ) {
        run_recovery(&ops, &mut Seen::default(), Checks { inv25: false, inv26: true, inv27: false })?;
    }

    /// 27. The list of losses never loses a loss across restarts, each loss
    /// keeps one identity however its range grows, acknowledging or closing a
    /// loss changes only its state and the health state, and a loss closed
    /// by self-repair stays listed (§8 Self-repair, Acknowledging a loss,
    /// §11 Health); a corruption alerts until a record names it seen.
    #[test]
    fn inv27_losses_are_never_lost_and_acknowledging_changes_only_health(
        ops in proptest::collection::vec(arb_rop(), 1..120)
    ) {
        run_recovery(&ops, &mut Seen::default(), Checks { inv25: false, inv26: false, inv27: true })?;
    }
}

/// Guards the recovery-history properties (invariants 25–27): positions
/// delivered and then assigned again after a reset, `resync`s carrying
/// `forget_above`, confirmations that forced a mark sync, stale marks, gaps,
/// rule 1 resets, acknowledged losses and both kinds of loss.
#[test]
fn recovery_generator_coverage() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let strat = proptest::collection::vec(arb_rop(), 80..120);
    let mut seen = Seen::default();
    let (mut possible, mut data_lost) = (0, 0);
    for _ in 0..300 {
        let ops = strat.new_tree(&mut runner).unwrap().current();
        let mut s = Seen::default();
        run_recovery(&ops, &mut s, ALL).unwrap();
        seen.reassigned += s.reassigned;
        seen.forget_resyncs += s.forget_resyncs;
        seen.behind_resyncs += s.behind_resyncs;
        seen.forced_syncs += s.forced_syncs;
        seen.stale += s.stale;
        seen.gaps += s.gaps;
        seen.rule_1_resets += s.rule_1_resets;
        seen.acknowledged += s.acknowledged;
        seen.closed_by_repair += s.closed_by_repair;
        seen.restored += s.restored;
        seen.lost_in_bucket += s.lost_in_bucket;
        seen.judged_lost += s.judged_lost;
        seen.corruptions_seen += s.corruptions_seen;
        possible += usize::from(s.possible > 0);
        data_lost += usize::from(s.data_lost > 0);
    }
    eprintln!(
        "reassigned {} forget-resyncs {} behind-resyncs {} forced-syncs {} stale {} gaps {} rule-1 {} acknowledged {} possible {possible} data-lost {data_lost}",
        seen.reassigned,
        seen.forget_resyncs,
        seen.behind_resyncs,
        seen.forced_syncs,
        seen.stale,
        seen.gaps,
        seen.rule_1_resets,
        seen.acknowledged
    );
    assert!(seen.reassigned > 7 && seen.forget_resyncs > 150 && seen.forced_syncs > 30);
    assert!(seen.behind_resyncs > 30);
    assert!(seen.stale > 30 && seen.gaps > 7 && seen.rule_1_resets > 30);
    assert!(seen.acknowledged > 75 && possible > 150 && data_lost > 75);
    eprintln!(
        "closed-by-repair {} restored {} judged-lost {} lost-in-bucket {} corruptions-seen {}",
        seen.closed_by_repair,
        seen.restored,
        seen.judged_lost,
        seen.lost_in_bucket,
        seen.corruptions_seen
    );
    assert!(seen.closed_by_repair > 20 && seen.corruptions_seen > 20);
    assert!(seen.restored > 0 && seen.judged_lost > 5 && seen.lost_in_bucket > 0);
}
