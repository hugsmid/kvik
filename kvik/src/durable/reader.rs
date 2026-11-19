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
use super::read::cut;
use super::read::flight::Flight;
use super::read::latest::{self, Latest};
use super::read::view::{Cuts, View};
use crate::backend::Error;
use crate::env::{Clock, Env};
use crate::error::{OpenError, QueryError};
use crate::health::LossReport;
use crate::read::ReaderState;
use crate::time::Timestamp;
use crate::types::ResetStamp;

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
    #[cfg(test)]
    lookups_made: AtomicU64,
}

pub(crate) fn open_error(dir: &Path, e: ReaderError) -> OpenError {
    match e {
        ReaderError::Store(e) => super::store_error(dir, e),
        ReaderError::MarkUnreadable => OpenError::MarkUnreadable,
        ReaderError::Unsupported { path, .. } => OpenError::UnsupportedFormat {
            file: path,
            found: 0,
            supported: u32::from(kvik_log::FORMAT_MAJOR),
        },
        ReaderError::Io { path, source } => OpenError::Io { path, source },
        other => OpenError::Corrupt {
            file: PathBuf::from(format!("{}: {other}", dir.display())),
        },
    }
}

/// A copy of an error that several calls report: the error of a refresh one call led and others followed.
pub(crate) fn copy(e: &QueryError) -> QueryError {
    use QueryError as Q;
    match e {
        Q::UnknownSeries => Q::UnknownSeries,
        Q::DetailNotLocal { local_from } => Q::DetailNotLocal {
            local_from: *local_from,
        },
        Q::AggregateRefused { reason } => Q::AggregateRefused {
            reason: reason.clone(),
        },
        Q::UnknownZone => Q::UnknownZone,
        Q::NotAvailable { reason } => Q::NotAvailable {
            reason: reason.clone(),
        },
        Q::LostRange { id } => Q::LostRange { id: *id },
        Q::MarkUnreadable => Q::MarkUnreadable,
        Q::UnsupportedFormat { file } => Q::UnsupportedFormat { file: file.clone() },
        Q::Corrupt { file } => Q::Corrupt { file: file.clone() },
        Q::Io { path, source } => Q::Io {
            path: path.clone(),
            source: match source.raw_os_error() {
                Some(code) => std::io::Error::from_raw_os_error(code),
                None => std::io::Error::new(source.kind(), source.to_string()),
            },
        },
        Q::Invalid { reason } => Q::Invalid {
            reason: reason.clone(),
        },
        Q::Closed => Q::Closed,
        Q::CutExpired => Q::CutExpired,
    }
}

fn unshare(e: Arc<QueryError>) -> QueryError {
    Arc::try_unwrap(e).unwrap_or_else(|e| copy(&e))
}

