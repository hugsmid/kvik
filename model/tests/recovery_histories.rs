//! Property tests over crash and restart histories on a store with the timed
//! mark sync (spec §3 Recovery, §4 Resync, §11 Health): invariants 25–27, one
//! test each, and invariant 1 at every step of every history. Steps commit,
//! take and confirm deliveries, crash, lose power (losing acknowledged
//! batches above the last synced mark), remount, lose the mark or a cursor
//! file, damage a batch, fail a start's sync, acknowledge losses, compact,
//! upload, move files off the local tiers and let time pass, and self-repair:
//! damage a sealed segment, lose an object in the bucket, swap copies in,
//! close losses and name corruptions seen. A second series is at the `written`
//! level: its batches are flagged (§3 Batches, Q-170).

mod common;
#[path = "recovery_histories/ops.rs"]
mod ops;

use std::collections::{BTreeMap, BTreeSet};

use common::{T0, seqs};
use kvik_model::engine::ConsumerId;
use kvik_model::recovery::{Loss, LossCause, LossKind, LossState, Outcome};
use kvik_model::types::{DAY, Kind, Nanos, SECOND};
use ops::{ROp, arb_rop};
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

const RC: [ConsumerId; 2] = [ConsumerId(1), ConsumerId(2)];

/// The losses with their closing and state set aside: what invariant 27 says
/// acknowledging and closing never change.
/// How many losses are closed as restored in full.
fn restored(v: &[Loss]) -> usize {
    v.iter()
        .filter(|l| matches!(l.state, Some(LossState::Closed(Outcome::Restored(_)))))
        .count()
}

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
    /// The facts each consumer received that no `resync` since has told it
    /// to forget (`forget_above` below their position).
    known: BTreeMap<(usize, kvik_model::types::Position), kvik_model::types::Fact>,
    reassigned: usize,
    /// Reassigned positions that held a fact of the `written` series.
    flagged_reassigned: usize,
    /// Remounts with flagged batches not yet synced.
    flagged_remounts: usize,
    forget_resyncs: usize,
    behind_resyncs: usize,
    /// Starts accepted, and `NoCursor` resyncs received after them.
    starts: usize,
    no_cursor: usize,
    forced_syncs: usize,
    stale: usize,
    gaps: usize,
    rule_1_resets: usize,
    /// Starts on the same boot that kept flagged batches beyond the mark.
    flagged_kept: usize,
    /// Entries that are a `possible_loss` only for the `written` level.
    written_resets: usize,
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
    /// Positions of sealed log segments readable at some step.
    readable: BTreeSet<kvik_model::types::Position>,
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
    /// §4 Resync, principle 5: a position is given again only after a
    /// `resync` whose `forget_above` lies below it.
    reuse: bool,
}

const NONE: Checks = Checks {
    inv25: false,
    inv26: false,
    inv27: false,
    reuse: false,
};

const ALL: Checks = Checks {
    inv25: true,
    inv26: true,
    inv27: true,
    reuse: true,
};

