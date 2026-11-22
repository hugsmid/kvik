//! The damage scan of a seeded start (Q-947, Q-1110): the log below the seed read for invalid regions, each loss
//! noted in the process's latch as the start from the whole log notes it. By default it runs after the open returns,
//! as the derived files' start work does; [`ScanOrder::BeforeOpen`] restores the order in which a damaged store is
//! found before the open returns.

use std::path::Path;
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use kvik_log::manifest::LostRange;
use kvik_log::reader::{ReadEvent, ReaderError};
use kvik_log::writer::Position as LogPos;

use super::loss::{Finder, Latch, Regions};
use super::start::{PART, log_reader};
use crate::env::Env;

/// When a seeded start scans the log below its seed for damage (Q-1110).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScanOrder {
    /// After the open returns, on a thread of its own; a loss it finds is reported as one found while open.
    Background,
    /// Before the open returns; damage below the seed makes the start read the whole log.
    BeforeOpen,
}

/// The order a seeded start uses (Q-1110).
pub(crate) const SCAN_ORDER: ScanOrder = ScanOrder::Background;

/// Orders tests set for the store in a directory, in place of [`SCAN_ORDER`].
#[cfg(test)]
pub(crate) static SCAN_ORDERS: Mutex<Vec<(std::path::PathBuf, ScanOrder)>> = Mutex::new(Vec::new());

/// The order for the store in `dir`.
pub(crate) fn scan_order(dir: &Path) -> ScanOrder {
    #[cfg(test)]
    if let Some((_, o)) = SCAN_ORDERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|(d, _)| d == dir)
    {
        return *o;
    }
    let _ = dir;
    SCAN_ORDER
}

/// What a scan found below the seed: the regions it met and whether any held a position at or below it.
#[derive(Debug, Default)]
pub(crate) struct Scanned {
    pub(crate) regions: Regions,
    pub(crate) below: bool,
}

/// Reads the log up to `through` with a fresh reader, noting every loss found in `latch`. `None` when `stop` was set
/// first.
pub(crate) fn scan_below(
    dir: &Path,
    env: &Env,
    through: LogPos,
    latch: &Latch,
    stop: &AtomicBool,
) -> Result<Option<Scanned>, ReaderError> {
    let mut reader = log_reader(dir, env)?;
    let mut finder = Finder::default();
    let mut out = Scanned::default();
    let at = crate::types::Position::new(through.generation, through.sequence);
    let read = loop {
        if stop.load(Ordering::SeqCst) {
            break Ok(None);
        }
        let (u, more) = match reader.refresh_upto(PART, through) {
            Ok(r) => r,
            Err(e) => break Err(e),
        };
        let found = finder.apply(&u.events, u.state.durable);
        out.below |= found
            .iter()
            .any(|f| f.report.first.is_some_and(|p| p <= at));
        if !found.is_empty() {
            latch.note(&found);
        }
        for e in &u.events {
            if let ReadEvent::Corruption { file_id, region } = e {
                out.regions.insert(*file_id, region.start, region.end);
            }
        }
        reader.release_bytes();
        if !more {
            break Ok(Some(()));
        }
    };
    let _ = reader.close();
    Ok(read?.map(|()| out))
}

/// A background scan, stopped and joined when dropped.
#[derive(Debug)]
pub(crate) struct Scan {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Scan {
    /// Starts the scan below `through` on its own thread. Once it ends, the regions it met that the start's pass did
    /// not (`passed`) and that no lost range of `recorded` holds are added to `start_regions` (Q-755).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn(
        dir: &Path,
        env: &Env,
        through: LogPos,
        latch: Arc<Latch>,
        passed: Regions,
        recorded: Vec<LostRange>,
        standing: Vec<u64>,
        start_regions: impl FnOnce(u64) + Send + 'static,
    ) -> Scan {
        let stop = Arc::new(AtomicBool::new(false));
        let (dir, env) = (dir.to_path_buf(), env.clone());
        let s = stop.clone();
        let thread = std::thread::Builder::new()
            .name("kvik-scan".into())
            .spawn(move || {
                if let Ok(Some(found)) = scan_below(&dir, &env, through, &latch, &s) {
                    let mut own = Regions::default();
                    for (f, a, z) in found.regions.spans() {
                        if !passed.covers(f, a, z) {
                            own.insert(f, a, z);
                        }
                    }
                    start_regions(own.unrecorded(&recorded, &standing));
                }
            })
            .ok();
        Scan { stop, thread }
    }

    /// Whether the scan has ended.
    pub(crate) fn done(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Waits for the scan to end.
    #[cfg(test)]
    pub(crate) fn wait(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Scan {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
