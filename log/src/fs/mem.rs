//! [`MemFs`]: the in-memory file system for tests, with injected faults, an operation log and a durable image per
//! file.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::{DirEntry, FileIdentity, Fs, FsFile, OpenMode, lock};

/// An operation that [`MemFs`] can be told to fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FaultOp {
    /// [`Fs::open`] with [`OpenMode::CreateNew`].
    Create,
    /// [`FsFile::write_at`].
    Write,
    /// [`FsFile::sync_data`] or [`FsFile::sync_all`].
    Sync,
    /// [`FsFile::set_len`].
    SetLen,
    /// [`Fs::rename`].
    Rename,
    /// [`Fs::sync_dir`].
    SyncDir,
    /// [`Fs::remove_file`].
    Remove,
    /// The flush after a rename where the platform has no directory handle (A.13): the rename takes effect, then
    /// the call fails, as `StdFs::rename` does on Windows when the flush fails. Planned with [`MemFs::fail_nth`]
    /// only; never recorded as an operation of its own.
    RenameFlush,
}

/// A recorded operation of [`MemFs`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemOp {
    /// What was done.
    pub op: FaultOp,
    /// The file (for a rename, its new name; for a directory sync, the directory).
    pub path: PathBuf,
    /// Offset of a write, else 0.
    pub offset: u64,
    /// Length of a write or the new length of a file, else 0.
    pub len: u64,
    /// Whether it succeeded.
    pub ok: bool,
}

#[derive(Clone, Debug)]
struct Fault {
    op: FaultOp,
    path_contains: String,
    skip: usize,
    kind: io::ErrorKind,
}

/// A hook run before each file sync of a [`MemFs`] completes, with the file's path; the sync then succeeds or
/// fails as planned. Tests use it to act while a sync is in flight.
pub type SyncHook = Arc<dyn Fn(&Path) + Send + Sync>;

/// An in-memory file system for tests, with injected faults and crashes. Clones share the same files.
///
/// Every write is visible at once to every handle. Each file also has a **durable image**, its contents as of its
/// last successful sync, and the list of writes and length changes made since. Crashes:
/// - [`MemFs::power_loss`] puts every file back to its durable image; [`MemFs::power_loss_partial`] also applies a
///   seeded subset of the unsynced changes, one of them cut short at an 8-byte boundary, and may undo creations and
///   renames and removals not yet made durable by a directory sync;
/// - [`MemFs::crash_after`] makes every operation fail after a number of further operations, as if the process
///   stopped there.
///
/// Faults: [`MemFs::fail_syncs`] makes every sync fail, [`MemFs::fail_nth`] fails one chosen operation, and growth
/// beyond [`MemFs::set_free_space`] fails with [`io::ErrorKind::StorageFull`]. A failed sync models Linux's
/// write-back: the unsynced bytes stay visible but are no longer dirty, so a later sync does not make them durable
/// unless they are written again. Directories exist implicitly.
#[derive(Clone)]
pub struct MemFs {
    state: Arc<Mutex<MemState>>,
    hook: Arc<Mutex<Option<SyncHook>>>,
    /// Free bytes. A lock of its own, taken last: a write holds its file's lock while it grows the file, and a
    /// directory listing holds the state's lock while it reads each file's length, so growth must not wait for the
    /// state's lock (lock order: state, then a file, then this).
    free: Arc<Mutex<u64>>,
    /// [`Fs::free_space`] reports no source, as on a platform without one.
    space_unknown: Arc<std::sync::atomic::AtomicBool>,
}

impl fmt::Debug for MemFs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemFs").finish_non_exhaustive()
    }
}

#[derive(Debug)]
enum NameChange {
    Created(PathBuf),
    Renamed {
        from: PathBuf,
        to: PathBuf,
        /// The file the rename replaced, if `to` existed: an undone rename puts it back.
        replaced: Option<Arc<Mutex<MemData>>>,
    },
    Removed {
        path: PathBuf,
        data: Arc<Mutex<MemData>>,
    },
}

