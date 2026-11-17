//! A reader call's cost and its cuts. The cost against the age of the store (query design §3.1): the file-system
//! calls, directory entries listed and file writes of one call, counted through the `Fs` seam, on stores with 10 and
//! with 1,000 sealed segments. The lease rules of `spec.md` §7 Readers: the lease names the lowest epoch of the live
//! cuts, a cut ends after `cut_max_age`, and a lease found gone or changed ends every cut.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kvik_log::content::Item;
use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
use kvik_log::fs::{DirEntry, Fs, FsFile, MemFs, OpenMode};
use kvik_log::lease::{CutEnd, LEASE_REFRESH, LeaseSlot, lease_name};
use kvik_log::manifest::{RECORD_EMPTY, encode_record, read_manifest};
use kvik_log::reader::{LogReader, ReaderConfig};
use kvik_log::store::{NewStore, StoreDir};
use kvik_log::writer::{
    Call, CallSource as _, Clock, IngestQueue, LogWriter, MarkSync, WriterConfig, create_segment,
    seal_segment,
};

const STORE_ID: [u8; 16] = [6; 16];
const T0: i64 = 1_767_225_600_000_000_000;
const MS: Duration = Duration::from_millis(1);

#[derive(Debug, Default)]
struct ManualClock(Mutex<Duration>);

impl ManualClock {
    fn advance(&self, d: Duration) {
        *self.0.lock().unwrap() += d;
    }
}

impl Clock for ManualClock {
    fn monotonic(&self) -> Duration {
        *self.0.lock().unwrap()
    }
    fn wall(&self) -> i64 {
        T0 + self.monotonic().as_nanos() as i64
    }
}

/// What one call did through the seam.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Cost {
    /// Every call of `Fs` and `FsFile`.
    calls: u64,
    /// Directory entries returned by listings.
    listed: u64,
    /// Writes: `write_at`, `set_len`, file creation, rename and removal.
    writes: u64,
}

#[derive(Debug, Default)]
struct Counters {
    calls: AtomicU64,
    listed: AtomicU64,
    writes: AtomicU64,
    /// Bytes read from the manifest.
    manifest: AtomicU64,
}

impl Counters {
    fn take(&self) -> Cost {
        Cost {
            calls: self.calls.swap(0, Ordering::SeqCst),
            listed: self.listed.swap(0, Ordering::SeqCst),
            writes: self.writes.swap(0, Ordering::SeqCst),
        }
    }
    fn call(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
    fn write(&self) {
        self.writes.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Debug)]
struct CountFs {
    inner: MemFs,
    n: Arc<Counters>,
    /// Opens of a path containing this are not found.
    refuse: Arc<Mutex<Option<String>>>,
}

#[derive(Debug)]
struct CountFile {
    inner: Box<dyn FsFile>,
    n: Arc<Counters>,
    manifest: bool,
}

impl FsFile for CountFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        self.n.call();
        let got = self.inner.read_at(buf, offset)?;
        if self.manifest {
            self.n.manifest.fetch_add(got as u64, Ordering::SeqCst);
        }
        Ok(got)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> std::io::Result<()> {
        self.n.call();
        self.n.write();
        self.inner.write_at(buf, offset)
    }
    fn sync_data(&self) -> std::io::Result<()> {
        self.n.call();
        self.inner.sync_data()
    }
    fn sync_all(&self) -> std::io::Result<()> {
        self.n.call();
        self.inner.sync_all()
    }
    fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.n.call();
        self.n.write();
        self.inner.set_len(len)
    }
    fn len(&self) -> std::io::Result<u64> {
        self.n.call();
        self.inner.len()
    }
    fn identity(&self) -> std::io::Result<Option<kvik_log::fs::FileIdentity>> {
        self.n.call();
        self.inner.identity()
    }
    fn try_lock_exclusive(&self) -> std::io::Result<bool> {
        self.n.call();
        self.inner.try_lock_exclusive()
    }
}

