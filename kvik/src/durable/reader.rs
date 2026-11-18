//! A reader on the durable log (`docs/api.md` Reader processes): the log reader of `kvik-log`, which takes no lock
//! and reads up to the durable mark, and an [`Index`] of what it returned. Every call takes a view published by a
//! refresh that began after the call did, and runs on it with no lock under the view's cut (Q-902, Q-903,
//! [`cut`], [`view`](super::read::view)). The refresh notes the losses it finds in the process's latch and forgets
//! every range the latch took since the last one from the index the view is cloned from (Q-938).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::Duration;

use kvik_log::lease::CutEnd;
use kvik_log::manifest::LostRange;
use kvik_log::reader::{LogReader, ReaderConfig as LogReaderConfig, ReaderError};

use super::clock::Clocks;
use super::index::{Index, pos};
use super::loss::{FoundBy, Latch};
use super::read::cut;
use super::read::flight::Flight;
use super::read::view::{Cuts, View};
use crate::backend::Error;
use crate::env::{Clock, Env};
use crate::error::{OpenError, QueryError};
use crate::health::LossReport;
use crate::read::ReaderState;
use crate::time::Timestamp;
use crate::types::ResetStamp;

/// What the refresh leading a round works on: the log reader, and the index the next view is a clone of.
#[derive(Debug)]
struct Work {
    log: Option<LogReader>,
    index: Index,
    lost_ranges: Arc<[LostRange]>,
    /// The latch's closed ranges already forgotten.
    closed_seen: usize,
    /// Items a refresh takes at a time.
    part: usize,
    /// Parts taken.
    #[cfg(test)]
    parts: u64,
}

