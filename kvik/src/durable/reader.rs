//! A reader on the durable log (`docs/api.md` Reader processes): the log reader of `kvik-log`, which takes no lock
//! and reads up to the durable mark, the latest values, registry and losses it folds from a series directory and the
//! log above it ([`Latest`]), and the fact index of a second log reader, opened for the first call that reads facts
//! (Q-1000). Every call takes a view published by a refresh that began after the call did, and runs on it with no
//! lock under the view's cut (Q-902, Q-903, [`cut`], [`view`](super::read::view)). The refresh notes the losses it
//! finds in the process's latch and takes every range the latch took since the last one (Q-938).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::Duration;

use kvik_log::directory::logdir::LostEntry;
use kvik_log::directory::series::SeriesDirectory;
use kvik_log::directory::{DerivedError, Family};
use kvik_log::lease::CutEnd;
use kvik_log::manifest::LostRange;
use kvik_log::mark::ResetStamp as LogStamp;
use kvik_log::reader::{LogReader, ReadEvent, ReaderConfig as LogReaderConfig, ReaderError};
use kvik_log::writer::Position as LogPos;

use super::clock::Clocks;
use super::index::{Index, Stored, pos};
use super::loss::{Find, Latch};
use super::read::cache::{Accountant, DEFAULT_MEMORY, Sizes};
use super::read::cut;
use super::read::flight::Flight;
use super::read::hub::{Collector, Hub};
use super::read::latest::{self, Latest};
use super::read::view::{Cuts, View};
use crate::env::{Clock, Env};
use crate::error::{OpenError, QueryError};
use crate::health::LossReport;
use crate::read::ReaderState;
use crate::time::Timestamp;
use crate::types::ResetStamp;

pub(crate) use errors::{copy, open_error, query_error};

/// What the refresh leading a round works on: the log reader, the latest values the next view is a clone of, and the
/// fact index with its own log reader once a call has asked for facts.
#[derive(Debug)]
struct Work {
    log: Option<LogReader>,
    /// `None` until the first round seeds it.
    latest: Option<Latest>,
    /// The stamp the series directory was judged valid under, until the first pass has read the mark again.
    seed_stamp: Option<LogStamp>,
    facts: Option<FactsReader>,
    lost_ranges: Arc<[LostRange]>,
    /// The latch's closed ranges already taken.
    closed_seen: usize,
    /// Items a refresh takes at a time.
    part: usize,
    /// Parts taken.
    #[cfg(test)]
    parts: u64,
}

/// The fact index and the log reader that reads the kept log whole for it, brought to the main reader's cut at every
/// round (Q-1000).
#[derive(Debug)]
struct FactsReader {
    log: LogReader,
    index: Index,
}

impl Drop for Work {
    fn drop(&mut self) {
        // Deletes the leases; a reader that cannot is a lease left to expire.
        if let Some(r) = self.log.take() {
            let _ = r.close();
        }
        if let Some(f) = self.facts.take() {
            let _ = f.log.close();
        }
    }
}

/// The environment's clocks for the log reader, so that the lease's renewal and a cut's age follow an injected clock.
#[derive(Debug)]
struct ReaderClock(Arc<dyn Clock>);

impl kvik_log::writer::Clock for ReaderClock {
    fn monotonic(&self) -> Duration {
        Duration::from_nanos(self.0.monotonic().0)
    }
    fn wall(&self) -> i64 {
        self.0.wall().0
    }
}

