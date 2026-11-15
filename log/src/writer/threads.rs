//! The writer's threads (`api.md` Threads): `kvik-writer` runs [`LogWriter::step`] in a loop and `kvik-maint` creates
//! and seals segments. Both are started by [`Log::start`] and joined by [`Log::shutdown`]. The returned
//! [`Pending`] is runtime-neutral: the writer thread completes it, and it can be awaited on any executor or blocked
//! on.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::maint::{AheadSegment, CreateError, SealError, SealJob, SegmentRequest};
use super::{
    Ack, Call, CallError, CallSource, CommitError, Done, Fault, FaultOp, Halt, IngestQueue,
    LogStats, LogWriter, Reason, Wait, create_segment, seal_segment,
};
use crate::content::Item;
use crate::fs::Fs;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The writer's clock: a monotonic time and the wall clock. Tests of timing drive [`LogWriter`] directly instead.
pub trait Clock: Send + Sync + fmt::Debug {
    /// Time since an origin fixed per process; never decreases.
    fn monotonic(&self) -> Duration;
    /// The wall clock, nanoseconds since the Unix epoch.
    fn wall(&self) -> i64;
}

/// The system's clocks.
#[derive(Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn monotonic(&self) -> Duration {
        self.origin.elapsed()
    }
    fn wall(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
    }
}

/// Why a call was not queued.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SubmitError {
    /// Back-pressure ([`Log::try_submit`] only): the reason ingest would wait.
    #[error("busy: {0:?}")]
    Busy(Wait),
    /// The store is read-only.
    #[error("the store is read-only")]
    ReadOnly,
    /// The log is shutting down.
    #[error("the log is shutting down")]
    ShutDown,
    /// The call itself.
    #[error(transparent)]
    Call(#[from] CallError),
}

#[derive(Debug, Default)]
struct SlotState {
    result: Option<Result<Ack, CommitError>>,
    waker: Option<Waker>,
}

#[derive(Debug, Default)]
struct Slot {
    state: Mutex<SlotState>,
    cv: Condvar,
}

impl Slot {
    fn complete(&self, r: Result<Ack, CommitError>) {
        let waker = {
            let mut s = lock(&self.state);
            s.result = Some(r);
            s.waker.take()
        };
        self.cv.notify_all();
        // Woken outside the slot's lock, so that the waker may poll the call at once.
        if let Some(w) = waker {
            w.wake();
        }
    }
}

/// A queued call's acknowledgement, completed by the writer thread after the covering sync (and mark sync where
/// required). Await it, or block on [`Pending::wait`]. Dropping it cancels nothing.
#[derive(Debug)]
pub struct Pending {
    slot: Arc<Slot>,
}

