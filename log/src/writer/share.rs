//! The maintenance share (compaction design §4, Q-721): what a background task on `kvik-maint` may take of the CPU and
//! the device, and when it issues a device call. Time comes from the caller, so tests drive it with an injected clock.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// The size of one device call of a background task: a read or a write of at most this many bytes.
pub const PIECE: u64 = 128 * 1024;
/// The least floor rate, bytes a second.
const MIN_FLOOR: u64 = 128 * 1024;

/// The share's two limits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShareConfig {
    /// `maintenance.cpu_share`: the fraction of one core the task may work, above 0 and at most 1.
    pub cpu_share: f64,
    /// `maintenance.io_rate`: bytes a second read and written.
    pub io_rate: u64,
}

impl ShareConfig {
    /// The defaults for a machine of `cores` cores (§4): 0.25 of a core, 0.15 on one core; 8 MiB/s, a minimal SSD.
    pub fn for_cores(cores: usize) -> Self {
        Self {
            cpu_share: if cores <= 1 { 0.15 } else { 0.25 },
            io_rate: 8 << 20,
        }
    }

    /// The floor rate: an eighth of `io_rate`, at least 128 KiB/s.
    pub fn floor_rate(&self) -> u64 {
        (self.io_rate / 8).max(MIN_FLOOR)
    }

    /// Refuses a `cpu_share` that is not a number or not above 0 (Q-1843).
    pub fn check(&self) -> std::io::Result<()> {
        if self.cpu_share > 0.0 {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "maintenance.cpu_share must be above 0",
            ))
        }
    }
}

/// What the writer is doing, as the share reads it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriterState {
    /// A commit sync is in flight.
    pub syncing: bool,
    /// Commit syncs that have returned so far.
    pub syncs_returned: u64,
    /// Ingest waits (the writer's pressure flag).
    pub pressure: bool,
}

/// The writer's syncs as `kvik-maint` sees them: set by `kvik-writer` around each commit sync.
#[derive(Debug, Default)]
pub struct SyncSignal {
    syncing: AtomicBool,
    returned: AtomicU64,
    /// A task waits for a sync to return: the writer then wakes `kvik-maint`.
    wanted: AtomicBool,
    wake: AtomicBool,
}

impl SyncSignal {
    pub(crate) fn begin(&self) {
        self.syncing.store(true, Ordering::Release);
    }

    pub(crate) fn end(&self) {
        self.syncing.store(false, Ordering::Release);
        self.returned.fetch_add(1, Ordering::AcqRel);
        if self.wanted.swap(false, Ordering::AcqRel) {
            self.wake.store(true, Ordering::Release);
        }
    }

    /// Whether a sync returned that a waiting task asked to hear of.
    pub(crate) fn take_wake(&self) -> bool {
        self.wake.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn want(&self, on: bool) {
        self.wanted.store(on, Ordering::Release);
    }

    pub(crate) fn state(&self, pressure: bool) -> WriterState {
        WriterState {
            syncing: self.syncing.load(Ordering::Acquire),
            syncs_returned: self.returned.load(Ordering::Acquire),
            pressure,
        }
    }
}

/// What the share says about the next step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Run it now.
    Go,
    /// Not before `until`; also try again when a commit sync returns, if `on_sync`.
    Wait {
        /// When it may run at the latest.
        until: Duration,
        /// A returning sync may let it run sooner.
        on_sync: bool,
    },
}

/// The governor of one background task (§4): a duty cycle over its steps and a byte bucket over its device calls,
/// with device calls issued when the writer is not syncing, just after a sync returns, or at the floor rate's
/// deadline.
#[derive(Clone, Debug)]
pub struct Governor {
    cfg: ShareConfig,
    cpu_ready_at: Duration,
    tokens: f64,
    tokens_at: Duration,
    rate: u64,
    wanted_since: Option<Duration>,
    seen_returns: u64,
    /// Work and bytes so far, for `Stats`' share used.
    worked: Duration,
    bytes: u64,
}