pub(crate) fn query_error(dir: &Path, e: ReaderError) -> QueryError {
    match e {
        ReaderError::MarkUnreadable => QueryError::MarkUnreadable,
        ReaderError::Unsupported { path, .. } => QueryError::UnsupportedFormat { file: path },
        ReaderError::Content { path, .. } | ReaderError::TooLong { path, .. } => {
            QueryError::Corrupt { file: path }
        }
        ReaderError::Io { path, source } => QueryError::Io { path, source },
        ReaderError::CutEnded(_) => QueryError::CutExpired,
        ReaderError::SegmentMissing { file_id } => QueryError::NotAvailable {
            reason: format!("segment {file_id} is missing"),
        },
        other => QueryError::Corrupt {
            file: PathBuf::from(format!("{}: {other}", dir.display())),
        },
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
        )
    }

    /// [`DurableReader::open`] with the window clock, refresh instants and loss latch of the process; `ages` is
    /// `lease_refresh` and `cut_max_age`.
    pub(crate) fn open_with(
        dir: &Path,
        env: &Env,
        ages: (Duration, Duration),
        clocks: Arc<Clocks>,
        latch: Arc<Latch>,
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
            want_facts: AtomicBool::new(false),
            lookups: Mutex::new(None),
            rows: Mutex::new(BTreeMap::new()),
            rows_read: AtomicBool::new(false),
            searched: Mutex::new(BTreeMap::new()),
            searches: AtomicU64::new(0),
            #[cfg(test)]
            lookups_made: AtomicU64::new(0),
            seeded: AtomicBool::new(false),
        })
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

    fn monotonic(&self) -> Duration {
        Duration::from_nanos(self.clock.monotonic().0)
    }

    /// A view read after this call began ([`Flight`]).
    fn view(&self) -> Result<Arc<View>, QueryError> {
        self.flight
            .get(|current| self.round(current))
            .map_err(unshare)
    }

    /// A view read after this call began that holds every fact: a later round's if the first does not, the fact
    /// index then opened (Q-1000).
    fn view_with_facts(&self) -> Result<Arc<View>, QueryError> {
        let v = self.view()?;
        if v.index().is_some() {
            return Ok(v);
        }
        self.want_facts.store(true, Ordering::SeqCst);
        drop(v);
        self.release_dropped();
        let v = self.view()?;
        if v.index().is_none() {
            return Err(QueryError::CutExpired);
        }
        Ok(v)
    }

    /// Takes a view read after this call began that holds every fact, then runs `f` on it with no lock under its cut:
    /// a materialised call that reads facts ([`cut`]).
    pub(crate) fn with<R>(
        &self,
        f: impl FnOnce(&Index, &View) -> Result<R, Error>,
    ) -> Result<R, QueryError> {
        let v = self.view_with_facts()?;
        let r = f(v.index().expect("a view with facts"), &v).map_err(Error::query);
        self.end(v)?;
        r
    }

    /// [`DurableReader::with`] for a call that reads the latest values, the registry or the losses, which no fact
    /// index holds up; a broken state's view holds one (Q-1006).
    pub(crate) fn with_latest<R>(
        &self,
        f: impl FnOnce(&View) -> Result<R, QueryError>,
    ) -> Result<R, QueryError> {
        let v = self.snapshot()?;
        let r = f(&v);
        self.end(v)?;
        r
    }

    /// A view for the latest values, a snapshot's among them (Q-722, Q-1005): read after the call began, with every
    /// fact if its state is broken.
    pub(crate) fn snapshot(&self) -> Result<Arc<View>, QueryError> {
        let v = self.view()?;
        if v.latest().broken() && v.index().is_none() {
            drop(v);
            return self.view_with_facts();
        }
        Ok(v)
    }

    /// Ends a call on `v`: whether its cut is still live, and the release of views no call holds.
    fn end(&self, v: Arc<View>) -> Result<(), QueryError> {
        let live = self.check(&v);
        drop(v);
        self.release_dropped();
        live
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
    fn round(&self, current: Option<Arc<View>>) -> Result<Arc<View>, Arc<QueryError>> {
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
        let mut lose =
            |latest: &mut Latest, facts: &mut Option<FactsReader>, recorded: &[LostRange]| {
                for (first, last) in self.latch.closed_since(&mut w.closed_seen) {
                    if !own.contains(&(first, last)) {
                        latest.lose(first, last, recorded);
                    }
                    if let Some(f) = facts.as_mut() {
                        f.index.forget(first, last);
                    }
                }
            };
        lose(latest, &mut w.facts, log.lost_ranges());
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
        lose(latest, &mut w.facts, log.lost_ranges());
        if log.lost_ranges() != &*w.lost_ranges {
            w.lost_ranges = log.lost_ranges().into();
        }
        Ok(Arc::new(View::new(
            (latest.clone(), w.facts.as_ref().map(|f| f.index.clone())),
            st,
            w.lost_ranges.clone(),
            cut::take(log),
            began,
            self.cuts.clone(),
        )))
    }

    /// Whether the call on `v` ends under a live cut ([`View::live`]). A call as old as half `lease_refresh` may
    /// have outlived a lease no refresh renewed since: the log reader's check then renews it when due and finds it
    /// gone (Q-853, Q-903).
    pub(crate) fn check(&self, v: &View) -> Result<(), QueryError> {
        let now = self.monotonic();
        if !v.live(now, self.cut_max_age) {
            return Err(QueryError::CutExpired);
        }
        if v.age(now) < self.lease_refresh / 2 {
            return Ok(());
        }
        let mut w = self.lock();
        let Some(log) = w.log.as_mut() else {
            return Err(QueryError::Closed);
        };
        let r = cut::check(log, v.cut());
        if let Err(ReaderError::CutEnded(CutEnd::LeaseLost)) = r {
            self.cuts.lose_all();
        }
        r.map_err(|e| query_error(&self.dir, e))
    }

    /// [`DurableReader::with`], called once more if the first call's cut ended: for the store's own reads, which
    /// have no caller to start again.
    pub(crate) fn with_again<R>(
        &self,
        mut f: impl FnMut(&Index, &View) -> Result<R, Error>,
    ) -> Result<R, QueryError> {
        match self.with(&mut f) {
            Err(QueryError::CutExpired) => self.with(f),
            other => other,
        }
    }

    /// [`DurableReader::with_latest`], called once more if the first call's cut ended.
    pub(crate) fn with_latest_again<R>(
        &self,
        mut f: impl FnMut(&View) -> Result<R, QueryError>,
    ) -> Result<R, QueryError> {
        match self.with_latest(&mut f) {
            Err(QueryError::CutExpired) => self.with_latest(f),
            other => other,
        }
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

    /// Lookups made at a cut.
    #[cfg(test)]
    pub(crate) fn lookups(&self) -> u64 {
        self.lookups_made.load(Ordering::Relaxed)
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
        super::loss::merge(&mut out, i.losses(v.lost_ranges()));
    }
    out
}

mod at_cut;
#[cfg(test)]
mod tests;
