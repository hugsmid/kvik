//! The damage scan of a seeded start (Q-947, Q-1115): the log below the seed read for invalid regions, each loss
//! noted in the process's latch as the start from the whole log notes it. By default it runs after the open returns,
//! as the derived files' start work does; [`ScanOrder::BeforeOpen`] restores the order in which a damaged store is
//! found before the open returns.

#[cfg(test)]
use std::collections::BTreeMap;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
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

/// When a seeded start scans the log below its seed for damage (Q-1115).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScanOrder {
    /// After the open returns, on a thread of its own; a loss it finds is reported as one found while open.
    Background,
    /// Before the open returns; damage below the seed makes the start read the whole log.
    BeforeOpen,
}

/// The order a seeded start uses (Q-1115).
pub(crate) const SCAN_ORDER: ScanOrder = ScanOrder::Background;

/// Orders tests set for the store in a directory, in place of [`SCAN_ORDER`].
#[cfg(test)]
pub(crate) static SCAN_ORDERS: Mutex<BTreeMap<PathBuf, ScanOrder>> = Mutex::new(BTreeMap::new());

/// Gates tests set for the store in a directory: its background scan starts once the gate opens.
#[cfg(test)]
pub(crate) static SCAN_GATES: Mutex<BTreeMap<PathBuf, Arc<Gate>>> = Mutex::new(BTreeMap::new());

/// A gate a test holds a background scan at.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct Gate {
    open: Mutex<bool>,
    cv: std::sync::Condvar,
}

#[cfg(test)]
impl Gate {
    pub(crate) fn open(&self) {
        *self
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.cv.notify_all();
    }

    /// Waits until the gate opens or `stop` is set.
    fn wait(&self, stop: &AtomicBool) {
        let mut open = self
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !*open && !stop.load(Ordering::SeqCst) {
            open = self
                .cv
                .wait_timeout(open, std::time::Duration::from_millis(5))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

#[cfg(test)]
fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The order for the store in `dir`.
pub(crate) fn scan_order(dir: &Path) -> ScanOrder {
    #[cfg(test)]
    if let Some(o) = locked(&SCAN_ORDERS).get(dir) {
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
        // Every find names `first`; a region that lost no position (`last < first`) gives no find, so it is not below.
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
    latch.complete(finder.found());
    Ok(read?.map(|()| out))
}

/// How many of the regions a scan met (`found`) the start's pass did not (`passed`) and no lost range of `recorded`
/// holds (Q-755).
pub(crate) fn unmet(
    found: &Regions,
    passed: &Regions,
    recorded: &[LostRange],
    standing: &[u64],
) -> u64 {
    let mut own = Regions::default();
    for (f, a, z) in found.spans() {
        if !passed.covers(f, a, z) {
            own.insert(f, a, z);
        }
    }
    own.unrecorded(recorded, standing)
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
        #[cfg(test)]
        let gate = locked(&SCAN_GATES).get(dir).cloned();
        let (dir, env) = (dir.to_path_buf(), env.clone());
        let s = stop.clone();
        let thread = std::thread::Builder::new()
            .name("kvik-scan".into())
            .spawn(move || {
                #[cfg(test)]
                if let Some(g) = gate {
                    g.wait(&s);
                }
                if let Ok(Some(found)) = scan_below(&dir, &env, through, &latch, &s) {
                    start_regions(unmet(&found.regions, &passed, &recorded, &standing));
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
