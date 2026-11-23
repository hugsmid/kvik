//! The fan-out thread of the writer's process (the query design's section 3.6, Q-1118): woken by the writer's turns
//! and by refreshes of live staleness, it runs one refresh round for what the writer published, whose end rings only
//! the sessions the round reaches ([`super::hub`]), and rings the sessions that select a refreshed series. Each
//! session keeps its own batch interval: one rung inside it sleeps out the rest. Started by the first live session of
//! the process; it ends when the writer stops or the reader is dropped, which wakes it. A thread that panics leaves
//! every session rung by every round, as a failed spawn does.

use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError, Weak};
use std::task::{Wake, Waker};
use std::time::Duration;

use super::hub::Bell;
use crate::durable::reader::DurableReader;

/// The longest the thread waits with nothing waking it: a loss another read found reaches the sessions within it.
const QUIET: Duration = Duration::from_secs(1);

struct Kick(Bell);

impl Wake for Kick {
    fn wake(self: Arc<Self>) {
        self.0.ring();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.ring();
    }
}

/// Starts the thread for `r` unless it runs or `r` is not the writer's process's reader.
pub(crate) fn ensure(r: &Arc<DurableReader>) {
    let Some(watch) = r.writer().cloned() else {
        return;
    };
    if r.fanout.swap(true, Ordering::SeqCst) {
        return;
    }
    let kick = Arc::new(Kick(Bell::default()));
    let waker = Waker::from(kick.clone());
    let id = watch.on_progress(waker.clone());
    r.clocks().on_refresh(waker.clone());
    *r.fanout_kick.lock().unwrap_or_else(PoisonError::into_inner) = Some(waker);
    let weak = Arc::downgrade(r);
    let spawned = std::thread::Builder::new()
        .name("kvik-fanout".into())
        .spawn(move || {
            let _guard = Guard {
                reader: weak.clone(),
                progress: Some((watch.clone(), id)),
            };
            run(&weak, &kick.0, &watch);
        });
    if spawned.is_err() {
        // Without the thread every session is rung by every round, as each ends: slower, never silent.
        r.fanout_failed.store(true, Ordering::SeqCst);
    }
}

/// Ends the thread's watch; if the thread panics, has every session rung by every round and rings them all, so that
/// none waits on a bell nobody rings.
pub(crate) struct Guard {
    pub(crate) reader: Weak<DurableReader>,
    pub(crate) progress: Option<(kvik_log::writer::Watch, u64)>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some((watch, id)) = self.progress.take() {
            watch.off_progress(id);
        }
        if std::thread::panicking()
            && let Some(r) = self.reader.upgrade()
        {
            r.fanout_failed.store(true, Ordering::SeqCst);
            r.hub().ring_all();
        }
    }
}

fn run(weak: &Weak<DurableReader>, kick: &Bell, watch: &kvik_log::writer::Watch) {
    let mut seen = 0;
    loop {
        seen = kick.wait(seen, QUIET);
        let Some(r) = weak.upgrade() else {
            return;
        };
        if watch.stopped() {
            r.hub().ring_all();
            return;
        }
        if !r.hub().subscribed() {
            continue;
        }
        pass(&r);
    }
}

/// One pass: a round, which rings the sessions it reaches as it ends if it read something (a failed one is every
/// session's to return, Q-951), then the sessions that select a series refreshed since the last pass.
pub(crate) fn pass(r: &DurableReader) {
    if r.snapshot().and_then(|v| r.end(v)).is_err() {
        r.hub().ring_all();
    }
    let refreshed = r.clocks().take_refreshed();
    if !refreshed.is_empty() {
        r.hub().ring_refreshed(refreshed);
    }
}
