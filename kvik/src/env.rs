//! Environment seams (`docs/api.md` Environment seams): everything the engine
//! depends on from outside is a trait with a production and a test
//! implementation.
//!
//! | Seam | Production | Test |
//! |---|---|---|
//! | [`Clock`] | [`SystemClock`] | [`ManualClock`] |
//! | [`Fs`] | [`StdFs`] | [`MemFs`] |
//! | [`BootId`] | [`SystemBootId`] | [`FixedBootId`] |
//!
//! The in-memory model backend uses the clock and ignores the file system
//! and the boot identity; the durable engine (roadmap 4.2 onwards) uses all
//! three. Memory mapping is not part of [`Fs`]: the crate forbids `unsafe`
//! code, and mapping a file needs it; the segment reader (4.2) decides how
//! the mark file and segments are read.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use jiff::{SignedDuration, Timestamp};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ----- clock -----

/// The clocks the engine reads (spec §2.5, §4 Timers, §6 `now`).
pub trait Clock: Send + Sync + fmt::Debug {
    /// The system's wall clock, UTC. It may jump in either direction.
    fn wall(&self) -> Timestamp;
    /// A monotonic clock: never runs backwards and is not affected by
    /// corrections of the wall clock. Timers and live staleness use it.
    fn monotonic(&self) -> Instant;
    /// Whether the system reports clock synchronisation. Until it does,
    /// batches carry `unsynced_clock` and host-stamped readings wait (spec
    /// §2.5).
    fn synchronised(&self) -> bool;
}

/// The system clocks.
///
/// [`Clock::synchronised`] reports `true`: detecting synchronisation is
/// platform-specific and arrives with the engine.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn wall(&self) -> Timestamp {
        Timestamp::now()
    }
    fn monotonic(&self) -> Instant {
        Instant::now()
    }
    fn synchronised(&self) -> bool {
        true
    }
}

/// A clock that moves only when told to, for tests. Clones share the same
/// clock.
///
/// # Panics
/// [`ManualClock::advance`] and [`ManualClock::jump_wall`] panic when the wall
/// clock would leave the range of [`Timestamp`].
#[derive(Clone, Debug)]
pub struct ManualClock {
    state: Arc<Mutex<ManualState>>,
}

#[derive(Debug)]
struct ManualState {
    wall: Timestamp,
    base: Instant,
    elapsed: Duration,
    synchronised: bool,
}

impl ManualClock {
    /// A synchronised clock whose wall clock reads `wall`.
    pub fn new(wall: Timestamp) -> Self {
        ManualClock {
            state: Arc::new(Mutex::new(ManualState {
                wall,
                base: Instant::now(),
                elapsed: Duration::ZERO,
                synchronised: true,
            })),
        }
    }

    /// The wall clock now.
    pub fn wall(&self) -> Timestamp {
        lock(&self.state).wall
    }

    /// Advances both clocks by `d`.
    pub fn advance(&self, d: Duration) {
        let mut s = lock(&self.state);
        s.wall = s.wall.checked_add(d).expect("wall clock within range");
        s.elapsed += d;
    }

    /// Moves the wall clock only, as a correction of the system clock does
    /// (spec §9 row 22).
    pub fn jump_wall(&self, d: SignedDuration) {
        let mut s = lock(&self.state);
        s.wall = s.wall.checked_add(d).expect("wall clock within range");
    }

    /// Sets whether the clock reports synchronisation (spec §2.5).
    pub fn set_synchronised(&self, synchronised: bool) {
        lock(&self.state).synchronised = synchronised;
    }
}

impl Clock for ManualClock {
    fn wall(&self) -> Timestamp {
        lock(&self.state).wall
    }
    fn monotonic(&self) -> Instant {
        let s = lock(&self.state);
        s.base + s.elapsed
    }
    fn synchronised(&self) -> bool {
        lock(&self.state).synchronised
    }
}

// ----- file system -----

/// How [`Fs::open`] opens a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    /// Read an existing file.
    Read,
    /// Read and write an existing file.
    ReadWrite,
    /// Create a new file for reading and writing; fails if it exists.
    CreateNew,
}

