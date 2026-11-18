//! The series directory's entries and the status file's content, supplied to `kvik-maint` through the seal hook
//! (`format.md` A.18; the query design's section 4). [`SeriesFold`] folds every durable batch the deliverer's
//! [`super::index::Index`] takes into bounded per-series state, in position order, and answers as of a position at
//! or below the last synced mark: batches above the mark known when they arrive wait in a short queue.
//!
//! Per series: the latest recorded `t` and the lateness reference (spec §2.2), the fact count and orphans (§2.1),
//! the live candidate (the latest current fact that is not bad and not ahead of the write time, §2.3) and the next
//! pending fact. Where bounded state cannot tell, the entry says so ([`SeriesEntry::candidate_unsure`],
//! [`SeriesEntry::pending_more`]) and a reader finds the fact in force instead.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use kvik_log::content::Item;
use kvik_log::directory::series::{Candidate, CandidateValue, Pending, SeriesEntry};
use kvik_log::directory::status::{counter, health, reason};
use kvik_log::entries::{Entry, Versioning};
use kvik_log::facts::{FactRow, Quality, Value};
use kvik_log::manifest::LostRange;
use kvik_log::reader::ReadBatch;
use kvik_log::writer::{Position as LogPos, SealHook, SeriesEntries, StatusContent, Watch};

use super::loss::Latch;
use crate::health::LossKind;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A fact as the fold keeps it: enough to rank it and to lay out a candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Kept {
    t: i64,
    position: LogPos,
    changed_at: Option<u64>,
    quality: Quality,
    value: CandidateValue,
}

impl Kept {
    fn rank(&self, versioned: bool) -> (Option<u64>, LogPos) {
        (
            if versioned { self.changed_at } else { None },
            self.position,
        )
    }
}

/// One series' bounded state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct State {
    latest_t: Option<i64>,
    reference: Option<i64>,
    facts: u64,
    candidate: Option<Kept>,
    /// The candidate was corrected to bad: unsure until one with a later `t` than this replaces it.
    unsure_until: Option<i64>,
    /// Of the good facts ahead of the horizon, the earliest and the latest; `pending_more`: others lie between.
    pending: Option<Kept>,
    last_ahead: Option<Kept>,
    pending_more: bool,
    /// A kept fact ahead was corrected to bad: facts ahead may exist that the state does not bound.
    ahead_lost: bool,
    /// Versioned series: bad facts above the candidate's `t`, the highest rank at each, so that a good fact a bad one
    /// outranks is never taken; `bad_unknown`: bad facts not kept (past [`BAD_KEPT`], or before a continued file)
    /// may lie at or below this `t`.
    bad: BTreeMap<i64, (Option<u64>, LogPos)>,
    bad_unknown: Option<i64>,
}

/// The most bad facts above the candidate a versioned series' state keeps.
const BAD_KEPT: usize = 16;

/// A fact's value as a candidate's, or `None` for a fact recorded without one (bad).
fn value_of(v: &Value) -> Option<CandidateValue> {
    Some(match v {
        Value::F64(b) => CandidateValue::F64(*b),
        Value::Decimal { units, scale } => CandidateValue::Decimal {
            units: *units,
            scale: *scale,
        },
        Value::State(s) => CandidateValue::State(*s),
        Value::Bool(b) => CandidateValue::Bool(*b),
        Value::Text(_) => CandidateValue::Text,
        Value::Absent { .. } => return None,
    })
}

/// The later of two facts by `t`, then rank.
fn later(a: Option<Kept>, b: Kept, versioned: bool) -> Kept {
    match a {
        Some(a) if (a.t, a.rank(versioned)) > (b.t, b.rank(versioned)) => a,
        _ => b,
    }
}

impl State {
    /// Facts ahead that fell due by `horizon` reach the candidate: once the latest has, every one has.
    fn promote(&mut self, horizon: i64, versioned: bool) {
        if let Some(l) = self.last_ahead.filter(|l| l.t <= horizon) {
            self.candidate = Some(later(self.candidate, l, versioned));
            self.pending = None;
            self.last_ahead = None;
            self.pending_more = false;
            return;
        }
        if let Some(p) = self.pending.filter(|p| p.t <= horizon) {
            self.candidate = Some(later(self.candidate, p, versioned));
            self.pending = None;
            if !self.pending_more {
                self.pending = self.last_ahead;
            }
        }
    }