/// See the module documentation.
#[derive(Debug)]
pub(crate) struct DurableReader {
    dir: PathBuf,
    env: Env,
    /// Locked by the refresh that leads a round, and by the check of a call as old as half `lease_refresh`; never
    /// by a call's own work.
    work: Mutex<Work>,
    flight: Flight<View, Arc<QueryError>>,
    cuts: Arc<Cuts>,
    clock: Arc<dyn Clock>,
    /// The log reader's `lease_refresh` and `cut_max_age`.
    lease_refresh: Duration,
    cut_max_age: Duration,
    /// The window clock and refresh instants of this process: the writer's, for the reader of its own process.
    clocks: Arc<Clocks>,
    /// The site zone at creation, from the store descriptor: in force before the first site-zone entry (spec §5.7).
    created_zone: String,
    /// Where the losses its reads find go: the writer's, for the reader of its own process.
    latch: Arc<Latch>,
    store_id: [u8; 16],
    /// The writer's progress, for the reader of its own process: a round that would find nothing new is skipped.
    writer: Option<kvik_log::writer::Watch>,
    /// A call asked for facts: the next round opens the fact index (Q-1000).
    want_facts: AtomicBool,
    /// The log reader of the reads at a cut that a state cannot answer (Q-1004), opened on first use; locked for one
    /// lookup, never by a refresh.
    lookups: Mutex<Option<LogReader>>,
    /// The full facts of candidates the fold did not meet, read once each, by series.
    rows: Mutex<BTreeMap<u32, Stored>>,
    rows_read: AtomicBool,
    /// The live facts searched for, by series, while each holds (Q-1004).
    searched: Mutex<BTreeMap<u32, latest::Searched>>,
    /// Live values found in the log because a state could not tell.
    searches: AtomicU64,
    /// The first round continued a series directory.
    seeded: AtomicBool,
    /// What each round read, for the live sessions on this reader.
    hub: Hub,
    /// The fan-out thread was started ([`super::read::fanout`]), or could not be.
    pub(crate) fanout: AtomicBool,
    pub(crate) fanout_failed: AtomicBool,
    /// Wakes the fan-out thread, so that it ends as soon as the reader is dropped.
    pub(crate) fanout_kick: Mutex<Option<std::task::Waker>>,
    /// The memory cap of its queries (Q-1063).
    memory: Arc<Accountant>,
    /// The summary tree as last read for a cut (design 7.1b section 6).
    tree: Mutex<super::tree::read::Cache>,
    /// Reads take facts only, as the oracle of the tree's (invariant T7).
    #[cfg(test)]
    pub(crate) no_tree: AtomicBool,
    #[cfg(test)]
    lookups_made: AtomicU64,
    /// Lookups that would have passed their budget.
    #[cfg(test)]
    overflows: AtomicU64,
    /// Sealed segments lookups read through their series batches.
    #[cfg(test)]
    listed: AtomicU64,
    /// Lookups still to end with `CutExpired`, as a reset between the cut and the read ends them.
    #[cfg(test)]
    pub(crate) expire_lookups: AtomicU64,
    #[cfg(test)]
    pub(crate) after_lookup: AfterLookup,
}

impl Drop for DurableReader {
    fn drop(&mut self) {
        let kick = self
            .fanout_kick
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(w) = kick.take() {
            w.wake();
        }
    }
}

/// Run after each lookup of the reader's `facts` and `at`.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct AfterLookup(pub(crate) Mutex<Option<Box<dyn FnMut() + Send>>>);

#[cfg(test)]
impl std::fmt::Debug for AfterLookup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AfterLookup")
    }
}

/// A log reader configuration on the store in `dir` with this process's boot id and a fresh reader id.
fn log_config(dir: &Path, env: &Env, clock: Arc<dyn kvik_log::writer::Clock>) -> LogReaderConfig {
    let mut id = [0u8; 16];
    env.entropy.fill(&mut id);
    LogReaderConfig::new(dir, env.fs.clone(), clock, env.boot.current(), id)
}

/// Reads what was appended since the last round, up to `upto` if given, in parts of `part` items, handing each part
/// to `apply`, so that a first read of a long log holds one part's batches at a time, and noting each part's finds in
/// `latch` as the query's. A part the log reader discarded because the reset stamp changed while it read is read once
/// more; a second discard in the round is `CutExpired` (Q-850, Q-948).
fn take(
    log: &mut LogReader,
    part: usize,
    upto: Option<LogPos>,
    latch: &Latch,
    #[cfg(test)] parts: &mut u64,
    mut apply: impl FnMut(&[ReadEvent], LogPos) -> Vec<Find>,
) -> Result<Result<(), QueryError>, ReaderError> {
    let mut discarded = false;
    loop {
        let (update, more) = match upto {
            Some(u) => log.refresh_upto(part, u)?,
            None => log.refresh_some(part)?,
        };
        #[cfg(test)]
        {
            *parts += 1;
        }
        if update.discarded {
            if std::mem::replace(&mut discarded, true) {
                return Ok(Err(QueryError::CutExpired));
            }
            continue;
        }
        let found = apply(&update.events, update.state.durable);
        if !found.is_empty() {
            latch.note(&found);
        }
        if !more {
            return Ok(Ok(()));
        }
    }
}

/// The manifest's lost ranges merged by identity, as a series directory's `lost` counts them (`format.md` A.18).
fn record_pos((generation, sequence): (u32, u64)) -> LogPos {
    LogPos {
        generation,
        sequence,
    }
}