#[derive(Debug)]
struct MemState {
    files: BTreeMap<PathBuf, Arc<Mutex<MemData>>>,
    fail_syncs: bool,
    faults: Vec<Fault>,
    ops: Vec<MemOp>,
    /// Operations left before every operation fails.
    crash_in: Option<usize>,
    /// Creations, renames and removals not yet made durable by a directory sync, oldest first.
    names: Vec<NameChange>,
}

#[derive(Clone, Debug)]
enum Change {
    Write(usize, Vec<u8>),
    Len(usize),
}

#[derive(Debug, Default)]
struct MemData {
    /// Its identity: unique per created file in this process.
    id: u64,
    path: PathBuf,
    bytes: Vec<u8>,
    durable: Vec<u8>,
    locked: bool,
    /// Changes since the last successful sync, oldest first.
    pending: Vec<Change>,
    /// Byte ranges whose write-back failed: clean but not durable until written again.
    lost: Vec<(usize, usize)>,
}

impl MemData {
    fn forget_lost(&mut self, start: usize, end: usize) {
        let mut out = Vec::new();
        for &(a, b) in &self.lost {
            if b <= start || a >= end {
                out.push((a, b));
            } else {
                if a < start {
                    out.push((a, start));
                }
                if b > end {
                    out.push((end, b));
                }
            }
        }
        self.lost = out;
    }
}

#[derive(Debug)]
struct MemFile {
    fs: MemFs,
    data: Arc<Mutex<MemData>>,
    holds_lock: Mutex<bool>,
}

/// Why a [`MemFs`] operation failed.
struct Failure {
    err: io::Error,
    /// The file system has crashed: the operation never happened.
    crashed: bool,
}

impl From<Failure> for io::Error {
    fn from(f: Failure) -> Self {
        f.err
    }
}

impl Default for MemFs {
    fn default() -> Self {
        MemFs::new()
    }
}

/// A small deterministic generator for seeded crashes.
struct Rng(u64);

impl Rng {
    /// splitmix64.
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

impl MemFs {
    /// An empty file system with 1 TiB free.
    pub fn new() -> Self {
        MemFs {
            state: Arc::new(Mutex::new(MemState {
                files: BTreeMap::new(),
                fail_syncs: false,
                faults: Vec::new(),
                ops: Vec::new(),
                crash_in: None,
                names: Vec::new(),
            })),
            hook: Arc::new(Mutex::new(None)),
            free: Arc::new(Mutex::new(1 << 40)),
            space_unknown: Arc::default(),
        }
    }

    /// Makes [`Fs::free_space`] fail with [`io::ErrorKind::Unsupported`], as on a platform with no free-space source
    /// (or report the free space again). Growth is still bounded by [`MemFs::set_free_space`].
    pub fn set_free_space_unknown(&self, unknown: bool) {
        self.space_unknown
            .store(unknown, std::sync::atomic::Ordering::Relaxed);
    }

    /// Makes every later sync fail (or succeed again).
    pub fn fail_syncs(&self, fail: bool) {
        lock(&self.state).fail_syncs = fail;
    }

    /// Sets the free space; growing a file beyond it fails.
    pub fn set_free_space(&self, bytes: u64) {
        *lock(&self.free) = bytes;
    }

    /// Fails the operation `op` on a path containing `path_contains`, after `skip` such operations have succeeded,
    /// with an error of `kind`. Each planned fault fires once.
    pub fn fail_nth(&self, op: FaultOp, path_contains: &str, skip: usize, kind: io::ErrorKind) {
        lock(&self.state).faults.push(Fault {
            op,
            path_contains: path_contains.to_owned(),
            skip,
            kind,
        });
    }

    /// Lets `n` more operations (creations, writes, syncs, length changes, renames, removals, directory syncs)
    /// happen, then fails every operation, as if the process stopped there. [`MemFs::restart`] or a power loss
    /// ends it.
    pub fn crash_after(&self, n: usize) {
        lock(&self.state).crash_in = Some(n);
    }

    /// Whether a crash planned by [`MemFs::crash_after`] has happened.
    pub fn crashed(&self) -> bool {
        lock(&self.state).crash_in == Some(0)
    }