    /// Whether a fact ahead that the state does not name may have fallen due.
    fn ahead_unsure(&self) -> bool {
        self.ahead_lost || (self.pending.is_none() && self.pending_more)
    }

    fn apply(&mut self, f: &FactRow, position: LogPos, versioned: bool, horizon: i64) {
        self.promote(horizon, versioned);
        self.facts += 1;
        self.latest_t = Some(self.latest_t.map_or(f.t, |t| t.max(f.t)));
        if !f.flags.future && !f.flags.unsynced_clock {
            self.reference = Some(self.reference.map_or(f.t, |t| t.max(f.t)));
        }
        let value = value_of(&f.value).filter(|_| f.quality != Quality::Bad);
        let k = Kept {
            t: f.t,
            position,
            changed_at: f.changed_at,
            quality: f.quality,
            value: value.unwrap_or(CandidateValue::Text),
        };
        let good = value.is_some();
        if versioned && self.outranked(&k, good) {
            return;
        }
        if f.t > horizon {
            self.ahead(k, good, versioned);
            return;
        }
        let unsure = |u: &mut Option<i64>| *u = Some(u.map_or(k.t, |x| x.max(k.t)));
        match self.candidate {
            // A candidate continued from a file has no `changed_at` to rank a revision of it by.
            Some(c) if c.t == k.t && versioned && c.changed_at.is_none() => {
                unsure(&mut self.unsure_until);
            }
            Some(c) if c.t == k.t => {
                if k.rank(versioned) > c.rank(versioned) {
                    if good {
                        self.candidate = Some(k);
                    } else {
                        // The candidate was corrected to bad: the fact in force before it is not kept.
                        unsure(&mut self.unsure_until);
                    }
                }
            }
            Some(c) if c.t > k.t => {}
            _ if !good => {}
            _ => {
                if versioned && self.bad_unknown.is_some_and(|u| u >= k.t) {
                    unsure(&mut self.unsure_until);
                }
                self.candidate = Some(k);
                self.bad = self.bad.split_off(&k.t);
                if self.unsure_until.is_some_and(|u| u < k.t) {
                    self.unsure_until = None;
                }
            }
        }
    }

    /// Versioned series: records a bad fact above the candidate, and tells whether a good one is outranked by a bad
    /// one kept at its `t` (and so not current).
    fn outranked(&mut self, k: &Kept, good: bool) -> bool {
        let rank = k.rank(true);
        if !good {
            if self.candidate.is_none_or(|c| c.t < k.t) {
                if self.bad.len() >= BAD_KEPT && !self.bad.contains_key(&k.t) {
                    self.bad_unknown = Some(self.bad_unknown.map_or(k.t, |u| u.max(k.t)));
                } else {
                    let e = self.bad.entry(k.t).or_insert(rank);
                    *e = (*e).max(rank);
                }
            }
            return false;
        }
        match self.bad.get(&k.t) {
            Some(b) if *b > rank => true,
            Some(_) => {
                self.bad.remove(&k.t);
                false
            }
            None => {
                if self.bad_unknown.is_some_and(|u| u >= k.t)
                    && self.candidate.is_none_or(|c| c.t < k.t)
                {
                    self.ahead_lost = true;
                    self.pending_more = true;
                }
                false
            }
        }
    }

