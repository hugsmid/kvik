//! A store on an in-memory file system and a manual clock, and the tool run against it.

#![allow(dead_code)]

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use kvik::env::{DirEntry, Env, Fs, FsFile, ManualClock, MemFs, OpenMode};
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
        Fixture::with_seal_interval(None)
    }

    /// [`Fixture::new`] whose log seals by time after `seal` of the log's own clock, when given.
    pub fn with_seal_interval(seal: Option<Duration>) -> Fixture {
        let clock = ManualClock::new(Timestamp(T0));
        let mut env = Env::test(clock.clone());
        let fs = MemFs::new();
        env.fs = Arc::new(fs.clone());
        env.background_timers = false;
        let dir = PathBuf::from("store");
        let mut config = StoreConfig::new(&dir, "Etc/UTC")
            .create_if_missing(true)
            .env(env.clone());
        if let Some(d) = seal {
            config.log.seal_interval = d;
        }
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

    /// Opens the store again after [`Fixture::shut`].
    pub fn reopen(&mut self) {
        assert!(self.store.is_none());
        let config = StoreConfig::new(&self.dir, "Etc/UTC").env(self.env.clone());
        self.store = Some(block_on(Store::open(config)).unwrap());
    }

    /// Flips a byte of the eight bytes of `v` in every log segment holding them.
    pub fn damage(&self, v: f64) {
        let bytes = v.to_bits().to_le_bytes();
        let mut hit = false;
        for p in self.fs.paths() {
            if !p.extension().is_some_and(|e| e == "open" || e == "kvl") {
                continue;
            }
            let Some(b) = self.fs.read(&p) else { continue };
            if let Some(at) = b.windows(8).position(|x| x == bytes) {
                self.fs.poke(&p, at, &[b[at] ^ 0xFF]);
                hit = true;
            }
        }
        assert!(hit, "no segment holds {v}");
    }

    /// The store id from the descriptor.
    pub fn store_id(&self) -> [u8; 16] {
        kvik_log::store::read_descriptor(&self.fs, &self.dir)
            .unwrap()
            .store_id
    }

    /// The tool's output for `args`, split at spaces; `--store` is the fixture's.
    pub fn run(&self, args: &str) -> Result<String, kvik_cli::Error> {
        self.run_in(args, self.env.clone())
    }

    /// [`Fixture::run`] in `env`.
    pub fn run_in(&self, args: &str, env: Env) -> Result<String, kvik_cli::Error> {
        let (out, done) = self.run_both(args, env);
        done.map(|()| out)
    }

    /// What the tool wrote for `args` in `env`, and how it ended.
    pub fn run_both(&self, args: &str, env: Env) -> (String, Result<(), kvik_cli::Error>) {
        let v: Vec<String> = args.split_whitespace().map(String::from).collect();
        let mut out = Vec::new();
        let done = kvik_cli::run(&v, Some(self.dir.clone()), env, &mut out);
        (String::from_utf8(out).unwrap(), done)
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

/// A file system that records every call that could change a file or take a lock, with its path.
#[derive(Debug, Clone)]
pub struct Recording {
    inner: Arc<dyn Fs>,
    pub log: Arc<Mutex<Vec<(&'static str, PathBuf)>>>,
}

#[derive(Debug)]
struct RecordingFile {
    inner: Box<dyn FsFile>,
    path: PathBuf,
    log: Arc<Mutex<Vec<(&'static str, PathBuf)>>>,
}

impl Recording {
    pub fn new(inner: Arc<dyn Fs>) -> Self {
        Recording {
            inner,
            log: Arc::default(),
        }
    }

    fn note(&self, what: &'static str, p: &Path) {
        self.log.lock().unwrap().push((what, p.to_path_buf()));
    }
}

impl Fs for Recording {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FsFile>> {
        if mode != OpenMode::Read {
            self.note("open for writing", path);
        }
        let inner = self.inner.open(path, mode)?;
        Ok(Box::new(RecordingFile {
            inner,
            path: path.to_path_buf(),
            log: self.log.clone(),
        }))
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.note("create_dir_all", path);
        self.inner.create_dir_all(path)
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        self.inner.read_dir(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.note("rename from", from);
        self.note("rename to", to);
        self.inner.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.note("remove_file", path);
        self.inner.remove_file(path)
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.sync_dir(path)
    }
    fn free_space(&self, path: &Path) -> io::Result<u64> {
        self.inner.free_space(path)
    }
}

impl RecordingFile {
    fn note(&self, what: &'static str) {
        self.log.lock().unwrap().push((what, self.path.clone()));
    }
}

impl FsFile for RecordingFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.inner.read_at(buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.note("write_at");
        self.inner.write_at(buf, offset)
    }
    fn sync_data(&self) -> io::Result<()> {
        self.inner.sync_data()
    }
    fn sync_all(&self) -> io::Result<()> {
        self.inner.sync_all()
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.note("set_len");
        self.inner.set_len(len)
    }
    fn len(&self) -> io::Result<u64> {
        self.inner.len()
    }
    fn try_lock_exclusive(&self) -> io::Result<bool> {
        self.note("try_lock_exclusive");
        self.inner.try_lock_exclusive()
    }
    fn identity(&self) -> io::Result<Option<kvik_log::fs::FileIdentity>> {
        self.inner.identity()
    }
}
