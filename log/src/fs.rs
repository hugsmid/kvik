//! The file-system seam (`docs/api.md` Environment seams, `docs/format.md` A.13): every byte the engine persists
//! passes one of these calls, so the test implementation can fail any write or sync, run out of space, and drop
//! unsynced writes at a simulated power loss.
//!
//! | Implementation | Use |
//! |---|---|
//! | [`StdFs`] | the operating system's file system through `std::fs` |
//! | [`MemFs`] | in memory, for tests: injected faults, an operation log and a durable image per file |

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How [`Fs::open`] opens a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum OpenMode {
    /// Read an existing file.
    Read,
    /// Read and write an existing file.
    ReadWrite,
    /// Create a new file for reading and writing; fails if it exists.
    CreateNew,
}

/// An entry of a directory.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub struct DirEntry {
    /// File name.
    pub name: String,
    /// Length in bytes.
    pub len: u64,
}

impl DirEntry {
    /// An entry, for implementations of [`Fs::read_dir`].
    pub fn new(name: impl Into<String>, len: u64) -> Self {
        DirEntry {
            name: name.into(),
            len,
        }
    }
}

/// The file-system operations the durable engine uses (spec §3, §7).
///
/// Removal exists for retired files after their grace period (spec §7),
/// never for facts.
pub trait Fs: Send + Sync + fmt::Debug {
    /// Opens a file.
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FsFile>>;
    /// Creates a directory and its parents.
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    /// The entries of a directory, files and sub-directories, sorted by name; a sub-directory's length is 0.
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>>;
    /// Renames a file, replacing `to` if it exists.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Removes a file.
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// Makes the directory's entries durable.
    fn sync_dir(&self, path: &Path) -> io::Result<()>;
    /// Free bytes on the file system holding `path` (spec §3 Disk space).
    fn free_space(&self, path: &Path) -> io::Result<u64>;
}

/// An open file.
pub trait FsFile: Send + Sync + fmt::Debug {
    /// Reads at `offset`; returns the number of bytes read.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;
    /// Writes all of `buf` at `offset`.
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
    /// Makes the file's data durable.
    fn sync_data(&self) -> io::Result<()>;
    /// Makes the file's data and metadata durable.
    fn sync_all(&self) -> io::Result<()>;
    /// Sets the length (segments are preallocated, spec §3 Segments).
    fn set_len(&self, len: u64) -> io::Result<()>;
    /// Current length in bytes.
    fn len(&self) -> io::Result<u64>;
    /// Whether the file is empty.
    fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }
    /// Takes an exclusive advisory lock without waiting; `false` if another
    /// handle holds it (spec §3 Single writer). Released when the file is
    /// dropped.
    fn try_lock_exclusive(&self) -> io::Result<bool>;
}

/// The operating system's file system through `std::fs`.
///
/// [`Fs::free_space`] uses `statvfs` on Unix and returns [`io::ErrorKind::Unsupported`] on Windows, a development
/// platform (`spec.md` §3 Sync primitive) for which no safe source is verified yet. On Windows, which has no
/// directory handle, a created or renamed file is flushed itself and [`Fs::sync_dir`] does nothing more
/// (`format.md` A.13).
#[derive(Clone, Copy, Debug, Default)]
pub struct StdFs;

#[derive(Debug)]
struct StdFile(std::fs::File);

impl Fs for StdFs {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FsFile>> {
        let mut o = std::fs::OpenOptions::new();
        match mode {
            OpenMode::Read => o.read(true),
            OpenMode::ReadWrite => o.read(true).write(true),
            OpenMode::CreateNew => o.read(true).write(true).create_new(true),
        };
        let f = o.open(path)?;
        if cfg!(windows) && mode == OpenMode::CreateNew {
            f.sync_all()?;
        }
        Ok(Box::new(StdFile(f)))
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(path)? {
            let e = e?;
            let meta = e.metadata()?;
            out.push(DirEntry {
                name: e.file_name().to_string_lossy().into_owned(),
                len: if meta.is_file() { meta.len() } else { 0 },
            });
        }
        out.sort();
        Ok(out)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)?;
        if cfg!(windows) {
            std::fs::OpenOptions::new()
                .write(true)
                .open(to)?
                .sync_all()?;
        }
        Ok(())
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        // Windows cannot open a directory as a file: the created or renamed
        // file was flushed itself (A.13).
        if cfg!(windows) {
            return Ok(());
        }
        std::fs::File::open(path)?.sync_all()
    }
    fn free_space(&self, path: &Path) -> io::Result<u64> {
        #[cfg(unix)]
        return statvfs_free(path);
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "free space is not available on this platform",
            ))
        }
    }
}

