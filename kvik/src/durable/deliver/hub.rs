//! The deliverer of a writer with what its calls share, and the `kvik-deliver` thread.

use std::cell::Cell;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use kvik_log::writer::{Fault as LogFault, FaultOp, Watch};

use super::{Ctx, Deliverer};
use crate::durable::clock::Clocks;
use crate::env::Env;

/// The deliverer of a writer with what its calls share: the writer's progress, the clocks and a cursor file fault.
/// The `kvik-deliver` thread holds one too, never the writer, so dropping the last `Store` clone stops the store at
/// once, as a crash would.
#[derive(Debug)]
pub(crate) struct Hub {
    deliverer: Mutex<Deliverer>,
    watch: Mutex<Option<Watch>>,
    env: Env,
    dir: PathBuf,
    clocks: Arc<Clocks>,
    /// A cursor file write or sync failed: the store is read-only (`spec.md` §3 Write or sync failure).
    pub(crate) fault: Mutex<Option<LogFault>>,
    pub(crate) closed: AtomicBool,
    /// A consumer is known or being taken; never lowered. Until then an `emit` leaves the timers and the read-back of
    /// its batch to the `kvik-deliver` thread (Q-706).
    consumers: AtomicBool,
    /// When the timers last ran, on the process's clock: the `kvik-deliver` thread runs them only when no call has
    /// for a second, so calls that run them (emits, polls, `tick`) keep the order the model backend gives; and not
    /// at all when the environment turns background timers off (`Env::background_timers`).
    last_tick: Mutex<Instant>,
    /// Runs that took wakers, and those that have woken them: a call waits until every wake begun before it has
    /// ended ([`Hub::wait_wakes`]).
    wakes: Mutex<(u64, u64)>,
    wakes_cv: Condvar,
}

thread_local! {
    /// This thread is waking consumers: a waker that calls back into the store does not wait for itself.
    static IN_WAKE: Cell<bool> = const { Cell::new(false) };
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Hub {
    pub(crate) fn new(d: Deliverer, env: Env, dir: &Path, clocks: Arc<Clocks>) -> Arc<Hub> {
        Arc::new(Hub {
            consumers: AtomicBool::new(d.has_consumers()),
            deliverer: Mutex::new(d),
            watch: Mutex::new(None),
            env,
            dir: dir.to_path_buf(),
            clocks,
            fault: Mutex::new(None),
            closed: AtomicBool::new(false),
            last_tick: Mutex::new(Instant::now()),
            wakes: Mutex::new((0, 0)),
            wakes_cv: Condvar::new(),
        })
    }

    /// Runs `f` on the deliverer with the clocks now, then wakes the consumers waiting if something changed.
    pub(crate) fn run<R>(&self, f: impl FnOnce(&mut Deliverer, &Ctx<'_>) -> R) -> R {
        let r = self.run_quiet(f);
        let closed = self.closed.load(Ordering::SeqCst);
        let wakers = {
            let mut d = lock(&self.deliverer);
            let w = d.take_wakers(closed);
            if !w.is_empty() {
                lock(&self.wakes).0 += 1;
            }
            w
        };
        if !wakers.is_empty() {
            let was = IN_WAKE.with(|f| f.replace(true));
            for w in wakers {
                w.wake();
            }
            IN_WAKE.with(|f| f.set(was));
            lock(&self.wakes).1 += 1;
            self.wakes_cv.notify_all();
        }
        r
    }

    /// Waits until every wake of consumers begun before the call has ended, so that a call whose batch another
    /// thread delivered returns only once the consumers waiting for it are woken. A waker that calls back does not
    /// wait.
    pub(crate) fn wait_wakes(&self) {
        if IN_WAKE.with(Cell::get) {
            return;
        }
        let w = lock(&self.wakes);
        let target = w.0;
        drop(
            self.wakes_cv
                .wait_while(w, |w| w.1 < target)
                .unwrap_or_else(PoisonError::into_inner),
        );
    }

    /// Raised before a consumer is taken, ahead of the pump that sets it up: every batch acknowledged before an emit
    /// read the flag lowered is then taken before the consumer exists (Q-706).
    pub(crate) fn consumer_coming(&self) {
        self.consumers.store(true, Ordering::SeqCst);
    }

    /// Whether an emit must take its batch and run the timers itself (Q-706).
    pub(crate) fn has_consumers(&self) -> bool {
        self.consumers.load(Ordering::SeqCst)
    }

    /// Notes that the timers ran.
    pub(crate) fn ticked(&self) {
        *lock(&self.last_tick) = Instant::now();
    }

    /// Runs `f` on the deliverer without waking anyone: for a caller that holds a lock a waker might take. What
    /// changed is woken by the next run, the `kvik-deliver` thread's at the latest, which the writer's progress
    /// wakes.
    pub(crate) fn run_quiet<R>(&self, f: impl FnOnce(&mut Deliverer, &Ctx<'_>) -> R) -> R {
        let watch = lock(&self.watch).clone();
        let wall = self.env.clock.wall().0;
        let now = self.clocks.now();
        let mono = i64::try_from(self.env.clock.monotonic().0).unwrap_or(i64::MAX);
        let ctx = Ctx {
            watch: watch.as_ref(),
            env: &self.env,
            dir: &self.dir,
            clocks: &self.clocks,
            now,
            wall,
            mono,
        };
        f(&mut lock(&self.deliverer), &ctx)
    }

    /// Records a cursor file fault.
    pub(crate) fn failed(&self, e: &io::Error) {
        let mut f = lock(&self.fault);
        if f.is_none() {
            *f = Some(LogFault {
                op: FaultOp::Write,
                path: self.dir.join(kvik_log::cursor::CURSORS_DIR),
                kind: e.kind(),
                message: e.to_string(),
            });
        }
    }

    /// Starts watching the writer and the `kvik-deliver` thread, which takes acknowledged batches when the writer
    /// progresses, writes the cursor files that are due, and runs the timers when no call has for a second.
    pub(crate) fn start(self: &Arc<Self>, watch: Watch) -> io::Result<()> {
        *lock(&self.watch) = Some(watch.clone());
        let hub = self.clone();
        let timers = self.env.background_timers;
        std::thread::Builder::new()
            .name("kvik-deliver".into())
            .spawn(move || {
                let mut seen = 0;
                loop {
                    seen = watch.wait_progress(seen, Duration::from_secs(1));
                    if watch.stopped() || hub.closed.load(Ordering::SeqCst) {
                        break;
                    }
                    #[cfg(test)]
                    crate::durable::pause::at(
                        &hub.dir,
                        crate::durable::pause::Point::DeliverThread,
                    );
                    let due = timers && lock(&hub.last_tick).elapsed() >= Duration::from_secs(1);
                    if due {
                        hub.ticked();
                    }
                    let r = hub.run(|d, c| {
                        d.pump(c);
                        if due {
                            d.tick(c);
                        }
                        d.flush_due(c)
                    });
                    if let Err(e) = r {
                        hub.failed(&e);
                    }
                }
            })?;
        Ok(())
    }
}