    /// A fact ahead of the horizon: the earliest and latest good ones are kept, any between only counted.
    fn ahead(&mut self, k: Kept, good: bool, versioned: bool) {
        let outranks = |o: Option<Kept>| {
            o.is_some_and(|o| o.t == k.t && k.rank(versioned) > o.rank(versioned))
        };
        let same = |o: Option<Kept>| o.is_some_and(|o| o.t == k.t);
        if !good {
            // A kept fact corrected to bad: what lies ahead is no longer bounded.
            if outranks(self.pending) || outranks(self.last_ahead) {
                self.ahead_lost = true;
                self.pending_more = true;
                if same(self.pending) {
                    self.pending = None;
                }
                if same(self.last_ahead) {
                    self.last_ahead = None;
                }
            }
            return;
        }
        let (Some(p), Some(l)) = (self.pending, self.last_ahead) else {
            if self.pending.is_none() && self.last_ahead.is_none() && !self.ahead_lost {
                self.pending = Some(k);
                self.last_ahead = Some(k);
            } else {
                self.pending_more = true;
                self.ahead_lost = true;
            }
            return;
        };
        if same(Some(p)) || same(Some(l)) {
            if outranks(Some(p)) {
                self.pending = Some(k);
            }
            if outranks(Some(l)) {
                self.last_ahead = Some(k);
            }
        } else if k.t < p.t {
            self.pending_more |= p.t < l.t;
            self.pending = Some(k);
        } else if k.t > l.t {
            self.pending_more |= p.t < l.t;
            self.last_ahead = Some(k);
        } else {
            self.pending_more = true;
        }
    }

    fn entry(&self, id: u32, defined: bool) -> SeriesEntry {
        SeriesEntry {
            series_id: id,
            latest_t: self.latest_t,
            lateness_reference: self.reference,
            fact_count: self.facts,
            orphan_count: if defined { 0 } else { self.facts },
            candidate: self.candidate.map(|c| Candidate {
                t: c.t,
                position: c.position,
                quality: c.quality,
                value: c.value,
            }),
            candidate_unsure: self.unsure_until.is_some() || self.ahead_unsure(),
            pending: self.pending.map(|p| Pending {
                t: p.t,
                position: p.position,
            }),
            // The entry names the earliest fact ahead only: with a later one kept, others are not named.
            pending_more: self.pending_more
                || self.ahead_lost
                || self
                    .pending
                    .zip(self.last_ahead)
                    .is_some_and(|(p, l)| p.t != l.t),
        }
    }

    /// The state an entry describes, written at `written_at`, to continue the fold from (A.18 Crash and rebuild).
    fn of_entry(e: &SeriesEntry, written_at: i64) -> Self {
        let kept = |t, position: LogPos, quality, value| Kept {
            t,
            position,
            changed_at: None,
            quality,
            value,
        };
        let pending = e
            .pending
            .map(|p| kept(p.t, p.position, Quality::Good, CandidateValue::AtPosition));
        State {
            latest_t: e.latest_t,
            reference: e.lateness_reference,
            facts: e.fact_count,
            candidate: e
                .candidate
                .map(|c| kept(c.t, c.position, c.quality, c.value)),
            unsure_until: e.candidate_unsure.then_some(written_at),
            pending,
            // The latest fact ahead is not in the entry: with others ahead, they are unbounded.
            last_ahead: pending.filter(|_| !e.pending_more),
            pending_more: e.pending_more,
            ahead_lost: e.pending_more,
            bad: BTreeMap::new(),
            bad_unknown: e.latest_t,
        }
    }
}

/// A batch above the synced mark, waiting.
#[derive(Debug)]
struct Queued {
    last: LogPos,
    received_at: i64,
    items: Vec<(LogPos, Item)>,
}

#[derive(Debug, Default)]
struct Fold {
    /// Every batch up to here is folded.
    applied: LogPos,
    queued: VecDeque<Queued>,
    series: BTreeMap<u32, State>,
    /// Defined series, and whether each is versioned.
    defined: BTreeMap<u32, bool>,
    /// The fold's `now`: the latest receive time or write time it has seen.
    horizon: i64,
    /// A void or a lost range reached what was folded: the state no longer follows the log.
    broken: bool,
    /// The last synced mark known.
    synced: LogPos,
    /// The latch's closed ranges already looked at.
    closed_seen: usize,
}