    /// The process is started again after a crash without a power loss: what it wrote stays, as in the page
    /// cache; planned faults and the crash countdown are cleared.
    pub fn restart(&self) {
        let mut s = lock(&self.state);
        s.crash_in = None;
        s.faults.clear();
        s.fail_syncs = false;
    }

    /// Sets or clears the hook run before each file sync completes.
    pub fn set_sync_hook(&self, hook: Option<SyncHook>) {
        *lock(&self.hook) = hook;
    }

    /// The operations so far, in order.
    pub fn ops(&self) -> Vec<MemOp> {
        lock(&self.state).ops.clone()
    }

    /// Forgets the operations recorded so far.
    pub fn clear_ops(&self) {
        lock(&self.state).ops.clear();
    }

    /// A file's current contents.
    pub fn read(&self, path: &Path) -> Option<Vec<u8>> {
        let s = lock(&self.state);
        s.files.get(path).map(|d| lock(d).bytes.clone())
    }

    /// A file's durable image: its contents as of its last successful sync.
    pub fn read_durable(&self, path: &Path) -> Option<Vec<u8>> {
        let s = lock(&self.state);
        s.files.get(path).map(|d| lock(d).durable.clone())
    }

    /// Writes a file's bytes directly, bypassing faults and the operation log, durable at once: for damaging
    /// files in tests.
    pub fn poke(&self, path: &Path, offset: usize, bytes: &[u8]) {
        let s = lock(&self.state);
        if let Some(d) = s.files.get(path) {
            let mut g = lock(d);
            let d = &mut *g;
            for img in [&mut d.bytes, &mut d.durable] {
                if img.len() < offset + bytes.len() {
                    img.resize(offset + bytes.len(), 0);
                }
                img[offset..offset + bytes.len()].copy_from_slice(bytes);
            }
        }
    }

    /// An independent copy of every file and of the name changes not yet durable, with no faults, crash or hook
    /// planned and no file locked: the same store as another process would find it, for a second start.
    pub fn snapshot(&self) -> MemFs {
        let s = lock(&self.state);
        let copy = MemFs::new();
        let mut c = lock(&copy.state);
        let mut copied: BTreeMap<usize, Arc<Mutex<MemData>>> = BTreeMap::new();
        let mut copy_of = |data: &Arc<Mutex<MemData>>| {
            copied
                .entry(Arc::as_ptr(data) as usize)
                .or_insert_with(|| {
                    let d = lock(data);
                    Arc::new(Mutex::new(MemData {
                        id: d.id,
                        path: d.path.clone(),
                        bytes: d.bytes.clone(),
                        durable: d.durable.clone(),
                        locked: false,
                        pending: d.pending.clone(),
                        lost: d.lost.clone(),
                    }))
                })
                .clone()
        };
        for (path, data) in &s.files {
            c.files.insert(path.clone(), copy_of(data));
        }
        c.names = s
            .names
            .iter()
            .map(|n| match n {
                NameChange::Created(p) => NameChange::Created(p.clone()),
                NameChange::Renamed { from, to, replaced } => NameChange::Renamed {
                    from: from.clone(),
                    to: to.clone(),
                    replaced: replaced.as_ref().map(&mut copy_of),
                },
                NameChange::Removed { path, data } => NameChange::Removed {
                    path: path.clone(),
                    data: copy_of(data),
                },
            })
            .collect();
        drop(c);
        *lock(&copy.free) = *lock(&self.free);
        copy
    }

    /// Every file's path, sorted.
    pub fn paths(&self) -> Vec<PathBuf> {
        lock(&self.state).files.keys().cloned().collect()
    }

    /// A simulated power loss: every file goes back to its durable image and every lock is released. Every creation, rename or removal not yet made durable by a directory sync is undone.
    /// Handles still open stay usable, so a test drops them first.
    pub fn power_loss(&self) {
        self.crash(None);
    }

    /// A simulated power loss that lets part of the unsynced work reach the disk, chosen by `seed`: each unsynced
    /// change of each file is applied or not, in order, and one applied write may be cut short at an 8-byte
    /// boundary; each creation, rename or removal not yet made durable by a directory sync may be undone.
    pub fn power_loss_partial(&self, seed: u64) {
        self.crash(Some(Rng(seed)));
    }