fn lost_entries(ranges: &[LostRange]) -> Vec<LostEntry> {
    let at = record_pos;
    let mut by: BTreeMap<(u32, u64), LostEntry> = BTreeMap::new();
    for l in ranges {
        let e = by.entry(l.identity).or_insert(LostEntry {
            first: at(l.first),
            last: at(l.last),
            file_id: l.file_id,
            cause: l.cause as u8,
        });
        e.first = e.first.min(at(l.first));
        e.last = e.last.max(at(l.last));
    }
    by.into_values().collect()
}

impl DurableReader {
    /// Opens a reader on the store in `dir`; reads nothing until the first call.
    pub(crate) fn open(
        dir: &Path,
        env: &Env,
        lease_refresh: Duration,
        cut_max_age: Duration,
    ) -> Result<Self, OpenError> {
        Self::open_with(
            dir,
            env,
            (lease_refresh, cut_max_age),
            Arc::new(Clocks::new(env.clock.clone())),
            Arc::default(),
            None,
        )
    }

    /// [`DurableReader::open`] with the window clock, refresh instants and loss latch of the process; `ages` is
    /// `lease_refresh` and `cut_max_age`; `writer`, the writer's progress for the reader of its own process.
    pub(crate) fn open_with(
        dir: &Path,
        env: &Env,
        ages: (Duration, Duration),
        clocks: Arc<Clocks>,
        latch: Arc<Latch>,
        writer: Option<kvik_log::writer::Watch>,
    ) -> Result<Self, OpenError> {
        let mut cfg = log_config(dir, env, Arc::new(ReaderClock(env.clock.clone())));
        (cfg.lease_refresh, cfg.cut_max_age) = ages;
        let log = LogReader::open(cfg).map_err(|e| open_error(dir, e))?;
        let descriptor = kvik_log::store::read_descriptor(&*env.fs, dir)
            .map_err(|e| super::store_error(dir, e))?;
        let (lease_refresh, cut_max_age) = log.ages();
        Ok(DurableReader {
            dir: dir.to_path_buf(),
            env: env.clone(),
            work: Mutex::new(Work {
                log: Some(log),
                latest: None,
                seed_stamp: None,
                facts: None,
                lost_ranges: Arc::new([]),
                closed_seen: 0,
                part: super::start::PART,
                #[cfg(test)]
                parts: 0,
            }),
            flight: Flight::new(),
            cuts: Arc::default(),
            clock: env.clock.clone(),
            lease_refresh,
            cut_max_age,
            clocks,
            created_zone: descriptor.site_zone,
            store_id: descriptor.store_id,
            latch,
            writer,
            want_facts: AtomicBool::new(false),
            lookups: Mutex::new(None),
            rows: Mutex::new(BTreeMap::new()),
            rows_read: AtomicBool::new(false),
            searched: Mutex::new(BTreeMap::new()),
            searches: AtomicU64::new(0),
            #[cfg(test)]
            lookups_made: AtomicU64::new(0),
            #[cfg(test)]
            overflows: AtomicU64::new(0),
            #[cfg(test)]
            listed: AtomicU64::new(0),
            #[cfg(test)]
            expire_lookups: AtomicU64::new(0),
            #[cfg(test)]
            after_lookup: AfterLookup::default(),
            seeded: AtomicBool::new(false),
            hub: Hub::default(),
            fanout: AtomicBool::new(false),
            fanout_failed: AtomicBool::new(false),
            fanout_kick: Mutex::new(None),
            memory: Accountant::new(Sizes::of(DEFAULT_MEMORY)),
            tree: Mutex::default(),
            #[cfg(test)]
            no_tree: AtomicBool::new(false),
        })
    }

    /// The reader with a memory cap of `sizes` (`ReaderConfig::memory`).
    pub(crate) fn with_memory(mut self, sizes: Sizes) -> Self {
        self.memory = Accountant::new(sizes);
        self
    }

    /// The memory cap of its queries.
    pub(crate) fn memory(&self) -> &Arc<Accountant> {
        &self.memory
    }

    /// The summary tree at `v`'s cut, if one may serve it (design 7.1b section 6).
    pub(crate) fn tree_at(&self, v: &View) -> Option<Arc<super::tree::read::TreeAt>> {
        #[cfg(test)]
        if self.no_tree.load(Ordering::Relaxed) {
            return None;
        }
        super::tree::read::at_cut(&self.tree, &self.dir, &self.env, self.store_id, v.cut())
    }

