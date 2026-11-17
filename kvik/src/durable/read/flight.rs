//! The single-flight refresh (query design §3.5, Q-902): calls that arrive together share one refresh, and a call that
//! arrives while one runs waits for the next, so every call gets a view read after it began. A follower takes the
//! leader's view; on the leader's error it leads or follows once more, then reports the error.
//!
//! Under `--cfg kvik_loom` the locks are the model checker's, for the tests of [`loom_tests`] only (Q-901).

use std::sync::{Arc, PoisonError};

#[cfg(all(test, kvik_loom))]
use loom::sync::{Condvar, Mutex, MutexGuard};
#[cfg(not(all(test, kvik_loom)))]
use std::sync::{Condvar, Mutex, MutexGuard};

struct Rounds<V, E> {
    /// Rounds begun and rounds ended: one runs while they differ.
    started: u64,
    finished: u64,
    /// The view of the last round that succeeded, with its number.
    current: Option<(u64, Arc<V>)>,
    /// The error of the last round that failed.
    failed: Option<E>,
    /// The round each waiting call waits for.
    #[cfg(test)]
    waiting: Vec<u64>,
}

/// See the module documentation.
pub(crate) struct Flight<V, E> {
    rounds: Mutex<Rounds<V, E>>,
    ended: Condvar,
}

impl<V, E> std::fmt::Debug for Flight<V, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Flight")
    }
}

/// Ends the round its leader runs, also when the leader unwinds, so that no follower waits for ever.
struct Lead<'a, V, E> {
    flight: &'a Flight<V, E>,
    outcome: Option<Result<Arc<V>, E>>,
}

impl<V, E> Drop for Lead<'_, V, E> {
    fn drop(&mut self) {
        let mut g = self.flight.lock();
        g.finished += 1;
        let round = g.finished;
        match self.outcome.take() {
            Some(Ok(v)) => g.current = Some((round, v)),
            Some(Err(e)) => g.failed = Some(e),
            None => {}
        }
        drop(g);
        self.flight.ended.notify_all();
    }
}

impl<V, E> Flight<V, E> {
    fn lock(&self) -> MutexGuard<'_, Rounds<V, E>> {
        self.rounds.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<V, E: Clone> Flight<V, E> {
    /// A flight with no view yet.
    pub(crate) fn new() -> Self {
        Flight {
            rounds: Mutex::new(Rounds {
                started: 0,
                finished: 0,
                current: None,
                failed: None,
                #[cfg(test)]
                waiting: Vec::new(),
            }),
            ended: Condvar::new(),
        }
    }

    /// The view of the last round that succeeded.
    #[cfg(test)]
    pub(crate) fn current(&self) -> Option<Arc<V>> {
        self.lock().current.as_ref().map(|(_, v)| v.clone())
    }

    /// A view from a round that began after this call did. The call leads that round when none runs: it runs
    /// `refresh` with the current view, outside the flight's lock, and returns what it returns.
    pub(crate) fn get(
        &self,
        mut refresh: impl FnMut(Option<Arc<V>>) -> Result<Arc<V>, E>,
    ) -> Result<Arc<V>, E> {
        let mut g = self.lock();
        let mut target = g.started + 1;
        let mut retried = false;
        loop {
            if let Some((round, v)) = &g.current
                && *round >= target
            {
                return Ok(v.clone());
            }
            if g.finished >= target {
                match &g.failed {
                    Some(e) if retried => return Err(e.clone()),
                    _ => {
                        retried = true;
                        // Any round that ends from now on began after this call did.
                        target = g.finished + 1;
                        continue;
                    }
                }
            }
            if g.started == g.finished {
                g.started += 1;
                let current = g.current.as_ref().map(|(_, v)| v.clone());
                drop(g);
                let mut lead = Lead {
                    flight: self,
                    outcome: None,
                };
                let r = refresh(current);
                lead.outcome = Some(r.clone());
                return r;
            }
            #[cfg(test)]
            g.waiting.push(target);
            g = self.ended.wait(g).unwrap_or_else(PoisonError::into_inner);
            #[cfg(test)]
            {
                let at = g.waiting.iter().position(|t| *t == target);
                g.waiting.swap_remove(at.expect("pushed above"));
            }
        }
    }

    /// How many calls wait for round `round` or a later one.
    #[cfg(all(test, not(kvik_loom)))]
    fn waiting_from(&self, round: u64) -> usize {
        self.lock().waiting.iter().filter(|t| **t >= round).count()
    }
}

#[cfg(all(test, not(kvik_loom)))]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::thread;

