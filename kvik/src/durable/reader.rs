//! A reader on the durable log (`docs/api.md` Reader processes): the log reader of `kvik-log`, which takes no lock
//! and reads up to the durable mark, and an [`Index`] of what it returned. Every call reads the mark and the
//! entries appended since the last call first, forgets the ranges the process found lost since (Q-938), and then
//! runs under its cut ([`cut`]).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kvik_log::reader::{LogReader, ReaderConfig as LogReaderConfig, ReaderError};

use super::clock::Clocks;
use super::index::{Index, pos};
use super::loss::{FoundBy, Latch};
use super::read::cut;
use crate::backend::Error;
use crate::env::{Clock, Env};
use crate::error::{OpenError, QueryError};
use crate::health::LossReport;
use crate::read::ReaderState;
use crate::time::Timestamp;
use crate::types::ResetStamp;

#[derive(Debug)]
struct State {
    log: Option<LogReader>,
    index: Index,
    /// The latch's closed ranges already forgotten.
    closed_seen: usize,
    /// Items a refresh takes at a time.
    part: usize,
    /// Parts taken.
    #[cfg(test)]
    parts: u64,
}

impl Drop for State {
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
    state: Mutex<State>,
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
        Ok(DurableReader {
            dir: dir.to_path_buf(),
            state: Mutex::new(State {
                log: Some(log),
                index: Index::default(),
                closed_seen: 0,
                part: super::start::PART,
                #[cfg(test)]
                parts: 0,
            }),
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

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Reads what was appended since the last call, then runs `f` on the index under the call's cut: a materialised
    /// call ([`cut`]).
    pub(crate) fn with<R>(
        &self,
        f: impl FnOnce(&Index, &LogReader) -> Result<R, Error>,
    ) -> Result<R, QueryError> {
        let mut s = self.lock();
        let s = &mut *s;
        let Some(log) = s.log.as_mut() else {
            return Err(QueryError::Closed);
        };
        // In parts, so that a first read of a long log holds one part's batches beside the index at a time. A part
        // the log reader discarded because the reset stamp changed while it read is read once more; a second
        // discard in the call is `CutExpired` (Q-850).
        let mut discarded = false;
        loop {
            let (update, more) = log
                .refresh_some(s.part)
                .map_err(|e| query_error(&self.dir, e))?;
            #[cfg(test)]
            {
                s.parts += 1;
            }
            if update.discarded {
                if std::mem::replace(&mut discarded, true) {
                    return Err(QueryError::CutExpired);
                }
                continue;
            }
            let found = s.index.apply(&update.events, update.state.durable);
            if !found.is_empty() {
                self.latch.note(&found, FoundBy::Query);
            }
            if !more {
                break;
            }
        }
        for (first, last) in self.latch.closed_since(&mut s.closed_seen) {
            s.index.forget(first, last);
        }
        let c = cut::take(log);
        let r = f(&s.index, log).map_err(Error::query);
        let live = cut::check(log, &c);
        log.release(c.id);
        live.map_err(|e| query_error(&self.dir, e))?;
        r
    }

    /// [`DurableReader::with`], called once more if the first call's cut ended: for the store's own reads, which
    /// have no caller to start again.
    pub(crate) fn with_again<R>(
        &self,
        mut f: impl FnMut(&Index, &LogReader) -> Result<R, Error>,
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

    /// How many cuts the log reader holds.
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
    use crate::read::View;
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
                .with(|i, _| i.facts(SeriesId(1), range, View::History))
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
}