    /// The tree's read counters: cuts served from a root, and parts found damaged.
    #[cfg(test)]
    pub(crate) fn tree_counts(&self) -> super::tree::read::Counts {
        super::lock(&self.tree).counts()
    }

    /// The window clock now (spec §6 `now`).
    pub(crate) fn now(&self) -> i64 {
        self.clocks.now()
    }

    /// The window clock and refresh instants of this process.
    pub(crate) fn clocks(&self) -> &Clocks {
        &self.clocks
    }

    /// The site zone at creation.
    pub(crate) fn created_zone(&self) -> &str {
        &self.created_zone
    }

    /// The store id, from the descriptor read at open.
    pub(crate) fn store_id(&self) -> [u8; 16] {
        self.store_id
    }

    fn lock(&self) -> MutexGuard<'_, Work> {
        self.work.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn monotonic(&self) -> Duration {
        Duration::from_nanos(self.clock.monotonic().0)
    }

    /// Has this process's scanning read find the damage in the kept log, as a reader that holds every fact does
    /// (Q-1060): the regions it finds go to the latch, which lookups then pass over as lost.
    pub(crate) fn find_losses(&self) -> Result<(), QueryError> {
        let v = self.view_with_facts()?;
        self.end(v)
    }

    /// The shared tail of the live sessions on this reader.
    pub(crate) fn hub(&self) -> &Hub {
        &self.hub
    }

    /// The writer's progress, for the reader of the writer's process.
    pub(crate) fn writer(&self) -> Option<&kvik_log::writer::Watch> {
        self.writer.as_ref()
    }

    /// Whether the mark changed since the last round read it, reading the mark alone; `None` while a round runs or
    /// when the mark cannot be read.
    pub(crate) fn mark_moved(&self) -> Option<bool> {
        let w = self.work.try_lock().ok()?;
        w.log.as_ref()?.mark_moved().ok()
    }

    /// How many lost ranges the process found; the next round hands the new ones to the shared tail.
    pub(crate) fn found_lost(&self) -> usize {
        self.latch.closed_count()
    }

