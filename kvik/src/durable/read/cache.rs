//! One memory cap for everything a reader's queries allocate (`ReaderConfig::memory`, the query design's §1 Memory
//! and §3.5): grants for running passes, a log block's reserve within each, and the cache's share, a quarter of the
//! cap and at least what the lookups' log reader keeps (Q-1063).

use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};

use kvik_log::reader::LOOKUP_CACHE_BYTES;

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

/// The rows a stream holds for delivery, with the grant that counts them; emptied by an admission that needs the
/// memory, after which the stream reads them again (Q-1067).
pub(crate) type Shelf = Mutex<Option<(std::vec::IntoIter<Stored>, Grant)>>;

#[derive(Debug, Default)]
struct State {
    /// Granted to running passes, which wait on nothing.
    running: u64,
    /// Granted to rows streams hold for delivery.
    shelved: u64,
    shelves: Vec<Weak<Shelf>>,
}

/// See the module documentation.
#[derive(Debug)]
pub(crate) struct Accountant {
    sizes: Sizes,
    state: Mutex<State>,
    freed: Condvar,
}

/// Memory granted to one pass, or to the rows a stream holds, given back when dropped.
#[derive(Debug)]
pub(crate) struct Grant {
    acc: Arc<Accountant>,
    bytes: u64,
    shelved: bool,
}

impl Grant {
    /// The bytes of facts the pass may keep: its grant less the block reserve.
    pub(crate) fn rows(&self) -> u64 {
        self.bytes - self.acc.sizes.block
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        let mut s = self.acc.lock();
        if self.shelved {
            s.shelved -= self.bytes;
        } else {
            s.running -= self.bytes;
        }
        drop(s);
        self.acc.freed.notify_all();
    }
}

impl Accountant {
    pub(crate) fn new(sizes: Sizes) -> Arc<Self> {
        assert!(
            sizes.cap - sizes.floor() >= sizes.block + sizes.min_rows,
            "the cap admits one pass"
        );
        Arc::new(Accountant {
            sizes,
            state: Mutex::new(State::default()),
            freed: Condvar::new(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What passes and shelves may hold together: the cap less the cache's share.
    fn for_passes(&self) -> u64 {
        self.sizes.cap - self.sizes.floor()
    }

    fn least(&self) -> u64 {
        self.sizes.block + self.sizes.min_rows
    }

    /// Admits a pass: takes all that is free when that is under two least grants, else half of it. While less than
    /// the least grant is free it waits for running passes, which wait on nothing, when their grants would make
    /// room, and otherwise empties shelves, so no admission waits on a caller's iteration (Q-1063, Q-1067).
    pub(crate) fn admit(self: &Arc<Self>) -> Grant {
        let least = self.least();
        let mut s = self.lock();
        loop {
            let free = self.for_passes() - s.running - s.shelved;
            if free >= least {
                let bytes = if free < 2 * least {
                    free
                } else {
                    (free / 2).max(least)
                };
                s.running += bytes;
                return Grant {
                    acc: self.clone(),
                    bytes,
                    shelved: false,
                };
            }
            if s.running > 0 && free + s.running >= least {
                s = self.freed.wait(s).unwrap_or_else(PoisonError::into_inner);
                continue;
            }
            let shelves = std::mem::take(&mut s.shelves);
            drop(s);
            let mut kept = Vec::new();
            let mut emptied = false;
            for w in shelves {
                let Some(shelf) = w.upgrade() else { continue };
                if emptied {
                    kept.push(w);
                    continue;
                }
                let taken = shelf.lock().unwrap_or_else(PoisonError::into_inner).take();
                emptied = taken.is_some();
                drop(taken);
            }
            s = self.lock();
            s.shelves.extend(kept);
            assert!(
                emptied || s.running > 0 || s.shelved == 0,
                "shelved rows are on a shelf"
            );
        }
    }

    /// Puts `rows` on a shelf, the grant shrunk to the `held` bytes they take.
    pub(crate) fn shelve(&self, rows: Vec<Stored>, mut grant: Grant, held: u64) -> Arc<Shelf> {
        let mut s = self.lock();
        let held = held.min(grant.bytes);
        s.running -= grant.bytes;
        s.shelved += held;
        grant.bytes = held;
        grant.shelved = true;
        let shelf = Arc::new(Mutex::new(Some((rows.into_iter(), grant))));
        s.shelves.retain(|w| w.strong_count() > 0);
        s.shelves.push(Arc::downgrade(&shelf));
        drop(s);
        self.freed.notify_all();
        shelf
    }
}

/// The bytes `rows` take as a lookup counts them: the records their vector has room for and their texts.
pub(crate) fn held(rows: &Vec<Stored>) -> u64 {
    let texts: u64 = rows
        .iter()
        .map(|r| match &r.row.value {
            kvik_log::facts::Value::Text(t) => t.len() as u64,
            _ => 0,
        })
        .sum();
    (rows.capacity() * std::mem::size_of::<Stored>()) as u64 + texts
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
        assert!(
            rx.recv_timeout(Duration::from_secs(10))
                .expect("admitted once given back")
                > 0
        );
    }

    fn admitted_within(a: &Arc<Accountant>, d: Duration) -> Option<Grant> {
        let b = a.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(b.admit()));
        rx.recv_timeout(d).ok()
    }

    #[test]
    fn rows_held_for_delivery_are_counted_and_an_admission_needing_them_empties_their_shelf() {
        let a = Accountant::new(Sizes::of(LEAST_MEMORY));
        let g = a.admit();
        let small = a.shelve(Vec::new(), g, 1 << 20);
        let g = admitted_within(&a, Duration::from_secs(10)).expect("room beside a small shelf");
        assert!(
            small.lock().unwrap().is_some(),
            "a shelf is emptied only for room"
        );
        drop(small);
        let all = g.bytes;
        let full = a.shelve(Vec::new(), g, all);
        let g =
            admitted_within(&a, Duration::from_secs(10)).expect("no wait on a caller's iteration");
        assert!(full.lock().unwrap().is_none());
        assert!(all < g.bytes);
        assert_eq!(g.rows(), LEAST_MEMORY - LEAST_MEMORY / 4 - BLOCK_RESERVE);
    }
}