impl Fs for CountFs {
    fn open(&self, path: &Path, mode: OpenMode) -> std::io::Result<Box<dyn FsFile>> {
        self.n.call();
        if let Some(r) = self.refuse.lock().unwrap().as_deref()
            && path.to_string_lossy().contains(r)
        {
            return Err(std::io::ErrorKind::NotFound.into());
        }
        if mode == OpenMode::CreateNew {
            self.n.write();
        }
        Ok(Box::new(CountFile {
            inner: self.inner.open(path, mode)?,
            n: self.n.clone(),
            manifest: path.ends_with("MANIFEST"),
        }))
    }
    fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        self.n.call();
        self.inner.create_dir_all(path)
    }
    fn read_dir(&self, path: &Path) -> std::io::Result<Vec<DirEntry>> {
        self.n.call();
        let v = self.inner.read_dir(path)?;
        self.n.listed.fetch_add(v.len() as u64, Ordering::SeqCst);
        Ok(v)
    }
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.n.call();
        self.n.write();
        self.inner.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        self.n.call();
        self.n.write();
        self.inner.remove_file(path)
    }
    fn sync_dir(&self, path: &Path) -> std::io::Result<()> {
        self.n.call();
        self.inner.sync_dir(path)
    }
    fn free_space(&self, path: &Path) -> std::io::Result<u64> {
        self.n.call();
        self.inner.free_space(path)
    }
}

fn dir() -> PathBuf {
    PathBuf::from("store")
}

fn cfg() -> WriterConfig {
    WriterConfig {
        segment_size: 4096,
        max_batch_bytes: 2 << 10,
        seal_interval: 10 * MS,
        disk_critical: 0,
        disk_low_bytes: 0,
        relaxed_limits: true,
        mark_sync: MarkSync::Interval(Duration::from_millis(1)),
        ..WriterConfig::default()
    }
}

fn facts(n: usize, base: i64) -> Vec<Item> {
    (0..n)
        .map(|k| {
            Item::Fact(FactRow {
                series_id: 1,
                t: T0 + base + k as i64,
                quality: Quality::Good,
                flags: FactFlags::default(),
                value: Value::F64(k as u64),
                changed_at: None,
            })
        })
        .collect()
}

/// A writer driven by hand and a reader through the counting seam.
struct Rig {
    fs: MemFs,
    clock: Arc<ManualClock>,
    w: LogWriter,
    q: IngestQueue,
    reader: LogReader,
    n: Arc<Counters>,
    refuse: Arc<Mutex<Option<String>>>,
}

impl Rig {
    fn new() -> Self {
        let fs = MemFs::new();
        let clock = Arc::new(ManualClock::default());
        let new = NewStore {
            store_id: STORE_ID,
            created_at: T0,
            site_zone: "Etc/UTC".into(),
            tzdata: "2026a".into(),
            software: "kvik test".into(),
        };
        let store = StoreDir::open(Arc::new(fs.clone()), &dir(), Some(&new)).unwrap();
        let w =
            LogWriter::open(store, cfg(), Some([1; 16]), clock.monotonic(), clock.wall()).unwrap();
        let n = Arc::new(Counters::default());
        let refuse = Arc::new(Mutex::new(None));
        let seam = Arc::new(CountFs {
            inner: fs.clone(),
            n: n.clone(),
            refuse: refuse.clone(),
        });
        let reader = LogReader::open(ReaderConfig::new(
            dir(),
            seam,
            clock.clone(),
            Some([1; 16]),
            [9; 16],
        ))
        .unwrap();
        let mut r = Rig {
            fs,
            clock,
            q: IngestQueue::new(cfg().ingest_queue),
            w,
            reader,
            n,
            refuse,
        };
        r.maint();
        r
    }

    fn maint(&mut self) {
        let (now, wall) = (self.clock.monotonic(), self.clock.wall());
        for _ in 0..2 {
            if let Some(req) = self.w.segment_request(wall) {
                let res = create_segment(&self.fs, &req, &self.w.halt());
                self.w.segment_created(res);
            }
            for job in self.w.take_seal_jobs(now) {
                let res = seal_segment(&self.fs, &job, &self.w.halt());
                self.w.seal_done(job, res, now);
            }
        }
    }

