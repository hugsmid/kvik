//! The loops of `kvik-writer` and `kvik-maint`, and what each does if it panics. `kvik-maint` takes one piece of work
//! at a time (segment creation, a seal, a description, a step of the derived files), so a request or a seal never
//! waits behind more than one step of the derived files.

use std::io;
use std::sync::PoisonError;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::super::directories::Directories;
use super::super::maint::{describe_sealed_until, directory_record};
use super::super::share::{Background, Governor, Next, Task, Verdict};
use super::super::{
    CallSource, CommitError, Done, Fault, FaultOp, LogWriter, Wait, create_segment,
    seal_segment_described,
};
use super::{MaintWaits, Shared, SharedSource, lock};

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
    if let Some(r) = m.described.take() {
        w.unlisted_done(r, now);
    }
    let mut work = false;
    if let Some(req) = w.segment_request(s.clock.wall()) {
        m.request = Some(req);
        m.requested_at = Some(now);
        work = true;
    }
    let jobs = w.take_seal_jobs(now);
    work |= !jobs.is_empty();
    m.job_times.extend(jobs.iter().map(|_| now));
    m.jobs.extend(jobs);
    if !s.stopping.load(Ordering::SeqCst)
        && let Some(id) = w.take_unlisted(now)
    {
        m.describe = Some((w.log_dir().to_path_buf(), id));
        work = true;
    }
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

