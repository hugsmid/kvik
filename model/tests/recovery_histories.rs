//! Property tests over crash and restart histories on a store with the timed
//! mark sync (spec §3 Recovery, §4 Resync, §11 Health): invariants 25–27, one
//! test each, and invariant 1 at every step of every history. Steps commit,
//! take and confirm deliveries, crash, lose power (losing acknowledged
//! batches above the last synced mark), remount, lose the mark or a cursor
//! file, damage a batch, fail a start's sync, acknowledge losses, compact,
//! upload, move files off the local tiers and let time pass. A second series
//! is at the `written` level: its batches are flagged (§3 Batches, Q-170).

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::{T0, seqs};
use kvik_model::engine::ConsumerId;
use kvik_model::recovery::LossKind;
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
    /// The same on the series at the `written` level: a flagged batch.
    IngestWritten(u8, Option<kvik_model::engine::Fault>),
    /// The log sync that `max_unsynced` bounds (§3 Commit).
    SyncLog,
    /// A power loss keeping this many flagged batches not yet synced.
    PowerLossKeeping(usize),
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
        8 => (1u8..4, proptest::option::weighted(0.15, fault.clone())).prop_map(|(n, f)| ROp::Ingest(n, f)),
        4 => (1u8..4, proptest::option::weighted(0.15, fault)).prop_map(|(n, f)| ROp::IngestWritten(n, f)),
        1 => Just(ROp::SyncLog),
        1 => (0usize..3).prop_map(ROp::PowerLossKeeping),
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
    ]
}

const RC: [ConsumerId; 2] = [ConsumerId(1), ConsumerId(2)];

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
    /// Starts on the same boot that kept flagged batches beyond the mark.
    flagged_kept: usize,
    /// Entries that are a `possible_loss` only for the `written` level.
    written_resets: usize,
    acknowledged: usize,
    possible: usize,
    data_lost: usize,
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
    fast: kvik_model::types::SeriesId,
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
            checks,
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
                | ROp::Crash
                | ROp::PowerLoss(_)
                | ROp::PowerLossKeeping(_)
                | ROp::SyncLog
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
                if self.m.acknowledge_losses(&ids).is_ok() && self.checks.inv27 {
                    let after = self.m.losses();
                    let strip = |v: &[kvik_model::recovery::Loss]| -> Vec<_> {
                        v.iter()
                            .map(|l| {
                                let mut l = l.clone();
                                l.acknowledged = false;
                                l
                            })
                            .collect()
                    };
                    // 27. Acknowledging changes only the health state.
                    prop_assert_eq!(strip(&before), strip(&after));
                    prop_assert!(after.iter().all(|l| !l.degrades()));
                    seen.acknowledged += after.iter().filter(|l| l.acknowledged).count();
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
    /// keeps one identity however its range grows, and acknowledging changes
    /// only the health state (§8 Acknowledging a loss, §11 Health).
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
/// rule 1 resets, flagged batches kept by a same-boot start, entries reported
/// only for the `written` level, acknowledged losses and both kinds of loss.
#[test]
fn recovery_generator_coverage() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let strat = proptest::collection::vec(arb_rop(), 80..120);
    let mut seen = Seen::default();
    let (mut possible, mut data_lost) = (0, 0);
    for _ in 0..200 {
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
        seen.flagged_kept += s.flagged_kept;
        seen.written_resets += s.written_resets;
        seen.acknowledged += s.acknowledged;
        possible += usize::from(s.possible > 0);
        data_lost += usize::from(s.data_lost > 0);
    }
    eprintln!(
        "reassigned {} forget-resyncs {} behind-resyncs {} forced-syncs {} stale {} gaps {} rule-1 {} flagged-kept {} written-resets {} acknowledged {} possible {possible} data-lost {data_lost}",
        seen.reassigned,
        seen.forget_resyncs,
        seen.behind_resyncs,
        seen.forced_syncs,
        seen.stale,
        seen.gaps,
        seen.rule_1_resets,
        seen.flagged_kept,
        seen.written_resets,
        seen.acknowledged
    );
    assert!(seen.reassigned > 5 && seen.forget_resyncs > 100 && seen.forced_syncs > 20);
    assert!(seen.behind_resyncs > 20);
    assert!(seen.stale > 20 && seen.gaps > 5 && seen.rule_1_resets > 20);
    assert!(seen.flagged_kept > 20 && seen.written_resets > 20);
    assert!(seen.acknowledged > 50 && possible > 100 && data_lost > 50);
}
