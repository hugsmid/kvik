//! The file-system seam: the few operations the commit path needs, a real implementation and a simulated one
//! with injectable faults.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

/// An open file the commit path writes and syncs.
pub trait LogFile: Send {
    /// Writes all of `buf` at `offset`.
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()>;
    /// Makes everything written so far durable, with the sync call the file system was configured with.
    fn sync(&mut self) -> io::Result<()>;
}

/// Free and total bytes of the file system holding the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Space {
    /// Bytes available to the store.
    pub free: u64,
    /// Size of the file system.
    pub total: u64,
}

/// The operations of the commit path on the store directory.
pub trait FileSystem: Send + Sync {
    /// Creates `name` holding `header` followed by zeros up to `len` bytes, syncs it and syncs the directory.
    fn create_preallocated(
        &self,
        name: &str,
        len: u64,
        header: &[u8],
    ) -> io::Result<Box<dyn LogFile>>;
    /// Opens `name`, creating it empty if it does not exist.
    fn open(&self, name: &str) -> io::Result<Box<dyn LogFile>>;
    /// Free and total space.
    fn space(&self) -> io::Result<Space>;
    /// Syncs the log directory, so that a file created in it is durable.
    fn sync_log_dir(&self) -> io::Result<()>;
}

/// The call that makes a file durable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncCall {
    /// `fsync(2)`.
    #[cfg(unix)]
    Fsync,
    /// `fdatasync(2)`.
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    Fdatasync,
    /// `fcntl(F_FULLFSYNC)`.
    #[cfg(target_vendor = "apple")]
    FullFsync,
    /// The standard library's `File::sync_all`.
    StdSyncAll,
    /// The standard library's `File::sync_data`.
    StdSyncData,
}

impl SyncCall {
    /// The call that is durable across power loss on this operating system (see the measurement notes).
    pub fn durable() -> SyncCall {
        #[cfg(target_vendor = "apple")]
        return SyncCall::FullFsync;
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
        return SyncCall::Fdatasync;
        #[allow(unreachable_code)]
        SyncCall::StdSyncData
    }

    /// Every call available on this operating system.
    #[allow(clippy::vec_init_then_push, reason = "the pushes depend on the target")]
    pub fn all() -> Vec<SyncCall> {
        let mut v = Vec::new();
        #[cfg(unix)]
        v.push(SyncCall::Fsync);
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
        v.push(SyncCall::Fdatasync);
        #[cfg(target_vendor = "apple")]
        v.push(SyncCall::FullFsync);
        v.push(SyncCall::StdSyncAll);
        v.push(SyncCall::StdSyncData);
        v
    }

    /// Short name used in the measurement tables.
    pub fn name(self) -> &'static str {
        match self {
            #[cfg(unix)]
            SyncCall::Fsync => "fsync",
            #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
            SyncCall::Fdatasync => "fdatasync",
            #[cfg(target_vendor = "apple")]
            SyncCall::FullFsync => "F_FULLFSYNC",
            SyncCall::StdSyncAll => "std sync_all",
            SyncCall::StdSyncData => "std sync_data",
        }
    }

    /// Parses a name produced by [`SyncCall::name`].
    pub fn parse(s: &str) -> Option<SyncCall> {
        SyncCall::all().into_iter().find(|c| c.name() == s)
    }

    /// Syncs `file` with this call.
    pub fn sync(self, file: &std::fs::File) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            SyncCall::Fsync => rustix::fs::fsync(file).map_err(io::Error::from),
            #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
            SyncCall::Fdatasync => rustix::fs::fdatasync(file).map_err(io::Error::from),
            #[cfg(target_vendor = "apple")]
            SyncCall::FullFsync => rustix::fs::fcntl_fullfsync(file).map_err(io::Error::from),
            SyncCall::StdSyncAll => file.sync_all(),
            SyncCall::StdSyncData => file.sync_data(),
        }
    }
}

/// The real file system: files in one directory.
#[derive(Debug)]
pub struct RealFs {
    dir: PathBuf,
    call: SyncCall,
}