impl Drop for Work {
    fn drop(&mut self) {
        // Deletes the lease; a reader that cannot is a lease left to expire.
        if let Some(r) = self.log.take() {
            let _ = r.close();
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
fn copy(e: &QueryError) -> QueryError {
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

/// Reads what was appended since the last round into `index`, in parts of `part` items, so that a first read of a
/// long log holds one part's batches beside the index at a time, noting each part's finds in `latch` as the query's.
/// A part the log reader discarded because the reset stamp changed while it read is read once more; a second
/// discard in the round is `CutExpired` (Q-850, Q-948).
fn take(
    log: &mut LogReader,
    index: &mut Index,
    part: usize,
    latch: &Latch,
    #[cfg(test)] parts: &mut u64,
) -> Result<Result<(), QueryError>, ReaderError> {
    let mut discarded = false;
    loop {
        let (update, more) = log.refresh_some(part)?;
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
        let found = index.apply(&update.events, update.state.durable);
        if !found.is_empty() {
            latch.note(&found, FoundBy::Query);
        }
        if !more {
            return Ok(Ok(()));
        }
    }
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
            work: Mutex::new(Work {
                log: Some(log),
                index: Index::default(),
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

    /// Takes a view read after this call began ([`Flight`]), then runs `f` on it with no lock under its cut: a
    /// materialised call ([`cut`]).
    pub(crate) fn with<R>(
        &self,
        f: impl FnOnce(&Index, &View) -> Result<R, Error>,
    ) -> Result<R, QueryError> {
        let v = self
            .flight
            .get(|current| self.round(current))
            .map_err(unshare)?;
        let r = f(v.index(), &v).map_err(Error::query);
        let live = self.check(&v);
        drop(v);
        self.release_dropped();
        live?;
        r
    }

    /// Releases the cuts of views no call holds any more, unless a round runs, which releases them itself.
    fn release_dropped(&self) {
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

    /// One refresh, which the call leading a round runs: releases the cuts of dropped views, reads what was appended
    /// since the last round and publishes the next view, holding its own cut.
    fn round(&self, current: Option<Arc<View>>) -> Result<Arc<View>, Arc<QueryError>> {
        let began = self.monotonic();
        let mut w = self.lock();
        let w = &mut *w;
        let Some(log) = w.log.as_mut() else {
            return Err(Arc::new(QueryError::Closed));
        };
        for id in self.cuts.take_dropped() {
            log.release(id);
        }
        let taken = take(
            log,
            &mut w.index,
            w.part,
            &self.latch,
            #[cfg(test)]
            &mut w.parts,
        );
        // A renewal that found the lease gone or changed ended every cut, the current view's among them.
        if let Some(v) = &current
            && let Err(ReaderError::CutEnded(CutEnd::LeaseLost)) = cut::check(log, v.cut())
        {
            self.cuts.lose_all();
        }
        taken
            .map_err(|e| query_error(&self.dir, e))
            .and_then(|t| t)
            .map_err(Arc::new)?;
        for (first, last) in self.latch.closed_since(&mut w.closed_seen) {
            w.index.forget(first, last);
        }
        if log.lost_ranges() != &*w.lost_ranges {
            w.lost_ranges = log.lost_ranges().into();
        }
        Ok(Arc::new(View::new(
            w.index.clone(),
            log.state(),
            w.lost_ranges.clone(),
            cut::take(log),
            began,
            self.cuts.clone(),
        )))
    }

    /// Whether the call on `v` ends under a live cut ([`View::live`]). A call as old as half `lease_refresh` may
    /// have outlived a lease no refresh renewed since: the log reader's check then renews it when due and finds it
    /// gone (Q-853, Q-903).
    fn check(&self, v: &View) -> Result<(), QueryError> {
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

    /// Re-reads the mark, the manifest and new entries, and renews the lease when due.
    pub(crate) fn refresh(&self) -> Result<ReaderState, QueryError> {
        self.with(|_, log| {
            let st = log.state();
            Ok(ReaderState {
                durable: pos(st.durable),
                epoch: st.epoch,
                generation: st.generation,
                stamp: ResetStamp {
                    sequence: st.stamp.sequence,
                    nonce: st.stamp.nonce,
                },
                lease_renewed_at: Timestamp(st.lease_renewed_at),
            })
        })
    }

    /// How many cuts the log reader holds: the current view's and those of views not yet released.
    #[cfg(test)]
    pub(crate) fn cuts_held(&self) -> usize {
        self.lock().log.as_ref().map_or(0, LogReader::cuts_held)
    }

    /// Losses the files report: the manifest's lost ranges and the recovery entries of the kept log.
    pub(crate) fn losses(&self) -> Result<Vec<LossReport>, QueryError> {
        self.with(|index, log| Ok(index.losses(log.lost_ranges())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::ManualClock;
    use crate::read::View as Shown;
    use crate::series::{Kind, SeriesSpec, ValueType};
    use crate::time::TimeRange;
    use crate::types::{Reading, SeriesId, Value};
    use crate::{Store, StoreConfig};

    #[test]
    fn a_first_read_in_parts_builds_the_index_one_read_whole_does() {
        const T0: i64 = 1_767_225_600_000_000_000;
        const S: i64 = 1_000_000_000;
        let env = Env::test(ManualClock::new(Timestamp(T0 + 4_000 * S)));
        let dir = Path::new("reader-parts");
        let config = StoreConfig::new(dir, "Etc/UTC")
            .create_if_missing(true)
            .env(env.clone());
        let s = crate::durable::tests::block_on(Store::open(config)).unwrap();
        let spec = SeriesSpec::new(
            "a/b/v",
            Kind::Gauge {
                value: ValueType::F64,
            },
            "W",
        );
        crate::durable::tests::block_on(s.define_series(spec)).unwrap();
        for chunk in (0..3_000i64).collect::<Vec<_>>().chunks(250) {
            let rs: Vec<Reading> = chunk
                .iter()
                .map(|&k| Reading::new(SeriesId(1), Timestamp(T0 + k * S), Value::F64(k as f64)))
                .collect();
            crate::durable::tests::block_on(s.emit(&rs)).unwrap();
        }
        crate::durable::tests::block_on(s.shutdown()).unwrap();
        let read = |part: usize| {
            let r =
                DurableReader::open(dir, &env, Duration::from_secs(60), Duration::from_secs(60))
                    .unwrap();
            r.lock().part = part;
            let range = TimeRange::new(Timestamp(T0), Timestamp(T0 + 4_000 * S));
            let facts = r
                .with(|i, _| i.facts(SeriesId(1), range, Shown::History))
                .unwrap();
            let parts = r.lock().parts;
            (facts, parts)
        };
        let (parts, n) = read(100);
        let (whole, one) = read(usize::MAX);
        assert_eq!(parts.len(), 3_000);
        assert_eq!(parts, whole);
        assert!(n >= 12 && one == 1, "{n} parts, {one} for the whole");
    }

    #[test]
    fn a_copied_io_error_keeps_its_operating_system_code() {
        let e = QueryError::Io {
            path: PathBuf::from("a"),
            source: std::io::Error::from_raw_os_error(28),
        };
        let QueryError::Io { source, .. } = copy(&e) else {
            panic!("an I/O error")
        };
        assert_eq!(source.raw_os_error(), Some(28));
    }
}
