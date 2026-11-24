use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::lookup_properties::World;
use super::*;
use crate::fs::{DirEntry, Fs};

#[derive(Debug, Default)]
struct Open {
    now: AtomicUsize,
    peak: AtomicUsize,
}

struct CountingFs {
    inner: Arc<MemFs>,
    open: Arc<Open>,
}

impl fmt::Debug for CountingFs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CountingFs")
    }
}

#[derive(Debug)]
struct Counted {
    inner: Box<dyn FsFile>,
    open: Option<Arc<Open>>,
}

impl Drop for Counted {
    fn drop(&mut self) {
        if let Some(o) = &self.open {
            o.now.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl FsFile for Counted {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        self.inner.read_at(buf, offset)
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
    fn len(&self) -> std::io::Result<u64> {
        self.inner.len()
    }
    fn try_lock_exclusive(&self) -> std::io::Result<bool> {
        self.inner.try_lock_exclusive()
    }
    fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.inner.set_len(len)
    }
}

impl Fs for CountingFs {
    fn open(&self, path: &Path, mode: OpenMode) -> std::io::Result<Box<dyn FsFile>> {
        let inner = self.inner.open(path, mode)?;
        let segment = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| SegmentName::parse(n).is_some());
        let open = segment.then(|| self.open.clone());
        if let Some(o) = &open {
            let now = o.now.fetch_add(1, Ordering::SeqCst) + 1;
            o.peak.fetch_max(now, Ordering::SeqCst);
        }
        Ok(Box::new(Counted { inner, open }))
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

fn series(series_id: u32) -> [Wanted; 1] {
    [Wanted {
        series_id,
        from: i64::MIN,
        to: i64::MAX,
    }]
}

#[test]
fn a_listed_lookup_holds_one_segment_file_open_at_a_time_however_many_segments_it_spans() {
    let mut w = World::new();
    while w.sealed().len() < 8 {
        w.call(1);
    }
    let open = Arc::new(Open::default());
    let mut r = w.reader_on(Arc::new(CountingFs {
        inner: w.fs.clone(),
        open: open.clone(),
    }));
    open.peak.store(0, Ordering::SeqCst);
    let got = r.lookup(&series(1), &|_, _, _| false).unwrap();
    assert!(got.segments_listed >= 6, "listed {}", got.segments_listed);
    assert_eq!(got.complete, [true]);
    let peak = open.peak.load(Ordering::SeqCst);
    assert!(peak <= 2, "{peak} segment files open at once");
}