    fn submit(&mut self, items: Vec<Item>) {
        let call = Call::new(items, cfg().call_limit()).unwrap();
        self.q.push(call).unwrap();
        for _ in 0..100 {
            if self.q.is_empty() {
                break;
            }
            let (now, wall) = (self.clock.monotonic(), self.clock.wall());
            self.w.step(now, wall, &mut self.q);
            self.maint();
            self.clock.advance(MS);
        }
        for _ in 0..3 {
            let (now, wall) = (self.clock.monotonic(), self.clock.wall());
            self.w.step(now, wall, &mut self.q);
            self.maint();
            self.clock.advance(MS);
        }
    }

    fn sealed(&self) -> usize {
        self.fs
            .paths()
            .iter()
            .filter(|p| p.to_string_lossy().ends_with(".kvl"))
            .count()
    }

    /// A store with at least `sealed` sealed segments, one batch each, and the reader caught up.
    fn with_sealed(sealed: usize) -> Self {
        let mut r = Rig::new();
        let mut k = 0;
        while r.sealed() < sealed {
            r.submit(facts(4, k * 100));
            r.clock.advance(20 * MS);
            r.maint();
            k += 1;
        }
        r.reader.refresh().unwrap();
        r.reader.refresh().unwrap();
        r
    }

    /// The cost of a call with nothing new, of one after a batch was appended, of one after a seal and a batch in
    /// the next segment, and of one once half of `lease_refresh` has passed.
    fn per_call(&mut self) -> [Cost; 4] {
        self.n.take();
        let u = self.reader.refresh().unwrap();
        assert!(u.events.is_empty());
        let idle = self.n.take();
        self.submit(facts(4, 1 << 40));
        self.n.take();
        let u = self.reader.refresh().unwrap();
        assert_eq!(u.events.len(), 1, "one new batch");
        let new = self.n.take();
        let before = self.sealed();
        self.clock.advance(20 * MS);
        self.maint();
        self.submit(facts(4, 1 << 41));
        assert_eq!(self.sealed(), before + 1);
        self.n.take();
        let u = self.reader.refresh().unwrap();
        assert_eq!(u.events.len(), 1, "one new batch in the next segment");
        let rolled = self.n.take();
        self.clock.advance(LEASE_REFRESH / 2);
        self.reader.refresh().unwrap();
        let renewed = self.n.take();
        [idle, new, rolled, renewed]
    }

    fn lease(&self) -> LeaseSlot {
        let b = self.fs.read(&lease_path()).expect("the lease");
        LeaseSlot::decode_file(&b, &STORE_ID).expect("a valid lease")
    }

    /// Appends an empty record with the next epoch, as a publish would; returns that epoch.
    fn next_epoch(&self) -> u64 {
        let path = dir().join("MANIFEST");
        let man = self.fs.read(&path).unwrap();
        let epoch = read_manifest(&man, &STORE_ID).unwrap().last_epoch() + 1;
        self.fs.poke(
            &path,
            man.len(),
            &encode_record(RECORD_EMPTY, epoch, 0, T0, None),
        );
        epoch
    }

    /// The oldest epoch a live lease names, as the writer sees the lease files after a look.
    fn writer_view(&mut self) -> Option<u64> {
        self.clock.advance(Duration::from_secs(61));
        let (now, wall) = (self.clock.monotonic(), self.clock.wall());
        self.w.step(now, wall, &mut self.q);
        self.w.leases().oldest_live_epoch(now)
    }
}

fn lease_path() -> PathBuf {
    dir().join("leases").join(lease_name(&[9; 16]))
}

