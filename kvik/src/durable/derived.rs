//! The series directory's entries and the status file's content, supplied to `kvik-maint` through the seal hook
//! (`format.md` A.18). [`SeriesFold`] folds every durable batch the deliverer's
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
use kvik_log::directory::Family;
use kvik_log::directory::series::SeriesEntry;
use kvik_log::directory::status::{counter, health, reason};
use kvik_log::entries::{Entry, Versioning};
use kvik_log::manifest::LostRange;
use kvik_log::reader::ReadBatch;
use kvik_log::writer::{Position as LogPos, SealHook, SeriesEntries, StatusContent, Watch};

use super::loss::Latch;
use crate::health::LossKind;
use state::State;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
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

    /// Whether a lost range reached what the fold took, as of now.
    pub(crate) fn broken(&self) -> bool {
        let mut f = lock(&self.fold);
        self.take_losses(&mut f);
        f.broken
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

    fn series_void(&self) -> bool {
        self.fold.broken()
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
/// the kept log (A.18 Validation, as the writer applies it), or starts empty. A file is not continued, and the series
/// directories are removed, when the start met a region at or below its `through` that no lost range of the manifest
/// holds (`latch`, `recorded`): its counts include facts the store reports lost. The fold then watches `latch` for
/// losses found later.
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
    if let Some((f, d)) = kvik_log::writer::kept_series(fs, dir, &store_id, &start) {
        if !lost_since(&latch.found(), recorded, d.through) {
            fold.seed(d.through, d.judged_at.max(f.written_at), &d.entries);
        } else if kvik_log::directory::remove_family(fs, dir, Family::Series).is_err() {
            // The declined file must not stay the newest: the writer removes it once the fold is broken.
            lock(&fold.fold).broken = true;
        }
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

pub(crate) mod state;
#[cfg(test)]
mod tests;