struct RWorld {
    m: kvik_model::engine::Model,
    s: kvik_model::types::SeriesId,
    fast: kvik_model::types::SeriesId,
    t: Nanos,
    defs: usize,
    last: [Option<kvik_model::engine::DeliveryId>; 2],
    /// Each consumer's cursor after its last confirmation, with the stamp it was confirmed under.
    saved: [Option<kvik_model::recovery::SavedPoint>; 2],
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
        let fast = m
            .define_with(
                "market/feed/price",
                Kind::Gauge,
                kvik_model::types::ValueType::Number,
                "1",
                5 * SECOND,
                kvik_model::registry::SeriesOptions::durability(
                    kvik_model::registry::Durability::Written,
                ),
            )
            .unwrap();
        RWorld {
            m,
            s,
            fast,
            t: T0,
            defs: 0,
            last: [None, None],
            saved: [None, None],
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

    /// The scheduled check, `n` times 5 minutes apart.
    fn check_bucket(&mut self, n: usize, seen: &mut Seen) -> Result<(), TestCaseError> {
        let lost = self.m.disk.lost_ranges().len();
        let in_bucket = |m: &kvik_model::engine::Model| {
            m.losses()
                .iter()
                .filter(|l| l.cause == LossCause::LostInBucket)
                .count()
        };
        let before = in_bucket(&self.m);
        for i in 0..n {
            if i > 0 {
                self.m.advance(5 * 60 * SECOND);
            }
            let corruptions = self.m.corruptions().len();
            if let Ok(judged) = self.m.check_bucket() {
                seen.judged_lost += judged.len();
                // Each object judged lost is a corruption of its own.
                prop_assert_eq!(self.m.corruptions().len(), corruptions + judged.len());
            }
        }
        seen.lost_in_bucket += in_bucket(&self.m) - before;
        self.restart_after_run_time_loss(lost);
        Ok(())
    }

    /// Starts consumer `c` now, at a time, or at its last saved point, by `kind` modulo 3.
    fn start(&mut self, c: usize, kind: u8, seen: &mut Seen) {
        use kvik_model::engine::Start;
        let lost = self.m.cursor_lost(RC[c]);
        let start = match kind % 3 {
            0 => Start::Now,
            1 => Start::Time(self.m.wall - Nanos::from(kind % 4) * SECOND),
            _ => Start::Point(self.saved[c].unwrap_or(kvik_model::recovery::SavedPoint {
                position: kvik_model::types::Position::ORIGIN,
                stamp: kvik_model::recovery::ResetStamp::ZERO,
            })),
        };
        if self.m.start_consumer(RC[c], start).is_ok() {
            seen.starts += 1;
            self.last[c] = None;
            // A new consumer remembers nothing; a lost one forgets by its `NoCursor` resync.
            if !lost {
                seen.known.retain(|(k, _), _| *k != c);
            }
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
        let pending = self.pending_flagged();
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
                | ROp::DamageStoppedRestore(_)
                | ROp::Evict
                | ROp::Crash
                | ROp::PowerLoss(_)
                | ROp::PowerLossKeeping(_)
                | ROp::SyncLog
                | ROp::Remount(_)
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
            ROp::IngestWritten(n, fault) if up => {
                for _ in 0..*n {
                    self.t += SECOND;
                    let _ = self
                        .m
                        .emit(kvik_model::types::Reading::num(self.fast, self.t, 1.0));
                }
                let _ = self.m.commit(*fault);
            }
            ROp::SyncLog => self.m.sync_log(),
            ROp::PowerLossKeeping(k) => {
                if self.m.is_up() {
                    self.m.power_loss_keeping(*k);
                }
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
                // Taking a handle starts a consumer whose cursor file was lost, here from the origin (Q-131).
                if self.m.cursor_lost(RC[*c]) {
                    self.saved[*c] = None;
                    self.start(*c, 2, seen);
                }
                // The engine's catch-up runs as the consumer takes.
                self.m.catch_up(RC[*c], 32).unwrap();
                let got = self.m.receive(RC[*c], *n).unwrap();
                let cursor = self.m.disk.cursors.get(&RC[*c]).copied();
                let points = self.m.reset_points();
                for d in &got {
                    if let Some(x) = d.resync().and_then(|r| r.forget_above) {
                        seen.known.retain(|(k, p), _| *k != *c || *p <= x);
                    }
                    if d.resync()
                        .is_some_and(|r| r.cause == kvik_model::engine::ResyncCause::NoCursor)
                    {
                        seen.no_cursor += 1;
                    }
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
                    if let Some(old) = seen.known.insert((*c, f.position), f.clone()) {
                        prop_assert!(
                            !self.checks.reuse || old == *f,
                            "{:?} given again to {:?} with no forget_above below it: {:?}, then {:?}",
                            f.position,
                            RC[*c],
                            old,
                            f
                        );
                    }
                    if let Some(old) = seen.facts.insert((*c, f.position), f.clone())
                        && old != *f
                    {
                        seen.reassigned += 1;
                        seen.flagged_reassigned += usize::from(old.series == self.fast);
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
                    if r.is_ok() {
                        self.saved[*c] = self.m.disk.cursors.get(&RC[*c]).map(|&position| {
                            kvik_model::recovery::SavedPoint {
                                position,
                                stamp: self.m.stamp(),
                            }
                        });
                    }
                }
            }
            ROp::CatchUp { c } if up => self.m.catch_up(RC[*c], 32).unwrap(),
            ROp::Start { c, kind } if up => self.start(*c, *kind, seen),
            ROp::Reconnect { c } if up => {
                self.m.reconnect(RC[*c]).unwrap();
                self.last[*c] = None;
            }
            ROp::Crash => self.m.crash(),
            ROp::PowerLoss(n) => {
                self.m.power_loss_losing(*n);
            }
            ROp::Remount(keep) => {
                let unsynced = self.unsynced_flagged();
                seen.flagged_remounts += usize::from(unsynced > 0);
                // A remount that loses an acknowledged flagged batch is
                // Q-250's open case, excluded from the reuse property: there
                // every unsynced batch reached the disk.
                let keep = if self.checks.reuse && unsynced > *keep {
                    usize::MAX
                } else {
                    *keep
                };
                self.m.remount_keeping(keep);
            }
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
                    // A loss whose copy is being brought back stays open
                    // (Q-248); every other closes.
                    for l in &after {
                        let copying = before.iter().any(|b| {
                            b.identity == l.identity
                                && b.kind == l.kind
                                && b.state == Some(LossState::Copying)
                        });
                        prop_assert_eq!(l.degrades(), copying, "{:?}", l);
                    }
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
                    seen.restored += restored(&after) - restored(&before);
                }
            }
            ROp::LoseObject(k) => {
                let objects: Vec<_> = self.m.disk.bucket.objects.keys().copied().collect();
                if let Some(f) = objects.get(k % objects.len().max(1)) {
                    self.m.lose_object(*f);
                }
            }
            ROp::CheckBucket(n) if up => self.check_bucket(usize::from(*n), seen)?,
            ROp::LoseEvicted(k) if up => {
                let uploaded: Vec<_> = self
                    .m
                    .disk
                    .bucket
                    .objects
                    .keys()
                    .copied()
                    .filter(|f| !self.m.disk.quarantined.contains(f))
                    .collect();
                if let Some(f) = uploaded.get(k % uploaded.len().max(1)) {
                    self.m.disk.hot.remove(f);
                    self.m.lose_object(*f);
                    self.check_bucket(3, seen)?;
                }
            }
            ROp::DamageStoppedRestore(k) => {
                let uploaded: Vec<_> = self
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
                            && !self.m.disk.corrupt.contains(f)
                    })
                    .collect();
                if let Some(f) = uploaded.get(k % uploaded.len().max(1)) {
                    self.down();
                    self.m.corrupt_hot(*f);
                    self.m.scrub();
                    self.m.restart();
                    self.last = [None, None];
                    self.check(seen)?;
                    let before = self.m.losses();
                    self.m.restore_file(*f);
                    seen.restored += restored(&self.m.losses()) - restored(&before);
                }
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
        if let Some(last) = pending
            && self.m.is_up()
            && self
                .m
                .disk
                .mark
                .is_some_and(|m| Some(m.boot) == mark_boot && m.last >= last)
        {
            seen.flagged_kept += 1;
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
            seen.written_resets += usize::from(
                e.written_level
                    && e.tail.is_empty()
                    && e.mark_state == MarkState::Readable
                    && e.cursors_missing.is_empty(),
            );
            seen.stale += usize::from(e.mark_state == MarkState::Stale);
            seen.gaps += usize::from(e.gap(p).is_some());
            seen.rule_1_resets +=
                usize::from(e.mark_state == MarkState::Readable && mark_boot == Some(e.boot));
        }
        self.check(seen)
    }

