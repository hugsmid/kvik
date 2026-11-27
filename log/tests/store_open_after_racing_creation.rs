//! `format.md` A.12: whether a store is unfinished is judged under the lock, so a creation that finishes between
//! another process's listing and its lock is opened, not replaced.

use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use kvik_log::fs::{DirEntry, Fs, FsFile, MemFs, OpenMode};
use kvik_log::store::{NewStore, StoreDir};

/// Runs `after_listing` once, right after the first listing of the store directory returns.
struct Racing {
    inner: MemFs,
    after_listing: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl std::fmt::Debug for Racing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Racing")
    }
}

impl Fs for Racing {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FsFile>> {
        self.inner.open(path, mode)
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.inner.create_dir_all(path)
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let listing = self.inner.read_dir(path);
        let run = self.after_listing.lock().unwrap().take();
        if let Some(run) = run {
            run();
        }
        listing
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.inner.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_file(path)
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.sync_dir(path)
    }
    fn free_space(&self, path: &Path) -> io::Result<u64> {
        self.inner.free_space(path)
    }
}

fn new_store(id: u8) -> NewStore {
    NewStore {
        store_id: [id; 16],
        created_at: 1,
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: "kvik test".into(),
    }
}

#[test]
fn a_store_finished_and_written_to_after_the_listing_is_opened_not_refused() {
    let inner = MemFs::new();
    let dir = Path::new("store");
    let a_fs: Arc<dyn Fs> = Arc::new(inner.clone());
    let racing = Racing {
        inner: inner.clone(),
        after_listing: Mutex::new(Some(Box::new({
            let dir = dir.to_path_buf();
            let inner = inner.clone();
            move || {
                drop(StoreDir::open(a_fs, &dir, Some(&new_store(1))).unwrap());
                let f = inner
                    .open(&dir.join("log").join("seg"), OpenMode::CreateNew)
                    .unwrap();
                f.write_at(b"x", 0).unwrap();
            }
        }))),
    };
    let b = StoreDir::open(Arc::new(racing), dir, Some(&new_store(2)));
    assert_eq!(
        b.map(|b| b.descriptor().store_id)
            .map_err(|e| format!("{e:?}")),
        Ok([1; 16])
    );
}
