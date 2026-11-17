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

use super::directories::{Directories, DirectoryOptions};
use super::maint::{AheadSegment, CreateError, SealError, SealJob, SegmentRequest};
use super::maint_thread::maint_loop;
use super::{
    Ack, Call, CallError, CallSource, CommitError, Done, Fault, FaultOp, Halt, IngestQueue,
    LogStats, LogWriter, Reason, Wait,
};
use crate::content::Item;
use crate::fs::Fs;

pub(super) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
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
pub(super) struct Front {
    pub(super) queue: IngestQueue,
    slots: HashMap<u64, Arc<Slot>>,
    pub(super) wake: bool,
    pub(super) shutdown: bool,
    pub(super) read_only: bool,
    pub(super) waiting: Option<Wait>,
    /// A caller asked for the mark to be synced now ([`Log::sync_mark`]).
    pub(super) mark_sync: bool,
}

#[derive(Debug, Default)]
pub(super) struct Maint {
    pub(super) request: Option<SegmentRequest>,
    pub(super) created: Option<Result<AheadSegment, CreateError>>,
    pub(super) jobs: VecDeque<SealJob>,
    pub(super) results: Vec<(SealJob, Result<(), SealError>)>,
    pub(super) stop: bool,
}

#[derive(Debug)]
pub(super) struct Info {
    pub(super) stats: LogStats,
    pub(super) reasons: Vec<Reason>,
    pub(super) fault: Option<Fault>,
    /// Raised at every turn that published something, with `progress_cv` notified.
    pub(super) progress: u64,
    /// The writer thread has ended.
    pub(super) stopped: bool,
}

#[derive(Debug)]
pub(super) struct Shared {
    pub(super) front: Mutex<Front>,
    pub(super) writer_cv: Condvar,
    pub(super) room_cv: Condvar,
    pub(super) maint: Mutex<Maint>,
    pub(super) maint_cv: Condvar,
    pub(super) info: Mutex<Info>,
    pub(super) progress_cv: Condvar,
    pub(super) halt: Arc<Halt>,
    pub(super) fs: Arc<dyn Fs>,
    pub(super) clock: Arc<dyn Clock>,
    /// The writer's held batch entries and calls as the last acknowledgements were delivered, and just after.
    #[cfg(test)]
    pub(super) held: Mutex<Option<(usize, usize)>>,
}