    /// Flagged batches beyond the page cache's mark: written, not yet synced
    /// (§3 Commit).
    fn unsynced_flagged(&self) -> usize {
        let d = &self.m.disk;
        let (Some(f), false) = (d.active, d.cache_lost) else {
            return 0;
        };
        let floor = d
            .mark
            .map_or(0, |m| if m.file == f { m.batches } else { 0 });
        d.flagged
            .iter()
            .filter(|(g, b)| *g == f && *b >= floor)
            .count()
    }

    /// The last position of the flagged batches beyond the mark while the
    /// engine is down on the boot that wrote them, which a start keeps (§3
    /// Recovery rule 1).
    fn pending_flagged(&self) -> Option<kvik_model::types::Position> {
        let d = &self.m.disk;
        let (f, mark) = (d.active?, d.mark?);
        if self.m.is_up() || mark.boot != self.m.boot {
            return None;
        }
        let floor = if mark.file == f { mark.batches } else { 0 };
        let batches = &d.files[&f].batches;
        let run = (floor..batches.len()).take_while(|b| d.flagged.contains(&(f, *b)));
        run.last()
            .and_then(|b| batches[b].last())
            .map(|e| e.position)
    }

    fn check(&self, seen: &mut Seen) -> Result<(), TestCaseError> {
        let m = &self.m;
        // §3 Batches: a flagged batch holds only facts of a series at the
        // `written` level (none once damaged), so no flag outlives the batch
        // it was written for.
        for (f, b) in &m.disk.flagged {
            let batch = m.disk.files.get(f).and_then(|x| x.batches.get(*b));
            let facts_of_fast = batch.is_some_and(|x| {
                x.iter()
                    .all(|e| e.fact().is_some_and(|ft| ft.series == self.fast))
            });
            prop_assert!(
                facts_of_fast,
                "flag on {:?} batch {} that holds {:?}",
                f,
                b,
                batch
            );
        }
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
        // A position of a sealed log segment once readable and no longer
        // readable lies in a loss that is open or closed otherwise than
        // restored in full, or waits for the lost-object rule (§8
        // Self-repair).
        if self.checks.inv27 {
            let all = m.disk.all_entries();
            let sealed: Vec<(kvik_model::engine::FileId, kvik_model::types::Position)> = m
                .disk
                .files
                .iter()
                .filter(|(_, f)| f.sealed && f.class == kvik_model::engine::FileClass::Segment)
                .flat_map(|(id, f)| f.entries().map(move |e| (*id, e.position)))
                .collect();
            seen.readable.extend(
                sealed
                    .iter()
                    .map(|(_, p)| *p)
                    .filter(|p| all.contains_key(p)),
            );
            let losses = m.losses();
            for p in seen.readable.iter().filter(|p| !all.contains_key(p)) {
                let open = losses.iter().any(|l| {
                    l.kind == LossKind::DataLost
                        && l.first.zip(l.last).is_some_and(|(a, b)| a <= *p && *p <= b)
                        && !matches!(l.state, Some(LossState::Closed(Outcome::Restored(_))))
                });
                // Any file holding it, a compacted one included.
                let pending = m.disk.files.iter().any(|(f, file)| {
                    m.disk.bucket.missing.contains(f)
                        && !m.disk.judged_lost.contains(f)
                        && file.entries().any(|e| e.position == *p)
                });
                prop_assert!(open || pending, "{:?} unreadable outside an open loss", p);
            }
        }
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
        run_recovery(&ops, &mut Seen::default(), Checks { inv25: true, ..NONE })?;
    }

