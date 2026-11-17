//! The `kvik-maint` thread's loop: segment creation, seals and the derived files, one piece of work at a time, so
//! that a request or a seal never waits behind more than one step of the derived files.

use std::io;
use std::time::Duration;

use super::directories::Directories;
use super::maint::seal_described;
use super::threads::{Shared, lock};
use super::{Fault, FaultOp, create_segment};
use std::sync::PoisonError;

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

pub(super) fn maint_loop(s: &Shared, mut dirs: Option<Directories>) {
    let _guard = MaintGuard(s);
    let mut next: Option<Duration> = dirs.as_ref().map(|_| Duration::ZERO);
    loop {
        let mut m = lock(&s.maint);
        while m.request.is_none() && m.jobs.is_empty() && !m.stop {
            let now = s.clock.monotonic();
            match next {
                Some(t) if t <= now => break,
                Some(t) => {
                    m = s
                        .maint_cv
                        .wait_timeout(m, t - now)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
                None => m = s.maint_cv.wait(m).unwrap_or_else(PoisonError::into_inner),
            }
        }
        // After a fault no file operation starts (`spec.md` §3 Write or sync failure); dropped work stays as it is.
        if s.halt.is_set() {
            m.request = None;
            m.jobs.clear();
            if m.stop {
                return;
            }
            next = None;
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
            let r = seal_described(&*s.fs, &job, &s.halt, dirs.is_some());
            if let (Some(d), Ok(rec)) = (dirs.as_mut(), &r) {
                d.sealed(rec.clone());
                next = Some(Duration::ZERO);
            }
            lock(&s.maint).results.push((job, r.map(|_| ())));
        } else {
            let stop = m.stop;
            drop(m);
            if let Some(d) = dirs.as_mut() {
                if stop {
                    let synced = lock(&s.info).stats.mark_synced;
                    d.finish(synced, s.clock.monotonic(), s.clock.wall());
                    return;
                }
                next = run_directories(s, d);
            }
            if stop {
                return;
            }
            continue;
        }
        s.wake_writer();
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