fn publish(s: &Shared, w: &mut LogWriter) {
    {
        let mut i = lock(&s.info);
        i.stats = w.stats().clone();
        i.reasons = w.reasons();
        i.fault = w.fault().cloned();
        i.progress += 1;
        i.sealed.extend(w.take_sealed());
    }
    s.progressed();
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
        s.progressed();
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

pub(super) fn writer_loop(s: &Shared, mut w: LogWriter) -> (Result<(), Fault>, LogWriter) {
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
        let step = w.step(
            s.clock.monotonic(),
            s.clock.wall(),
            &mut SharedSource::new(s),
        );
        if s.sync.take_wake() {
            s.maint_cv.notify_all();
        }
        // A roll or a seal in this step asks for maintenance work at once. Statistics and the read-only state are
        // published before any call of this step completes, so a caller that holds its acknowledgement sees them.
        exchange(s, &mut w);
        publish(s, &mut w);
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
    publish(s, &mut w);
    lock(&s.info).stopped = true;
    s.progressed();
    (r, w)
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

pub(super) fn maint_loop(s: &Shared, mut dirs: Option<Directories>, task: Option<Task>) {
    let _guard = MaintGuard(s);
    let mut dirs_next: Option<Duration> = dirs.as_ref().map(|_| Duration::ZERO);
    let mut task = task.map(|t| Stepper::new(t, s.clock.monotonic()));
    let mut task_next: Option<Duration> = task.as_ref().map(|_| Duration::ZERO);
    loop {
        let mut m = lock(&s.maint);
        loop {
            // A description dropped at shutdown leaves the thread waiting for the seals the writer still hands out.
            if s.stopping.load(Ordering::SeqCst) {
                m.describe = None;
            }
            if m.request.is_some() || !m.jobs.is_empty() || m.describe.is_some() || m.stop {
                break;
            }
            let now = s.clock.monotonic();
            let next = match (dirs_next, task_next) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            match next {
                Some(t) if t <= now => break,
                Some(t) => {
                    m = s
                        .maint_cv
                        .wait_timeout(m, t - now)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                    // A returning commit sync wakes a task that waits for one.
                    if task.as_ref().is_some_and(|t| t.on_sync) {
                        task_next = task_next.map(|t| t.min(s.clock.monotonic()));
                    }
                }
                None => m = s.maint_cv.wait(m).unwrap_or_else(PoisonError::into_inner),
            }
        }
        // After a fault no file operation starts (`spec.md` §3 Write or sync failure); dropped work stays as it is.
        if s.halt.is_set() {
            m.request = None;
            m.requested_at = None;
            m.jobs.clear();
            m.job_times.clear();
            m.describe = None;
            if m.stop {
                return;
            }
            dirs_next = None;
            task_next = None;
            m = s.maint_cv.wait(m).unwrap_or_else(PoisonError::into_inner);
            drop(m);
            continue;
        }
        let now = s.clock.monotonic();
        if let Some(req) = m.request.take() {
            let waited = m.requested_at.take().map(|t| now.saturating_sub(t));
            drop(m);
            if let Some(d) = waited {
                MaintWaits::push(&mut lock(&s.waits).creation, d);
            }
            let r = create_segment(&*s.fs, &req, &s.halt);
            lock(&s.maint).created = Some(r);
        } else if let Some(job) = m.jobs.pop_front() {
            let waited = m.job_times.pop_front().map(|t| now.saturating_sub(t));
            drop(m);
            if let Some(d) = waited {
                MaintWaits::push(&mut lock(&s.waits).seal, d);
            }
            let r = seal_segment_described(&*s.fs, &job, &s.halt);
            if let (Some(d), Ok(sealed)) = (dirs.as_mut(), &r) {
                d.sealed(directory_record(&job, sealed, true));
                dirs_next = Some(Duration::ZERO);
            }
            if r.is_ok() && task.is_some() {
                task_next = Some(Duration::ZERO);
            }
            lock(&s.maint).results.push((job, r));
        } else if let Some((dir, id)) = m.describe.take() {
            drop(m);
            let stop = || s.stopping.load(Ordering::SeqCst) || s.halt.is_set();
            if let Some(r) = describe_sealed_until(&*s.fs, &dir, &id, &stop) {
                lock(&s.maint).described = Some(r);
            }
        } else {
            let stop = m.stop;
            drop(m);
            if let Some(d) = dirs.as_mut() {
                if stop {
                    let synced = lock(&s.info).stats.mark_synced;
                    d.finish(synced, s.clock.monotonic(), s.clock.wall());
                    return;
                }
                if dirs_next.is_some_and(|t| t <= now) {
                    dirs_next = run_directories(s, d);
                }
            }
            if stop {
                return;
            }
            if let Some(t) = task.as_mut()
                && task_next.is_some_and(|n| n <= now)
            {
                task_next = Some(t.turn(s));
            }
            continue;
        }
        s.wake_writer();
    }
}

/// A background task, its governor and whether it waits for a commit sync to return.
struct Stepper {
    task: Box<dyn Background>,
    governor: Governor,
    on_sync: bool,
}

/// How long an idle task waits before it is asked again, when no seal comes first.
const IDLE_RECHECK: Duration = Duration::from_secs(1);

impl Stepper {
    fn new(t: Task, now: Duration) -> Self {
        Self {
            task: t.task,
            governor: Governor::new(t.share, now),
            on_sync: false,
        }
    }

    /// Runs one step if the share admits it; returns when to come back.
    fn turn(&mut self, s: &Shared) -> Duration {
        let now = s.clock.monotonic();
        let piece = match self.task.next() {
            Next::Idle => {
                self.on_sync = false;
                s.sync.want(false);
                return now + IDLE_RECHECK;
            }
            Next::Cpu => None,
            Next::Device(n) => Some(n),
        };
        let pressure = lock(&s.front).waiting.is_some();
        let state = s.sync.state(pressure);
        match self.governor.admit(now, piece, &state) {
            Verdict::Go => {
                self.on_sync = false;
                s.sync.want(false);
                self.task.step();
                let end = s.clock.monotonic();
                self.governor
                    .worked(now, end, piece, &s.sync.state(pressure));
                lock(&s.waits).steps += 1;
                end
            }
            Verdict::Wait { until, on_sync } => {
                self.on_sync = on_sync;
                s.sync.want(on_sync);
                until
            }
        }
    }
}

/// Runs the derived files' due work; asks the writer for a mark sync when a sealed segment waits for one (`spec.md`
/// §3 Durable mark: a write that would name a position above the last synced mark forces the next mark sync).
fn run_directories(s: &Shared, d: &mut Directories) -> Option<Duration> {
    let synced = lock(&s.info).stats.mark_synced;
    let now = s.clock.monotonic();
    let after = d.run(synced, now, s.clock.wall());
    if d.waits_for(synced) {
        {
            let mut f = lock(&s.front);
            f.mark_sync = true;
            f.wake = true;
        }
        s.writer_cv.notify_one();
    }
    after.map(|a| now + a)
}