impl Shared {
    pub(super) fn wake_writer(&self) {
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

/// Watches the writer's progress ([`Log::watch`]).
#[derive(Clone, Debug)]
pub struct Watch {
    shared: Arc<Shared>,
}

impl Watch {
    /// As [`Log::wait_progress`].
    pub fn wait_progress(&self, seen: u64, timeout: Duration) -> u64 {
        let i = lock(&self.shared.info);
        let (i, _) = self
            .shared
            .progress_cv
            .wait_timeout_while(i, timeout, |i| i.progress == seen && !i.stopped)
            .unwrap_or_else(PoisonError::into_inner);
        i.progress
    }

    /// Whether the writer thread has ended.
    pub fn stopped(&self) -> bool {
        lock(&self.shared.info).stopped
    }

    /// As [`Log::stats`].
    pub fn stats(&self) -> LogStats {
        lock(&self.shared.info).stats.clone()
    }

    /// The last synced mark, without copying the rest of the statistics.
    pub fn mark_synced(&self) -> super::Position {
        lock(&self.shared.info).stats.mark_synced
    }

    /// As [`Log::reasons`].
    pub fn reasons(&self) -> Vec<Reason> {
        lock(&self.shared.info).reasons.clone()
    }

    /// As [`Log::fault`].
    pub fn fault(&self) -> Option<Fault> {
        lock(&self.shared.info).fault.clone()
    }

    /// As [`Log::queue_depth`].
    pub fn queue_depth(&self) -> usize {
        lock(&self.shared.front).queue.depth()
    }

    /// Asks the writer to sync the mark now rather than at its timer, and waits until the mark on stable storage
    /// names at least the durable position at the call (`spec.md` §3 Durable mark: a file that would name a
    /// position above the last synced mark waits for the next mark sync or forces one). Returns the last synced
    /// position, or the fault if the store is or becomes read-only; after a shutdown, the last synced position.
    pub fn sync_mark(&self) -> Result<super::Position, Fault> {
        let target = lock(&self.shared.info).stats.durable;
        {
            let mut f = lock(&self.shared.front);
            f.mark_sync = true;
            f.wake = true;
        }
        self.shared.writer_cv.notify_one();
        let i = lock(&self.shared.info);
        let i = self
            .shared
            .progress_cv
            .wait_while(i, |i| {
                i.fault.is_none() && !i.stopped && i.stats.mark_synced < target
            })
            .unwrap_or_else(PoisonError::into_inner);
        match &i.fault {
            Some(f) => Err(f.clone()),
            None => Ok(i.stats.mark_synced),
        }
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
        Self::start_with(writer, clock, None)
    }

    /// [`Log::start`], with `kvik-maint` also writing the derived files (`format.md` A.18) as `dirs` says.
    pub fn start_with(
        mut writer: LogWriter,
        clock: Arc<dyn Clock>,
        dirs: Option<DirectoryOptions>,
    ) -> io::Result<Self> {
        let dirs = dirs.map(|opts| {
            let start = writer.directory_start();
            let store = writer.store();
            Directories::new(
                store.fs().clone(),
                store.dir(),
                store.descriptor().store_id,
                start,
                opts,
            )
        });
        writer.set_sync_clock(clock.clone());
        let cfg = writer.config().clone();
        let shared = Arc::new(Shared {
            front: Mutex::new(Front {
                queue: IngestQueue::new(cfg.ingest_queue),
                slots: HashMap::new(),
                wake: false,
                shutdown: false,
                read_only: false,
                waiting: None,
                mark_sync: false,
            }),
            writer_cv: Condvar::new(),
            room_cv: Condvar::new(),
            maint: Mutex::new(Maint::default()),
            maint_cv: Condvar::new(),
            info: Mutex::new(Info {
                stats: writer.stats().clone(),
                reasons: Vec::new(),
                fault: None,
                progress: 0,
                stopped: false,
            }),
            progress_cv: Condvar::new(),
            halt: writer.halt(),
            fs: writer.store().fs().clone(),
            clock,
            #[cfg(test)]
            held: Mutex::new(None),
        });
        let s = shared.clone();
        let maint = std::thread::Builder::new()
            .name("kvik-maint".into())
            .spawn(move || maint_loop(&s, dirs))?;
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

    /// Waits until the writer has published a turn after `seen` (a value this returned before; 0 at first), at most
    /// `timeout`, and returns the latest value. A caller that watches for acknowledged batches, and must not run on
    /// the writer thread, waits here and then looks at [`Log::stats`]. Returns at once once the writer has stopped.
    pub fn wait_progress(&self, seen: u64, timeout: Duration) -> u64 {
        self.watch().wait_progress(seen, timeout)
    }

    /// A handle that waits for the writer's progress without holding the log, for a thread that outlives a borrow
    /// of it.
    pub fn watch(&self) -> Watch {
        Watch {
            shared: self.shared.clone(),
        }
    }

    /// Asks the writer to sync the mark now rather than at its timer, and waits until the mark on stable storage
    /// names at least the durable position at the call (`spec.md` §3 Durable mark: a file that would name a
    /// position above the last synced mark waits for the next mark sync or forces one). Returns the last synced
    /// position, or the fault if the store is or becomes read-only; after a shutdown, the last synced position.
    pub fn sync_mark(&self) -> Result<super::Position, Fault> {
        self.watch().sync_mark()
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
        // Called on one of the log's own threads (the last owner dropped there), it cannot wait for them: they end
        // on their own and are not joined.
        let me = Some(std::thread::current().id());
        let writer = self.writer.as_ref().map(|h| h.thread().id());
        let maint = self.maint.as_ref().map(|h| h.thread().id());
        if writer == me || maint == me {
            lock(&self.shared.maint).stop = true;
            self.shared.maint_cv.notify_all();
            self.writer = None;
            self.maint = None;
            return Ok(());
        }
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
        i.progress += 1;
    }
    s.progress_cv.notify_all();
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
        {
            let mut i = lock(&s.info);
            i.fault = s.halt.fault();
            i.stopped = true;
        }
        s.progress_cv.notify_all();
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
        let sync_now = {
            let mut f = lock(&s.front);
            f.wake = false;
            std::mem::take(&mut f.mark_sync)
        };
        if sync_now {
            w.sync_mark_now(s.clock.monotonic());
        }
        exchange(s, &mut w);
        let step = w.step(s.clock.monotonic(), s.clock.wall(), &mut SharedSource(s));
        // A roll or a seal in this step asks for maintenance work at once. Statistics and the read-only state are
        // published before any call of this step completes, so a caller that holds its acknowledgement sees them.
        exchange(s, &mut w);
        publish(s, &w);
        #[cfg(test)]
        let at_acks = (!step.done.is_empty()).then(|| w.batch_held());
        complete(s, step.done);
        s.room_cv.notify_all();
        w.release_batch();
        #[cfg(test)]
        if let Some(at) = at_acks {
            *lock(&s.held) = Some((at, w.batch_held()));
        }
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
    lock(&s.info).stopped = true;
    s.progress_cv.notify_all();
    r
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::content::Item;
    use crate::entries::Entry;
    use crate::fs::MemFs;
    use crate::store::{NewStore, StoreDir};
    use crate::writer::WriterConfig;

    #[test]
    fn the_batch_is_held_while_its_calls_are_acknowledged_and_freed_just_after() {
        let new = NewStore {
            store_id: [1; 16],
            created_at: 0,
            site_zone: "Etc/UTC".into(),
            tzdata: "2026a".into(),
            software: "test".into(),
        };
        let store = StoreDir::open(Arc::new(MemFs::new()), Path::new("s"), Some(&new)).unwrap();
        let w = LogWriter::open(
            store,
            WriterConfig::default(),
            Some([2; 16]),
            Duration::ZERO,
            0,
        )
        .unwrap();
        let log = Log::start(w, Arc::new(SystemClock::default())).unwrap();
        let alias = |id| {
            Item::Entry(Entry::Alias {
                id,
                name: format!("a{id}"),
            })
        };
        log.submit(vec![alias(1), alias(2)])
            .unwrap()
            .wait()
            .unwrap();
        let start = std::time::Instant::now();
        let held = loop {
            if let Some(h) = *lock(&log.shared.held) {
                break h;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "no batch acknowledged"
            );
            std::thread::sleep(Duration::from_millis(1));
        };
        log.shutdown().unwrap();
        assert_eq!(
            held,
            (3, 0),
            "two entries and one call held at the acknowledgement, none after"
        );
    }
}
