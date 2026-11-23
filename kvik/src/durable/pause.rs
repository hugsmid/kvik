//! Pause points for the unit tests, armed per store directory.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Point {
    /// Under the decision lock, after a call's facts are taken for provisional streams, before its offer.
    BeforeOffer,
    /// Right after a call's decision.
    AfterDecision,
    /// After a call's acknowledgement, before its delivery step.
    AfterAck,
    /// A call that met a full ingest queue, with the decision lock dropped, before it waits for room.
    WaitingForRoom,
    /// In `take_consumer`, under the deliverer's lock, before the pump that sets the consumer up.
    SetUpPump,
    /// In `take_consumer`, once the consumer is set up.
    TakeSetUp,
    /// In the `kvik-deliver` thread's loop, before it runs the deliverer.
    DeliverThread,
}

type Hook = Arc<dyn Fn() + Send + Sync>;

static HOOKS: Mutex<Vec<(PathBuf, Point, Hook)>> = Mutex::new(Vec::new());

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Runs the hook armed for `dir` at `point`, if any.
pub(crate) fn at(dir: &Path, point: Point) {
    let hook = lock(&HOOKS)
        .iter()
        .find(|(d, p, _)| d == dir && *p == point)
        .map(|(_, _, h)| h.clone());
    if let Some(h) = hook {
        h();
    }
}

fn arm(dir: &Path, point: Point, hook: Hook) {
    let mut hooks = lock(&HOOKS);
    hooks.retain(|(d, p, _)| !(d == dir && *p == point));
    hooks.push((dir.to_path_buf(), point, hook));
}

fn disarm(dir: &Path, point: Point) {
    lock(&HOOKS).retain(|(d, p, _)| !(d == dir && *p == point));
}

/// A one-shot pause: the first thread to reach the point stops there until [`Gate::release`].
pub(crate) struct Gate {
    dir: PathBuf,
    point: Point,
    reached: Receiver<()>,
    release: Sender<()>,
}

impl Gate {
    pub(crate) fn new(dir: &Path, point: Point) -> Gate {
        let (reached_tx, reached) = mpsc::channel();
        let (release, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let armed = std::sync::atomic::AtomicBool::new(true);
        arm(
            dir,
            point,
            Arc::new(move || {
                if armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    let _ = reached_tx.send(());
                    let _ = lock(&release_rx).recv();
                }
            }),
        );
        Gate {
            dir: dir.to_path_buf(),
            point,
            reached,
            release,
        }
    }

    /// Waits until a thread stops at the point.
    pub(crate) fn reached(&self) {
        self.reached
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("{:?} not reached", self.point));
    }

    pub(crate) fn release(self) {}
}

impl Drop for Gate {
    fn drop(&mut self) {
        disarm(&self.dir, self.point);
        let _ = self.release.send(());
    }
}

/// Holds every thread that reaches the point until dropped; [`Hold::new`] returns once one is held.
pub(crate) struct Hold {
    dir: PathBuf,
    point: Point,
    held: Arc<(Mutex<bool>, Condvar)>,
}

impl Hold {
    pub(crate) fn new(dir: &Path, point: Point) -> Hold {
        let held = Arc::new((Mutex::new(true), Condvar::new()));
        let (parked_tx, parked) = mpsc::channel();
        let parked_tx = Mutex::new(parked_tx);
        let h = held.clone();
        arm(
            dir,
            point,
            Arc::new(move || {
                let _ = lock(&parked_tx).send(());
                let (m, cv) = &*h;
                drop(
                    cv.wait_while(lock(m), |on| *on)
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                );
            }),
        );
        parked
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("{point:?} not held"));
        Hold {
            dir: dir.to_path_buf(),
            point,
            held,
        }
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        disarm(&self.dir, self.point);
        let (m, cv) = &*self.held;
        *lock(m) = false;
        cv.notify_all();
    }
}