    /// 26. No cursor file or derived file is written naming a position above
    /// the last synced mark, nor any manifest record except a lost-range
    /// record written by a start (§3 Durable mark), under the timed mark
    /// sync.
    #[test]
    fn inv26_nothing_names_a_position_above_the_last_synced_mark(
        ops in proptest::collection::vec(arb_rop(), 1..120)
    ) {
        run_recovery(&ops, &mut Seen::default(), Checks { inv26: true, ..NONE })?;
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
        run_recovery(&ops, &mut Seen::default(), Checks { inv27: true, ..NONE })?;
    }
}

proptest! {
    #![proptest_config(cases(256))]

    /// Principle 5 and §4 Resync: a consumer never receives a different fact
    /// at a position it received before unless a `resync` whose
    /// `forget_above` lies below that position came between, across crashes,
    /// power losses and remounts over flagged batches not yet synced, except
    /// a remount that loses one (Q-250, excluded).
    #[test]
    fn s4_a_position_is_given_again_only_after_a_forget_above_below_it(
        ops in proptest::collection::vec(arb_rop(), 1..120)
    ) {
        run_recovery(&ops, &mut Seen::default(), Checks { reuse: true, ..NONE })?;
    }
}

#[test]
fn recovery_history_a_compacted_object_lost_while_read_only_waits_for_the_lost_object_rule() {
    let ops = [
        ROp::PowerLoss(0),
        ROp::Ingest(1, None),
        ROp::Ingest(1, None),
        ROp::Define,
        ROp::Compact,
        ROp::CorruptSealed(0),
        ROp::Upload,
        ROp::Restart(true),
        ROp::LoseEvicted(0),
    ];
    run_recovery(&ops, &mut Seen::default(), ALL).unwrap();
}