impl RealFs {
    /// Uses `dir` (creating it and its `log` subdirectory) and syncs files with `call`.
    pub fn new(dir: impl Into<PathBuf>, call: SyncCall) -> io::Result<RealFs> {
        let dir = dir.into();
        std::fs::create_dir_all(dir.join("log"))?;
        Ok(RealFs { dir, call })
    }

    /// The directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// Syncs a directory so that a created or renamed entry in it is durable.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

struct RealFile {
    file: std::fs::File,
    call: SyncCall,
}

impl LogFile for RealFile {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::FileExt::write_all_at(&self.file, buf, offset)
        }
        #[cfg(not(unix))]
        {
            use std::io::{Seek, SeekFrom, Write};
            self.file.seek(SeekFrom::Start(offset))?;
            self.file.write_all(buf)
        }
    }

    fn sync(&mut self) -> io::Result<()> {
        self.call.sync(&self.file)
    }
}

impl FileSystem for RealFs {
    fn create_preallocated(
        &self,
        name: &str,
        len: u64,
        header: &[u8],
    ) -> io::Result<Box<dyn LogFile>> {
        use std::io::Write;
        let path = self.dir.join(name);
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(header)?;
        // Zero-fill for real: `set_len` would leave a sparse file whose blocks are allocated at first write.
        let zeros = vec![0u8; 1 << 20];
        let mut left = len.saturating_sub(header.len() as u64);
        while left > 0 {
            let n = left.min(zeros.len() as u64) as usize;
            file.write_all(&zeros[..n])?;
            left -= n as u64;
        }
        // The size and allocation must be durable before the first batch relies on a data-only sync.
        file.sync_all()?;
        sync_dir(path.parent().unwrap_or(&self.dir))?;
        Ok(Box::new(RealFile {
            file,
            call: self.call,
        }))
    }

    fn open(&self, name: &str) -> io::Result<Box<dyn LogFile>> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join(name))?;
        Ok(Box::new(RealFile {
            file,
            call: self.call,
        }))
    }

    fn sync_log_dir(&self) -> io::Result<()> {
        sync_dir(&self.dir.join("log"))
    }

    fn space(&self) -> io::Result<Space> {
        #[cfg(unix)]
        {
            let s = rustix::fs::statvfs(&self.dir)?;
            Ok(Space {
                free: s.f_bavail * s.f_frsize,
                total: s.f_blocks * s.f_frsize,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Space {
                free: u64::MAX,
                total: u64::MAX,
            })
        }
    }
}

/// A fault the simulated file system returns once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// `EIO`.
    Io,
    /// `ENOSPC`.
    NoSpace,
}

impl Fault {
    fn error(self) -> io::Error {
        match self {
            Fault::Io => io::Error::other("injected I/O error"),
            Fault::NoSpace => io::Error::new(
                io::ErrorKind::StorageFull,
                "injected no space left on device",
            ),
        }
    }
}

#[derive(Default)]
struct SimFile {
    /// What a process reads: the page cache.
    cache: Vec<u8>,
    /// What survives a power loss.
    disk: Vec<u8>,
    /// Whether the cache holds bytes not yet on disk.
    dirty: bool,
    syncs: usize,
}

#[derive(Default)]
struct SimState {
    files: HashMap<String, SimFile>,
    space: Option<Space>,
    /// Prefix, number of matching writes to let through first, fault.
    write_faults: Vec<(String, usize, Fault)>,
    sync_faults: Vec<(String, Fault)>,
    create_faults: Vec<(String, Fault)>,
    /// Syncs of files whose name starts with this prefix block.
    syncs_paused: Option<String>,
    syncs_started: usize,
    /// Files whose cache reaches the disk after every write, as background writeback can.
    writeback_on_write: Vec<String>,
    dir_syncs: usize,
}

/// A simulated file system with a page cache, a disk, injectable faults and scripted free space.
///
/// A failed sync follows Linux on ext4 and XFS: the dirty pages are marked clean without reaching the disk,
/// so a later sync of the same file returns success although the bytes were never written. Dirtiness is kept
/// per file, not per page: a later write followed by a successful sync copies the whole cache, so it would
/// also write a failed batch's bytes, which Linux would not. The spike never syncs again after a failure.
#[derive(Clone, Default)]
pub struct SimFs {
    state: Arc<(Mutex<SimState>, Condvar)>,
}

