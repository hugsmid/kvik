//! Tests that the shutdown's registry file and the store's series, losses and statistics come from held state, not
//! from a read of the whole log.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use kvik_log::fs::{DirEntry, Fs, FsFile, MemFs, OpenMode};

use super::tests::block_on;
use crate::env::{Env, ManualClock};
use crate::series::{Kind, SeriesSpec, ValueType};
use crate::time::Timestamp;
use crate::types::{Reading, SeriesId, Value};
use crate::{Store, StoreConfig};

const T0: i64 = 1_767_225_600_000_000_000;
const S: i64 = 1_000_000_000;

/// A file system that counts the bytes read from log segments.
#[derive(Debug)]
struct Counting {
    inner: Arc<MemFs>,
    segment_bytes: Arc<AtomicU64>,
}

#[derive(Debug)]
struct CountingFile {
    inner: Box<dyn FsFile>,
    segment: bool,
    bytes: Arc<AtomicU64>,
}

impl FsFile for CountingFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        let n = self.inner.read_at(buf, offset)?;
        if self.segment {
            self.bytes.fetch_add(n as u64, Ordering::Relaxed);
        }
        Ok(n)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> std::io::Result<()> {
        self.inner.write_at(buf, offset)
    }
    fn sync_data(&self) -> std::io::Result<()> {
        self.inner.sync_data()
    }
    fn sync_all(&self) -> std::io::Result<()> {
        self.inner.sync_all()
    }
    fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.inner.set_len(len)
    }
    fn len(&self) -> std::io::Result<u64> {
        self.inner.len()
    }
    fn try_lock_exclusive(&self) -> std::io::Result<bool> {
        self.inner.try_lock_exclusive()
    }
}

impl Fs for Counting {
    fn open(&self, path: &Path, mode: OpenMode) -> std::io::Result<Box<dyn FsFile>> {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        Ok(Box::new(CountingFile {
            inner: self.inner.open(path, mode)?,
            segment: kvik_log::name::SegmentName::parse(name).is_some(),
            bytes: self.segment_bytes.clone(),
        }))
    }
    fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        self.inner.create_dir_all(path)
    }
    fn read_dir(&self, path: &Path) -> std::io::Result<Vec<DirEntry>> {
        self.inner.read_dir(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.inner.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        self.inner.remove_file(path)
    }
    fn sync_dir(&self, path: &Path) -> std::io::Result<()> {
        self.inner.sync_dir(path)
    }
    fn free_space(&self, path: &Path) -> std::io::Result<u64> {
        self.inner.free_space(path)
    }
}

/// A store of four gauges with `calls` calls of 50 readings of each, and the count of segment bytes read.
fn store(dir: &str, calls: i64) -> (Store, Env, Arc<AtomicU64>) {
    let mut env = Env::test(ManualClock::new(Timestamp(T0 + calls * 50 * S)));
    let bytes = Arc::new(AtomicU64::new(0));
    env.fs = Arc::new(Counting {
        inner: Arc::new(MemFs::new()),
        segment_bytes: bytes.clone(),
    });
    let mut config = StoreConfig::new(dir, "Etc/UTC")
        .create_if_missing(true)
        .env(env.clone());
    config.log.min_sync_interval = std::time::Duration::ZERO;
    let s = block_on(Store::open(config)).unwrap();
    for i in 0..4 {
        let spec = SeriesSpec::new(
            format!("a/s{i}/v"),
            Kind::Gauge {
                value: ValueType::F64,
            },
            "W",
        );
        block_on(s.define_series(spec)).unwrap();
    }
    block_on(s.alias(SeriesId(2), "a/s1/w")).unwrap();
    for k in 0..calls {
        let rs: Vec<Reading> = (k * 50..(k + 1) * 50)
            .flat_map(|t| {
                (1..=4).map(move |id| {
                    Reading::new(SeriesId(id), Timestamp(T0 + t * S), Value::F64(t as f64))
                })
            })
            .collect();
        block_on(s.emit(&rs)).unwrap();
    }
    (s, env, bytes)
}

#[test]
fn a_clean_shutdown_writes_the_registry_file_a_full_read_gives_without_reading_the_log() {
    let dir = "held-registry";
    let (s, env, bytes) = store(dir, 40);
    bytes.store(0, Ordering::Relaxed);
    block_on(s.shutdown()).unwrap();
    let read = bytes.load(Ordering::Relaxed);
    let store_id = kvik_log::store::read_descriptor(&*env.fs, Path::new(dir))
        .unwrap()
        .store_id;
    let (_, file) = kvik_log::registry::read_latest(&*env.fs, Path::new(dir), &store_id)
        .unwrap()
        .unwrap();
    let r = super::reader::DurableReader::open(
        Path::new(dir),
        &env,
        std::time::Duration::from_secs(60),
    )
    .unwrap();
    let full = r
        .with(|i, log| Ok(i.registry_file(log.state().durable, file.max_series_id)))
        .unwrap();
    assert_eq!(file, full);
    assert_eq!(file.entries.len(), 5);
    assert!(read < 16 << 10, "{read} bytes of segments read at shutdown");
}