impl Fold {
    fn take(&mut self, received_at: i64, last: LogPos, items: &[(LogPos, &Item)]) {
        self.horizon = self.horizon.max(received_at);
        for (p, item) in items {
            match item {
                Item::Entry(e) => {
                    if let Entry::Definition(d) = &**e {
                        self.defined.insert(d.id, d.versioning != Versioning::None);
                    }
                }
                Item::Fact(f) if *p > self.applied => {
                    let versioned = self.defined.get(&f.series_id).copied().unwrap_or(false);
                    self.series.entry(f.series_id).or_default().apply(
                        f,
                        *p,
                        versioned,
                        self.horizon,
                    );
                }
                _ => {}
            }
        }
        self.applied = self.applied.max(last);
    }

    fn drain(&mut self, limit: LogPos) {
        while self.queued.front().is_some_and(|q| q.last <= limit) {
            let q = self.queued.pop_front().expect("looked at above");
            let items: Vec<(LogPos, &Item)> = q.items.iter().map(|(p, i)| (*p, i)).collect();
            self.take(q.received_at, q.last, &items);
        }
    }
}

/// The writer's last synced mark, as the fold reads it.
type Synced = Box<dyn Fn() -> LogPos + Send + Sync>;

/// See the module documentation.
#[derive(Default)]
pub(crate) struct SeriesFold {
    fold: Mutex<Fold>,
    synced: OnceLock<Synced>,
    /// The process's losses: a range found lost after the fold took it breaks the fold, as a void below it does
    /// (A.18: a file is a function of the kept log only while nothing at or below `through` is lost since).
    latch: OnceLock<Arc<Latch>>,
}

impl std::fmt::Debug for SeriesFold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeriesFold").finish_non_exhaustive()
    }
}

impl SeriesFold {
    /// A fold whose batches up to `synced` (the last synced mark at the start) are folded as they arrive.
    pub(crate) fn new(synced: LogPos) -> Self {
        SeriesFold {
            fold: Mutex::new(Fold {
                synced,
                horizon: i64::MIN,
                ..Fold::default()
            }),
            synced: OnceLock::new(),
            latch: OnceLock::new(),
        }
    }

    /// Breaks the fold on every range the latch takes from now on that reaches what it took, folded or waiting.
    pub(crate) fn watch_losses(&self, latch: Arc<Latch>) {
        let mut f = lock(&self.fold);
        let _ = latch.closed_since(&mut f.closed_seen);
        let _ = self.latch.set(latch);
    }

    fn take_losses(&self, f: &mut Fold) {
        let Some(latch) = self.latch.get() else {
            return;
        };
        let taken = f.queued.back().map_or(f.applied, |q| q.last.max(f.applied));
        for (first, _) in latch.closed_since(&mut f.closed_seen) {
            if first <= taken {
                f.broken = true;
            }
        }
    }

    /// Continues from a series directory found valid: its entries as of `through`, written at `written_at`.
    pub(crate) fn seed(&self, through: LogPos, written_at: i64, entries: &[SeriesEntry]) {
        let mut f = lock(&self.fold);
        f.applied = through;
        f.horizon = f.horizon.max(written_at);
        f.series = entries
            .iter()
            .map(|e| (e.series_id, State::of_entry(e, written_at)))
            .collect();
    }

    /// Follows the writer's synced mark from now on, as `synced` reads it.
    pub(crate) fn follow(&self, synced: impl Fn() -> LogPos + Send + Sync + 'static) {
        let _ = self.synced.set(Box::new(synced));
    }

    /// A batch the index took, in position order. One at or below the synced mark is folded as it is, with nothing
    /// copied; only one above it waits, and a fact at or below what is folded is neither folded nor kept.
    pub(crate) fn batch(&self, b: &ReadBatch) {
        let synced = self.synced.get().map(|f| f());
        let mut f = lock(&self.fold);
        self.take_losses(&mut f);
        if let Some(s) = synced {
            f.synced = f.synced.max(s);
        }
        let limit = f.synced;
        f.drain(limit);
        let mut sequence = b.first.sequence;
        let items: Vec<(LogPos, &Item)> = b
            .content
            .items
            .iter()
            .map(|i| {
                let p = LogPos {
                    generation: b.first.generation,
                    sequence,
                };
                sequence += i.sequences();
                (p, i)
            })
            .collect();
        if b.last <= f.synced && f.queued.is_empty() {
            f.take(b.content.received_at, b.last, &items);
            return;
        }
        let applied = f.applied;
        let kept = items
            .into_iter()
            .filter(|(p, i)| matches!(i, Item::Entry(_)) || *p > applied)
            .map(|(p, i)| (p, i.clone()))
            .collect();
        f.queued.push_back(Queued {
            last: b.last,
            received_at: b.content.received_at,
            items: kept,
        });
    }