impl Pending {
    /// Blocks until the call is acknowledged or fails.
    pub fn wait(self) -> Result<Ack, CommitError> {
        let mut s = lock(&self.slot.state);
        loop {
            if let Some(r) = s.result.take() {
                return r;
            }
            s = self.slot.cv.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

impl Future for Pending {
    type Output = Result<Ack, CommitError>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut s = lock(&self.slot.state);
        match s.result.take() {
            Some(r) => Poll::Ready(r),
            None => {
                s.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

#[derive(Debug)]
struct Front {
    queue: IngestQueue,
    slots: HashMap<u64, Arc<Slot>>,
    wake: bool,
    shutdown: bool,
    read_only: bool,
    waiting: Option<Wait>,
}

#[derive(Debug, Default)]
struct Maint {
    request: Option<SegmentRequest>,
    created: Option<Result<AheadSegment, CreateError>>,
    jobs: VecDeque<SealJob>,
    results: Vec<(SealJob, Result<(), SealError>)>,
    stop: bool,
}

#[derive(Debug)]
struct Info {
    stats: LogStats,
    reasons: Vec<Reason>,
    fault: Option<Fault>,
}

#[derive(Debug)]
struct Shared {
    front: Mutex<Front>,
    writer_cv: Condvar,
    room_cv: Condvar,
    maint: Mutex<Maint>,
    maint_cv: Condvar,
    info: Mutex<Info>,
    halt: Arc<Halt>,
    fs: Arc<dyn Fs>,
    clock: Arc<dyn Clock>,
}

impl Shared {
    fn wake_writer(&self) {
        lock(&self.front).wake = true;
        self.writer_cv.notify_one();
    }
}

struct SharedSource<'a>(&'a Shared);

impl CallSource for SharedSource<'_> {
    fn is_empty(&self) -> bool {
        lock(&self.0.front).queue.is_empty()
    }
    fn take(&mut self, max_bytes: u64) -> Vec<Call> {
        let calls = lock(&self.0.front).queue.take(max_bytes);
        self.0.room_cv.notify_all();
        calls
    }
    fn put_back(&mut self, calls: Vec<Call>) {
        lock(&self.0.front).queue.put_back(calls);
    }
}

/// The log writer on its threads. See the module documentation of [`crate::writer`].
#[derive(Debug)]
pub struct Log {
    shared: Arc<Shared>,
    call_limit: usize,
    writer: Option<JoinHandle<Result<(), Fault>>>,
    maint: Option<JoinHandle<()>>,
}

impl Log {
    /// Starts `kvik-writer` and `kvik-maint` on an opened writer.
    pub fn start(writer: LogWriter, clock: Arc<dyn Clock>) -> io::Result<Self> {
        let cfg = writer.config().clone();
        let shared = Arc::new(Shared {
            front: Mutex::new(Front {
                queue: IngestQueue::new(cfg.ingest_queue),
                slots: HashMap::new(),
                wake: false,
                shutdown: false,
                read_only: false,
                waiting: None,
            }),
            writer_cv: Condvar::new(),
            room_cv: Condvar::new(),
            maint: Mutex::new(Maint::default()),
            maint_cv: Condvar::new(),
            info: Mutex::new(Info {
                stats: writer.stats().clone(),
                reasons: Vec::new(),
                fault: None,
            }),
            halt: writer.halt(),
            fs: writer.store().fs().clone(),
            clock,
        });
        let s = shared.clone();
        let maint = std::thread::Builder::new()
            .name("kvik-maint".into())
            .spawn(move || maint_loop(&s))?;
        let s = shared.clone();
        let w = std::thread::Builder::new()
            .name("kvik-writer".into())
            .spawn(move || writer_loop(&s, writer));
        let w = match w {
            Ok(w) => w,
            Err(e) => {
                lock(&shared.maint).stop = true;
                shared.maint_cv.notify_all();
                let _ = maint.join();
                return Err(e);
            }
        };
        Ok(Self {
            shared,
            call_limit: cfg.call_limit(),
            writer: Some(w),
            maint: Some(maint),
        })
    }

    fn enqueue(&self, items: Vec<Item>, wait: bool) -> Result<Pending, SubmitError> {
        let call = Call::new(items, self.call_limit)?;
        let mut f = lock(&self.shared.front);
        loop {
            if f.read_only {
                return Err(SubmitError::ReadOnly);
            }
            if f.shutdown {
                return Err(SubmitError::ShutDown);
            }
            if !wait && let Some(w) = f.waiting {
                return Err(SubmitError::Busy(w));
            }
            if f.queue.has_room(call.len()) {
                break;
            }
            if !wait {
                return Err(SubmitError::Busy(Wait::QueueFull));
            }
            f = self
                .shared
                .room_cv
                .wait(f)
                .unwrap_or_else(PoisonError::into_inner);
        }
        let id = f
            .queue
            .push(call)
            .map_err(|_| SubmitError::Busy(Wait::QueueFull))?;
        let slot = Arc::new(Slot::default());
        f.slots.insert(id, slot.clone());
        f.wake = true;
        drop(f);
        self.shared.writer_cv.notify_one();
        Ok(Pending { slot })
    }

    /// Queues a call, waiting while the queue is full (back-pressure).
    pub fn submit(&self, items: Vec<Item>) -> Result<Pending, SubmitError> {
        self.enqueue(items, true)
    }

    /// Queues a call without waiting: [`SubmitError::Busy`] names the reason ingest would wait.
    pub fn try_submit(&self, items: Vec<Item>) -> Result<Pending, SubmitError> {
        self.enqueue(items, false)
    }

    /// The writer's statistics, as of its last turn.
    pub fn stats(&self) -> LogStats {
        lock(&self.shared.info).stats.clone()
    }

    /// The health reasons, as of the writer's last turn.
    pub fn reasons(&self) -> Vec<Reason> {
        lock(&self.shared.info).reasons.clone()
    }

    /// The fault that made the store read-only, if any.
    pub fn fault(&self) -> Option<Fault> {
        lock(&self.shared.info).fault.clone()
    }

    /// Entries queued.
    pub fn queue_depth(&self) -> usize {
        lock(&self.shared.front).queue.depth()
    }

    /// Clean shutdown: commits the calls already queued, finishes the seals under way, syncs the mark and joins
    /// both threads. Calls that cannot be committed (ingest waiting on the disk) fail with
    /// [`CommitError::ShutDown`]. Returns the fault if the store became read-only.
    pub fn shutdown(mut self) -> Result<(), Fault> {
        self.stop()
    }

    fn stop(&mut self) -> Result<(), Fault> {
        {
            lock(&self.shared.front).shutdown = true;
        }
        self.shared.writer_cv.notify_one();
        self.shared.room_cv.notify_all();
        let w = self.writer.take().map(JoinHandle::join);
        {
            lock(&self.shared.maint).stop = true;
        }
        self.shared.maint_cv.notify_all();
        let m = self.maint.take().map(JoinHandle::join);
        // Both threads are joined before a panic of either is passed on.
        let r = match w {
            Some(Ok(r)) => r,
            Some(Err(p)) => std::panic::resume_unwind(p),
            None => Ok(()),
        };
        if let Some(Err(p)) = m {
            std::panic::resume_unwind(p);
        }
        r
    }
}

impl Drop for Log {
    fn drop(&mut self) {
        if self.writer.is_some() && !std::thread::panicking() {
            let _ = self.stop();
        }
    }
}

/// Hands requests and seal jobs to `kvik-maint` and takes its results.
fn exchange(s: &Shared, w: &mut LogWriter) {
    let now = s.clock.monotonic();
    let mut m = lock(&s.maint);
    if let Some(r) = m.created.take() {
        w.segment_created(r);
    }
    for (job, r) in m.results.drain(..) {
        w.seal_done(job, r, now);
    }
    let mut work = false;
    if let Some(req) = w.segment_request(s.clock.wall()) {
        m.request = Some(req);
        work = true;
    }
    let jobs = w.take_seal_jobs(now);
    work |= !jobs.is_empty();
    m.jobs.extend(jobs);
    drop(m);
    if work {
        s.maint_cv.notify_all();
    }
}

fn complete(s: &Shared, done: Vec<Done>) {
    if done.is_empty() {
        return;
    }
    let slots: Vec<_> = {
        let mut f = lock(&s.front);
        done.into_iter()
            .filter_map(|(id, r)| f.slots.remove(&id).map(|slot| (slot, r)))
            .collect()
    };
    for (slot, r) in slots {
        slot.complete(r);
    }
}

fn publish(s: &Shared, w: &LogWriter) {
    {
        let mut i = lock(&s.info);
        i.stats = w.stats().clone();
        i.reasons = w.reasons();
        i.fault = w.fault().cloned();
    }
    let mut f = lock(&s.front);
    f.read_only = w.fault().is_some();
    f.waiting = w.waiting();
}

/// If `kvik-writer` panics: the store turns read-only, every queued or pending call fails with
/// [`CommitError::OutcomeUnknown`], and `kvik-maint` is told to stop.
struct PanicGuard<'a>(&'a Shared);

impl Drop for PanicGuard<'_> {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        let s = self.0;
        let fault = Fault {
            op: FaultOp::Panic,
            path: Default::default(),
            kind: io::ErrorKind::Other,
            message: "the writer thread panicked".into(),
        };
        s.halt.set(fault.clone());
        lock(&s.info).fault = s.halt.fault();
        let slots: Vec<_> = {
            let mut f = lock(&s.front);
            f.read_only = true;
            f.queue.drain();
            f.slots.drain().map(|(_, slot)| slot).collect()
        };
        for slot in slots {
            slot.complete(Err(CommitError::OutcomeUnknown));
        }
        s.room_cv.notify_all();
        lock(&s.maint).stop = true;
        s.maint_cv.notify_all();
    }
}

fn writer_loop(s: &Shared, mut w: LogWriter) -> Result<(), Fault> {
    let _guard = PanicGuard(s);
    loop {
        lock(&s.front).wake = false;
        exchange(s, &mut w);
        let step = w.step(s.clock.monotonic(), s.clock.wall(), &mut SharedSource(s));
        // A roll or a seal in this step asks for maintenance work at once. Statistics and the read-only state are
        // published before any call of this step completes, so a caller that holds its acknowledgement sees them.
        exchange(s, &mut w);
        publish(s, &w);
        complete(s, step.done);
        s.room_cv.notify_all();
        let mut f = lock(&s.front);
        if f.shutdown {
            let stuck = f
                .waiting
                .is_some_and(|x| matches!(x, Wait::DiskCritical { .. }));
            if f.queue.is_empty() || f.read_only || stuck {
                let rest: Vec<_> = f.queue.drain().into_iter().map(|c| c.id).collect();
                drop(f);
                complete(
                    s,
                    rest.into_iter()
                        .map(|id| (id, Err(CommitError::ShutDown)))
                        .collect(),
                );
                break;
            }
        }
        let now = s.clock.monotonic();
        match step.next {
            Some(t) if t <= now => continue,
            _ if f.wake => continue,
            Some(t) => {
                drop(
                    s.writer_cv
                        .wait_timeout(f, t - now)
                        .unwrap_or_else(PoisonError::into_inner),
                );
            }
            None => {
                drop(s.writer_cv.wait(f).unwrap_or_else(PoisonError::into_inner));
            }
        }
    }
    // Finish the seals under way (a blocked seal is left for the next start), then sync the mark.
    while w.fault().is_none() && w.seals_running() {
        exchange(s, &mut w);
        let f = lock(&s.front);
        if !f.wake {
            drop(
                s.writer_cv
                    .wait_timeout(f, Duration::from_millis(50))
                    .unwrap_or_else(PoisonError::into_inner),
            );
        } else {
            drop(f);
            lock(&s.front).wake = false;
        }
    }
    let r = w.shutdown(s.clock.monotonic());
    publish(s, &w);
    r
}

/// If `kvik-maint` panics: the store turns read-only and the writer is woken, so that it stops writing and its
/// shutdown does not wait for a seal that will never finish.
struct MaintGuard<'a>(&'a Shared);