#[test]
fn recovery_history_entries_in_a_quarantined_segment_keep_their_reset_points_before_a_catch_up() {
    let ops = [
        ROp::LoseMark(false),
        ROp::Take { c: 0, n: 4 },
        ROp::PowerLossKeeping(0),
        ROp::Ingest(1, None),
        ROp::Upload,
        ROp::Ingest(1, None),
        ROp::LoseObject(0),
        ROp::PowerLossKeeping(0),
        ROp::Ingest(1, None),
        ROp::PowerLossKeeping(0),
        ROp::Ingest(1, None),
        ROp::PowerLossKeeping(0),
        ROp::Define,
        ROp::Ingest(1, None),
        ROp::Ingest(1, None),
        ROp::Ingest(1, None),
        ROp::CorruptSealed(3),
        ROp::CatchUp { c: 0 },
        ROp::Compact,
        ROp::Take { c: 0, n: 4 },
    ];
    run_recovery(&ops, &mut Seen::default(), ALL).unwrap();
}

#[test]
fn recovery_history_an_entry_in_a_damaged_sealed_segment_keeps_its_reset_point_after_a_failed_sync()
{
    let ops = [
        ROp::Ingest(2, None),
        ROp::Take { c: 1, n: 4 },
        ROp::PowerLoss(1),
        ROp::Ingest(1, None),
        ROp::Ingest(1, None),
        ROp::Ingest(
            1,
            Some(kvik_model::engine::Fault::SyncError(
                kvik_model::engine::WriteOutcome::Lost,
            )),
        ),
        ROp::CorruptSealed(0),
        ROp::Take { c: 1, n: 4 },
    ];
    run_recovery(&ops, &mut Seen::default(), ALL).unwrap();
}

#[test]
fn a_recovery_entry_in_a_quarantined_segment_awaiting_its_copy_stays_a_reset_point() {
    let ops = [
        ROp::Ingest(2, None),
        ROp::Take { c: 0, n: 4 },
        ROp::PowerLoss(1),
        ROp::Ingest(1, None),
        ROp::Ingest(1, None),
        ROp::Upload,
        ROp::LoseObject(0),
        ROp::CorruptSealed(0),
        ROp::Take { c: 0, n: 4 },
    ];
    run_recovery(
        &ops,
        &mut Seen::default(),
        Checks {
            reuse: true,
            ..NONE
        },
    )
    .unwrap();
}

#[test]
fn a_recovery_entry_in_a_recorded_lost_range_gives_only_the_range_reset_point() {
    let ops = [
        ROp::Ingest(2, None),
        ROp::Take { c: 0, n: 4 },
        ROp::PowerLoss(1),
        ROp::Ingest(1, None),
        ROp::Ingest(1, None),
        ROp::Upload,
        ROp::LoseObject(0),
        ROp::CorruptSealed(0),
        ROp::CheckBucket(3),
    ];
    let mut w = RWorld::new(NONE);
    let mut seen = Seen::default();
    for op in &ops {
        w.apply(op, &mut seen).unwrap();
    }
    let at = |n| kvik_model::types::Position::new(0, n);
    let (from, to) = (at(1), at(5));
    assert!(w.m.recovery_entries().contains_key(&at(3)));
    assert_eq!(w.m.disk.lost_ranges(), vec![(from, to)]);
    assert_eq!(
        w.m.reset_points(),
        vec![kvik_model::recovery::ResetPoint {
            id: to,
            forget_above: at(0),
        }]
    );
}