    /// The index voided everything above `above`.
    pub(crate) fn void(&self, above: LogPos) {
        let mut f = lock(&self.fold);
        f.queued.retain(|q| q.last <= above);
        if f.applied > above {
            f.broken = true;
        }
    }

    /// The entries as of a position at or below `limit`, judged at the later of `written_at` and every receive
    /// time folded.
    pub(crate) fn entries(&self, limit: LogPos, written_at: i64) -> Option<SeriesEntries> {
        let mut f = lock(&self.fold);
        self.take_losses(&mut f);
        if f.broken {
            return None;
        }
        f.synced = f.synced.max(limit);
        f.drain(limit);
        let horizon = f.horizon.max(written_at);
        f.horizon = horizon;
        let defined = f.defined.clone();
        let entries = f
            .series
            .iter_mut()
            .map(|(id, s)| {
                let versioned = defined.get(id).copied().unwrap_or(false);
                s.promote(horizon, versioned);
                s.entry(*id, defined.contains_key(id))
            })
            .collect();
        Some(SeriesEntries {
            through: f.applied,
            judged_at: horizon,
            entries,
        })
    }
}

/// What the status file is made from, shared with the store: none of it keeps the store alive, so the hook never
/// holds the last reference to the writer on `kvik-maint`.
struct Parts {
    watch: Watch,
    counters: Arc<Mutex<super::Counters>>,
    own_fault: Arc<Mutex<Option<kvik_log::writer::Fault>>>,
    fs: Arc<dyn kvik_log::fs::Fs>,
    dir: std::path::PathBuf,
}

/// The seal hook of a durable store.
pub(crate) struct Hook {
    pub(crate) fold: Arc<SeriesFold>,
    parts: OnceLock<Parts>,
}

impl std::fmt::Debug for Hook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hook").finish_non_exhaustive()
    }
}

impl Hook {
    /// Gives the hook what the status file is made from, once the log runs.
    pub(crate) fn share(
        &self,
        watch: Watch,
        counters: Arc<Mutex<super::Counters>>,
        own_fault: Arc<Mutex<Option<kvik_log::writer::Fault>>>,
        fs: Arc<dyn kvik_log::fs::Fs>,
        dir: &std::path::Path,
    ) {
        let _ = self.parts.set(Parts {
            watch,
            counters,
            own_fault,
            fs,
            dir: dir.to_path_buf(),
        });
    }
}

/// A log reason's code in the status file (A.18).
fn status_reason(r: &kvik_log::writer::Reason) -> u16 {
    use kvik_log::writer::Reason as LogReason;
    match r {
        LogReason::IngestWaiting(_) => reason::INGEST_WAITING,
        LogReason::DiskLow { .. } => reason::DISK_LOW,
        LogReason::DiskCritical { .. } => reason::DISK_CRITICAL,
        LogReason::SealBlocked { .. } => reason::SEAL_BLOCKED,
        // A sealed segment no record lists that fails its checks or cannot be read (Q-221, Q-640): no code of its own
        // in A.18.
        _ => reason::OTHER,
    }
}

/// The level and the sorted codes the status file carries for `reasons`.
fn status_health(reasons: Vec<kvik_log::writer::Reason>) -> (u8, Vec<u16>) {
    let mut codes: Vec<u16> = reasons.iter().map(status_reason).collect();
    codes.sort_unstable();
    codes.dedup();
    let level = if codes.contains(&reason::DISK_CRITICAL) {
        health::CRITICAL
    } else if codes.is_empty() {
        health::OK
    } else {
        health::DEGRADED
    };
    (level, codes)
}

impl Parts {
    /// The reasons the file carries and the level over them (A.18: partial until `kvik health`, roadmap 9.0e).
    fn health(&self) -> (u8, Vec<u16>) {
        status_health(self.watch.reasons())
    }

