//! The writer's threads (`api.md` Threads): `kvik-writer` runs [`LogWriter::step`] in a loop and `kvik-maint` creates
//! and seals segments. Both are started by [`Log::start`] and joined by [`Log::shutdown`]. The returned
//! [`Pending`] is runtime-neutral: the writer thread completes it, and it can be awaited on any executor or blocked
//! on.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::directories::{Directories, DirectoryOptions};
use super::maint::{AheadSegment, CreateError, DescribeError, SealError, SealJob, SegmentRequest};
use super::share::{SyncSignal, Task};
use super::{
    Ack, Call, CallError, CallSource, CommitError, Fault, Halt, IngestQueue, LogStats, LogWriter,
    Reason, Wait,
};
use crate::batch::SegmentIdentity;
use crate::content::Item;
use crate::fs::Fs;
use crate::segment::SealedFile;

mod loops;
mod watch;
use loops::{maint_loop, writer_loop};
pub use watch::Watch;

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
    /// A caller asked for the mark to be synced now ([`Log::sync_mark`]).
    mark_sync: bool,
}

#[derive(Debug, Default)]
struct Maint {
    request: Option<SegmentRequest>,
    /// When the request and each job were handed over, for [`MaintWaits`].
    requested_at: Option<Duration>,
    job_times: VecDeque<Duration>,
    created: Option<Result<AheadSegment, CreateError>>,
    jobs: VecDeque<SealJob>,
    results: Vec<(SealJob, Result<SealedFile, SealError>)>,
    describe: Option<(PathBuf, SegmentIdentity)>,
    described: Option<Result<SealedFile, DescribeError>>,
    stop: bool,
}

#[derive(Debug)]
struct Info {
    stats: LogStats,
    reasons: Vec<Reason>,
    fault: Option<Fault>,
    /// Raised at every turn that published something, with `progress_cv` notified.
    progress: u64,
    /// The writer thread has ended.
    stopped: bool,
    sealed: Vec<SealedFile>,
}

#[derive(Debug)]
struct Shared {
    front: Mutex<Front>,
    writer_cv: Condvar,
    room_cv: Condvar,
    maint: Mutex<Maint>,
    maint_cv: Condvar,
    info: Mutex<Info>,
    progress_cv: Condvar,
    /// Woken with `progress_cv`, by id ([`Watch::on_progress`]).
    progress_wakers: Mutex<(u64, BTreeMap<u64, Waker>)>,
    /// A shutdown has begun: no description is handed out and one in flight is dropped (Q-221 (b)).
    stopping: AtomicBool,
    halt: Arc<Halt>,
    fs: Arc<dyn Fs>,
    clock: Arc<dyn Clock>,
    sync: Arc<SyncSignal>,
    waits: Mutex<MaintWaits>,
    commit_clock: Arc<super::CommitClock>,
    /// The writer's held batch entries and calls as the last acknowledgements were delivered, and just after.
    #[cfg(test)]
    held: Mutex<Option<(usize, usize)>>,
}

impl Shared {
    fn wake_writer(&self) {
        lock(&self.front).wake = true;
        self.writer_cv.notify_one();
    }
}

/// How long creation ahead and seals waited for `kvik-maint` after the writer handed them over: the delay a
/// background task adds is measured from these (compaction design §4, Q-801).
#[doc(hidden)]
#[derive(Clone, Debug, Default)]
pub struct MaintWaits {
    /// Waits of creation ahead, in order, at most [`MaintWaits::KEPT`].
    pub creation: Vec<Duration>,
    /// Waits of seals.
    pub seal: Vec<Duration>,
    /// Steps of the background task run.
    pub steps: u64,
}

impl MaintWaits {
    /// The most waits kept of each kind.
    pub const KEPT: usize = 1 << 16;

    fn push(v: &mut Vec<Duration>, d: Duration) {
        if v.len() < Self::KEPT {
            v.push(d);
        }
    }
}