impl SimFs {
    /// An empty file system with unlimited space.
    pub fn new() -> SimFs {
        SimFs::default()
    }

    fn lock(&self) -> MutexGuard<'_, SimState> {
        self.state.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sets the space [`FileSystem::space`] reports. Creating a file larger than `free` fails with `ENOSPC`.
    pub fn set_space(&self, free: u64, total: u64) {
        self.lock().space = Some(Space { free, total });
    }

    /// The next write to a file whose name starts with `prefix` fails with `fault`.
    pub fn fail_next_write(&self, prefix: &str, fault: Fault) {
        self.fail_write_after(prefix, 0, fault);
    }

    /// After `skip` more writes to a file whose name starts with `prefix`, the next one fails with `fault`.
    pub fn fail_write_after(&self, prefix: &str, skip: usize, fault: Fault) {
        self.lock()
            .write_faults
            .push((prefix.to_owned(), skip, fault));
    }

    /// From now on every write to a file whose name starts with `prefix` also reaches the disk at once, as
    /// background writeback may do between any two writes.
    pub fn writeback_on_write(&self, prefix: &str) {
        self.lock().writeback_on_write.push(prefix.to_owned());
    }

    /// Number of syncs of the log directory.
    pub fn dir_syncs(&self) -> usize {
        self.lock().dir_syncs
    }

    /// The next sync of a file whose name starts with `prefix` fails with `fault`.
    pub fn fail_next_sync(&self, prefix: &str, fault: Fault) {
        self.lock().sync_faults.push((prefix.to_owned(), fault));
    }

    /// The next creation of a file whose name starts with `prefix` fails with `fault`, leaving a partial file.
    pub fn fail_next_create(&self, prefix: &str, fault: Fault) {
        self.lock().create_faults.push((prefix.to_owned(), fault));
    }

    /// Makes syncs of files whose name starts with `prefix` block until [`SimFs::resume_syncs`] (`""`: all).
    pub fn pause_syncs(&self, prefix: &str) {
        self.lock().syncs_paused = Some(prefix.to_owned());
    }

    /// Releases blocked syncs.
    pub fn resume_syncs(&self) {
        self.lock().syncs_paused = None;
        self.state.1.notify_all();
    }

    /// Number of syncs started so far, on any file.
    pub fn syncs_started(&self) -> usize {
        self.lock().syncs_started
    }

    /// Number of syncs of `name`.
    pub fn syncs_of(&self, name: &str) -> usize {
        self.lock().files.get(name).map_or(0, |f| f.syncs)
    }

    /// Names of the files, sorted.
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.lock().files.keys().cloned().collect();
        v.sort();
        v
    }

    /// What a process reads from `name`.
    pub fn read(&self, name: &str) -> Option<Vec<u8>> {
        self.lock().files.get(name).map(|f| f.cache.clone())
    }

    /// What is on the disk for `name`.
    pub fn read_disk(&self, name: &str) -> Option<Vec<u8>> {
        self.lock().files.get(name).map(|f| f.disk.clone())
    }

    /// Removes `name`, as if it were lost.
    pub fn remove(&self, name: &str) {
        self.lock().files.remove(name);
    }

    /// Power loss: every file reverts to what is on the disk.
    pub fn power_loss(&self) {
        for f in self.lock().files.values_mut() {
            f.cache = f.disk.clone();
            f.dirty = false;
        }
    }

    /// Syncs `name` as recovery would, through a fresh handle.
    pub fn sync_file(&self, name: &str) -> io::Result<()> {
        SimHandle {
            fs: self.clone(),
            name: name.to_owned(),
        }
        .sync()
    }

    /// Background writeback: the cache of `name` reaches the disk without a sync call.
    pub fn writeback(&self, name: &str) {
        if let Some(f) = self.lock().files.get_mut(name) {
            f.disk = f.cache.clone();
        }
    }

    /// Truncates `name` to `len` in the cache (recovery's truncation).
    pub fn truncate(&self, name: &str, len: u64) {
        if let Some(f) = self.lock().files.get_mut(name) {
            f.cache.truncate(len as usize);
            f.dirty = true;
        }
    }
}