impl Governor {
    /// A governor whose first piece may be issued at once; an `io_rate` below the least floor is raised to it (Q-1843).
    pub fn new(cfg: ShareConfig, now: Duration) -> Self {
        let cfg = ShareConfig {
            io_rate: cfg.io_rate.max(MIN_FLOOR),
            ..cfg
        };
        Self {
            cfg,
            cpu_ready_at: now,
            tokens: PIECE as f64,
            tokens_at: now,
            rate: cfg.io_rate,
            wanted_since: None,
            seen_returns: 0,
            worked: Duration::ZERO,
            bytes: 0,
        }
    }

    fn share(&self, pressure: bool) -> f64 {
        let s = self.cfg.cpu_share.clamp(0.01, 1.0);
        if pressure { s / 2.0 } else { s }
    }

    fn refill(&mut self, now: Duration, pressure: bool) {
        let dt = now.saturating_sub(self.tokens_at).as_secs_f64();
        self.tokens = (self.tokens + dt * self.rate as f64).min(PIECE as f64);
        self.tokens_at = self.tokens_at.max(now);
        self.rate = if pressure {
            self.cfg.floor_rate()
        } else {
            self.cfg.io_rate
        };
    }

    /// May the next step start at `now`? `piece` is the device call it issues: `Some(bytes)` for a read, a write
    /// (bytes at most [`PIECE`]) or a sync or rename (0 bytes); `None` for a step of CPU only.
    pub fn admit(&mut self, now: Duration, piece: Option<u64>, w: &WriterState) -> Verdict {
        self.refill(now, w.pressure);
        let mut until = self.cpu_ready_at.max(now);
        let Some(bytes) = piece else {
            return if until <= now {
                Verdict::Go
            } else {
                Verdict::Wait {
                    until,
                    on_sync: false,
                }
            };
        };
        let bytes = bytes.min(PIECE);
        if (self.tokens as u64) < bytes {
            let short = (bytes as f64 - self.tokens) / self.rate as f64;
            until = until.max(now + Duration::from_secs_f64(short));
        }
        let since = *self.wanted_since.get_or_insert(now);
        let floor = self.cfg.floor_rate().min(self.rate.max(1));
        let deadline = since + Duration::from_secs_f64(bytes.max(PIECE) as f64 / floor as f64);
        // A return counts once: one seen while the step waited for its CPU or bytes is spent.
        let fresh = w.syncs_returned != self.seen_returns;
        self.seen_returns = w.syncs_returned;
        let idle = !w.syncing || fresh || now >= deadline;
        if until <= now && idle {
            return Verdict::Go;
        }
        if !idle {
            until = until.max(deadline);
        }
        Verdict::Wait {
            until,
            on_sync: !idle,
        }
    }

    /// The step admitted at `start` ended at `end`, having issued `piece` (as [`Self::admit`] was told).
    pub fn worked(&mut self, start: Duration, end: Duration, piece: Option<u64>, w: &WriterState) {
        let work = end.saturating_sub(start);
        let s = self.share(w.pressure);
        let wait = (work.as_nanos() as f64 * (1.0 - s) / s).ceil();
        self.cpu_ready_at = end + Duration::from_nanos(wait as u64);
        self.worked += work;
        if let Some(bytes) = piece {
            self.refill(end, w.pressure);
            self.tokens -= bytes.min(PIECE) as f64;
            self.bytes += bytes;
            self.wanted_since = None;
        }
    }

    /// Time worked and bytes issued so far.
    pub fn used(&self) -> (Duration, u64) {
        (self.worked, self.bytes)
    }
}

/// The next step of a background task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Nothing is due; asked again after a seal or a second.
    Idle,
    /// A step of CPU only.
    Cpu,
    /// A step that issues one device call of this many bytes (0 for a sync or a rename).
    Device(u64),
}

/// A background task stepped by `kvik-maint` within its share (compaction design §4). Creation ahead and sealing
/// are served between steps, so each waits at most one step.
pub trait Background: Send {
    /// What the next step does.
    fn next(&mut self) -> Next;
    /// Runs one step: at most one block decoded or encoded and one blocking device call.
    fn step(&mut self);
}

/// A background task with its share, for [`crate::writer::Log::start_with_task`].
pub struct Task {
    /// The task.
    pub task: Box<dyn Background>,
    /// Its share.
    pub share: ShareConfig,
}

impl std::fmt::Debug for Task {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Task").field("share", &self.share).finish()
    }
}

#[cfg(test)]
mod tests;
