//! Helpers shared by the integration tests: a small executor and a store on
//! a manual clock.

#![allow(dead_code)]

pub mod durable;
pub mod world;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use std::collections::HashMap;
use std::sync::Mutex;

use kvik::env::{Env, ManualClock};
use kvik::{Backend, LogConfig, ReaderConfig, Span, Store, StoreConfig, Timestamp};

/// Both backends, for the tests that run on each.
pub const BACKENDS: [Backend; 2] = [Backend::Model, Backend::Durable];

/// The environment of each durable test store, by name, so that a store can be opened again and read by a
/// reader on the same in-memory file system.
static ENVS: Mutex<Option<HashMap<String, Env>>> = Mutex::new(None);

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

    /// Whether it is woken within five seconds; clears the flag. The durable backend wakes a consumer from the
    /// `kvik-deliver` thread, shortly after the emit that fed it returns (Q-1119).
    pub fn take_soon(&self) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if self.take() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        false
    }
}

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Noon UTC on a Monday in January, when the site zone is on UTC.
pub fn start() -> Timestamp {
    "2026-01-05T12:00:00Z".parse().unwrap()
}

/// A configuration for a store unique to the test, on a manual clock,
/// created if missing, on the model backend.
pub fn config(name: &str, clock: &ManualClock) -> StoreConfig {
    config_on(Backend::Model, name, clock)
}

/// The test environment of a durable store: one in-memory file system per
/// name, with the given clock.
pub fn env_for(name: &str, clock: &ManualClock) -> Env {
    let mut envs = ENVS.lock().unwrap();
    let map = envs.get_or_insert_with(HashMap::new);
    let mut env = map
        .entry(name.to_string())
        .or_insert_with(|| Env::test(clock.clone()))
        .clone();
    env.clock = Arc::new(clock.clone());
    env
}

/// The directory of a test store on a backend.
pub fn dir_on(backend: Backend, name: &str) -> String {
    match backend {
        Backend::Model => format!("kvik-test/{name}"),
        _ => format!("kvik-test/durable/{name}"),
    }
}

/// [`config`] on a backend. A durable store has small segments (16 MiB,
/// the least allowed), on an in-memory file system shared by every open of
/// the same name.
pub fn config_on(backend: Backend, name: &str, clock: &ManualClock) -> StoreConfig {
    let c = StoreConfig::new(dir_on(backend, name), "Europe/London")
        .create_if_missing(true)
        .backend(backend);
    match backend {
        Backend::Model => c.env(Env::test(clock.clone())),
        _ => {
            let mut log = LogConfig::default();
            log.segment_size = 16 << 20;
            c.env(env_for(name, clock)).log(log)
        }
    }
}

/// A reader configuration for a test store on a backend.
pub fn reader_on(backend: Backend, name: &str, clock: &ManualClock) -> ReaderConfig {
    let c = ReaderConfig::new(dir_on(backend, name)).backend(backend);
    match backend {
        Backend::Model => c,
        _ => c.env(env_for(name, clock)),
    }
}

/// A new store unique to the test, on a manual clock at [`start`], on the
/// model backend.
pub fn store(name: &str) -> (Store, ManualClock) {
    store_on(Backend::Model, name)
}

/// [`store`] on a backend.
pub fn store_on(backend: Backend, name: &str) -> (Store, ManualClock) {
    let clock = ManualClock::new(start());
    let s = block_on(Store::open(config_on(backend, name, &clock))).unwrap();
    (s, clock)
}

/// Seconds after [`start`].
pub fn at(secs: i64) -> Timestamp {
    start() + Span::from_secs(secs)
}

/// Waits until a seeded start's scan of the log below its seed has ended (Q-1115): damage made after it is met by the
/// store's own reads, not by the scan.
pub fn scanned(s: &kvik::Store) {
    while s.stats().ingest.start_scanning {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}