#[test]
fn series_losses_and_statistics_answer_as_a_full_read_does_without_reading_the_log() {
    let dir = "held-series";
    let (s, env, bytes) = store(dir, 40);
    block_on(s.retire(SeriesId(3), Timestamp(T0 + 10_000 * S))).unwrap();
    let r = super::reader::DurableReader::open(
        Path::new(dir),
        &env,
        std::time::Duration::from_secs(60),
    )
    .unwrap();
    bytes.store(0, Ordering::Relaxed);
    let mut held = Vec::new();
    for id in 1..=5 {
        held.push((
            s.series(SeriesId(id)).map_err(|e| e.to_string()),
            s.series_stats(SeriesId(id)).map(|st| st.latest_t),
        ));
    }
    let named = s.series("a/s1/w").unwrap();
    let losses = s.losses();
    let read = bytes.load(Ordering::Relaxed);
    assert!(read < 16 << 10, "{read} bytes of segments read");
    let full: Vec<_> = (1..=5)
        .map(|id| {
            let info = r
                .with(|i, _| i.series(&SeriesId(id).into()))
                .map_err(|e| e.to_string());
            let latest = r
                .with(|i, _| Ok(i.latest_t(id)))
                .ok()
                .and_then(|t| t.map(Timestamp));
            (info, (id <= 4).then_some(latest))
        })
        .collect();
    assert_eq!(held, full);
    assert_eq!(named.id, SeriesId(2));
    assert_eq!(losses, r.losses().unwrap());
    assert_eq!(
        held[0].0.as_ref().unwrap().latest_t,
        Some(Timestamp(T0 + 1_999 * S))
    );
    block_on(s.shutdown()).unwrap();
    assert_eq!(s.series(SeriesId(1)).unwrap(), full[0].0.clone().unwrap());
}

#[test]
fn summaries_read_in_two_passes_follow_a_recovery_entry_voiding_what_was_taken() {
    use kvik_log::content::Item;
    use kvik_log::entries::{Entry, MarkState, Recovery};
    use kvik_log::facts::{FactFlags, FactRow, Quality, Value as LogValue};
    use kvik_log::store::{NewStore, StoreDir};
    use kvik_log::writer::{Log, LogWriter, SystemClock, WriterConfig};

    let fs = Arc::new(MemFs::new());
    let new = NewStore {
        store_id: [1; 16],
        created_at: 0,
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: "test".into(),
    };
    let store = StoreDir::open(fs.clone(), Path::new("v"), Some(&new)).unwrap();
    let w = LogWriter::open(
        store,
        WriterConfig::default(),
        Some([2; 16]),
        std::time::Duration::ZERO,
        0,
    )
    .unwrap();
    let log = Log::start(w, Arc::new(SystemClock::default())).unwrap();
    let fact = |series_id: u32, t: i64| {
        Item::Fact(FactRow {
            series_id,
            t,
            quality: Quality::Good,
            flags: FactFlags::default(),
            value: LogValue::F64(1),
            changed_at: None,
        })
    };
    for k in 0..30 {
        log.submit(vec![fact(1, k), fact(2, 100 + k)])
            .unwrap()
            .wait()
            .unwrap();
    }
    let after = log.stats().durable.sequence;
    log.submit(vec![fact(1, 1_000), fact(3, 7)])
        .unwrap()
        .wait()
        .unwrap();
    let r = Recovery {
        after,
        stamp: None,
        boot_id: [0; 16],
        tail_bytes: 0,
        readable_commits: 0,
        mark_state: MarkState::Readable,
        id_floor: 0,
        consumers: Vec::new(),
    };
    log.submit(vec![Item::from(Entry::Recovery(r))])
        .unwrap()
        .wait()
        .unwrap();
    log.submit(vec![fact(1, 500)]).unwrap().wait().unwrap();
    log.shutdown().unwrap();
    let mut env = Env::test(ManualClock::new(Timestamp(T0)));
    env.fs = fs;
    let open = || super::start::log_reader(Path::new("v"), &env);
    let mut full = super::index::Index::default();
    full.apply(&open().unwrap().refresh().unwrap().events);
    let mut held = super::index::Index::without_facts();
    held.apply(&open().unwrap().refresh().unwrap().events);
    assert!(held.summary_stale());
    held.set_summary(super::index::read_summary(open, 7).unwrap());
    for id in 1..=3 {
        assert_eq!(held.latest_t(id), full.latest_t(id), "series {id}");
        assert_eq!(
            held.lateness_reference(id),
            full.lateness_reference(id),
            "series {id}"
        );
    }
    assert_eq!((held.latest_t(1), held.latest_t(3)), (Some(500), None));
}
