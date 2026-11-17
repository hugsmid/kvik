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
    let r = super::reader::DurableReader::open(Path::new(dir), &env, std::time::Duration::from_secs(60))
        .unwrap();
    let full = r
        .with(|i, log| Ok(i.registry_file(log.state().durable, file.max_series_id)))
        .unwrap();
    assert_eq!(file, full);
    assert_eq!(file.entries.len(), 5);
    assert!(read < 16 << 10, "{read} bytes of segments read at shutdown");
}