#[test]
fn a_calls_file_system_calls_and_writes_do_not_grow_with_the_sealed_segments() {
    let (mut small, mut large) = (Rig::with_sealed(10), Rig::with_sealed(1_000));
    let at_10 = small.per_call();
    let at_1000 = large.per_call();
    for (what, (a, b)) in ["idle", "one new batch", "after a seal", "lease due"]
        .iter()
        .zip(at_10.iter().zip(&at_1000))
    {
        eprintln!("{what}: {a:?} at 10, {b:?} at 1,000 sealed segments");
    }
    assert_eq!(at_10, at_1000);
    for c in &at_10 {
        assert_eq!(c.listed, 0, "no listing per call: {c:?}");
    }
    let [idle, new, rolled, renewed] = at_10;
    for c in [idle, new, rolled] {
        assert_eq!(c.writes, 0, "no lease write per call: {c:?}");
    }
    assert_eq!(renewed.writes, 1, "the lease at half of lease_refresh");
}

#[test]
fn a_caught_up_reader_that_cannot_open_a_listed_segment_by_name_stops_there_as_a_first_pass_does() {
    let mut r = Rig::with_sealed(2);
    let next = r.reader.state().durable;
    r.clock.advance(20 * MS);
    r.maint();
    r.submit(facts(4, 1 << 41));
    let mark_segment =
        r.fs.paths()
            .into_iter()
            .filter(|p| p.to_string_lossy().ends_with(".open"))
            .min()
            .unwrap();
    let name = mark_segment
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let stem = name.trim_end_matches(".open").to_string();
    *r.refuse.lock().unwrap() = Some(stem.clone());
    let refused = r.reader.refresh();
    assert!(
        matches!(
            refused,
            Err(kvik_log::reader::ReaderError::SegmentMissing { .. })
        ),
        "{refused:?}"
    );
    assert_eq!(r.reader.state().durable, next);
    *r.refuse.lock().unwrap() = None;
    let u = r.reader.refresh().unwrap();
    assert_eq!(u.events.len(), 1);
}

#[test]
fn an_unchanged_manifest_is_read_at_its_last_record_a_longer_one_past_it_and_a_replaced_one_whole()
{
    let mut r = Rig::with_sealed(2);
    let path = dir().join("MANIFEST");
    let man = r.fs.read(&path).unwrap();
    let len = man.len();
    let last = read_manifest(&man, &STORE_ID)
        .unwrap()
        .records
        .pop()
        .unwrap();
    let last_len = last.end - last.offset;
    r.n.manifest.swap(0, Ordering::SeqCst);
    r.reader.refresh().unwrap();
    assert_eq!(r.n.manifest.swap(0, Ordering::SeqCst), last_len);
    let epoch = r.next_epoch();
    let added = (r.fs.read(&path).unwrap().len() - len) as u64;
    r.reader.refresh().unwrap();
    assert_eq!(r.reader.state().epoch, epoch);
    assert_eq!(r.n.manifest.swap(0, Ordering::SeqCst), last_len + added);
    // A manifest rebuilt and renamed into place: read again from its start.
    let man = r.fs.read(&path).unwrap();
    let tmp = dir().join("MANIFEST.tmp");
    let f = r.fs.open(&tmp, OpenMode::CreateNew).unwrap();
    f.write_at(&man[..len], 0).unwrap();
    let rebuilt = encode_record(RECORD_EMPTY, epoch + 5, 0, T0, None);
    f.write_at(&rebuilt, len as u64).unwrap();
    r.fs.rename(&tmp, &path).unwrap();
    r.reader.refresh().unwrap();
    assert_eq!(r.reader.state().epoch, epoch + 5);
    assert!(r.n.manifest.swap(0, Ordering::SeqCst) >= (len + rebuilt.len()) as u64);
}

