//! A durable store on an in-memory file system, with the helpers the durable tests share.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use kvik::env::{Env, FixedBootId, ManualClock, MemFs};
use kvik::{
    Fact, Kind, LogConfig, LossKind, Position, Reader, ReaderConfig, Reading, SeriesId, SeriesSpec,
    Store, StoreConfig, Value, ValueType, View,
};

use super::{at, block_on, start};

/// A durable store's world: its in-memory file system, boot identity and clock.
pub struct World {
    pub dir: String,
    pub fs: Arc<MemFs>,
    pub boot: Arc<FixedBootId>,
    pub clock: ManualClock,
    pub boot_count: u8,
}

impl World {
    pub fn new(name: &str) -> Self {
        World {
            dir: format!("durable/{name}"),
            fs: Arc::new(MemFs::new()),
            boot: Arc::new(FixedBootId::new(Some([1; 16]))),
            clock: ManualClock::new(start()),
            boot_count: 1,
        }
    }

    pub fn env(&self) -> Env {
        let mut env = Env::test(self.clock.clone());
        env.fs = self.fs.clone();
        env.boot = self.boot.clone();
        env
    }

    pub fn config(&self) -> StoreConfig {
        let mut log = LogConfig::default();
        log.segment_size = 16 << 20;
        StoreConfig::new(&self.dir, "Europe/London")
            .create_if_missing(true)
            .env(self.env())
            .log(log)
    }

    pub fn open(&self) -> Store {
        block_on(Store::open(self.config())).unwrap()
    }

    pub fn reader(&self) -> Reader {
        Reader::open(ReaderConfig::new(&self.dir).env(self.env())).unwrap()
    }

    /// The process stops (every later file operation fails), the store's handles are dropped, then the power
    /// goes: every file returns to what was synced. The machine boots again with a new boot identity.
    pub fn power_loss(&mut self, store: Store) {
        self.fs.crash_after(0);
        drop(store);
        self.fs.power_loss();
        self.boot_count += 1;
        self.boot.set(Some([self.boot_count; 16]));
    }

    /// The bytes of the open segments, with their paths.
    pub fn segments(&self) -> Vec<(std::path::PathBuf, Vec<u8>)> {
        self.fs
            .paths()
            .into_iter()
            .filter(|p| {
                p.starts_with(Path::new(&self.dir).join("log"))
                    && p.extension().is_some_and(|e| e == "open" || e == "kvl")
            })
            .map(|p| {
                let b = self.fs.read(&p).unwrap();
                (p, b)
            })
            .collect()
    }
}

pub fn gauge(name: &str) -> SeriesSpec {
    SeriesSpec::new(
        name,
        Kind::Gauge {
            value: ValueType::F64,
        },
        "W",
    )
    .expected_interval(Duration::from_secs(1))
}

pub fn num(id: SeriesId, secs: i64, v: f64) -> Reading {
    Reading::new(id, at(secs), Value::F64(v))
}

pub fn history(r: &Reader, id: SeriesId) -> Vec<Fact> {
    r.facts(id, at(-1_000_000)..at(1_000_000), View::History)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

/// Damages `text` in place in every segment file that holds it (a segment being sealed is briefly held twice).
pub fn damage(w: &World, text: &[u8]) {
    let held: Vec<_> = w
        .segments()
        .into_iter()
        .filter(|(_, b)| b.windows(text.len()).any(|x| x == text))
        .collect();
    assert!(!held.is_empty());
    for (path, bytes) in held {
        let at_text = bytes.windows(text.len()).position(|x| x == text).unwrap();
        w.fs.poke(&path, at_text, b"X");
    }
}

pub fn note(id: SeriesId, secs: i64, text: &str) -> Reading {
    Reading::new(id, at(secs), Value::Text(text.into()))
}

pub fn data_lost(
    losses: &[kvik::LossReport],
) -> Vec<(Position, Option<Position>, Option<Position>, bool)> {
    losses
        .iter()
        .filter(|l| l.kind == LossKind::DataLost)
        .map(|l| (l.identity, l.first, l.last, l.recorded))
        .collect()
}
