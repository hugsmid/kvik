//! Derived files under losses recorded after they were written and under failures that back off (`format.md` A.18).
mod dirs_rig;

use dirs_rig::*;
use kvik_log::manifest::{LossCause, LostRange, RECORD_LOST_RANGE};

fn loss(identity: u64, first: u64, last: u64, extends: bool) -> LostRange {
    LostRange {
        identity: (0, identity),
        first: (0, first),
        last: (0, last),
        file_id: 1,
        offset: 0,
        length: 0,
        cause: LossCause::Corruption,
        boot_id: [0; 16],
        id_floor: 0,
        time_before: None,
        time_after: None,
        extends,
        stamp: None,
    }
}

fn record_loss(r: &Rig, l: &LostRange) {
    let path = dir().join("MANIFEST");
    let mut bytes = r.fs.read(&path).unwrap();
    let m = read_manifest(&bytes, &STORE_ID).unwrap();
    bytes.truncate(m.valid_end as usize);
    bytes.extend_from_slice(&encode_record(
        RECORD_LOST_RANGE,
        m.last_epoch() + 1,
        0,
        T0,
        Some(l),
    ));
    rewrite(r, &path, &bytes);
}

#[test]
fn a_loss_extended_under_its_identity_below_the_series_directory_drops_it() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 300, 0);
    r.settle();
    let (f, s) = r.series().unwrap();
    let start = r.w.as_ref().unwrap().directory_start();
    let at = s.through.sequence - 3;
    record_loss(&r, &loss(at, at, at, false));
    assert!(
        kept_series(&r.fs, &dir(), &STORE_ID, &start).is_none_or(|(_, k)| k.through.sequence < at)
    );
    // Written with that loss known: one position lost at or below `through`.
    let known = SeriesDirectory {
        lost: 1,
        ..s.clone()
    };
    let frame = Frame {
        file_id: f.file_id + 1,
        ..f
    };
    directory::write_file(
        &r.fs,
        &dir(),
        Family::Series,
        frame.file_id,
        &known.encode(frame, STORE_ID).unwrap(),
    )
    .unwrap();
    let kept = kept_series(&r.fs, &dir(), &STORE_ID, &start).expect("kept with the loss known");
    assert_eq!(kept.1.through, s.through);
    // The loss grows downward by twenty positions under the same identity.
    record_loss(&r, &loss(at, at - 20, at, true));
    let kept = kept_series(&r.fs, &dir(), &STORE_ID, &start);
    assert!(
        kept.as_ref()
            .is_none_or(|(_, k)| k.through.sequence < at - 20),
        "kept through {:?} although positions {}..={} below it are now lost",
        kept.map(|k| k.1.through.sequence),
        at - 20,
        at
    );
}

#[test]
fn a_loss_that_grows_only_above_the_series_directory_keeps_it() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 300, 0);
    r.settle();
    let (f, s) = r.series().unwrap();
    let start = r.w.as_ref().unwrap().directory_start();
    let at = s.through.sequence;
    record_loss(&r, &loss(at, at, at, false));
    let known = SeriesDirectory {
        lost: 1,
        ..s.clone()
    };
    let frame = Frame {
        file_id: f.file_id + 1,
        ..f
    };
    directory::write_file(
        &r.fs,
        &dir(),
        Family::Series,
        frame.file_id,
        &known.encode(frame, STORE_ID).unwrap(),
    )
    .unwrap();
    record_loss(&r, &loss(at, at, at + 50, true));
    let kept =
        kept_series(&r.fs, &dir(), &STORE_ID, &start).expect("no position below it is newly lost");
    assert_eq!(kept.1.through, s.through);
}

struct NoSeries;

impl SealHook for NoSeries {
    fn series(&self, _: Position, _: i64) -> Option<SeriesEntries> {
        None
    }
    fn status(&self) -> Option<StatusContent> {
        Some(StatusContent {
            health: 0,
            pressure: false,
            reasons: Vec::new(),
            counters: Vec::new(),
        })
    }
}

