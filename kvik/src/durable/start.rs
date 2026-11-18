//! The writer's read of the kept log at a start (roadmap 11.0): the registry, the commit times and the recovery
//! entries in an [`Index`] that keeps no facts, the decision state of [`Decider`], and the counters rebuilt from the
//! log, taken in parts so that memory holds one part of the log at a time and, once read, what the recent window
//! and the revised-key index need.
//!
//! A part that voids what was taken before (a recovery entry voiding intact batches above its `after`) cannot be
//! undone from that state: the log is then read again whole into an index of every fact, as before roadmap 11.0.
//!
//! Every event goes through the loss watch's [`Finder`] first, so each region the start passes over is a loss in
//! every run, as a reader process reports it (Q-947), and the reader that read the log becomes the watch.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use kvik_log::content::Item;
use kvik_log::manifest::LostRange;
use kvik_log::reader::{LogReader, ReadEvent, ReaderConfig as LogReaderConfig, ReaderError};

use super::LogClock;
use super::by_id::ById;
use super::history::span_of;
use super::index::Index;
use super::ingest::Decider;
use super::keys::{LocalFacts, LogFacts};
use super::loss::{Finder, FoundBy, Latch, Regions, Watch};
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
    /// Damaged regions passed over (Q-755), and the manifest's lost ranges as read.
    pub(crate) passed: Regions,
    pub(crate) recorded: Vec<LostRange>,
    /// The reader that read the log, at its head.
    pub(crate) watch: Watch,
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

fn count(counters: &mut ById<SeriesStats>, row: &kvik_log::facts::FactRow) {
    let st = counters.get_or_insert_with(row.series_id, SeriesStats::default);
    st.recorded += 1;
    st.late += u64::from(row.flags.late);
    st.future += u64::from(row.flags.future);
}

/// Orphaned facts belong to no series (spec §2.1): only series the registry holds are counted.
fn defined(counters: ById<SeriesStats>, index: &Index) -> BTreeMap<u32, SeriesStats> {
    counters
        .into_iter()
        .filter(|(id, _)| index.registry().get(*id).is_some())
        .collect()
}

/// Reads the kept log with `reader` in parts of `part` items (see the module documentation), into a decision state
/// of at most `budget` bytes, noting the losses it passes over in `latch`; `open` gives a fresh reader, for the slow
/// path and for a read again.
pub(crate) fn read_kept(
    mut reader: LogReader,
    open: impl Fn() -> Result<LogReader, ReaderError>,
    part: usize,
    budget: usize,
    latch: &Arc<Latch>,
) -> Result<Kept, ReaderError> {
    let mut index = Index::without_facts();
    let mut decider = Decider::with_budget(budget);
    let mut counters = ById::default();
    let mut lost = Regions::default();
    let mut finder = Finder::default();
    loop {
        let (u, more) = reader.refresh_some(part)?;
        let found = finder.apply(&u.events, u.state.durable);
        if !found.is_empty() {
            latch.note(&found, FoundBy::Scan);
        }
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
                    return read_whole(open()?, open, budget, latch);
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
    let recorded = reader.lost_ranges().to_vec();
    reader.release_bytes();
    latch.started(reader.state().durable);
    Ok(Kept {
        counters: defined(counters, &index),
        index,
        decider,
        local: Box::new(LogFacts::new(open()?, lost.clone(), latch.clone())),
        read_whole: false,
        passed: lost,
        recorded,
        watch: Watch::from_start(reader, finder),
    })
}

/// The log read whole into an index of every fact.
pub(crate) fn read_whole(
    mut reader: LogReader,
    open: impl Fn() -> Result<LogReader, ReaderError>,
    budget: usize,
    latch: &Arc<Latch>,
) -> Result<Kept, ReaderError> {
    let update = reader.refresh()?;
    let events = update.events;
    let mut finder = Finder::default();
    let found = finder.apply(&events, update.state.durable);
    if !found.is_empty() {
        latch.note(&found, FoundBy::Scan);
    }
    let mut index = Index::default();
    index.apply(&events, update.state.durable);
    let mut lost = Regions::default();
    for e in &events {
        if let ReadEvent::Corruption { file_id, region } = e {
            lost.insert(*file_id, region.start, region.end);
        }
    }
    drop(events);
    let mut counters = ById::default();
    for id in index.fact_series() {
        for f in index.series_facts(id) {
            count(&mut counters, &f.row);
        }
    }
    let decider = Decider::from_index(&index, budget);
    let recorded = reader.lost_ranges().to_vec();
    reader.release_bytes();
    latch.started(reader.state().durable);
    Ok(Kept {
        counters: defined(counters, &index),
        index,
        decider,
        local: Box::new(LogFacts::new(open()?, lost.clone(), latch.clone())),
        read_whole: true,
        passed: lost,
        recorded,
        watch: Watch::from_start(reader, finder),
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
            &Arc::new(Latch::default()),
        )
        .unwrap();
        let held = kept.decider.history.held();
        assert!(held.window <= 4 * 901, "{held:?}");
        assert!(held.window <= 4 * WINDOW_FACTS);
        assert_eq!((held.revised_keys, held.overlay), (0, 0));
        assert_eq!(kept.index.fact_series().count(), 0);
        assert_eq!(kept.index.registry().len(), 4);
        assert!(kept.counters.values().all(|c| c.recorded == 7_200));
        assert_eq!(kept.local.kept_bytes(), 0);
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
        // A batch to damage, and one after it.
        for (k, v) in [(3_100, 0.123_456_789), (3_200, 2.0)] {
            block_on(s.emit(&[Reading::new(
                SeriesId(1),
                Timestamp(T0 + k * S),
                Value::F64(v),
            )]))
            .unwrap();
        }
        // A power loss: the next start writes a recovery entry.
        fs.crash_after(0);
        drop(s);
        fs.power_loss();
        let s = block_on(Store::open(config())).unwrap();
        let last = Reading::new(SeriesId(1), Timestamp(T0 + 4_000 * S), Value::F64(1.0));
        block_on(s.emit(&[last])).unwrap();
        block_on(s.shutdown()).unwrap();
        let mark = 0.123_456_789f64.to_bits().to_le_bytes();
        let (path, bytes) = fs
            .paths()
            .into_iter()
            .filter(|p| p.starts_with(dir.join("log")))
            .filter_map(|p| fs.read(&p).map(|b| (p, b)))
            .find(|(_, b)| b.windows(8).any(|x| x == mark))
            .unwrap();
        let i = bytes.windows(8).position(|x| x == mark).unwrap();
        fs.poke(&path, i, &[bytes[i] ^ 0xFF]);
        let open = || log_reader(dir, &env);
        let (in_parts, at_once) = (Arc::new(Latch::default()), Arc::new(Latch::default()));
        let parts = read_kept(open().unwrap(), open, 100, usize::MAX, &in_parts).unwrap();
        let whole = read_whole(open().unwrap(), open, usize::MAX, &at_once).unwrap();
        assert_eq!(in_parts.found().len(), 1);
        assert_eq!(in_parts.found(), at_once.found());
        assert!(whole.read_whole && !parts.read_whole);
        assert_eq!(parts.counters, whole.counters);
        assert_eq!(parts.decider.history.held(), whole.decider.history.held());
        assert_eq!(parts.index.registry().len(), whole.index.registry().len());
        assert_eq!((parts.local.kept_bytes(), whole.local.kept_bytes()), (0, 0));
    }
}