    /// Releases the cuts of views no call holds any more, unless a round runs, which releases them itself.
    pub(crate) fn release_dropped(&self) {
        let mut w = match self.work.try_lock() {
            Ok(w) => w,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        if let Some(log) = w.log.as_mut() {
            for id in self.cuts.take_dropped() {
                log.release(id);
            }
        }
    }

    /// A log reader of its own on the store, with this reader's ages: the fact index's and the lookups'.
    fn second_reader(&self) -> Result<LogReader, ReaderError> {
        let mut cfg = log_config(
            &self.dir,
            &self.env,
            Arc::new(ReaderClock(self.env.clock.clone())),
        );
        (cfg.lease_refresh, cfg.cut_max_age) = (self.lease_refresh, self.cut_max_age);
        LogReader::open(cfg)
    }

    /// The state a new reader starts from (Q-1003): the newest series directory of the mark's generation, written
    /// under its reset stamp, not behind a loss since (`format.md` A.18), with a registry file of that generation,
    /// and the log reader started after the directory's `through`; otherwise the kept log from its start.
    fn seed(&self, log: &mut LogReader) -> Result<(Latest, Option<LogStamp>), ReaderError> {
        // The mark and the manifest as they are now, not as at the open: a directory written since is judged
        // against a mark at or past its `through` (the query design's section 3.2).
        log.reread()?;
        let st = log.state();
        let lost = lost_entries(log.lost_ranges());
        let fs = &*self.env.fs;
        let id = self.store_id;
        // The conditions are applied per generation, so a newest file that fails them falls back to the one kept.
        let dir = kvik_log::directory::read_latest(fs, &self.dir, Family::Series, |b| {
            let (f, d) = SeriesDirectory::decode(b, &id)?;
            let valid = f.generation == st.generation
                && d.through.generation == st.generation
                && d.through <= st.durable
                && d.stamp.unwrap_or_default() == st.stamp
                && d.lost != u32::MAX
                && kvik_log::writer::lost_below(&lost, d.through) <= d.lost;
            if valid {
                Ok((f, d))
            } else {
                Err(DerivedError::Invalid("not valid for this reader"))
            }
        })
        .ok()
        .flatten();
        let file = kvik_log::registry::read_latest_where(fs, &self.dir, &id, |f| {
            f.generation == st.generation && f.through <= st.durable.sequence
        })
        .ok()
        .flatten()
        .map(|(_, f)| f);
        if let (Some((frame, d)), Some(file)) = (dir, file)
            && log.start_after(d.through)?
        {
            let judged = d.judged_at.max(frame.written_at);
            return Ok((Latest::seeded(&file, &d, judged), Some(st.stamp)));
        }
        Ok((Latest::new(), None))
    }

    /// One refresh, which the call leading a round runs: releases the cuts of dropped views, reads what was appended
    /// since the last round, brings the fact index to the same position when there is one or a call asked for it,
    /// and publishes the next view, holding its own cut.
    fn round(&self, current: Option<Arc<View>>, quick: bool) -> Result<Arc<View>, Arc<QueryError>> {
        let began = self.monotonic();
        let mut w = self.lock();
        let w = &mut *w;
        let Some(log) = w.log.as_mut() else {
            return Err(Arc::new(QueryError::Closed));
        };
        let failed = |e: ReaderError| Arc::new(query_error(&self.dir, e));
        for id in self.cuts.take_dropped() {
            log.release(id);
        }
        if let (true, Some(v)) = (quick, &current)
            && self.fresh(v, began)
        {
            return Ok(v.clone());
        }
        if w.latest.is_none() {
            let (l, stamp) = self.seed(log).map_err(failed)?;
            w.latest = Some(l);
            self.seeded.store(stamp.is_some(), Ordering::Relaxed);
            w.seed_stamp = stamp;
            // The ranges the seed was judged against; any recorded later is compared with what was folded.
            w.lost_ranges = log.lost_ranges().into();
        }
        let latest = w.latest.as_mut().expect("seeded above");
        let mut own = Vec::new();
        let mut tail = self.hub.collector();
        let taken = take(
            log,
            w.part,
            None,
            &self.latch,
            #[cfg(test)]
            &mut w.parts,
            |events, durable| {
                let found = latest.apply(events, durable);
                own.extend(found.iter().map(|f| f.closed));
                if let Some(c) = tail.as_mut() {
                    c.events(events, &found);
                }
                found
            },
        );
        // A renewal that found the lease gone or changed ended every cut, the current view's among them.
        if let Some(v) = &current
            && let Err(ReaderError::CutEnded(CutEnd::LeaseLost)) = cut::check(log, v.cut())
        {
            self.cuts.lose_all();
        }
        taken.map_err(failed)?.map_err(Arc::new)?;
        let st = log.state();
        // A reset between the directory's check and the first pass: the directory may cover what it voided.
        if w.seed_stamp.take().is_some_and(|s| s != st.stamp) {
            latest.break_off();
        }
        // A range the manifest records since the last round (a start found it): the log reader never reads again
        // what it returned, so a range at or below what was folded is met only here (Q-1006).
        let recorded_since: Vec<(LogPos, LogPos)> = log
            .lost_ranges()
            .iter()
            .filter(|r| !w.lost_ranges.contains(r))
            .map(|r| (record_pos(r.first), record_pos(r.last)))
            .collect();
        for (first, _) in &recorded_since {
            latest.lose_recorded(*first);
        }
        // The ranges this pass found held no fact it folded; any other closed range may (Q-938).
        let mut lose = |latest: &mut Latest,
                        facts: &mut Option<FactsReader>,
                        tail: &mut Option<Collector>,
                        recorded: &[LostRange]| {
            for (first, last) in self.latch.closed_since(&mut w.closed_seen) {
                if !own.contains(&(first, last)) {
                    latest.lose(first, last, recorded);
                    if let Some(c) = tail.as_mut() {
                        c.gap(first, last);
                    }
                }
                if let Some(f) = facts.as_mut() {
                    f.index.forget(first, last);
                }
            }
        };
        lose(latest, &mut w.facts, &mut tail, log.lost_ranges());
        if (latest.broken() || self.want_facts.load(Ordering::SeqCst)) && w.facts.is_none() {
            w.facts = Some(FactsReader {
                log: self.second_reader().map_err(failed)?,
                index: Index::default(),
            });
        }
        if let Some(f) = w.facts.as_mut() {
            let index = &mut f.index;
            take(
                &mut f.log,
                w.part,
                Some(st.durable),
                &self.latch,
                #[cfg(test)]
                &mut w.parts,
                |events, durable| index.apply(events, durable),
            )
            .map_err(failed)?
            .map_err(Arc::new)?;
            for (first, last) in &recorded_since {
                index.forget(*first, *last);
            }
            let fs = f.log.state();
            if (fs.generation, fs.stamp) != (st.generation, st.stamp) || fs.durable < st.durable {
                // Read under another reset stamp than the main reader's: read whole again at the next round.
                if let Some(f) = w.facts.take() {
                    let _ = f.log.close();
                }
            }
        }
        lose(latest, &mut w.facts, &mut tail, log.lost_ranges());
        if log.lost_ranges() != &*w.lost_ranges {
            w.lost_ranges = log.lost_ranges().into();
        }
        let seq = self.hub.finish(tail, self.monotonic(), self.cut_max_age);
        let mut v = View::new(
            (latest.clone(), w.facts.as_ref().map(|f| f.index.clone())),
            st,
            w.lost_ranges.clone(),
            (cut::take(log), seq),
            began,
            self.cuts.clone(),
        );
        v.closed_taken = w.closed_seen;
        Ok(Arc::new(v))
    }

    /// Live values found in the log because a state could not tell.
    #[cfg(test)]
    pub(crate) fn searches(&self) -> u64 {
        self.searches.load(Ordering::Relaxed)
    }

    /// Whether the first round continued a series directory.
    #[cfg(test)]
    pub(crate) fn seeded(&self) -> bool {
        self.seeded.load(Ordering::Relaxed)
    }

    /// The `through` of the series directory the first round continued, if it continued one.
    #[cfg(test)]
    pub(crate) fn seeded_through(&self) -> Option<crate::types::Position> {
        let w = self.lock();
        let l = w.latest.as_ref().filter(|_| self.seeded())?;
        Some(pos(l.seeded_through()))
    }

    /// Refreshes of the log the rounds have run, each of which reads the mark file.
    #[cfg(test)]
    pub(crate) fn parts_taken(&self) -> u64 {
        self.lock().parts
    }

    /// Lookups made at a cut.
    #[cfg(test)]
    pub(crate) fn lookups(&self) -> u64 {
        self.lookups_made.load(Ordering::Relaxed)
    }

    /// Lookups that would have passed their budget, so that their pass was split and read again.
    #[cfg(test)]
    pub(crate) fn overflows(&self) -> u64 {
        self.overflows.load(Ordering::Relaxed)
    }

    /// Sealed segments lookups read through their series batches (`format.md` A.8a).
    #[cfg(test)]
    pub(crate) fn listed(&self) -> u64 {
        self.listed.load(Ordering::Relaxed)
    }

    /// What the reader holds: the latest values' series, full facts and entries, whether it holds a fact index, and
    /// the bytes its log reader keeps.
    #[cfg(test)]
    pub(crate) fn footprint(&self) -> ((usize, usize, usize), bool, usize) {
        let w = self.lock();
        (
            w.latest.as_ref().map_or((0, 0, 0), Latest::footprint),
            w.facts.is_some(),
            w.log.as_ref().map_or(0, LogReader::kept_bytes),
        )
    }

    /// Re-reads the mark, the manifest and new entries, and renews the lease when due.
    pub(crate) fn refresh(&self) -> Result<ReaderState, QueryError> {
        self.with_latest(|v| Ok(state_of(v)))
    }

    /// How many cuts the log reader holds: the current view's and those of views not yet released.
    #[cfg(test)]
    pub(crate) fn cuts_held(&self) -> usize {
        self.lock().log.as_ref().map_or(0, LogReader::cuts_held)
    }

    /// Losses the files report: the manifest's lost ranges, the recovery entries of the registry file and the log
    /// read, and the lost ranges this reader's reads found.
    pub(crate) fn losses(&self) -> Result<Vec<LossReport>, QueryError> {
        self.with_latest(|v| Ok(losses_of(v)))
    }
}

/// What the log reader held at `v`'s cut.
pub(crate) fn state_of(v: &View) -> ReaderState {
    let st = v.state();
    ReaderState {
        durable: pos(st.durable),
        epoch: st.epoch,
        generation: st.generation,
        stamp: ResetStamp {
            sequence: st.stamp.sequence,
            nonce: st.stamp.nonce,
        },
        lease_renewed_at: Timestamp(st.lease_renewed_at),
    }
}

/// Losses at `v`: its latest values' and, when it holds them, its fact index's finds.
pub(crate) fn losses_of(v: &View) -> Vec<LossReport> {
    let mut out = v.latest().losses(v.lost_ranges());
    if let Some(i) = v.index() {
        let records = super::loss::identities(v.lost_ranges());
        super::loss::merge(&mut out, i.losses(v.lost_ranges()), &records);
    }
    out
}

mod at_cut;
mod errors;
pub(crate) mod facts;
#[cfg(test)]
mod tests;
mod views;