impl FsFile for StdFile {
    #[cfg(unix)]
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        std::os::unix::fs::FileExt::read_at(&self.0, buf, offset)
    }
    #[cfg(windows)]
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        std::os::windows::fs::FileExt::seek_read(&self.0, buf, offset)
    }
    #[cfg(unix)]
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        std::os::unix::fs::FileExt::write_all_at(&self.0, buf, offset)
    }
    #[cfg(windows)]
    fn write_at(&self, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
        while !buf.is_empty() {
            let n = std::os::windows::fs::FileExt::seek_write(&self.0, buf, offset)?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            buf = &buf[n..];
            offset += n as u64;
        }
        Ok(())
    }
    fn sync_data(&self) -> io::Result<()> {
        self.0.sync_data()
    }
    fn sync_all(&self) -> io::Result<()> {
        self.0.sync_all()
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }
    fn len(&self) -> io::Result<u64> {
        Ok(self.0.metadata()?.len())
    }
    fn try_lock_exclusive(&self) -> io::Result<bool> {
        match self.0.try_lock() {
            Ok(()) => Ok(true),
            Err(std::fs::TryLockError::WouldBlock) => Ok(false),
            Err(std::fs::TryLockError::Error(e)) => Err(e),
        }
    }
}

#[cfg(unix)]
fn statvfs_free(path: &Path) -> io::Result<u64> {
    let s = rustix::fs::statvfs(path).map_err(io::Error::from)?;
    Ok(s.f_bavail.saturating_mul(s.f_frsize))
}

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
    free: u64,
    faults: Vec<Fault>,
    ops: Vec<MemOp>,
    /// Operations left before every operation fails.
    crash_in: Option<usize>,
    /// Creations, renames and removals not yet made durable by a directory sync, oldest first.
    names: Vec<NameChange>,
}

#[derive(Debug)]
enum Change {
    Write(usize, Vec<u8>),
    Len(usize),
}