#[test]
fn a_series_hook_that_keeps_failing_does_not_starve_the_status_file() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 300, 0);
    r.settle();
    let synced = r.w.as_ref().unwrap().stats().mark_synced;
    let mut start = r.w.as_ref().unwrap().directory_start();
    start.series_bytes = 1;
    for p in r.derived() {
        r.fs.remove_file(&p).unwrap();
    }
    let mut d = Directories::new(
        Arc::new(r.fs.clone()),
        &dir(),
        STORE_ID,
        start,
        DirectoryOptions {
            hook: Some(Arc::new(NoSeries)),
            status_interval: Some(Duration::from_secs(60)),
        },
    );
    let status = |r: &Rig| {
        read_latest(&r.fs, &dir(), Family::Status, |b| {
            StatusFile::decode(b, &STORE_ID)
        })
        .unwrap()
        .map(|(f, _)| f.file_id)
    };
    let begin = Duration::from_secs(1_000);
    let (mut now, mut wall) = (begin, T0);
    let mut last = None;
    let mut writes = 0;
    // The maintenance thread's loop: run, then wait as long as asked, at most a second.
    while now <= begin + Duration::from_secs(600) {
        let after = d.run(synced, now, wall).unwrap_or(Duration::from_secs(1));
        let s = status(&r);
        if s != last {
            writes += 1;
            last = s;
        }
        let step = after.clamp(Duration::from_millis(1), Duration::from_secs(1));
        now += step;
        wall += i64::try_from(step.as_nanos()).unwrap();
    }
    assert!(
        writes >= 9,
        "the status file was written {writes} times in 600 s at a 60 s interval"
    );
    assert!(r.load().is_some(), "the root is written all the same");
}

#[derive(Debug)]
struct NoDerived(MemFs);

impl Fs for NoDerived {
    fn open(
        &self,
        path: &std::path::Path,
        mode: kvik_log::fs::OpenMode,
    ) -> std::io::Result<Box<dyn kvik_log::fs::FsFile>> {
        self.0.open(path, mode)
    }
    fn create_dir_all(&self, path: &std::path::Path) -> std::io::Result<()> {
        self.0.create_dir_all(path)
    }
    fn read_dir(&self, path: &std::path::Path) -> std::io::Result<Vec<kvik_log::fs::DirEntry>> {
        if path.ends_with(DERIVED_DIR) {
            return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        }
        self.0.read_dir(path)
    }
    fn rename(&self, from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
        self.0.rename(from, to)
    }
    fn remove_file(&self, path: &std::path::Path) -> std::io::Result<()> {
        self.0.remove_file(path)
    }
    fn sync_dir(&self, path: &std::path::Path) -> std::io::Result<()> {
        self.0.sync_dir(path)
    }
    fn free_space(&self, path: &std::path::Path) -> std::io::Result<u64> {
        self.0.free_space(path)
    }
}

#[test]
fn a_start_that_backs_off_asks_to_run_again_after_its_wait() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 100, 0);
    r.settle();
    let synced = r.w.as_ref().unwrap().stats().mark_synced;
    let start = r.w.as_ref().unwrap().directory_start();
    let mut d = Directories::new(
        Arc::new(NoDerived(r.fs.clone())),
        &dir(),
        STORE_ID,
        start,
        DirectoryOptions::default(),
    );
    let now = Duration::from_secs(1_000);
    let asked: Vec<_> = (0..1_000).map(|_| d.run(synced, now, T0)).collect();
    let at_once = asked.iter().filter(|a| **a == Some(Duration::ZERO)).count();
    assert!(
        at_once <= 1,
        "{at_once} of 1000 runs at one instant ask to run again at once"
    );
    assert_eq!(d.stats().failed, 1, "tried once while it backs off");
    assert!(
        asked[1..]
            .iter()
            .all(|a| a.is_some_and(|w| w <= Duration::from_millis(100)))
    );
    let later = now + Duration::from_millis(100);
    assert!(
        d.run(synced, later, T0)
            .is_some_and(|w| w == Duration::from_millis(200))
    );
    assert_eq!(d.stats().failed, 2, "tried again once the wait is over");
}