struct SharedSource<'a> {
    shared: &'a Shared,
    taken_at: Option<i64>,
}

impl<'a> SharedSource<'a> {
    fn new(shared: &'a Shared) -> Self {
        SharedSource {
            shared,
            taken_at: None,
        }
    }
}

impl CallSource for SharedSource<'_> {
    fn is_empty(&self) -> bool {
        lock(&self.shared.front).queue.is_empty()
    }
    fn take(&mut self, max_bytes: u64) -> Vec<Call> {
        let calls = lock(&self.shared.front).queue.take(max_bytes);
        // After the take, so after every call taken was queued (Q-1070); not under the lock, so a slow clock stalls
        // no submitter.
        self.taken_at = Some(self.shared.clock.wall());
        self.shared.room_cv.notify_all();
        calls
    }
    fn put_back(&mut self, calls: Vec<Call>) {
        lock(&self.shared.front).queue.put_back(calls);
    }
    fn taken_at(&self) -> Option<i64> {
        self.taken_at
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Wait,
    Try,
    Offer,
}

/// Waits for room in a log's ingest queue ([`Log::room`]).
#[derive(Debug, Clone)]
pub struct Room {
    shared: Arc<Shared>,
}

impl Room {
    /// Blocks until a call of `len` entries would be queued, or the log turns read-only or shuts down. Room found
    /// here may be taken by another caller before this one queues.
    pub fn wait(&self, len: usize) -> Result<(), SubmitError> {
        let mut f = lock(&self.shared.front);
        loop {
            if f.read_only {
                return Err(SubmitError::ReadOnly);
            }
            if f.shutdown {
                return Err(SubmitError::ShutDown);
            }
            if f.queue.has_room(len) {
                return Ok(());
            }
            f = self
                .shared
                .room_cv
                .wait(f)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// The log writer on its threads. See the module documentation of [`crate::writer`].
#[derive(Debug)]
pub struct Log {
    shared: Arc<Shared>,
    call_limit: usize,
    writer: Option<JoinHandle<(Result<(), Fault>, LogWriter)>>,
    maint: Option<JoinHandle<()>>,
}

impl Log {
    /// Starts `kvik-writer` and `kvik-maint` on an opened writer.
    pub fn start(writer: LogWriter, clock: Arc<dyn Clock>) -> io::Result<Self> {
        Self::start_with(writer, clock, None)
    }

    /// [`Log::start`], with `kvik-maint` also writing the derived files (`format.md` A.18) as `dirs` says.
    pub fn start_with(
        writer: LogWriter,
        clock: Arc<dyn Clock>,
        dirs: Option<DirectoryOptions>,
    ) -> io::Result<Self> {
        Self::start_with_task(writer, clock, dirs, None)
    }

    /// [`Log::start_with`], with `kvik-maint` also stepping a background task within its share.
    #[doc(hidden)]
    pub fn start_with_task(
        mut writer: LogWriter,
        clock: Arc<dyn Clock>,
        dirs: Option<DirectoryOptions>,
        task: Option<Task>,
    ) -> io::Result<Self> {
        if let Some(t) = &task {
            t.share.check()?;
        }
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
        let sync = Arc::new(SyncSignal::default());
        if task.is_some() {
            writer.set_sync_signal(sync.clone());
        }
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
                sealed: Vec::new(),
            }),
            progress_cv: Condvar::new(),
            progress_wakers: Mutex::new((0, BTreeMap::new())),
            stopping: AtomicBool::new(false),
            halt: writer.halt(),
            fs: writer.store().fs().clone(),
            commit_clock: writer.commit_clock(),
            clock,
            sync,
            waits: Mutex::new(MaintWaits::default()),
            #[cfg(test)]
            held: Mutex::new(None),
        });
        let s = shared.clone();
        let maint = std::thread::Builder::new()
            .name("kvik-maint".into())
            .spawn(move || maint_loop(&s, dirs, task))?;
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