#[derive(Debug, Default)]
struct MemData {
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
                free: 1 << 40,
                faults: Vec::new(),
                ops: Vec::new(),
                crash_in: None,
                names: Vec::new(),
            })),
            hook: Arc::new(Mutex::new(None)),
        }
    }

    /// Makes every later sync fail (or succeed again).
    pub fn fail_syncs(&self, fail: bool) {
        lock(&self.state).fail_syncs = fail;
    }

    /// Sets the free space; growing a file beyond it fails.
    pub fn set_free_space(&self, bytes: u64) {
        lock(&self.state).free = bytes;
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

    /// Every file's path, sorted.
    pub fn paths(&self) -> Vec<PathBuf> {
        lock(&self.state).files.keys().cloned().collect()
    }

    /// A simulated power loss: every file goes back to its durable image and every lock is released. Names stay.
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
        s.free = (s.free as i128 + freed).max(0) as u64;
        let names = std::mem::take(&mut s.names);
        if let Some(r) = rng.as_mut() {
            for n in names.into_iter().rev() {
                if r.next() % 2 == 0 {
                    continue;
                }
                match n {
                    NameChange::Created(p) => {
                        if let Some(d) = s.files.remove(&p) {
                            s.free += lock(&d).bytes.len() as u64;
                        }
                    }
                    NameChange::Renamed { from, to } => {
                        if !s.files.contains_key(&from)
                            && let Some(d) = s.files.remove(&to)
                        {
                            lock(&d).path = from.clone();
                            s.files.insert(from, d);
                        }
                    }
                    NameChange::Removed { path, data } => {
                        if !s.files.contains_key(&path) {
                            let len = {
                                let mut d = lock(&data);
                                d.bytes = d.durable.clone();
                                d.pending.clear();
                                d.lost.clear();
                                d.locked = false;
                                d.bytes.len() as u64
                            };
                            s.free = s.free.saturating_sub(len);
                            s.files.insert(path, data);
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
        let mut s = lock(&self.state);
        if new > old {
            let by = (new - old) as u64;
            if by > s.free {
                return Err(io::Error::new(io::ErrorKind::StorageFull, "disk full"));
            }
            s.free -= by;
        } else {
            s.free += (old - new) as u64;
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
                let d = Arc::new(Mutex::new(MemData {
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
            out.entry(name.clone()).or_insert(DirEntry { name, len });
        }
        Ok(out.into_values().collect())
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.check(FaultOp::Rename, to, 0, 0)?;
        let mut s = lock(&self.state);
        let d = s.files.remove(from).ok_or_else(not_found)?;
        lock(&d).path = to.to_path_buf();
        if let Some(old) = s.files.insert(to.to_path_buf(), d) {
            s.free += lock(&old).bytes.len() as u64;
        }
        s.names.push(NameChange::Renamed {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        });
        Ok(())
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.check(FaultOp::Remove, path, 0, 0)?;
        let mut s = lock(&self.state);
        let d = s.files.remove(path).ok_or_else(not_found)?;
        s.free += lock(&d).bytes.len() as u64;
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
        Ok(lock(&self.state).free)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn exercise(fs: &dyn Fs, dir: &Path) {
        let p = dir.join("segment");
        let f = fs.open(&p, OpenMode::CreateNew).unwrap();
        assert!(fs.open(&p, OpenMode::CreateNew).is_err());
        f.set_len(64).unwrap();
        assert_eq!(f.len().unwrap(), 64);
        f.write_at(b"kvik", 8).unwrap();
        f.sync_data().unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(f.read_at(&mut buf, 8).unwrap(), 4);
        assert_eq!(&buf, b"kvik");
        assert!(f.try_lock_exclusive().unwrap());
        drop(f);
        let q = dir.join("sealed");
        fs.rename(&p, &q).unwrap();
        fs.sync_dir(dir).unwrap();
        let entries = fs.read_dir(dir).unwrap();
        assert_eq!(entries, vec![DirEntry::new("sealed", 64)]);
        let g = fs.open(&q, OpenMode::Read).unwrap();
        assert_eq!(g.len().unwrap(), 64);
        drop(g);
        fs.remove_file(&q).unwrap();
        assert!(fs.read_dir(dir).unwrap().is_empty());
    }

    #[test]
    fn mem_fs_behaves_like_a_file_system() {
        exercise(&MemFs::new(), Path::new("store"));
    }

    #[test]
    fn std_fs_behaves_like_a_file_system() {
        let dir = std::env::temp_dir().join(format!("kvik-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        exercise(&StdFs, &dir);
        if cfg!(unix) {
            assert!(StdFs.free_space(&dir).unwrap() > 0);
        }
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn directories_are_listed() {
        let fs = MemFs::new();
        drop(fs.open(Path::new("d/sub/f"), OpenMode::CreateNew).unwrap());
        drop(fs.open(Path::new("d/g"), OpenMode::CreateNew).unwrap());
        assert_eq!(
            fs.read_dir(Path::new("d")).unwrap(),
            vec![DirEntry::new("g", 0), DirEntry::new("sub", 0)]
        );
        let dir = std::env::temp_dir().join(format!("kvik-dirs-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        assert_eq!(StdFs.read_dir(&dir).unwrap(), vec![DirEntry::new("sub", 0)]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn mem_fs_injects_faults() {
        let fs = MemFs::new();
        let f = fs.open(Path::new("a"), OpenMode::CreateNew).unwrap();
        fs.fail_syncs(true);
        assert!(f.sync_data().is_err());
        fs.fail_syncs(false);
        f.sync_data().unwrap();
        fs.set_free_space(3);
        let e = f.write_at(b"four", 0).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::StorageFull);
        let g = fs.open(Path::new("a"), OpenMode::ReadWrite).unwrap();
        assert!(f.try_lock_exclusive().unwrap());
        assert!(!g.try_lock_exclusive().unwrap());
        drop(f);
        assert!(g.try_lock_exclusive().unwrap());
    }

    #[test]
    fn mem_fs_keeps_a_durable_image_and_plans_faults() {
        let fs = MemFs::new();
        let p = Path::new("d/f");
        let f = fs.open(p, OpenMode::CreateNew).unwrap();
        f.write_at(b"ab", 0).unwrap();
        f.sync_data().unwrap();
        f.write_at(b"cd", 2).unwrap();
        assert_eq!(fs.read(p).unwrap(), b"abcd");
        assert_eq!(fs.read_durable(p).unwrap(), b"ab");
        fs.fail_nth(FaultOp::Sync, "d/f", 1, io::ErrorKind::Other);
        f.sync_data().unwrap();
        f.write_at(b"e", 4).unwrap();
        assert!(f.sync_data().is_err());
        assert_eq!(fs.read_durable(p).unwrap(), b"abcd");
        f.sync_data().unwrap();
        drop(f);
        let ops = fs.ops();
        assert_eq!(
            ops.iter()
                .filter(|o| o.op == FaultOp::Sync && !o.ok)
                .count(),
            1
        );
        // The byte whose write-back failed is not made durable by the later sync (Linux), until written again.
        assert_eq!(fs.read_durable(p).unwrap(), b"abcd\0");
        let g = fs.open(p, OpenMode::ReadWrite).unwrap();
        g.write_at(b"e", 4).unwrap();
        g.sync_data().unwrap();
        assert_eq!(fs.read_durable(p).unwrap(), b"abcde");
        g.write_at(b"zz", 0).unwrap();
        drop(g);
        fs.power_loss();
        assert_eq!(fs.read(p).unwrap(), b"abcde");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        fs.set_sync_hook(Some(Arc::new(move |p: &Path| {
            lock(&s2).push(p.to_path_buf())
        })));
        fs.open(p, OpenMode::Read).unwrap().sync_all().unwrap();
        assert_eq!(*lock(&seen), vec![p.to_path_buf()]);
    }

    #[test]
    fn mem_fs_crashes() {
        let fs = MemFs::new();
        let p = Path::new("d/f");
        let f = fs.open(p, OpenMode::CreateNew).unwrap();
        f.write_at(&[1; 16], 0).unwrap();
        f.sync_all().unwrap();
        fs.sync_dir(Path::new("d")).unwrap();
        fs.crash_after(1);
        f.write_at(&[2; 16], 0).unwrap();
        assert!(f.write_at(&[3; 16], 0).is_err());
        assert!(fs.crashed());
        assert_eq!(fs.read(p).unwrap(), vec![2; 16]);
        fs.restart();
        f.write_at(&[4; 16], 0).unwrap();
        drop(f);
        // A partial power loss keeps some unsynced writes, perhaps cut short at an 8-byte boundary.
        let mut seen = std::collections::BTreeSet::new();
        for seed in 0..64 {
            let fs2 = MemFs::new();
            let q = Path::new("d/g");
            let g = fs2.open(q, OpenMode::CreateNew).unwrap();
            g.write_at(&[1; 16], 0).unwrap();
            g.sync_all().unwrap();
            g.write_at(&[2; 16], 0).unwrap();
            drop(g);
            fs2.power_loss_partial(seed);
            // The creation was never made durable by a directory sync: it may be undone.
            seen.insert(fs2.read(q));
        }
        assert!(seen.contains(&None));
        assert!(seen.contains(&Some(vec![1; 16])));
        assert!(seen.contains(&Some(vec![2; 16])));
        assert!(seen.contains(&Some([vec![2; 8], vec![1; 8]].concat())));
    }
}
