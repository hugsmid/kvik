//! One memory cap for everything a reader's queries allocate (`ReaderConfig::memory`, the query design's §1 Memory
//! and §3.5): grants for running passes, a log block's reserve within each, and a cache of decoded passes that gives
//! way down to a quarter of the cap before any pass is squeezed (Q-1063).

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use kvik_log::mark::ResetStamp as LogStamp;
use kvik_log::reader::LOOKUP_CACHE_BYTES;
use kvik_log::writer::Position as LogPos;

use crate::durable::index::Stored;

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

/// What a decoded pass is keyed by: its series, inclusive event-time range and position bound, under one reset stamp
/// and the losses known when it was read (a fact found lost later is never shown, Q-1006).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PassKey {
    pub(crate) series: Vec<u32>,
    pub(crate) from: i64,
    pub(crate) to: i64,
    pub(crate) bound: LogPos,
    pub(crate) stamp: LogStamp,
    pub(crate) losses: (usize, usize),
}

#[derive(Debug)]
struct Entry {
    rows: Arc<[Stored]>,
    bytes: u64,
    used: u64,
}

#[derive(Debug, Default)]
struct State {
    granted: u64,
    grants: usize,
    cache: HashMap<PassKey, Entry>,
    cached: u64,
    tick: u64,
    #[cfg(test)]
    peak: u64,
}

impl State {
    /// Drops the least recently used entries until the cache holds at most `room`.
    fn trim(&mut self, room: u64) {
        while self.cached > room {
            let Some(k) = self
                .cache
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            let e = self.cache.remove(&k).expect("just found");
            self.cached -= e.bytes;
        }
    }
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

    /// Admits a pass: waits until the least grant is free, the cache trimmed to its share first, and takes all that
    /// is free when that is under two least grants, else half of it. Only running passes hold grants, and a pass
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
                let room = self.sizes.cap - s.granted - self.sizes.lookups;
                s.trim(room);
                #[cfg(test)]
                {
                    s.peak = s.peak.max(s.granted + s.cached + self.sizes.lookups);
                }
                return Grant {
                    acc: self.clone(),
                    bytes,
                };
            }
            s = self.freed.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// The rows of a decoded pass, if cached.
    pub(crate) fn cached(&self, key: &PassKey) -> Option<Arc<[Stored]>> {
        let mut s = self.lock();
        s.tick += 1;
        let tick = s.tick;
        let e = s.cache.get_mut(key)?;
        e.used = tick;
        Some(e.rows.clone())
    }

    /// Keeps a decoded pass of `bytes` if it fits beside what passes hold.
    pub(crate) fn keep(&self, key: PassKey, rows: Arc<[Stored]>, bytes: u64) {
        let mut s = self.lock();
        let room = self.sizes.cap - s.granted - self.sizes.lookups;
        if bytes > room {
            return;
        }
        s.tick += 1;
        let used = s.tick;
        if let Some(old) = s.cache.insert(key, Entry { rows, bytes, used }) {
            s.cached -= old.bytes;
        }
        s.cached += bytes;
        s.trim(room);
        #[cfg(test)]
        {
            s.peak = s.peak.max(s.granted + s.cached + self.sizes.lookups);
        }
    }

    /// The most bytes grants, the cache and the lookups' cache held together.
    #[cfg(test)]
    pub(crate) fn peak(&self) -> u64 {
        self.lock().peak
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn key(n: i64) -> PassKey {
        PassKey {
            series: vec![1],
            from: n,
            to: n,
            bound: LogPos::default(),
            stamp: LogStamp::default(),
            losses: (0, 0),
        }
    }

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
        let t = std::thread::spawn(move || b.admit().rows());
        std::thread::sleep(Duration::from_millis(50));
        assert!(!t.is_finished(), "a second pass waits under the least cap");
        drop(g);
        assert!(t.join().unwrap() > 0);
    }

    #[test]
    fn the_cache_gives_way_to_a_pass_down_to_its_share() {
        let sizes = Sizes {
            cap: 1_000,
            block: 100,
            min_rows: 100,
            lookups: 100,
        };
        let a = Accountant::new(sizes);
        for n in 0..9 {
            a.keep(key(n), Arc::from(Vec::new()), 100);
        }
        assert!(a.cached(&key(8)).is_some());
        let g = a.admit();
        assert_eq!(g.bytes, 750 / 2);
        assert!(a.peak() <= 1_000);
        assert!(
            a.cached(&key(0)).is_none(),
            "the least recently used went first"
        );
        assert!(a.cached(&key(8)).is_some());
        drop(g);
        a.keep(key(20), Arc::from(Vec::new()), 901);
        assert!(
            a.cached(&key(20)).is_none(),
            "an entry above the cap is not kept"
        );
    }
}