/// The file-system operations the durable engine uses (spec §3, §7).
///
/// Paths are as the engine forms them under the store directory. There is no
/// operation that changes a file's existing bytes other than
/// [`FsFile::write_at`]; removal exists for retired files after their grace
/// period (spec §7), never for facts.
pub trait Fs: Send + Sync + fmt::Debug {
    /// Opens a file.
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FsFile>>;
    /// Renames a file, replacing `to` if it exists.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Removes a file.
    fn remove(&self, path: &Path) -> io::Result<()>;
    /// Creates a directory and its parents.
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    /// The entries of a directory, sorted.
    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>>;
    /// Makes the directory's entries durable (after creating, sealing or
    /// renaming a file, spec §3 Segments).
    fn sync_dir(&self, path: &Path) -> io::Result<()>;
    /// Free bytes on the file system holding `path` (spec §3 Disk space).
    fn free_space(&self, path: &Path) -> io::Result<u64>;
}

/// An open file.
pub trait FsFile: Send + Sync + fmt::Debug {
    /// Current length in bytes.
    fn len(&self) -> io::Result<u64>;
    /// Whether the file is empty.
    fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }
    /// Extends the file to at least `len` bytes (segments are preallocated,
    /// spec §3 Segments).
    fn preallocate(&self, len: u64) -> io::Result<()>;
    /// Reads at `offset`; returns the number of bytes read.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;
    /// Writes at `offset`; returns the number of bytes written.
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize>;
    /// Makes the file's data durable.
    fn sync_data(&self) -> io::Result<()>;
    /// Makes the file's data and metadata durable.
    fn sync_all(&self) -> io::Result<()>;
    /// Takes an exclusive advisory lock without waiting; `false` if another
    /// handle holds it (spec §3 Single writer). Released when the file is
    /// dropped.
    fn try_lock_exclusive(&self) -> io::Result<bool>;
}

/// The operating system's file system through `std::fs`.
///
/// [`Fs::free_space`] returns [`io::ErrorKind::Unsupported`]: the standard
/// library has no portable call for it; it arrives with the engine.
/// [`FsFile::preallocate`] extends the file with `set_len`, which may leave
/// it sparse.
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
        Ok(Box::new(StdFile(o.open(path)?)))
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
    fn remove(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let mut out = std::fs::read_dir(path)?
            .map(|e| e.map(|e| e.path()))
            .collect::<io::Result<Vec<_>>>()?;
        out.sort();
        Ok(out)
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        // Windows cannot open a directory as a file; its file system orders
        // metadata itself.
        if cfg!(windows) {
            return Ok(());
        }
        std::fs::File::open(path)?.sync_all()
    }
    fn free_space(&self, _path: &Path) -> io::Result<u64> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "free space is not available through the standard library",
        ))
    }
}

impl FsFile for StdFile {
    fn len(&self) -> io::Result<u64> {
        Ok(self.0.metadata()?.len())
    }
    fn preallocate(&self, len: u64) -> io::Result<()> {
        if self.len()? < len {
            self.0.set_len(len)?;
        }
        Ok(())
    }
    #[cfg(unix)]
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        std::os::unix::fs::FileExt::read_at(&self.0, buf, offset)
    }
    #[cfg(windows)]
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        std::os::windows::fs::FileExt::seek_read(&self.0, buf, offset)
    }
    #[cfg(unix)]
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        std::os::unix::fs::FileExt::write_at(&self.0, buf, offset)
    }
    #[cfg(windows)]
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        std::os::windows::fs::FileExt::seek_write(&self.0, buf, offset)
    }
    fn sync_data(&self) -> io::Result<()> {
        self.0.sync_data()
    }
    fn sync_all(&self) -> io::Result<()> {
        self.0.sync_all()
    }
    fn try_lock_exclusive(&self) -> io::Result<bool> {
        match self.0.try_lock() {
            Ok(()) => Ok(true),
            Err(std::fs::TryLockError::WouldBlock) => Ok(false),
            Err(std::fs::TryLockError::Error(e)) => Err(e),
        }
    }
}

/// An in-memory file system for tests, with injected faults: failing syncs
/// and a full disk. Clones share the same files.
///
/// Every write is visible at once; [`MemFs::fail_syncs`] makes every sync
/// return an error, and writes beyond [`MemFs::set_free_space`] fail with
/// [`io::ErrorKind::StorageFull`]. Directories exist implicitly.
#[derive(Clone, Debug)]
pub struct MemFs {
    state: Arc<Mutex<MemState>>,
}

