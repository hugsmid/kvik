//! A reader on the durable log (`docs/api.md` Reader processes): the log reader of `kvik-log`, which takes no lock
//! and reads up to the durable mark, and an [`Index`] of what it returned. Every call reads the mark and the
//! entries appended since the last call first.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kvik_log::reader::{LogReader, ReaderConfig as LogReaderConfig, ReaderError};

use super::LogClock;
use super::index::{Index, pos};
use crate::backend::Error;
use crate::env::Env;
use crate::error::{OpenError, QueryError};
use crate::health::LossReport;
use crate::read::ReaderState;
use crate::time::Timestamp;
use crate::types::ResetStamp;

#[derive(Debug)]
struct State {
    log: Option<LogReader>,
    index: Index,
}

impl Drop for State {
    fn drop(&mut self) {
        // Deletes the lease; a reader that cannot is a lease left to expire.
        if let Some(r) = self.log.take() {
            let _ = r.close();
        }
    }
}

/// See the module documentation.
#[derive(Debug)]
pub(crate) struct DurableReader {
    dir: PathBuf,
    state: Mutex<State>,
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

fn query_error(dir: &Path, e: ReaderError) -> QueryError {
    match e {
        ReaderError::MarkUnreadable => QueryError::MarkUnreadable,
        ReaderError::Unsupported { path, .. } => QueryError::UnsupportedFormat { file: path },
        ReaderError::Content { path, .. } | ReaderError::TooLong { path, .. } => {
            QueryError::Corrupt { file: path }
        }
        ReaderError::Io { path, source } => QueryError::Io { path, source },
        ReaderError::SegmentMissing { file_id } => QueryError::NotAvailable {
            reason: format!("segment {file_id} is missing"),
        },
        other => QueryError::Corrupt {
            file: PathBuf::from(format!("{}: {other}", dir.display())),
        },
    }
}

impl DurableReader {
    /// Opens a reader on the store in `dir`; reads nothing until the first call.
    pub(crate) fn open(dir: &Path, env: &Env, lease_refresh: Duration) -> Result<Self, OpenError> {
        let mut id = [0u8; 16];
        env.entropy.fill(&mut id);
        let mut cfg = LogReaderConfig::new(
            dir,
            env.fs.clone(),
            Arc::new(LogClock::new(env.clock.clone())),
            env.boot.current(),
            id,
        );
        cfg.lease_refresh = lease_refresh;
        let log = LogReader::open(cfg).map_err(|e| open_error(dir, e))?;
        Ok(DurableReader {
            dir: dir.to_path_buf(),
            state: Mutex::new(State {
                log: Some(log),
                index: Index::default(),
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Reads what was appended since the last call, then runs `f` on the index.
    pub(crate) fn with<R>(
        &self,
        f: impl FnOnce(&Index, &LogReader) -> Result<R, Error>,
    ) -> Result<R, QueryError> {
        let mut s = self.lock();
        let s = &mut *s;
        let Some(log) = s.log.as_mut() else {
            return Err(QueryError::Closed);
        };
        let update = log.refresh().map_err(|e| query_error(&self.dir, e))?;
        s.index.apply(&update.events);
        f(&s.index, log).map_err(Error::query)
    }

    /// Re-reads the mark, the manifest and new entries and renews the lease.
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

    /// Losses the files report: the manifest's lost ranges and the recovery entries of the kept log.
    pub(crate) fn losses(&self) -> Result<Vec<LossReport>, QueryError> {
        self.with(|index, log| Ok(index.losses(log.lost_ranges())))
    }
}
