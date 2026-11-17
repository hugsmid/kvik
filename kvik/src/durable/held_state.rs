//! Tests that the shutdown's registry file and the store's series, losses and statistics come from held state, not
//! from a read of the whole log.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kvik_log::fs::{DirEntry, Fs, FsFile, MemFs, OpenMode};

use super::tests::block_on;
use crate::env::{Env, ManualClock};
use crate::series::{Kind, SeriesSpec, ValueType};
use crate::time::Timestamp;
use crate::types::{Reading, SeriesId, Value};
use crate::{Store, StoreConfig};

const T0: i64 = 1_767_225_600_000_000_000;
const S: i64 = 1_000_000_000;

/// What reads of log segments do: the bytes read, and whether they fail.
#[derive(Debug, Default)]
struct Io {
    bytes: AtomicU64,
    fail: AtomicBool,
}

impl Io {
    /// The bytes read since the last call.
    fn take(&self) -> u64 {
        self.bytes.swap(0, Ordering::Relaxed)
    }
}

/// A file system that counts, or fails, the reads of log segments.
#[derive(Debug)]
struct Counting {
    inner: Arc<MemFs>,
    io: Arc<Io>,
}

#[derive(Debug)]
struct CountingFile {
    inner: Box<dyn FsFile>,
    segment: bool,
    io: Arc<Io>,
}

