//! A durable store on an in-memory file system, with crashes and power losses, for the consumer tests.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kvik::env::{Env, FixedBootId, ManualClock, MemFs};
use kvik::{
    Consumer, Delivery, DeliveryKind, LogConfig, MarkSync, Reading, ResyncPart, SeriesId,
    SeriesSpec, Store, StoreConfig, Value, ValueType,
};

use super::{at, block_on, start};

/// A durable store's world: its in-memory file system, boot identity and clock.
pub struct World {
    pub dir: String,
    pub fs: Arc<MemFs>,
    pub boot: Arc<FixedBootId>,
    pub clock: ManualClock,
    boot_count: u8,
    /// Whether the delivery thread runs timers on its own (`Env::background_timers`).
    pub background_timers: bool,
    /// The mark sync; every commit unless a test sets an interval.
    pub mark_sync: MarkSync,
}

impl World {
    pub fn new(name: &str) -> Self {
        World {
            dir: format!("consumers/{name}"),
            fs: Arc::new(MemFs::new()),
            boot: Arc::new(FixedBootId::new(Some([1; 16]))),
            clock: ManualClock::new(start()),
            boot_count: 1,
            background_timers: true,
            mark_sync: LogConfig::default().mark_sync,
        }
    }

    /// A world whose timers run only when the store is called, as on the model backend.
    pub fn quiet(name: &str) -> Self {
        let mut w = World::new(name);
        w.background_timers = false;
        w
    }

    pub fn config(&self) -> StoreConfig {
        let mut env = Env::test(self.clock.clone());
        env.fs = self.fs.clone();
        env.boot = self.boot.clone();
        env.background_timers = self.background_timers;
        let mut log = LogConfig::default();
        log.segment_size = 16 << 20;
        log.min_sync_interval = Duration::ZERO;
        log.mark_sync = self.mark_sync;
        StoreConfig::new(&self.dir, "Europe/London")
            .create_if_missing(true)
            .env(env)
            .log(log)
    }

    pub fn open(&self) -> Store {
        block_on(Store::open(self.config())).unwrap()
    }

    /// The process stops (every later file operation fails) and its handles are dropped; with `power`, the power
    /// goes too: every file returns to what was synced and the machine boots with a new boot identity.
    pub fn crash(&mut self, store: Store, consumers: Vec<Consumer>, power: bool) {
        self.stop(store, consumers, power.then_some(None));
    }

    /// As [`World::crash`], after the process has already been stopped (by `MemFs::crash_after`), with a power loss
    /// that lets part of the unsynced work through when `power` is `Some(Some(seed))`.
    pub fn stop(&mut self, store: Store, consumers: Vec<Consumer>, power: Option<Option<u64>>) {
        self.fs.crash_after(0);
        drop(consumers);
        drop(store);
        match power {
            Some(seed) => {
                match seed {
                    Some(seed) => self.fs.power_loss_partial(seed),
                    None => self.fs.power_loss(),
                }
                self.boot_count += 1;
                self.boot.set(Some([self.boot_count; 16]));
            }
            None => self.fs.restart(),
        }
    }

    /// The path of a file of the store whose path ends with the components of `rel`; matched by component so that
    /// it holds whatever the platform's separator.
    pub fn path_with(&self, rel: impl AsRef<Path>) -> Option<PathBuf> {
        let rel = rel.as_ref();
        self.fs
            .paths()
            .into_iter()
            .find(|p| p.starts_with(&self.dir) && p.ends_with(rel))
    }
}

pub fn gauge(name: &str) -> SeriesSpec {
    SeriesSpec::new(
        name,
        kvik::Kind::Gauge {
            value: ValueType::F64,
        },
        "W",
    )
    .expected_interval(Duration::from_secs(1))
}

pub fn num(id: SeriesId, secs: i64, v: f64) -> Reading {
    Reading::new(id, at(secs), Value::F64(v))
}

/// Every ready delivery, each confirmed.
pub fn drain(c: &mut Consumer) -> Vec<Delivery> {
    let mut out = Vec::new();
    while let Some(d) = c.try_next().unwrap() {
        block_on(c.confirm(d.id)).unwrap();
        out.push(d);
    }
    out
}

/// Every ready delivery, none confirmed.
pub fn ready(c: &mut Consumer) -> Vec<Delivery> {
    let mut out = Vec::new();
    while let Some(d) = c.try_next().unwrap() {
        out.push(d);
    }
    out
}

pub fn resync(d: &Delivery) -> Option<&ResyncPart> {
    match &d.kind {
        DeliveryKind::Resync(r) => Some(r),
        _ => None,
    }
}
