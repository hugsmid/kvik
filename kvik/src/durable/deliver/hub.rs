//! The deliverer of a writer with what its calls share, and the `kvik-deliver` thread.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use kvik_log::writer::{Fault as LogFault, FaultOp, Watch};

use super::{Ctx, Deliverer, Stamp};
use crate::durable::clock::Clocks;
use crate::env::Env;
use kvik_log::writer::Position as LogPos;

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
    /// its batch to the `kvik-deliver` thread (Q-706). Raised at open when the start found a consumer only as a
    /// precaution: a consumer not taken in this run is neither fed nor evaluated.
    consumers: AtomicBool,
    /// When the timers last ran, on the process's clock: the `kvik-deliver` thread runs them only when no call has
    /// for a second, so calls that run them (emits, polls, `tick`) keep the order the model backend gives; and not
    /// at all when the environment turns background timers off (`Env::background_timers`).
    last_tick: Mutex<Instant>,
    /// Emits acknowledged since the deliverer last took them, with the clocks at each ([`Hub::acked`]).
    acked: Mutex<Vec<(LogPos, Stamp)>>,
}

/// How often the `kvik-deliver` thread reads acknowledged batches back while no consumer is known: it bounds how far
/// behind the delivery reader, its lease and its counts run (Q-1490).
const IDLE_READ: Duration = Duration::from_secs(1);

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
            acked: Mutex::new(Vec::new()),
        })
    }

    /// Runs `f` on the deliverer with the clocks now, then wakes the consumers waiting if something changed.
    pub(crate) fn run<R>(&self, f: impl FnOnce(&mut Deliverer, &Ctx<'_>) -> R) -> R {
        let r = self.run_quiet(f);
        let closed = self.closed.load(Ordering::SeqCst);
        let wakers = lock(&self.deliverer).take_wakers(closed);
        for w in wakers {
            w.wake();
        }
        r
    }

    /// Notes that an emit whose call ends at `last` was acknowledged now. Its batch is read back by the
    /// `kvik-deliver` thread or the next call that takes batches, off the emit's path, and taken as of this instant.
    pub(crate) fn acked(&self, last: LogPos) {
        let at = Stamp {
            now: self.clocks.now(),
            wall: self.env.clock.wall().0,
            mono: i64::try_from(self.env.clock.monotonic().0).unwrap_or(i64::MAX),
        };
        lock(&self.acked).push((last, at));
    }

    /// Raised before a consumer is taken, ahead of the pumps that set it up: every batch acknowledged before an emit
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
        let mut d = lock(&self.deliverer);
        // The clocks are read once the deliverer is held: a call that waited for it acts at the time after the wait,
        // never at one from before it (a condition runner created in the past would fire early).
        let wall = self.env.clock.wall().0;
        let now = self.clocks.now();
        let mono = i64::try_from(self.env.clock.monotonic().0).unwrap_or(i64::MAX);
        let acked = std::mem::take(&mut *lock(&self.acked));
        // Kept by the deliverer whatever `f` does: a call that takes no batch must not drop an instant (Q-1119).
        d.note_acked(acked);
        let ctx = Ctx {
            watch: watch.as_ref(),
            env: &self.env,
            dir: &self.dir,
            clocks: &self.clocks,
            now,
            wall,
            mono,
        };
        f(&mut d, &ctx)
    }

    /// Holds the deliverer, for a test that makes a call wait for it.
    #[cfg(test)]
    pub(crate) fn hold(&self) -> MutexGuard<'_, Deliverer> {
        lock(&self.deliverer)
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
                let mut read_at = Instant::now();
                let mut sealed = watch.stats().sealed_segments;
                let mut wait = Duration::from_secs(1);
                loop {
                    seen = watch.wait_progress(seen, wait);
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
                    // With no consumer known the batches have no reader yet: a take pumps before it computes the
                    // consumer's start, so reading them back on each progress is work nobody needs (Q-1490). A seal
                    // still reads at once: the series directory written for it covers what the read-back folded.
                    let now_sealed = watch.stats().sealed_segments;
                    let read = due
                        || hub.has_consumers()
                        || now_sealed != sealed
                        || read_at.elapsed() >= IDLE_READ;
                    if !read {
                        continue;
                    }
                    read_at = Instant::now();
                    sealed = now_sealed;
                                        let (r, next) = hub.run(|d, c| {
                        d.pump(c);
                        if due {
                            d.tick(c);
                        }
                        (d.flush_due(c), d.gate_wait())
                    });
                    // A gate timer due before the next second wakes the thread for it (spec §6 Gates, Clock).
                    wait = next.filter(|_| timers).unwrap_or(Duration::from_secs(1));
                    if let Err(e) = r {
                        hub.failed(&e);
                    }
                }
            })?;
        Ok(())
    }
}