#[test]
fn the_lease_names_the_lowest_epoch_of_the_live_cuts_until_the_last_is_released() {
    let mut r = Rig::with_sealed(2);
    let held = r.reader.hold();
    let first = held.epoch;
    assert_eq!(r.lease().epoch, first);
    let later = r.next_epoch();
    r.reader.refresh().unwrap();
    let second = r.reader.hold();
    assert_eq!(second.epoch, later);
    r.clock.advance(LEASE_REFRESH / 2);
    r.reader.refresh().unwrap();
    assert_eq!(
        r.lease().epoch,
        first,
        "never a later epoch while a cut is live"
    );
    // What a retirement after the first epoch would need: the writer keeps it.
    assert_eq!(r.writer_view(), Some(first));
    r.reader.release(held.id);
    r.reader.refresh().unwrap();
    assert_eq!(
        r.lease().epoch,
        later,
        "written at once when its epoch changes"
    );
    assert_eq!(r.writer_view(), Some(later));
    assert_eq!(r.reader.check_cut(second.id), Ok(()));
}

#[test]
fn a_cut_held_for_cut_max_age_ends_and_no_longer_holds_the_lease_down() {
    let mut r = Rig::with_sealed(2);
    let held = r.reader.hold();
    let later = r.next_epoch();
    r.clock.advance(LEASE_REFRESH - MS);
    r.reader.refresh().unwrap();
    assert_eq!(r.reader.check_cut(held.id), Ok(()));
    assert_eq!(r.lease().epoch, held.epoch);
    r.clock.advance(MS);
    assert_eq!(r.reader.check_cut(held.id), Err(CutEnd::Expired));
    r.reader.refresh().unwrap();
    assert_eq!(r.lease().epoch, later);
    r.reader.release(held.id);
    assert_eq!(r.reader.check_cut(held.id), Err(CutEnd::Unknown));
    assert_eq!(r.reader.cuts_held(), 0);
}

#[test]
fn a_cut_max_age_above_the_grace_period_is_taken_as_the_grace_period() {
    let mut r = Rig::new();
    let mut c = ReaderConfig::new(
        dir(),
        Arc::new(r.fs.clone()),
        r.clock.clone(),
        Some([1; 16]),
        [8; 16],
    );
    c.cut_max_age = Duration::from_secs(4 * 3_600);
    let mut reader = LogReader::open(c).unwrap();
    let held = reader.hold();
    r.clock.advance(Duration::from_secs(3_600) - MS);
    reader.keep_alive().unwrap();
    assert_eq!(reader.check_cut(held.id), Ok(()));
    r.clock.advance(MS);
    assert_eq!(reader.check_cut(held.id), Err(CutEnd::Expired));
    r.reader.refresh().unwrap();
}

#[test]
fn a_lease_found_gone_ends_every_cut_and_is_written_again_naming_the_current_epoch() {
    let mut r = Rig::with_sealed(2);
    let (a, b) = (r.reader.hold(), r.reader.hold());
    let later = r.next_epoch();
    r.fs.remove_file(&lease_path()).unwrap();
    r.clock.advance(LEASE_REFRESH / 2);
    r.reader.refresh().unwrap();
    assert_eq!(r.reader.check_cut(a.id), Err(CutEnd::LeaseLost));
    assert_eq!(r.reader.check_cut(b.id), Err(CutEnd::LeaseLost));
    assert_eq!(r.lease().epoch, later);
    let c = r.reader.hold();
    assert_eq!(r.reader.check_cut(c.id), Ok(()));
}

#[test]
fn a_lease_found_changed_ends_every_cut() {
    let mut r = Rig::with_sealed(2);
    let a = r.reader.hold();
    let mut other = r.lease();
    other.refresh_counter += 7;
    r.fs.poke(&lease_path(), 64, &other.encode());
    assert!(r.reader.keep_alive().is_ok_and(|w| !w), "not due");
    r.clock.advance(LEASE_REFRESH / 2);
    assert!(r.reader.keep_alive().unwrap());
    assert_eq!(r.reader.check_cut(a.id), Err(CutEnd::LeaseLost));
    // The lease as this reader wrote it is not a change.
    let b = r.reader.hold();
    r.clock.advance(LEASE_REFRESH / 2);
    assert!(r.reader.keep_alive().unwrap());
    assert_eq!(r.reader.check_cut(b.id), Ok(()));
}