    fn enqueue(&self, items: Vec<Item>, mode: Mode) -> Result<Pending, SubmitError> {
        let call = Call::new(items, self.call_limit)?;
        let mut f = lock(&self.shared.front);
        loop {
            if f.read_only {
                return Err(SubmitError::ReadOnly);
            }
            if f.shutdown {
                return Err(SubmitError::ShutDown);
            }
            if mode == Mode::Try
                && let Some(w) = f.waiting
            {
                return Err(SubmitError::Busy(w));
            }
            if f.queue.has_room(call.len()) {
                break;
            }
            if mode != Mode::Wait {
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
        self.enqueue(items, Mode::Wait)
    }

    /// Queues a call without waiting: [`SubmitError::Busy`] names the reason ingest would wait.
    pub fn try_submit(&self, items: Vec<Item>) -> Result<Pending, SubmitError> {
        self.enqueue(items, Mode::Try)
    }

    /// Queues a call if the queue has room for it now, as [`Log::submit`] would without waiting, whatever else
    /// ingest waits on; [`SubmitError::Busy`] with [`Wait::QueueFull`] otherwise. With [`Log::room`], a caller
    /// that must not wait while it holds a lock queues as `submit` does.
    pub fn offer(&self, items: Vec<Item>) -> Result<Pending, SubmitError> {
        self.enqueue(items, Mode::Offer)
    }

    /// A handle that waits for room in the queue without borrowing the log.
    pub fn room(&self) -> Room {
        Room {
            shared: self.shared.clone(),
        }
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

    /// The descriptions of segments sealed, and of sealed segments no manifest record listed at the start, since the
    /// last call ([`LogWriter::take_sealed`]).
    pub fn take_sealed(&self) -> Vec<SealedFile> {
        std::mem::take(&mut lock(&self.shared.info).sealed)
    }

    /// The waits of creation ahead and seals for `kvik-maint` so far.
    #[doc(hidden)]
    pub fn maint_waits(&self) -> MaintWaits {
        lock(&self.shared.waits).clone()
    }

    /// Entries queued.
    pub fn queue_depth(&self) -> usize {
        lock(&self.shared.front).queue.depth()
    }

    /// Clean shutdown: commits the calls already queued, finishes the seals under way, drops a description in flight
    /// (Q-221 (b)), syncs the mark and joins both threads. Calls that cannot be committed (ingest waiting on the disk) fail with
    /// [`CommitError::ShutDown`]. Returns the fault if the store became read-only.
    pub fn shutdown(mut self) -> Result<(), Fault> {
        self.stop().0
    }

    /// [`Log::shutdown`], then, if the store did not become read-only, `then` on the stopped writer before the
    /// writer, and with it the store's lock, is dropped: for work that must finish under the single-writer lock
    /// after the clean shutdown's mark sync (`spec.md` §3 Single writer).
    pub fn shutdown_then<R>(mut self, then: impl FnOnce(&LogWriter) -> R) -> Result<R, Fault> {
        let (r, writer) = self.stop();
        r?;
        Ok(then(
            writer.as_ref().expect("a writer after a clean shutdown"),
        ))
    }

    fn detach(&mut self) {
        lock(&self.shared.maint).stop = true;
        self.shared.maint_cv.notify_all();
        self.writer = None;
        self.maint = None;
    }

    /// Unwinding must not join (a thread's panic would abort); the threads end on their own and release the store.
    fn signal_stop_detached(&mut self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        lock(&self.shared.front).shutdown = true;
        self.shared.writer_cv.notify_one();
        self.shared.room_cv.notify_all();
        self.detach();
    }

    fn stop(&mut self) -> (Result<(), Fault>, Option<LogWriter>) {
        self.shared.stopping.store(true, Ordering::SeqCst);
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
            self.detach();
            return (Ok(()), None);
        }
        let w = self.writer.take().map(JoinHandle::join);
        {
            lock(&self.shared.maint).stop = true;
        }
        self.shared.maint_cv.notify_all();
        let m = self.maint.take().map(JoinHandle::join);
        // Both threads are joined before a panic of either is passed on.
        let r = match w {
            Some(Ok((r, writer))) => (r, Some(writer)),
            Some(Err(p)) => std::panic::resume_unwind(p),
            None => (Ok(()), None),
        };
        if let Some(Err(p)) = m {
            std::panic::resume_unwind(p);
        }
        r
    }
}

impl Drop for Log {
    fn drop(&mut self) {
        if self.writer.is_none() {
            return;
        }
        if std::thread::panicking() {
            self.signal_stop_detached();
        } else {
            let _ = self.stop();
        }
    }
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
            Item::from(Entry::Alias {
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

    #[test]
    fn signalling_a_detached_stop_sets_every_flag_and_lets_go_of_both_threads() {
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
        let mut log = Log::start(w, Arc::new(SystemClock::default())).unwrap();
        let shared = log.shared.clone();
        log.signal_stop_detached();
        assert!(shared.stopping.load(Ordering::SeqCst));
        assert!(lock(&shared.front).shutdown);
        assert!(lock(&shared.maint).stop);
        assert!(log.writer.is_none() && log.maint.is_none());
    }

    /// A clock moved by hand whose wall reads on the writer thread can be held, each returning the time it was
    /// read at once released.
    #[derive(Debug, Default)]
    struct HeldClock {
        state: Mutex<Held>,
        cv: Condvar,
    }

    #[derive(Debug, Default)]
    struct Held {
        wall: i64,
        on: bool,
        reads: usize,
        released: usize,
    }

    impl HeldClock {
        fn set(&self, wall: i64) {
            lock(&self.state).wall = wall;
        }
        fn hold(&self, on: bool) {
            lock(&self.state).on = on;
            self.cv.notify_all();
        }
        /// Waits until the writer is held in its `n`th wall read since the hold began.
        fn held_in(&self, n: usize) {
            let s = lock(&self.state);
            drop(
                self.cv
                    .wait_while(s, |s| s.reads < n)
                    .unwrap_or_else(PoisonError::into_inner),
            );
        }
        fn release_one(&self) {
            lock(&self.state).released += 1;
            self.cv.notify_all();
        }
    }

    impl Clock for HeldClock {
        fn monotonic(&self) -> Duration {
            Duration::ZERO
        }
        fn wall(&self) -> i64 {
            let mut s = lock(&self.state);
            let at = s.wall;
            if s.on && std::thread::current().name() == Some("kvik-writer") {
                s.reads += 1;
                let me = s.reads;
                self.cv.notify_all();
                s = self
                    .cv
                    .wait_while(s, |s| s.on && s.released < me)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            drop(s);
            at
        }
    }

    #[test]
    fn a_call_queued_after_the_writer_read_the_clock_commits_no_earlier_than_it_was_queued() {
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
        let clock = Arc::new(HeldClock::default());
        clock.set(52);
        let log = Log::start(w, clock.clone()).unwrap();
        let alias = |id| {
            Item::from(Entry::Alias {
                id,
                name: format!("a{id}"),
            })
        };
        log.submit(vec![alias(1)]).unwrap().wait().unwrap();
        clock.hold(true);
        log.shared.wake_writer();
        // A writer's turn reads the wall clock once handing work to maintenance, then for its step.
        clock.held_in(1);
        clock.release_one();
        clock.held_in(2);
        clock.set(157);
        let pending = log.submit(vec![alias(2)]).unwrap();
        clock.hold(false);
        pending.wait().unwrap();
        let committed = log
            .shutdown_then(|w| w.commit_clock().floor(i64::MIN).0)
            .unwrap();
        assert_eq!(committed, 157);
    }
}