    use super::Flight;

    type Call = thread::JoinHandle<Result<Arc<u32>, u32>>;

    /// A call on another thread; if it leads round `r`, its refresh returns `outcome(r)` once `calls - r` calls wait for
    /// it or a later round, so that round `r` ends with every call that has not ended waiting.
    fn call(
        flight: &Arc<Flight<u32, u32>>,
        rounds: &Arc<AtomicU32>,
        calls: u32,
        outcome: fn(u32) -> Result<u32, u32>,
    ) -> Call {
        let (f, n) = (flight.clone(), rounds.clone());
        thread::spawn(move || {
            let inner = f.clone();
            f.get(move |_| {
                let round = n.fetch_add(1, Ordering::SeqCst) + 1;
                while inner.waiting_from(round.into()) < calls.saturating_sub(round) as usize {
                    thread::yield_now();
                }
                outcome(round).map(Arc::new)
            })
        })
    }

    fn started(rounds: &AtomicU32, n: u32) {
        while rounds.load(Ordering::SeqCst) < n {
            thread::yield_now();
        }
    }

    #[test]
    fn calls_that_arrive_during_a_refresh_share_the_next_one() {
        let flight = Arc::new(Flight::new());
        let rounds = Arc::new(AtomicU32::new(0));
        let a = call(&flight, &rounds, 3, Ok);
        started(&rounds, 1);
        let b = call(&flight, &rounds, 3, Ok);
        let c = call(&flight, &rounds, 3, Ok);
        assert_eq!(*a.join().unwrap().unwrap(), 1);
        let (b, c) = (b.join().unwrap().unwrap(), c.join().unwrap().unwrap());
        assert_eq!((*b, *c), (2, 2));
        assert!(Arc::ptr_eq(&b, &c));
        assert_eq!(rounds.load(Ordering::SeqCst), 2);
        assert!(Arc::ptr_eq(&flight.current().unwrap(), &b));
    }