    fn crash(&self, mut rng: Option<Rng>) {
        let mut s = lock(&self.state);
        let mut freed: i128 = 0;
        for d in s.files.values() {
            let mut d = lock(d);
            let mut img = d.durable.clone();
            if let Some(r) = rng.as_mut() {
                let cut = r.next() % (d.pending.len() as u64 + 1);
                for (i, c) in d.pending.iter().enumerate() {
                    if r.next() % 2 == 0 {
                        continue;
                    }
                    match c {
                        Change::Len(n) => img.resize(*n, 0),
                        Change::Write(at, b) => {
                            let n = if i as u64 == cut {
                                (r.next() as usize % (b.len() / 8 + 1)) * 8
                            } else {
                                b.len()
                            };
                            if img.len() < at + n {
                                img.resize(at + n, 0);
                            }
                            img[*at..at + n].copy_from_slice(&b[..n]);
                        }
                    }
                }
            }
            freed += d.bytes.len() as i128 - img.len() as i128;
            d.bytes = img.clone();
            d.durable = img;
            d.pending.clear();
            d.lost.clear();
            d.locked = false;
        }
        {
            let mut f = lock(&self.free);
            *f = (*f as i128 + freed).max(0) as u64;
        }
        let names = std::mem::take(&mut s.names);
        {
            for n in names.into_iter().rev() {
                if rng.as_mut().is_some_and(|r| r.next() % 2 == 0) {
                    continue;
                }
                match n {
                    NameChange::Created(p) => {
                        if let Some(d) = s.files.remove(&p) {
                            *lock(&self.free) += lock(&d).bytes.len() as u64;
                        }
                    }
                    NameChange::Renamed { from, to, replaced } => {
                        if !s.files.contains_key(&from)
                            && let Some(d) = s.files.remove(&to)
                        {
                            lock(&d).path = from.clone();
                            s.files.insert(from, d);
                            // The file the rename replaced is there again, as of its last sync.
                            if let Some(old) = replaced {
                                let len = {
                                    let mut d = lock(&old);
                                    d.path = to.clone();
                                    d.bytes = d.durable.clone();
                                    d.pending.clear();
                                    d.lost.clear();
                                    d.locked = false;
                                    d.bytes.len() as u64
                                };
                                let mut f = lock(&self.free);
                                *f = f.saturating_sub(len);
                                s.files.insert(to, old);
                            }
                        }
                    }
                    NameChange::Removed { path, data } => {
                        if let std::collections::btree_map::Entry::Vacant(e) = s.files.entry(path) {
                            let len = {
                                let mut d = lock(&data);
                                d.bytes = d.durable.clone();
                                d.pending.clear();
                                d.lost.clear();
                                d.locked = false;
                                d.bytes.len() as u64
                            };
                            let mut f = lock(&self.free);
                            *f = f.saturating_sub(len);
                            e.insert(data);
                        }
                    }
                }
            }
        }
        s.crash_in = None;
    }

    /// Records `op` and fails it if the file system has crashed or a planned fault matches.
    fn check(&self, op: FaultOp, path: &Path, offset: u64, len: u64) -> Result<(), Failure> {
        let mut s = lock(&self.state);
        let name = path.to_string_lossy();
        let mut err = None;
        let mut crashed = false;
        match s.crash_in {
            Some(0) => {
                crashed = true;
                err = Some(io::Error::other("the process has stopped"));
            }
            Some(n) => s.crash_in = Some(n - 1),
            None => {}
        }
        if err.is_none() && matches!(op, FaultOp::Sync | FaultOp::SyncDir) && s.fail_syncs {
            err = Some(io::Error::other("injected sync failure"));
        }
        if err.is_none()
            && let Some(i) = s
                .faults
                .iter()
                .position(|f| f.op == op && name.contains(f.path_contains.as_str()))
        {
            if s.faults[i].skip == 0 {
                let f = s.faults.remove(i);
                err = Some(io::Error::new(f.kind, "injected fault"));
            } else {
                s.faults[i].skip -= 1;
            }
        }
        if !crashed {
            s.ops.push(MemOp {
                op,
                path: path.to_path_buf(),
                offset,
                len,
                ok: err.is_none(),
            });
        }
        err.map_or(Ok(()), |err| Err(Failure { err, crashed }))
    }

