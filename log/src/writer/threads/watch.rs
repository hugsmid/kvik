//! Watching the writer's progress: the condition variable and the wakers each published turn wakes.

use std::sync::{Arc, PoisonError};
use std::task::Waker;
use std::time::Duration;

use super::super::{Fault, LogStats, Reason};
use super::{Shared, lock};

impl Shared {
    /// Wakes the progress waiters and wakers once `info` names the new turn.
    pub(super) fn progressed(&self) {
        self.progress_cv.notify_all();
        for w in lock(&self.progress_wakers).1.values() {
            w.wake_by_ref();
        }
    }
}

/// Watches the writer's progress ([`Log::watch`](super::Log::watch)).
#[derive(Clone, Debug)]
pub struct Watch {
    pub(super) shared: Arc<Shared>,
}

impl Watch {
    /// As [`Log::wait_progress`](super::Log::wait_progress).
    pub fn wait_progress(&self, seen: u64, timeout: Duration) -> u64 {
        let i = lock(&self.shared.info);
        let (i, _) = self
            .shared
            .progress_cv
            .wait_timeout_while(i, timeout, |i| i.progress == seen && !i.stopped)
            .unwrap_or_else(PoisonError::into_inner);
        i.progress
    }

    /// The writer's commit clock.
    pub fn commit_clock(&self) -> Arc<super::super::CommitClock> {
        self.shared.commit_clock.clone()
    }

    /// Whether the writer thread has ended.
    pub fn stopped(&self) -> bool {
        lock(&self.shared.info).stopped
    }

    /// Wakes `waker` at every turn the writer publishes and when it stops, until [`Watch::off_progress`] with the id
    /// returned: one thread can then wake whom a turn concerns, rather than every waiter waking on the condition
    /// variable.
    pub fn on_progress(&self, waker: Waker) -> u64 {
        let mut w = lock(&self.shared.progress_wakers);
        w.0 += 1;
        let id = w.0;
        w.1.insert(id, waker);
        id
    }

    /// Stops waking the waker registered as `id`.
    pub fn off_progress(&self, id: u64) {
        lock(&self.shared.progress_wakers).1.remove(&id);
    }

    /// As [`Log::stats`](super::Log::stats).
    pub fn stats(&self) -> LogStats {
        lock(&self.shared.info).stats.clone()
    }

    /// The durable position, without copying the rest of the statistics.
    pub fn durable(&self) -> super::super::Position {
        lock(&self.shared.info).stats.durable
    }

    /// The last synced mark, without copying the rest of the statistics.
    pub fn mark_synced(&self) -> super::super::Position {
        lock(&self.shared.info).stats.mark_synced
    }

    /// As [`Log::reasons`](super::Log::reasons).
    pub fn reasons(&self) -> Vec<Reason> {
        lock(&self.shared.info).reasons.clone()
    }

    /// As [`Log::fault`](super::Log::fault).
    pub fn fault(&self) -> Option<Fault> {
        lock(&self.shared.info).fault.clone()
    }

    /// As [`Log::queue_depth`](super::Log::queue_depth).
    pub fn queue_depth(&self) -> usize {
        lock(&self.shared.front).queue.depth()
    }

    /// Asks the writer to sync the mark now rather than at its timer, and waits until the mark on stable storage
    /// names at least the durable position at the call (`spec.md` §3 Durable mark: a file that would name a
    /// position above the last synced mark waits for the next mark sync or forces one). Returns the last synced
    /// position, or the fault if the store is or becomes read-only; after a shutdown, the last synced position.
    pub fn sync_mark(&self) -> Result<super::super::Position, Fault> {
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