    #[test]
    fn a_follower_leads_once_more_after_its_leaders_error_and_the_leader_reports_it() {
        let flight = Arc::new(Flight::new());
        let rounds = Arc::new(AtomicU32::new(0));
        let first_fails = |r| if r == 1 { Err(r) } else { Ok(r) };
        let a = call(&flight, &rounds, 2, first_fails);
        started(&rounds, 1);
        let b = call(&flight, &rounds, 2, first_fails);
        assert_eq!(a.join().unwrap(), Err(1));
        assert_eq!(*b.join().unwrap().unwrap(), 2);
        assert_eq!(rounds.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_follower_whose_second_round_fails_too_reports_that_error() {
        let flight = Arc::new(Flight::new());
        let rounds = Arc::new(AtomicU32::new(0));
        let a = call(&flight, &rounds, 4, Err);
        started(&rounds, 1);
        let rest: Vec<_> = (0..3).map(|_| call(&flight, &rounds, 4, Err)).collect();
        assert_eq!(a.join().unwrap(), Err(1));
        let mut errs: Vec<u32> = rest
            .into_iter()
            .map(|c| c.join().unwrap().unwrap_err())
            .collect();
        errs.sort_unstable();
        // Round 2's leader reports it; its followers lead and follow round 3 and report that.
        assert_eq!(errs, [2, 3, 3]);
        assert_eq!(rounds.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_round_whose_leader_panics_ends_and_its_follower_leads_the_next() {
        let flight = Arc::new(Flight::<u32, u32>::new());
        let (f, go) = (flight.clone(), Arc::new(AtomicU32::new(0)));
        let g = go.clone();
        let a = thread::spawn(move || {
            let inner = f.clone();
            f.get(move |_| {
                g.store(1, Ordering::SeqCst);
                while inner.waiting_from(1) < 1 {
                    thread::yield_now();
                }
                panic!("refresh panicked")
            })
        });
        started(&go, 1);
        let f = flight.clone();
        let b = thread::spawn(move || f.get(|_| Ok(Arc::new(7))));
        assert!(a.join().is_err());
        assert_eq!(*b.join().unwrap().unwrap(), 7);
    }
}

#[cfg(all(test, kvik_loom))]
mod loom_tests {
    use std::sync::Arc;

    use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use loom::thread;

    use super::Flight;

    fn model(f: impl Fn() + Send + Sync + 'static) {
        let mut b = loom::model::Builder::new();
        if b.preemption_bound.is_none() {
            b.preemption_bound = Some(3);
        }
        b.check(f);
    }

    /// A refresh that reads `mark` and fails on the rounds `fail` names; it checks that no two run at once.
    fn refresh(
        mark: &AtomicU64,
        busy: &AtomicBool,
        runs: &AtomicU64,
        fail: fn(u64) -> bool,
    ) -> Result<Arc<u64>, u64> {
        assert!(!busy.swap(true, Ordering::SeqCst), "two refreshes at once");
        let run = runs.fetch_add(1, Ordering::SeqCst);
        let m = mark.load(Ordering::SeqCst);
        busy.store(false, Ordering::SeqCst);
        if fail(run) { Err(run) } else { Ok(Arc::new(m)) }
    }

    struct World {
        flight: Flight<u64, u64>,
        mark: AtomicU64,
        busy: AtomicBool,
        runs: AtomicU64,
    }

    /// A call that reads the mark first; returns that mark, the result and whether its own refresh failed.
    fn call(w: &World, fail: fn(u64) -> bool) -> (u64, Result<u64, u64>, bool) {
        let seen = w.mark.load(Ordering::SeqCst);
        let mut failed_here = false;
        let r = w.flight.get(|_| {
            let r = refresh(&w.mark, &w.busy, &w.runs, fail);
            failed_here = r.is_err();
            r
        });
        (seen, r.map(|v| *v), failed_here)
    }

    fn world() -> Arc<World> {
        Arc::new(World {
            flight: Flight::new(),
            mark: AtomicU64::new(0),
            busy: AtomicBool::new(false),
            runs: AtomicU64::new(0),
        })
    }

    #[test]
    fn every_call_gets_a_view_read_after_it_began_and_refreshes_never_overlap() {
        model(|| {
            let w = world();
            let writer = {
                let w = w.clone();
                thread::spawn(move || {
                    w.mark.fetch_add(1, Ordering::SeqCst);
                })
            };
            let other = {
                let w = w.clone();
                thread::spawn(move || call(&w, |_| false))
            };
            let mine = call(&w, |_| false);
            writer.join().unwrap();
            for (seen, r, _) in [mine, other.join().unwrap()] {
                assert!(r.unwrap() >= seen, "a view read before its call began");
            }
            let current = *w.flight.current().unwrap();
            assert!(current <= w.mark.load(Ordering::SeqCst));
        });
    }

    /// Three calls whose refreshes fail on the runs `fail` names.
    fn three(fail: fn(u64) -> bool) -> [(u64, Result<u64, u64>, bool); 3] {
        let w = world();
        let others: Vec<_> = (0..2)
            .map(|_| {
                let w = w.clone();
                thread::spawn(move || call(&w, fail))
            })
            .collect();
        let mine = call(&w, fail);
        let mut it = others.into_iter().map(|t| t.join().unwrap());
        [mine, it.next().unwrap(), it.next().unwrap()]
    }

    #[test]
    fn only_the_leader_of_a_failed_round_reports_its_error() {
        model(|| {
            for (seen, r, failed_here) in three(|run| run == 0) {
                match r {
                    Ok(m) => assert!(m >= seen),
                    Err(run) => assert!(
                        failed_here && run == 0,
                        "a follower reported its leader's error"
                    ),
                }
            }
        });
    }

    #[test]
    fn a_follower_reports_an_error_only_after_two_failed_rounds() {
        model(|| {
            for (_, r, failed_here) in three(|_| true) {
                let run = r.unwrap_err();
                assert!(
                    failed_here || run >= 1,
                    "a follower gave up after one failed round"
                );
            }
        });
    }
}