    /// Changes the free space by the growth of a file from `old` to `new` bytes; fails without a change if there
    /// is not enough.
    fn resize(&self, old: usize, new: usize) -> io::Result<()> {
        let mut free = lock(&self.free);
        if new > old {
            let by = (new - old) as u64;
            if by > *free {
                return Err(io::Error::new(io::ErrorKind::StorageFull, "disk full"));
            }
            *free -= by;
        } else {
            *free += (old - new) as u64;
        }
        Ok(())
    }
}

fn not_found() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "no such file")
}

impl Fs for MemFs {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FsFile>> {
        if mode == OpenMode::CreateNew {
            self.check(FaultOp::Create, path, 0, 0)?;
        }
        let mut s = lock(&self.state);
        let data = match (mode, s.files.get(path)) {
            (OpenMode::CreateNew, Some(_)) => {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "file exists"));
            }
            (OpenMode::CreateNew, None) => {
                static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
                let d = Arc::new(Mutex::new(MemData {
                    id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                    path: path.to_path_buf(),
                    ..MemData::default()
                }));
                s.files.insert(path.to_path_buf(), d.clone());
                s.names.push(NameChange::Created(path.to_path_buf()));
                d
            }
            (_, Some(d)) => d.clone(),
            (_, None) => return Err(not_found()),
        };
        Ok(Box::new(MemFile {
            fs: self.clone(),
            data,
            holds_lock: Mutex::new(false),
        }))
    }
    fn create_dir_all(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        // Directories exist implicitly: a file deeper below `path` makes its first component a sub-directory.
        let mut out = BTreeMap::new();
        for (p, d) in &lock(&self.state).files {
            let Ok(rest) = p.strip_prefix(path) else {
                continue;
            };
            let mut parts = rest.components();
            let (Some(first), deeper) = (parts.next(), parts.next().is_some()) else {
                continue;
            };
            let name = first.as_os_str().to_string_lossy().into_owned();
            let len = if deeper {
                0
            } else {
                lock(d).bytes.len() as u64
            };
            out.entry(name.clone())
                .or_insert_with(|| DirEntry::new(name, len))
                .is_dir |= deeper;
        }
        Ok(out.into_values().collect())
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.check(FaultOp::Rename, to, 0, 0)?;
        let mut s = lock(&self.state);
        let d = s.files.remove(from).ok_or_else(not_found)?;
        lock(&d).path = to.to_path_buf();
        let replaced = s.files.insert(to.to_path_buf(), d);
        if let Some(old) = &replaced {
            *lock(&self.free) += lock(old).bytes.len() as u64;
        }
        s.names.push(NameChange::Renamed {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
            replaced,
        });
        // A planned failure of the flush after the rename: the rename stands, the call fails. The planned faults
        // are consulted directly, so no crash point is used and no operation is recorded.
        let name = to.to_string_lossy();
        if let Some(i) = s
            .faults
            .iter()
            .position(|f| f.op == FaultOp::RenameFlush && name.contains(f.path_contains.as_str()))
        {
            if s.faults[i].skip == 0 {
                let f = s.faults.remove(i);
                return Err(io::Error::new(
                    f.kind,
                    "injected flush failure after a rename",
                ));
            }
            s.faults[i].skip -= 1;
        }
        Ok(())
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.check(FaultOp::Remove, path, 0, 0)?;
        let mut s = lock(&self.state);
        let d = s.files.remove(path).ok_or_else(not_found)?;
        *lock(&self.free) += lock(&d).bytes.len() as u64;
        let created_unsynced = s
            .names
            .iter()
            .any(|n| matches!(n, NameChange::Created(p) if p == path));
        s.names
            .retain(|n| !matches!(n, NameChange::Created(p) if p == path));
        // A removal not yet made durable by a directory sync may be undone by a power loss.
        if !created_unsynced {
            s.names.push(NameChange::Removed {
                path: path.to_path_buf(),
                data: d,
            });
        }
        Ok(())
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.check(FaultOp::SyncDir, path, 0, 0)?;
        lock(&self.state).names.retain(|n| {
            let p = match n {
                NameChange::Created(p) => p,
                NameChange::Renamed { to, .. } => to,
                NameChange::Removed { path, .. } => path,
            };
            p.parent() != Some(path)
        });
        Ok(())
    }
    fn free_space(&self, _path: &Path) -> io::Result<u64> {
        if self
            .space_unknown
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "free space is not available",
            ));
        }
        Ok(*lock(&self.free))
    }
}