#[derive(Debug)]
struct MemState {
    files: BTreeMap<PathBuf, Arc<Mutex<MemData>>>,
    fail_syncs: bool,
    free: u64,
}

#[derive(Debug, Default)]
struct MemData {
    bytes: Vec<u8>,
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
            })),
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

    fn sync(&self) -> io::Result<()> {
        if lock(&self.state).fail_syncs {
            return Err(io::Error::other("injected sync failure"));
        }
        Ok(())
    }

    fn grow(&self, by: u64) -> io::Result<()> {
        let mut s = lock(&self.state);
        if by > s.free {
            return Err(io::Error::new(io::ErrorKind::StorageFull, "disk full"));
        }
        s.free -= by;
        Ok(())
    }
}

impl Fs for MemFs {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FsFile>> {
        let mut s = lock(&self.state);
        let data = match (mode, s.files.get(path)) {
            (OpenMode::CreateNew, Some(_)) => {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "file exists"));
            }
            (OpenMode::CreateNew, None) => {
                let d = Arc::new(Mutex::new(MemData::default()));
                s.files.insert(path.to_path_buf(), d.clone());
                d
            }
            (_, Some(d)) => d.clone(),
            (_, None) => return Err(io::Error::new(io::ErrorKind::NotFound, "no such file")),
        };
        Ok(Box::new(MemFile {
            fs: self.clone(),
            data,
            holds_lock: Mutex::new(false),
        }))
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut s = lock(&self.state);
        let d = s
            .files
            .remove(from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))?;
        s.files.insert(to.to_path_buf(), d);
        Ok(())
    }
    fn remove(&self, path: &Path) -> io::Result<()> {
        let mut s = lock(&self.state);
        let d = s
            .files
            .remove(path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))?;
        s.free += lock(&d).bytes.len() as u64;
        Ok(())
    }
    fn create_dir_all(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        Ok(lock(&self.state)
            .files
            .keys()
            .filter(|p| p.parent() == Some(path))
            .cloned()
            .collect())
    }
    fn sync_dir(&self, _path: &Path) -> io::Result<()> {
        self.sync()
    }
    fn free_space(&self, _path: &Path) -> io::Result<u64> {
        Ok(lock(&self.state).free)
    }
}