    fn read_only(&self) -> bool {
        self.watch.fault().is_some() || lock(&self.own_fault).is_some()
    }
}

impl SealHook for Hook {
    fn series(&self, limit: LogPos, written_at: i64) -> Option<SeriesEntries> {
        self.fold.entries(limit, written_at)
    }

    fn health(&self) -> Option<(u8, bool)> {
        let p = self.parts.get().filter(|p| !p.read_only())?;
        Some((p.health().0, false))
    }

    fn status(&self) -> Option<StatusContent> {
        let p = self.parts.get().filter(|p| !p.read_only())?;
        let (level, reasons) = p.health();
        let st = p.watch.stats();
        let ns = |d: std::time::Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        let (waited, p50, p99, max) = {
            let c = lock(&p.counters);
            let mut sorted: Vec<std::time::Duration> = c.latencies.iter().copied().collect();
            sorted.sort_unstable();
            let pct = |q: usize| {
                sorted
                    .get((sorted.len() * q / 100).min(sorted.len().saturating_sub(1)))
                    .copied()
                    .unwrap_or_default()
            };
            (c.waited, pct(50), pct(99), c.max_latency)
        };
        Some(StatusContent {
            health: level,
            pressure: false,
            reasons,
            counters: vec![
                (counter::QUEUE_DEPTH, p.watch.queue_depth() as u64),
                (counter::WAITED_NS, ns(waited)),
                (counter::BATCHES, st.batches),
                (counter::SYNCS, st.syncs),
                (counter::COMMIT_P50_NS, ns(p50)),
                (counter::COMMIT_P99_NS, ns(p99)),
                (counter::COMMIT_MAX_NS, ns(max)),
                (counter::SEALED_SEGMENTS, st.sealed_segments),
                (counter::DISK_FREE, p.fs.free_space(&p.dir).unwrap_or(0)),
            ],
        })
    }
}

/// The seal hook of a store opening: its fold continues from the newest series directory that is still a function of
/// the kept log (A.18 Validation, as the writer applies it), or starts empty. A file is not continued when the start
/// met a region at or below its `through` that no lost range of the manifest holds (`latch`, `recorded`): its
/// counts include facts the store reports lost. The fold then watches `latch` for losses found later.
pub(crate) fn hook(
    writer: &kvik_log::writer::LogWriter,
    fs: &dyn kvik_log::fs::Fs,
    dir: &std::path::Path,
    store_id: [u8; 16],
    latch: &Arc<Latch>,
    recorded: &[LostRange],
) -> Arc<Hook> {
    let fold = SeriesFold::new(writer.stats().mark_synced);
    let start = writer.directory_start();
    if let Some((f, d)) = kvik_log::writer::kept_series(fs, dir, &store_id, &start)
        && !lost_since(&latch.found(), recorded, d.through)
    {
        fold.seed(d.through, d.judged_at.max(f.written_at), &d.entries);
    }
    fold.watch_losses(latch.clone());
    Arc::new(Hook {
        fold: Arc::new(fold),
        parts: OnceLock::new(),
    })
}

/// Whether a lost range in `found` begins at or below `through` and no lost range of the manifest holds it.
fn lost_since(
    found: &[crate::health::LossReport],
    recorded: &[LostRange],
    through: LogPos,
) -> bool {
    let at = |p: crate::types::Position| (p.generation, p.sequence);
    found
        .iter()
        .filter(|l| l.kind == LossKind::DataLost)
        .filter_map(|l| Some((at(l.first?), at(l.last?))))
        .any(|(first, last)| {
            first <= (through.generation, through.sequence)
                && !recorded.iter().any(|r| r.first <= first && last <= r.last)
        })
}

/// The options `kvik-maint` writes the derived files with.
pub(crate) fn options(hook: &Arc<Hook>) -> kvik_log::writer::DirectoryOptions {
    kvik_log::writer::DirectoryOptions {
        hook: Some(hook.clone()),
        status_interval: Some(kvik_log::writer::STATUS_INTERVAL),
    }
}

#[cfg(test)]
mod tests;
