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

/// An in-memory file system for tests, with injected faults. Clones share the same files.
///
/// Every write is visible at once to every handle. Each file also has a **durable image**, its contents as of its
/// last successful sync; [`MemFs::power_loss`] puts every file back to it. Directory entries are not modelled:
/// names survive a power loss. Faults: [`MemFs::fail_syncs`] makes every sync fail, [`MemFs::fail_nth`] fails one
/// chosen operation, and growth beyond [`MemFs::set_free_space`] fails with [`io::ErrorKind::StorageFull`].
/// Directories exist implicitly.
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
struct MemState {
    files: BTreeMap<PathBuf, Arc<Mutex<MemData>>>,
    fail_syncs: bool,
    free: u64,
    faults: Vec<Fault>,
    ops: Vec<MemOp>,
}

#[derive(Debug, Default)]
struct MemData {
    path: PathBuf,
    bytes: Vec<u8>,
    durable: Vec<u8>,
    locked: bool,
}

#[derive(Debug)]
struct MemFile {
    fs: MemFs,
    data: Arc<Mutex<MemData>>,
    holds_lock: Mutex<bool>,
}

impl Default for MemFs {
    fn default() -> Self {
        MemFs::new()
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

    /// Every file's path, sorted.
    pub fn paths(&self) -> Vec<PathBuf> {
        lock(&self.state).files.keys().cloned().collect()
    }

    /// A simulated power loss: every file goes back to its durable image and every lock is released. Handles
    /// still open stay usable, so a test drops them first.
    pub fn power_loss(&self) {
        let mut s = lock(&self.state);
        let mut freed = 0u64;
        for d in s.files.values() {
            let mut d = lock(d);
            let durable = d.durable.clone();
            freed = freed
                .wrapping_add(d.bytes.len() as u64)
                .wrapping_sub(durable.len() as u64);
            d.bytes = durable;
            d.locked = false;
        }
        s.free = s.free.wrapping_add(freed);
    }

    /// Records `op` and fails it if a planned fault matches.
    fn check(&self, op: FaultOp, path: &Path, offset: u64, len: u64) -> io::Result<()> {
        let mut s = lock(&self.state);
        let name = path.to_string_lossy();
        let mut err = None;
        if op == FaultOp::Sync && s.fail_syncs {
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
        s.ops.push(MemOp {
            op,
            path: path.to_path_buf(),
            offset,
            len,
            ok: err.is_none(),
        });
        err.map_or(Ok(()), Err)
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
        Ok(())
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        let mut s = lock(&self.state);
        let d = s.files.remove(path).ok_or_else(not_found)?;
        s.free += lock(&d).bytes.len() as u64;
        Ok(())
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.check(FaultOp::SyncDir, path, 0, 0)?;
        if lock(&self.state).fail_syncs {
            return Err(io::Error::other("injected sync failure"));
        }
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
        self.fs.check(FaultOp::Sync, &path, 0, 0)?;
        let mut d = lock(&self.data);
        d.durable = d.bytes.clone();
        Ok(())
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
        let g = fs.open(p, OpenMode::ReadWrite).unwrap();
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
}
