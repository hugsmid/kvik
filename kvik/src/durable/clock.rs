//! The window clock and the refresh instants of live staleness, kept per process.
//!
//! `now` (spec §6) is an engine UTC clock that never runs backwards: max(system clock, previous value), advanced by
//! the monotonic clock. Live staleness of an observed series (spec §5.4) is measured on the monotonic clock since
//! the series' last commit of a fact that is not bad quality, not a future fact and not outranked, taken when the
//! call's acknowledgement is; the instant lives only in the writer's process, so a reader process, and the writer before its first such commit, measure
//! it in event time, `now − t` of the live-state fact, as the reference model does after a restart (Q-16).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::env::Clock;

#[derive(Debug)]
struct State {
    /// The last `now` and the monotonic time it was taken at.
    now: i64,
    mono: u64,
    /// Per series id, the monotonic time of its last commit that refreshes live staleness.
    refreshed: BTreeMap<u32, u64>,
}

/// See the module documentation.
#[derive(Debug)]
pub(crate) struct Clocks {
    env: Arc<dyn Clock>,
    state: Mutex<State>,
}

impl Clocks {
    /// Clocks starting at the wall clock now.
    pub(crate) fn new(env: Arc<dyn Clock>) -> Self {
        let state = State {
            now: env.wall().0,
            mono: env.monotonic().0,
            refreshed: BTreeMap::new(),
        };
        Clocks {
            env,
            state: Mutex::new(state),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The window clock now.
    pub(crate) fn now(&self) -> i64 {
        let mut s = self.lock();
        let mono = self.env.monotonic().0;
        let advanced = i64::try_from(mono.saturating_sub(s.mono)).unwrap_or(i64::MAX);
        s.mono = s.mono.max(mono);
        s.now = s.now.saturating_add(advanced).max(self.env.wall().0);
        s.now
    }

    /// Notes a commit that refreshes the live staleness of series `id`.
    pub(crate) fn refresh(&self, id: u32) {
        let mono = self.env.monotonic().0;
        self.lock().refreshed.insert(id, mono);
    }

    /// Nanoseconds on the monotonic clock since the last commit that refreshed series `id` in this process.
    pub(crate) fn since_refresh(&self, id: u32) -> Option<i64> {
        let mono = self.env.monotonic().0;
        self.lock()
            .refreshed
            .get(&id)
            .map(|m| i64::try_from(mono.saturating_sub(*m)).unwrap_or(i64::MAX))
    }
}