impl Drop for MaintGuard<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.halt.set(Fault {
                op: FaultOp::Panic,
                path: Default::default(),
                kind: io::ErrorKind::Other,
                message: "the maintenance thread panicked".into(),
            });
            self.0.wake_writer();
        }
    }
}

fn maint_loop(s: &Shared) {
    let _guard = MaintGuard(s);
    loop {
        let mut m = lock(&s.maint);
        while m.request.is_none() && m.jobs.is_empty() && !m.stop {
            m = s.maint_cv.wait(m).unwrap_or_else(PoisonError::into_inner);
        }
        // After a fault no file operation starts (`spec.md` §3 Write or sync failure); dropped work stays as it is.
        if s.halt.is_set() {
            m.request = None;
            m.jobs.clear();
            if m.stop {
                return;
            }
            m = s.maint_cv.wait(m).unwrap_or_else(PoisonError::into_inner);
            drop(m);
            continue;
        }
        if let Some(req) = m.request.take() {
            drop(m);
            let r = create_segment(&*s.fs, &req, &s.halt);
            lock(&s.maint).created = Some(r);
        } else if let Some(job) = m.jobs.pop_front() {
            drop(m);
            let r = seal_segment(&*s.fs, &job, &s.halt);
            lock(&s.maint).results.push((job, r));
        } else {
            return;
        }
        s.wake_writer();
    }
}