impl MemFile {
    fn path(&self) -> PathBuf {
        lock(&self.data).path.clone()
    }

    fn sync(&self) -> io::Result<()> {
        let path = self.path();
        let hook = lock(&self.fs.hook).clone();
        if let Some(h) = hook {
            h(&path);
        }
        let mut r = self.fs.check(FaultOp::Sync, &path, 0, 0);
        let mut d = lock(&self.data);
        match &mut r {
            Ok(()) => {
                let mut durable = d.bytes.clone();
                for &(a, b) in &d.lost {
                    for i in a..b.min(durable.len()) {
                        durable[i] = d.durable.get(i).copied().unwrap_or(0);
                    }
                }
                d.durable = durable;
                d.pending.clear();
            }
            // A failed write-back: the pages are clean but not on disk (Linux).
            Err(f) if !f.crashed => {
                let ranges: Vec<_> = d
                    .pending
                    .iter()
                    .filter_map(|c| match c {
                        Change::Write(at, b) => Some((*at, at + b.len())),
                        Change::Len(_) => None,
                    })
                    .collect();
                d.lost.extend(ranges);
                d.pending.clear();
            }
            Err(_) => {}
        }
        r.map_err(io::Error::from)
    }
}

impl FsFile for MemFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let d = lock(&self.data);
        let start = usize::try_from(offset).map_err(io::Error::other)?;
        if start >= d.bytes.len() {
            return Ok(0);
        }
        let n = buf.len().min(d.bytes.len() - start);
        buf[..n].copy_from_slice(&d.bytes[start..start + n]);
        Ok(n)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.fs
            .check(FaultOp::Write, &self.path(), offset, buf.len() as u64)?;
        let mut d = lock(&self.data);
        let start = usize::try_from(offset).map_err(io::Error::other)?;
        let end = start + buf.len();
        if end > d.bytes.len() {
            self.fs.resize(d.bytes.len(), end)?;
            d.bytes.resize(end, 0);
        }
        d.bytes[start..end].copy_from_slice(buf);
        d.pending.push(Change::Write(start, buf.to_vec()));
        d.forget_lost(start, end);
        Ok(())
    }
    fn sync_data(&self) -> io::Result<()> {
        self.sync()
    }
    fn sync_all(&self) -> io::Result<()> {
        self.sync()
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.fs.check(FaultOp::SetLen, &self.path(), 0, len)?;
        let mut d = lock(&self.data);
        let len = usize::try_from(len).map_err(io::Error::other)?;
        self.fs.resize(d.bytes.len(), len)?;
        d.bytes.resize(len, 0);
        d.pending.push(Change::Len(len));
        Ok(())
    }
    fn len(&self) -> io::Result<u64> {
        Ok(lock(&self.data).bytes.len() as u64)
    }
    fn identity(&self) -> io::Result<Option<FileIdentity>> {
        Ok(Some(FileIdentity {
            device: 0,
            inode: lock(&self.data).id,
        }))
    }
    fn try_lock_exclusive(&self) -> io::Result<bool> {
        let mut held = lock(&self.holds_lock);
        if *held {
            return Ok(true);
        }
        let mut d = lock(&self.data);
        if d.locked {
            return Ok(false);
        }
        d.locked = true;
        *held = true;
        Ok(true)
    }
}

impl Drop for MemFile {
    fn drop(&mut self) {
        if *lock(&self.holds_lock) {
            lock(&self.data).locked = false;
        }
    }
}