impl FsFile for MemFile {
    fn len(&self) -> io::Result<u64> {
        Ok(lock(&self.data).bytes.len() as u64)
    }
    fn preallocate(&self, len: u64) -> io::Result<()> {
        let mut d = lock(&self.data);
        let len = usize::try_from(len).map_err(io::Error::other)?;
        if d.bytes.len() < len {
            self.fs.grow((len - d.bytes.len()) as u64)?;
            d.bytes.resize(len, 0);
        }
        Ok(())
    }
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
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        let mut d = lock(&self.data);
        let start = usize::try_from(offset).map_err(io::Error::other)?;
        let end = start + buf.len();
        if end > d.bytes.len() {
            self.fs.grow((end - d.bytes.len()) as u64)?;
            d.bytes.resize(end, 0);
        }
        d.bytes[start..end].copy_from_slice(buf);
        Ok(buf.len())
    }
    fn sync_data(&self) -> io::Result<()> {
        self.fs.sync()
    }
    fn sync_all(&self) -> io::Result<()> {
        self.fs.sync()
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

// ----- boot identity -----

/// The identity of the current boot of the machine (spec §3 Durable mark,
/// Recovery): it changes when the machine restarts, so recovery can tell a
/// process crash from a power loss.
pub trait BootId: Send + Sync + fmt::Debug {
    /// The current boot's identity, or `None` when the platform has none;
    /// recovery then treats every start as a new boot.
    fn current(&self) -> Option<[u8; 16]>;
}

/// The operating system's boot identity: on Linux the kernel's `boot_id`;
/// `None` elsewhere for now (other platforms' sources arrive with recovery,
/// roadmap 4.5).
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemBootId;

impl BootId for SystemBootId {
    fn current(&self) -> Option<[u8; 16]> {
        if !cfg!(target_os = "linux") {
            return None;
        }
        let s = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        parse_uuid(s.trim())
    }
}

fn parse_uuid(s: &str) -> Option<[u8; 16]> {
    let hex: Vec<u8> = s.bytes().filter(|b| *b != b'-').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, pair) in hex.chunks(2).enumerate() {
        let pair = std::str::from_utf8(pair).ok()?;
        out[i] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

/// A boot identity set by the test, which can change it to simulate a
/// reboot. Clones share the same identity.
#[derive(Clone, Debug)]
pub struct FixedBootId {
    id: Arc<Mutex<Option<[u8; 16]>>>,
}

impl FixedBootId {
    /// A boot identity.
    pub fn new(id: Option<[u8; 16]>) -> Self {
        FixedBootId {
            id: Arc::new(Mutex::new(id)),
        }
    }

    /// Changes the identity, as a reboot does.
    pub fn set(&self, id: Option<[u8; 16]>) {
        *lock(&self.id) = id;
    }
}

impl BootId for FixedBootId {
    fn current(&self) -> Option<[u8; 16]> {
        *lock(&self.id)
    }
}

// ----- the environment -----

/// The environment a store runs in: its clock, file system and boot
/// identity.
#[derive(Clone, Debug)]
pub struct Env {
    /// Clock.
    pub clock: Arc<dyn Clock>,
    /// File system.
    pub fs: Arc<dyn Fs>,
    /// Boot identity.
    pub boot_id: Arc<dyn BootId>,
}

impl Env {
    /// The production environment: system clock, `std::fs`, the system's
    /// boot identity.
    pub fn system() -> Self {
        Env {
            clock: Arc::new(SystemClock),
            fs: Arc::new(StdFs),
            boot_id: Arc::new(SystemBootId),
        }
    }

    /// A test environment: the given manual clock, an in-memory file system
    /// and a fixed boot identity.
    pub fn test(clock: ManualClock) -> Self {
        Env {
            clock: Arc::new(clock),
            fs: Arc::new(MemFs::new()),
            boot_id: Arc::new(FixedBootId::new(Some([1; 16]))),
        }
    }
}

impl Default for Env {
    fn default() -> Self {
        Env::system()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_moves_only_when_told() {
        let c = ManualClock::new(Timestamp::from_second(1_000).unwrap());
        let m0 = Clock::monotonic(&c);
        c.advance(Duration::from_secs(5));
        assert_eq!(Clock::wall(&c), Timestamp::from_second(1_005).unwrap());
        assert_eq!(Clock::monotonic(&c) - m0, Duration::from_secs(5));
        c.jump_wall(SignedDuration::from_secs(-100));
        assert_eq!(Clock::wall(&c), Timestamp::from_second(905).unwrap());
        assert_eq!(Clock::monotonic(&c) - m0, Duration::from_secs(5));
        c.set_synchronised(false);
        assert!(!c.synchronised());
    }

    fn exercise(fs: &dyn Fs, dir: &Path) {
        let p = dir.join("segment");
        let f = fs.open(&p, OpenMode::CreateNew).unwrap();
        assert!(fs.open(&p, OpenMode::CreateNew).is_err());
        f.preallocate(64).unwrap();
        assert_eq!(f.len().unwrap(), 64);
        assert_eq!(f.write_at(b"kvik", 8).unwrap(), 4);
        f.sync_data().unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(f.read_at(&mut buf, 8).unwrap(), 4);
        assert_eq!(&buf, b"kvik");
        assert!(f.try_lock_exclusive().unwrap());
        drop(f);
        let q = dir.join("sealed");
        fs.rename(&p, &q).unwrap();
        fs.sync_dir(dir).unwrap();
        assert_eq!(fs.read_dir(dir).unwrap(), vec![q.clone()]);
        let g = fs.open(&q, OpenMode::Read).unwrap();
        assert_eq!(g.len().unwrap(), 64);
        drop(g);
        fs.remove(&q).unwrap();
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
        std::fs::remove_dir(&dir).unwrap();
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
    fn boot_ids() {
        assert_eq!(
            parse_uuid("00112233-4455-6677-8899-aabbccddeeff"),
            Some([
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff
            ])
        );
        assert_eq!(parse_uuid("not a uuid"), None);
        let b = FixedBootId::new(Some([2; 16]));
        b.set(None);
        assert_eq!(b.current(), None);
        if cfg!(target_os = "linux") {
            assert!(SystemBootId.current().is_some());
        }
    }
}
