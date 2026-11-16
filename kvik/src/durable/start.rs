//! The writer's read of the kept log at a start (roadmap 11.0): the registry, the commit times and the recovery
//! entries in an [`Index`] that keeps no facts, the decision state of [`Decider`], and the counters rebuilt from the
//! log, taken in parts so that memory holds one part of the log at a time and, once read, what the recent window
//! and the revised-key index need.
//!
//! A part that voids what was taken before (a recovery entry voiding intact batches above its `after`) cannot be
//! undone from that state: the log is then read again whole into an index of every fact, as before roadmap 11.0.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use kvik_log::content::Item;
use kvik_log::reader::{LogReader, ReadEvent, ReaderConfig as LogReaderConfig, ReaderError};

use super::LogClock;
use super::history::{Key, span_of};
use super::index::Index;
use super::ingest::Decider;
use super::keys::{Found, LocalFacts, LogFacts, Lost};
use crate::env::Env;
use crate::health::SeriesStats;

/// Items a start takes from the log at a time.
pub(crate) const PART: usize = 1 << 16;

/// What a start reads from the kept log.
pub(crate) struct Kept {
    pub(crate) index: Index,
    pub(crate) decider: Decider,
    pub(crate) counters: BTreeMap<u32, SeriesStats>,
    pub(crate) local: Box<dyn LocalFacts>,
    /// The log was read again whole into memory (module documentation).
    pub(crate) read_whole: bool,
}

impl Kept {
    /// The state of a start that could not read the log: the store is read-only.
    pub(crate) fn unreadable() -> Self {
        Kept {
            index: Index::default(),
            decider: Decider::default(),
            counters: BTreeMap::new(),
            local: Box::new(Unreadable),
            read_whole: false,
        }
    }
}

/// [`LocalFacts`] where nothing can be read.
#[derive(Debug)]
pub(crate) struct Unreadable;

impl LocalFacts for Unreadable {
    fn at(&mut self, keys: &[Key]) -> Found {
        Found {
            unreadable: keys.iter().copied().collect(),
            ..Found::default()
        }
    }
    fn between(&mut self, _: u32, _: i64, _: i64) -> Option<Vec<kvik_log::reader::FoundFact>> {
        None
    }
}

/// A log reader of this process, its lease renewed at least every 30 minutes.
pub(crate) fn log_reader(dir: &Path, env: &Env) -> Result<LogReader, ReaderError> {
    let mut id = [0u8; 16];
    env.entropy.fill(&mut id);
    let mut cfg = LogReaderConfig::new(
        dir,
        env.fs.clone(),
        Arc::new(LogClock::new(env.clock.clone())),
        env.boot.current(),
        id,
    );
    cfg.lease_refresh = Duration::from_secs(30 * 60);
    LogReader::open(cfg)
}

fn count(counters: &mut BTreeMap<u32, SeriesStats>, row: &kvik_log::facts::FactRow) {
    let st = counters.entry(row.series_id).or_default();
    st.recorded += 1;
    st.late += u64::from(row.flags.late);
    st.future += u64::from(row.flags.future);
}

/// Reads the kept log with `reader` in parts of `part` items (see the module documentation), into a decision state
/// of at most `budget` bytes; `open` gives a fresh reader for a read again.
pub(crate) fn read_kept(
    mut reader: LogReader,
    open: impl FnOnce() -> Result<LogReader, ReaderError>,
    part: usize,
    budget: usize,
) -> Result<Kept, ReaderError> {
    let mut index = Index::without_facts();
    let mut decider = Decider::with_budget(budget);
    let mut counters = BTreeMap::new();
    let mut lost = Lost::default();
    loop {
        let (u, more) = reader.refresh_some(part)?;
        for e in u.events {
            match e {
                ReadEvent::Batch(b) => {
                    index.batch(&b);
                    for item in &b.content.items {
                        if let Item::Fact(row) = item {
                            count(&mut counters, row);
                            decider.note_kept(row, || span_of(index.registry(), row.series_id));
                        }
                    }
                }
                ReadEvent::Void { .. } => {
                    let _ = reader.close();
                    return read_whole(open()?, budget);
                }
                ReadEvent::Corruption { file_id, region } => {
                    lost.insert(file_id, region.start, region.end);
                }
            }
        }
        if !more {
            break;
        }
    }
    decider.registry = index.registry().clone();
    Ok(Kept {
        index,
        decider,
        counters,
        local: Box::new(LogFacts::new(reader, lost)),
        read_whole: false,
    })
}