impl FsFile for CountingFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        if self.segment && self.io.fail.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("a failing read"));
        }
        let n = self.inner.read_at(buf, offset)?;
        if self.segment {
            self.io.bytes.fetch_add(n as u64, Ordering::Relaxed);
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
            io: self.io.clone(),
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

/// A store of four gauges with `calls` calls of 50 readings of each, and its segment reads.
fn store(dir: &str, calls: i64) -> (Store, Env, Arc<Io>) {
    let mut env = Env::test(ManualClock::new(Timestamp(T0 + calls * 50 * S)));
    let io = Arc::new(Io::default());
    env.fs = Arc::new(Counting {
        inner: Arc::new(MemFs::new()),
        io: io.clone(),
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
    (s, env, io)
}

#[test]
fn a_clean_shutdown_writes_the_registry_file_a_full_read_gives_without_reading_the_log() {
    let dir = "held-registry";
    let (s, env, io) = store(dir, 40);
    io.take();
    block_on(s.shutdown()).unwrap();
    let read = io.take();
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
    let (s, env, io) = store(dir, 40);
    block_on(s.retire(SeriesId(3), Timestamp(T0 + 10_000 * S))).unwrap();
    let r = super::reader::DurableReader::open(
        Path::new(dir),
        &env,
        std::time::Duration::from_secs(60),
    )
    .unwrap();
    io.take();
    let mut held = Vec::new();
    for id in 1..=5 {
        held.push((
            s.series(SeriesId(id)).map_err(|e| e.to_string()),
            s.series_stats(SeriesId(id)).map(|st| st.latest_t),
        ));
    }
    let named = s.series("a/s1/w").unwrap();
    let losses = s.losses();
    let read = io.take();
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

#[test]
fn series_returns_the_read_error_while_the_log_cannot_be_read() {
    let (s, _env, io) = store("held-failing", 2);
    let before = s.series(SeriesId(1)).unwrap().latest_t;
    io.fail.store(true, Ordering::SeqCst);
    let late = Reading::new(SeriesId(1), Timestamp(T0 + 5_000 * S), Value::F64(1.0));
    block_on(s.emit(&[late])).unwrap();
    assert!(s.series(SeriesId(1)).is_err());
    io.fail.store(false, Ordering::SeqCst);
    let after = s.series(SeriesId(1)).unwrap().latest_t;
    assert_eq!(
        (before, after),
        (
            Some(Timestamp(T0 + 99 * S)),
            Some(Timestamp(T0 + 5_000 * S))
        )
    );
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_store_whose_load_meets_a_void_in_a_later_part_answers_as_a_full_read() {
    use kvik_log::content::Item;
    use kvik_log::entries::{Entry, MarkState, Recovery};
    use kvik_log::facts::{FactFlags, FactRow, Quality, Value as LogValue};
    use kvik_log::store::StoreDir;
    use kvik_log::writer::{Log, LogWriter, SystemClock, WriterConfig};

    let dir = "held-void-parts";
    let (s, env, io) = store(dir, 3);
    block_on(s.shutdown()).unwrap();
    drop(s);
    let sd = StoreDir::open(env.fs.clone(), Path::new(dir), None).unwrap();
    let w = LogWriter::open(
        sd,
        WriterConfig::default(),
        Some([2; 16]),
        std::time::Duration::ZERO,
        0,
    )
    .unwrap();
    let log = Log::start(w, Arc::new(SystemClock::default())).unwrap();
    let fact = |series_id: u32, t: i64, future: bool| {
        Item::Fact(FactRow {
            series_id,
            t: T0 + t * S,
            quality: Quality::Good,
            flags: FactFlags {
                future,
                ..FactFlags::default()
            },
            value: LogValue::F64(1),
            changed_at: None,
        })
    };
    log.submit(vec![fact(1, 500, false)])
        .unwrap()
        .wait()
        .unwrap();
    let after = log.stats().durable.sequence;
    // Voided: the latest facts of series 1 and 2.
    log.submit(vec![fact(1, 9_000, false), fact(2, 8_000, false)])
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
    log.submit(vec![fact(1, 700, true), fact(3, 600, false)])
        .unwrap()
        .wait()
        .unwrap();
    log.shutdown().unwrap();
    // Parts of two items: the void arrives after the batches it removes were taken.
    super::deliver::LOAD_PARTS
        .lock()
        .unwrap()
        .push((Path::new(dir).to_path_buf(), 2));
    let config = StoreConfig::new(dir, "Etc/UTC").env(env.clone());
    let s = block_on(Store::open(config)).unwrap();
    let r = super::reader::DurableReader::open(
        Path::new(dir),
        &env,
        std::time::Duration::from_secs(60),
    )
    .unwrap();
    for _ in 0..2 {
        for id in 1..=4 {
            let held = s.series(SeriesId(id)).map_err(|e| e.to_string());
            let full = r
                .with(|i, _| i.series(&SeriesId(id).into()))
                .map_err(|e| e.to_string());
            assert_eq!(held, full, "series {id}");
            let latest = s.series_stats(SeriesId(id)).map(|st| st.latest_t);
            assert_eq!(latest, full.ok().map(|i| i.latest_t), "series {id}");
        }
    }
    let one = s.series(SeriesId(1)).unwrap();
    assert_eq!(one.latest_t, Some(Timestamp(T0 + 700 * S)));
    assert_eq!(one.lateness_reference, Some(Timestamp(T0 + 500 * S)));
    assert_eq!(s.losses(), r.losses().unwrap());
    io.take();
    block_on(s.emit(&[Reading::new(
        SeriesId(2),
        Timestamp(T0 + 650 * S),
        Value::F64(2.0),
    )]))
    .unwrap();
    assert_eq!(
        s.series(SeriesId(2)).unwrap().latest_t,
        Some(Timestamp(T0 + 650 * S))
    );
    assert!(io.take() < 16 << 10, "the summaries are rebuilt once");
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_routing_change_writes_the_registry_file_a_full_read_gives_without_reading_the_log() {
    use crate::consume::{Funnel, Select, Start};
    let dir = "held-route";
    let (s, env, io) = store(dir, 40);
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    io.take();
    block_on(c.route(Funnel::facts(Select::All))).unwrap();
    let read = io.take();
    assert!(read < 16 << 10, "{read} bytes of segments read by a route");
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
        .with(
            |i, _| Ok(i.registry_file(log_pos(file.generation, file.through), file.max_series_id)),
        )
        .unwrap();
    assert_eq!(file, full);
    assert!(
        file.entries
            .iter()
            .any(|(_, _, e)| matches!(e, kvik_log::entries::Entry::Routing(_)))
    );
    drop(c);
    block_on(s.shutdown()).unwrap();
}

fn log_pos(generation: u32, sequence: u64) -> kvik_log::writer::Position {
    kvik_log::writer::Position {
        generation,
        sequence,
    }
}

#[test]
fn losses_after_a_shutdown_are_those_before_it() {
    let dir = "held-losses";
    let mut env = Env::test(ManualClock::new(Timestamp(T0 + 1_000 * S)));
    let mem = Arc::new(MemFs::new());
    let io = Arc::new(Io::default());
    env.fs = Arc::new(Counting {
        inner: mem.clone(),
        io: io.clone(),
    });
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
    block_on(s.define_series(SeriesSpec::new("a/b/lost", Kind::Text, ""))).unwrap();
    for k in 0..3 {
        let r = Reading::new(SeriesId(1), Timestamp(T0 + k * S), Value::F64(1.0));
        block_on(s.emit(&[r])).unwrap();
    }
    block_on(s.shutdown()).unwrap();
    drop(s);
    // Synced bytes of the second definition's batch lost on the disk: a lost range in the manifest.
    let log_dir = Path::new(dir).join(kvik_log::store::LOG_DIR);
    let (path, at) = mem
        .read_dir(&log_dir)
        .unwrap()
        .into_iter()
        .map(|e| log_dir.join(e.name))
        .find_map(|p| {
            let b = mem.read(&p)?;
            let at = b.windows(8).position(|x| x == b"a/b/lost")?;
            Some((p, at))
        })
        .unwrap();
    mem.poke(&path, at, b"X");
    let s = block_on(Store::open(config())).unwrap();
    let before = s.losses();
    assert!(before.iter().any(|l| l.file_id.is_some()), "{before:?}");
    block_on(s.shutdown()).unwrap();
    io.take();
    assert_eq!(s.losses(), before);
    assert_eq!(io.take(), 0);
}
