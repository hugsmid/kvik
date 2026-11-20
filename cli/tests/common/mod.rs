//! A store on an in-memory file system and a manual clock, and the tool run against it.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use kvik::env::{Env, ManualClock, MemFs};
use kvik::{
    Kind, Quality, Reading, SeriesId, SeriesSpec, StateValues, Store, StoreConfig, Timestamp,
    Value, ValueType,
};

pub const T0: i64 = 1_767_225_600_000_000_000;
pub const S: i64 = 1_000_000_000;

struct ThreadWaker(Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

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

pub struct Fixture {
    pub clock: ManualClock,
    pub env: Env,
    pub fs: MemFs,
    pub dir: PathBuf,
    pub store: Option<Store>,
    pub ids: Vec<SeriesId>,
}

impl Fixture {
    /// Four series of different kinds, one aliased, with readings a minute old and newer; one series has none.
    pub fn new() -> Fixture {
        let clock = ManualClock::new(Timestamp(T0));
        let mut env = Env::test(clock.clone());
        let fs = MemFs::new();
        env.fs = Arc::new(fs.clone());
        env.background_timers = false;
        let dir = PathBuf::from("store");
        let config = StoreConfig::new(&dir, "Etc/UTC")
            .create_if_missing(true)
            .env(env.clone());
        let store = block_on(Store::open(config)).unwrap();
        let f64 = ValueType::F64;
        let specs = [
            SeriesSpec::new("site/meter/power", Kind::Gauge { value: f64 }, "W"),
            SeriesSpec::new("site/meter/energy", Kind::Counter { value: f64 }, "kWh"),
            SeriesSpec::new(
                "site/hall/mode",
                Kind::State {
                    values: StateValues::Named(vec!["off".into(), "on, quiet".into()]),
                },
                "",
            ),
            SeriesSpec::new("site/hall/note", Kind::Text, ""),
        ];
        let ids: Vec<SeriesId> = specs
            .into_iter()
            .map(|s| block_on(store.define_series(s)).unwrap())
            .collect();
        block_on(store.alias(ids[0], "site/meter/watts")).unwrap();
        let mut f = Fixture {
            clock,
            env,
            fs,
            dir,
            store: Some(store),
            ids,
        };
        f.emit(&[
            Reading::new(f.ids[0], f.at(-60), Value::F64(412.5)),
            Reading::new(f.ids[1], f.at(-60), Value::F64(1_204.25)),
            Reading::new(f.ids[2], f.at(-30), Value::State(1)),
            Reading::new(f.ids[0], f.at(-10), Value::F64(420.0)).with_quality(Quality::Suspect),
        ]);
        f
    }

    pub fn at(&self, secs: i64) -> Timestamp {
        Timestamp(self.clock.wall().0 + secs * S)
    }

    pub fn emit(&mut self, readings: &[Reading]) {
        let store = self.store.as_ref().expect("an open store");
        block_on(store.emit(readings)).unwrap();
    }

    pub fn shut(&mut self) {
        if let Some(s) = self.store.take() {
            block_on(s.shutdown()).unwrap();
        }
    }

    /// The tool's output for `args`, split at spaces; `--store` is the fixture's.
    pub fn run(&self, args: &str) -> Result<String, kvik_cli::Error> {
        let v: Vec<String> = args.split_whitespace().map(String::from).collect();
        let mut out = Vec::new();
        kvik_cli::run(&v, Some(self.dir.clone()), self.env.clone(), &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    /// Every file of the store with its bytes.
    pub fn files(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let mut v: Vec<_> = self
            .fs
            .paths()
            .into_iter()
            .filter(|p| p.starts_with(&self.dir))
            .map(|p| {
                let b = self.fs.read(&p).unwrap_or_default();
                (p, b)
            })
            .collect();
        v.sort();
        v
    }
}