/// The log read whole into an index of every fact.
pub(crate) fn read_whole(mut reader: LogReader, budget: usize) -> Result<Kept, ReaderError> {
    let events = reader.refresh()?.events;
    let mut index = Index::default();
    index.apply(&events);
    let mut lost = Lost::default();
    for e in &events {
        if let ReadEvent::Corruption { file_id, region } = e {
            lost.insert(*file_id, region.start, region.end);
        }
    }
    drop(events);
    let mut counters = BTreeMap::new();
    for id in index.fact_series() {
        for f in index.series_facts(id) {
            count(&mut counters, &f.row);
        }
    }
    let decider = Decider::from_index(&index, budget);
    Ok(Kept {
        index,
        decider,
        counters,
        local: Box::new(LogFacts::new(reader, lost)),
        read_whole: true,
    })
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::task::{Context, Poll, Wake};

    use super::*;
    use crate::durable::history::WINDOW_FACTS;
    use crate::env::ManualClock;
    use crate::series::{Kind, SeriesSpec, ValueType};
    use crate::time::Timestamp;
    use crate::types::{Reading, SeriesId, Value};
    use crate::{Store, StoreConfig};

    const T0: i64 = 1_767_225_600_000_000_000;
    const S: i64 = 1_000_000_000;

    fn block_on<F: Future>(f: F) -> F::Output {
        struct Unpark(std::thread::Thread);
        impl Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let mut f = std::pin::pin!(f);
        let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
        let mut cx = Context::from_waker(&waker);
        loop {
            if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return v;
            }
            std::thread::park();
        }
    }

    #[test]
    fn a_start_keeps_the_recent_window_and_counters_but_no_facts() {
        let env = Env::test(ManualClock::new(Timestamp(T0 + 7_200 * S)));
        let dir = Path::new("start-bounded");
        let s = block_on(Store::open(
            StoreConfig::new(dir, "Etc/UTC")
                .create_if_missing(true)
                .env(env.clone()),
        ))
        .unwrap();
        let spec = |i| {
            SeriesSpec::new(
                format!("a/s{i}/v"),
                Kind::Gauge {
                    value: ValueType::F64,
                },
                "W",
            )
        };
        for i in 0..4 {
            block_on(s.define_series(spec(i))).unwrap();
        }
        for chunk in (0..7_200).collect::<Vec<i64>>().chunks(900) {
            let readings: Vec<Reading> = chunk
                .iter()
                .flat_map(|&k| {
                    (1..=4).map(move |id| {
                        Reading::new(SeriesId(id), Timestamp(T0 + k * S), Value::F64(k as f64))
                    })
                })
                .collect();
            block_on(s.emit(&readings)).unwrap();
        }
        block_on(s.shutdown()).unwrap();
        let kept = read_kept(
            log_reader(dir, &env).unwrap(),
            || log_reader(dir, &env),
            PART,
            usize::MAX,
        )
        .unwrap();
        let held = kept.decider.history.held();
        assert!(held.window <= 4 * 901, "{held:?}");
        assert!(held.window <= 4 * WINDOW_FACTS);
        assert_eq!((held.revised_keys, held.overlay), (0, 0));
        assert_eq!(kept.index.fact_series().count(), 0);
        assert_eq!(kept.index.registry().len(), 4);
        assert!(kept.counters.values().all(|c| c.recorded == 7_200));
    }

    #[test]
    fn the_read_again_whole_gives_what_the_read_in_parts_gives() {
        let clock = ManualClock::new(Timestamp(T0 + 7_200 * S));
        let mut env = Env::test(clock);
        let fs = Arc::new(kvik_log::fs::MemFs::new());
        env.fs = fs.clone();
        let dir = Path::new("start-whole");
        let config = || {
            StoreConfig::new(dir, "Etc/UTC")
                .create_if_missing(true)
                .env(env.clone())
        };
        let s = block_on(Store::open(config())).unwrap();
        let spec = SeriesSpec::new(
            "a/b/v",
            Kind::Gauge {
                value: ValueType::F64,
            },
            "W",
        );
        block_on(s.define_series(spec)).unwrap();
        let readings: Vec<Reading> = (0..3_000)
            .map(|k| Reading::new(SeriesId(1), Timestamp(T0 + k * S), Value::F64(k as f64)))
            .collect();
        block_on(s.emit(&readings)).unwrap();
        // A power loss: the next start writes a recovery entry.
        fs.crash_after(0);
        drop(s);
        fs.power_loss();
        let s = block_on(Store::open(config())).unwrap();
        let last = Reading::new(SeriesId(1), Timestamp(T0 + 4_000 * S), Value::F64(1.0));
        block_on(s.emit(&[last])).unwrap();
        block_on(s.shutdown()).unwrap();
        let open = || log_reader(dir, &env);
        let parts = read_kept(open().unwrap(), open, 100, usize::MAX).unwrap();
        let whole = read_whole(open().unwrap(), usize::MAX).unwrap();
        assert!(whole.read_whole && !parts.read_whole);
        assert_eq!(parts.counters, whole.counters);
        assert_eq!(parts.decider.history.held(), whole.decider.history.held());
        assert_eq!(parts.index.registry().len(), whole.index.registry().len());
    }
}
