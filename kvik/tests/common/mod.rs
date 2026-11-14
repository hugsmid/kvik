//! Helpers shared by the integration tests: a small executor and a store on
//! a manual clock.

#![allow(dead_code)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use kvik::env::{Env, ManualClock};
use kvik::jiff::Timestamp;
use kvik::{Store, StoreConfig};

struct ThreadWaker(Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Runs a future to completion on this thread, parking while it waits.
///
/// # Panics
/// Panics if the future has not completed after ten seconds, so that a test
/// that would wait forever fails instead.
pub fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = std::pin::pin!(f);
    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "the future did not complete");
        thread::park_timeout(left);
    }
}

/// Polls a pinned future once with a waker that records whether it was woken.
pub fn poll_once<F: Future>(f: Pin<&mut F>, woken: &Arc<Flag>) -> Poll<F::Output> {
    let waker = Waker::from(woken.clone());
    f.poll(&mut Context::from_waker(&waker))
}

/// A waker that sets a flag.
#[derive(Default)]
pub struct Flag(std::sync::atomic::AtomicBool);

impl Flag {
    /// Whether it was woken; clears the flag.
    pub fn take(&self) -> bool {
        self.0.swap(false, std::sync::atomic::Ordering::SeqCst)
    }
}

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Noon UTC on a Monday in January, when London is on UTC.
pub fn start() -> Timestamp {
    "2026-01-05T12:00:00Z".parse().unwrap()
}

/// A configuration for a store unique to the test, on a manual clock.
pub fn config(name: &str, clock: &ManualClock) -> StoreConfig {
    let mut c = StoreConfig::new(format!("kvik-test/{name}"), "Europe/London");
    c.env = Env::test(clock.clone());
    c
}

/// A new store unique to the test, on a manual clock at [`start`].
pub fn store(name: &str) -> (Store, ManualClock) {
    let clock = ManualClock::new(start());
    let s = block_on(Store::open(config(name, &clock))).unwrap();
    (s, clock)
}

/// Seconds after [`start`].
pub fn at(secs: i64) -> Timestamp {
    start()
        .checked_add(kvik::jiff::SignedDuration::from_secs(secs))
        .unwrap()
}
