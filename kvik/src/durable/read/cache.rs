//! One memory cap for everything a reader's queries allocate (`ReaderConfig::memory`, the query design's §1 Memory
//! and §3.5): grants for running passes, a log block's reserve within each, and the cache's share, a quarter of the
//! cap and at least what the lookups' log reader keeps (Q-1063).

use std::sync::{Arc, Condvar, Mutex, PoisonError};

use kvik_log::reader::LOOKUP_CACHE_BYTES;

/// The default cap (`ReaderConfig::memory`).
pub(crate) const DEFAULT_MEMORY: u64 = 64 << 20;
/// The least cap: a log block's body may be 16 MiB and is read whole to verify (the query design's §1 Memory).
pub(crate) const LEAST_MEMORY: u64 = 32 << 20;
/// What a pass reserves for the largest log block it may read.
const BLOCK_RESERVE: u64 = 16 << 20;
/// The least row budget a pass is admitted with.
const MIN_ROWS: u64 = 1 << 20;

/// The sizes the accountant works with; other values in tests only, to force multi-pass reads on small histories.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Sizes {
    pub(crate) cap: u64,
    pub(crate) block: u64,
    pub(crate) min_rows: u64,
    /// The cache the lookups' log reader keeps, counted in the cache's share.
    pub(crate) lookups: u64,
}

impl Sizes {
    pub(crate) fn of(cap: u64) -> Self {
        Sizes {
            cap,
            block: BLOCK_RESERVE,
            min_rows: MIN_ROWS,
            lookups: LOOKUP_CACHE_BYTES as u64,
        }
    }

    /// The cache's share, which no pass takes from it.
    fn floor(&self) -> u64 {
        (self.cap / 4).max(self.lookups)
    }
}

#[derive(Debug, Default)]
struct State {
    granted: u64,
    grants: usize,
    #[cfg(test)]
    peak: u64,
}

/// See the module documentation.
#[derive(Debug)]
pub(crate) struct Accountant {
    sizes: Sizes,
    state: Mutex<State>,
    freed: Condvar,
}

/// Memory granted to one pass, given back when dropped.
#[derive(Debug)]
pub(crate) struct Grant {
    acc: Arc<Accountant>,
    bytes: u64,
}

impl Grant {
    /// The bytes of facts the pass may decode: its grant less the block reserve.
    pub(crate) fn rows(&self) -> u64 {
        self.bytes - self.acc.sizes.block
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        let mut s = self.acc.lock();
        s.granted -= self.bytes;
        s.grants -= 1;
        drop(s);
        self.acc.freed.notify_all();
    }
}

impl Accountant {
    pub(crate) fn new(sizes: Sizes) -> Arc<Self> {
        Arc::new(Accountant {
            sizes,
            state: Mutex::new(State::default()),
            freed: Condvar::new(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What passes may hold together: the cap less the cache's share.
    fn for_passes(&self) -> u64 {
        self.sizes.cap - self.sizes.floor()
    }

    fn least(&self) -> u64 {
        self.sizes.block + self.sizes.min_rows
    }

    /// Admits a pass: waits until the least grant is free, and takes all that is free when that is under two least grants, else half of it. Only running passes hold grants, and a pass
    /// waits on nothing while it holds one, so the wait ends (Q-1063).
    pub(crate) fn admit(self: &Arc<Self>) -> Grant {
        let least = self.least();
        let mut s = self.lock();
        loop {
            let free = self.for_passes() - s.granted;
            if free >= least {
                let bytes = if free < 2 * least {
                    free
                } else {
                    (free / 2).max(least)
                };
                s.granted += bytes;
                s.grants += 1;
                #[cfg(test)]
                {
                    s.peak = s.peak.max(s.granted + self.sizes.floor());
                }
                return Grant {
                    acc: self.clone(),
                    bytes,
                };
            }
            s = self.freed.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// The most bytes grants and the cache's share held together.
    #[cfg(test)]
    pub(crate) fn peak(&self) -> u64 {
        self.lock().peak
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_default_cap_admits_two_passes_and_the_least_one() {
        let a = Accountant::new(Sizes::of(DEFAULT_MEMORY));
        let (g1, g2) = (a.admit(), a.admit());
        assert!(g1.rows() >= MIN_ROWS && g2.rows() >= MIN_ROWS);
        assert!(g1.bytes + g2.bytes + DEFAULT_MEMORY / 4 <= DEFAULT_MEMORY);
        let least = Accountant::new(Sizes::of(LEAST_MEMORY));
        let g = least.admit();
        assert_eq!(g.rows(), LEAST_MEMORY - LEAST_MEMORY / 4 - BLOCK_RESERVE);
    }

    #[test]
    fn a_pass_waits_until_a_grant_is_given_back() {
        let a = Accountant::new(Sizes::of(LEAST_MEMORY));
        let g = a.admit();
        let b = a.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(b.admit().rows()));
        assert!(
            rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "a second pass waits under the least cap"
        );
        drop(g);
        assert!(rx.recv_timeout(Duration::from_secs(10)).expect("admitted once given back") > 0);
    }
}