/// The durable backend's history in `kvik/tests/durable_reset_points.rs`: the segment holding the recovery entry is
/// sealed, the engine stops, and the segment is damaged or removed (Q-1033).
fn entry_segment_harmed(remove: bool) {
    let mut w = RWorld::new(Checks {
        reuse: true,
        ..NONE
    });
    let mut seen = Seen::default();
    for op in [
        ROp::Ingest(2, None),
        ROp::Take { c: 0, n: 4 },
        ROp::PowerLoss(1),
        ROp::Ingest(1, None),
        ROp::Ingest(1, None),
        ROp::Seal,
        ROp::Crash,
    ] {
        w.apply(&op, &mut seen).unwrap();
    }
    let entry = *w.m.recovery_entries().keys().next().unwrap();
    let f =
        *w.m.disk
            .files
            .iter()
            .find(|(_, x)| x.sealed && x.entries().any(|e| e.position == entry))
            .unwrap()
            .0;
    if remove {
        w.m.disk.hot.remove(&f);
    } else {
        w.m.corrupt_hot(f);
        w.m.scrub();
    }
    w.apply(&ROp::Take { c: 0, n: 16 }, &mut seen).unwrap();
    assert!(seen.forget_resyncs + seen.behind_resyncs > 0);
}

#[test]
fn a_recovery_entry_in_a_damaged_sealed_segment_stays_a_reset_point() {
    entry_segment_harmed(false);
}

#[test]
fn a_recovery_entry_in_a_removed_sealed_segment_stays_a_reset_point() {
    entry_segment_harmed(true);
}

/// Guards the recovery-history properties (invariants 25–27 and reuse):
/// positions delivered and then assigned again after a reset (flagged ones
/// among them), `resync`s carrying `forget_above`, confirmations that forced
/// a mark sync, stale marks, gaps, rule 1 resets, flagged batches kept by a
/// same-boot start, remounts over flagged batches not yet synced, entries
/// reported only for the `written` level, acknowledged losses, both kinds of
/// loss, losses closed by self-repair and restored in full, objects judged
/// lost and losses of cause lost in the bucket.
#[test]
fn recovery_generator_coverage() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let strat = proptest::collection::vec(arb_rop(), 80..120);
    let mut seen = Seen::default();
    let (mut possible, mut data_lost) = (0, 0);
    for _ in 0..450 {
        let ops = strat.new_tree(&mut runner).unwrap().current();
        let mut s = Seen::default();
        run_recovery(&ops, &mut s, ALL).unwrap();
        seen.reassigned += s.reassigned;
        seen.forget_resyncs += s.forget_resyncs;
        seen.behind_resyncs += s.behind_resyncs;
        seen.starts += s.starts;
        seen.no_cursor += s.no_cursor;
        seen.forced_syncs += s.forced_syncs;
        seen.stale += s.stale;
        seen.gaps += s.gaps;
        seen.rule_1_resets += s.rule_1_resets;
        seen.flagged_kept += s.flagged_kept;
        seen.flagged_reassigned += s.flagged_reassigned;
        seen.flagged_remounts += s.flagged_remounts;
        seen.written_resets += s.written_resets;
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
        "reassigned {} forget-resyncs {} behind-resyncs {} forced-syncs {} stale {} gaps {} rule-1 {} flagged-kept {} written-resets {} flagged-reassigned {} flagged-remounts {} acknowledged {} possible {possible} data-lost {data_lost}",
        seen.reassigned,
        seen.forget_resyncs,
        seen.behind_resyncs,
        seen.forced_syncs,
        seen.stale,
        seen.gaps,
        seen.rule_1_resets,
        seen.flagged_kept,
        seen.written_resets,
        seen.flagged_reassigned,
        seen.flagged_remounts,
        seen.acknowledged
    );
    assert!(seen.reassigned > 7 && seen.forget_resyncs > 1500 && seen.forced_syncs > 30);
    assert!(seen.behind_resyncs > 200);
    assert!(seen.starts > 100 && seen.no_cursor > 10);
    assert!(seen.stale > 60 && seen.gaps > 7 && seen.rule_1_resets > 180);
    assert!(seen.flagged_kept > 20 && seen.written_resets > 20);
    assert!(seen.flagged_reassigned > 0 && seen.flagged_remounts > 20);
    assert!(seen.acknowledged > 225 && possible > 150 && data_lost > 125);
    eprintln!(
        "closed-by-repair {} restored {} judged-lost {} lost-in-bucket {} corruptions-seen {}",
        seen.closed_by_repair,
        seen.restored,
        seen.judged_lost,
        seen.lost_in_bucket,
        seen.corruptions_seen
    );
    assert!(seen.closed_by_repair > 35 && seen.corruptions_seen > 100);
    assert!(seen.restored > 30 && seen.judged_lost > 50 && seen.lost_in_bucket > 20);
}