fn take_fault(faults: &mut Vec<(String, Fault)>, name: &str) -> Option<Fault> {
    let i = faults
        .iter()
        .position(|(p, _)| name.starts_with(p.as_str()))?;
    Some(faults.remove(i).1)
}

struct SimHandle {
    fs: SimFs,
    name: String,
}

impl LogFile for SimHandle {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        let mut s = self.fs.lock();
        let name = self.name.as_str();
        if let Some(i) = s
            .write_faults
            .iter()
            .position(|(p, ..)| name.starts_with(p.as_str()))
        {
            if s.write_faults[i].1 == 0 {
                return Err(s.write_faults.remove(i).2.error());
            }
            s.write_faults[i].1 -= 1;
        }
        let writeback = s
            .writeback_on_write
            .iter()
            .any(|p| name.starts_with(p.as_str()));
        let f = s
            .files
            .get_mut(&self.name)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let end = offset as usize + buf.len();
        if f.cache.len() < end {
            f.cache.resize(end, 0);
        }
        f.cache[offset as usize..end].copy_from_slice(buf);
        f.dirty = true;
        if writeback {
            f.disk = f.cache.clone();
        }
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        let (lock, wake) = &*self.fs.state;
        let mut s = lock.lock().unwrap_or_else(|e| e.into_inner());
        s.syncs_started += 1;
        while s
            .syncs_paused
            .as_deref()
            .is_some_and(|p| self.name.starts_with(p))
        {
            s = wake.wait(s).unwrap_or_else(|e| e.into_inner());
        }
        let fault = take_fault(&mut s.sync_faults, &self.name);
        let f = s
            .files
            .get_mut(&self.name)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        f.syncs += 1;
        if let Some(fault) = fault {
            // Linux after a failed writeback: the pages are clean in the cache, not written on the disk.
            f.dirty = false;
            return Err(fault.error());
        }
        if f.dirty {
            f.disk = f.cache.clone();
            f.dirty = false;
        }
        Ok(())
    }
}

impl FileSystem for SimFs {
    fn create_preallocated(
        &self,
        name: &str,
        len: u64,
        header: &[u8],
    ) -> io::Result<Box<dyn LogFile>> {
        let mut s = self.lock();
        if s.files.contains_key(name) {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        let fault = take_fault(&mut s.create_faults, name);
        let short = match s.space {
            Some(space) if space.free < len => Some(space.free),
            _ => None,
        };
        if fault.is_some() || short.is_some() {
            // A partial file is left behind, as a real zero-fill that runs out of space would.
            let partial = short.unwrap_or(len / 2) as usize;
            s.files.insert(
                name.to_owned(),
                SimFile {
                    cache: vec![0; partial],
                    ..SimFile::default()
                },
            );
            return Err(fault.unwrap_or(Fault::NoSpace).error());
        }
        if let Some(space) = s.space.as_mut() {
            space.free -= len;
        }
        let mut bytes = vec![0; len as usize];
        bytes[..header.len()].copy_from_slice(header);
        s.files.insert(
            name.to_owned(),
            SimFile {
                cache: bytes.clone(),
                disk: bytes,
                dirty: false,
                syncs: 0,
            },
        );
        Ok(Box::new(SimHandle {
            fs: self.clone(),
            name: name.to_owned(),
        }))
    }

    fn open(&self, name: &str) -> io::Result<Box<dyn LogFile>> {
        self.lock().files.entry(name.to_owned()).or_default();
        Ok(Box::new(SimHandle {
            fs: self.clone(),
            name: name.to_owned(),
        }))
    }

    fn space(&self) -> io::Result<Space> {
        Ok(self.lock().space.unwrap_or(Space {
            free: u64::MAX / 2,
            total: u64::MAX / 2,
        }))
    }

    fn sync_log_dir(&self) -> io::Result<()> {
        self.lock().dir_syncs += 1;
        Ok(())
    }
}
