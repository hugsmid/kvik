//! The file-system seam (`docs/api.md` Environment seams, `docs/format.md` A.13): every byte the engine persists
//! passes one of these calls, so the test implementation can fail any write or sync, run out of space, and drop
//! unsynced writes at a simulated power loss.
//!
//! | Implementation | Use |
//! |---|---|
//! | [`StdFs`] | the operating system's file system through `std::fs` |
//! | [`MemFs`] | in memory, for tests: injected faults, an operation log and a durable image per file |

use std::fmt;
use std::io;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

mod mem;

pub use mem::{FaultOp, MemFs, MemOp};

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
    /// Whether the entry is a directory.
    pub is_dir: bool,
}

impl DirEntry {
    /// A file's entry, for implementations of [`Fs::read_dir`].
    pub fn new(name: impl Into<String>, len: u64) -> Self {
        DirEntry {
            name: name.into(),
            len,
            is_dir: false,
        }
    }

    /// A directory's entry.
    pub fn directory(name: impl Into<String>) -> Self {
        DirEntry {
            is_dir: true,
            ..DirEntry::new(name, 0)
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
    /// The file's identity where the platform gives one; `None` otherwise. A file renamed over another has a new
    /// identity at that path.
    fn identity(&self) -> io::Result<Option<FileIdentity>> {
        Ok(None)
    }
}

/// A file's identity: on Unix, its device and inode numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    /// The device.
    pub device: u64,
    /// The file on that device.
    pub inode: u64,
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
                is_dir: meta.is_dir(),
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
    #[cfg(unix)]
    fn identity(&self) -> io::Result<Option<FileIdentity>> {
        use std::os::unix::fs::MetadataExt;
        let m = self.0.metadata()?;
        Ok(Some(FileIdentity {
            device: m.dev(),
            inode: m.ino(),
        }))
    }
    fn try_lock_exclusive(&self) -> io::Result<bool> {
        match self.0.try_lock() {
            Ok(()) => Ok(true),
            Err(std::fs::TryLockError::WouldBlock) => Ok(false),
            Err(std::fs::TryLockError::Error(e)) => Err(e),
        }
    }
}

/// Whether `e` is a sharing error: another handle keeps the file from being renamed or resized (`format.md` A.13).
/// On Windows these are the system errors 5 (access denied), 32 (sharing violation), 33 (lock violation) and 1224
/// (user-mapped section); on any platform, [`io::ErrorKind::ResourceBusy`].
pub fn is_sharing_error(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::ResourceBusy
        || (cfg!(windows) && matches!(e.raw_os_error(), Some(5 | 32 | 33 | 1224)))
}

#[cfg(unix)]
fn statvfs_free(path: &Path) -> io::Result<u64> {
    let s = rustix::fs::statvfs(path).map_err(io::Error::from)?;
    Ok(s.f_bavail.saturating_mul(s.f_frsize))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// A file growing on one thread while another lists its directory, removes and renames files: the two lock
    /// orders (a file's lock while growing it, the state's lock while reading each file's length) must not deadlock.
    #[test]
    fn growth_and_listing_on_two_threads_do_not_deadlock() {
        let fs = MemFs::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let a = fs.clone();
        let grow = std::thread::spawn(move || {
            let f = a.open(Path::new("d/grow"), OpenMode::CreateNew).unwrap();
            for i in 0..20_000u64 {
                f.write_at(&[1], i).unwrap();
                f.set_len(i + 1).unwrap();
            }
        });
        let b = fs.clone();
        let list = std::thread::spawn(move || {
            for i in 0..20_000u64 {
                b.read_dir(Path::new("d")).unwrap();
                if i % 64 == 0 {
                    let p = Path::new("d/other");
                    let _ = b.open(p, OpenMode::CreateNew);
                    b.rename(p, Path::new("d/moved")).unwrap();
                    b.remove_file(Path::new("d/moved")).unwrap();
                }
            }
        });
        std::thread::spawn(move || {
            grow.join().unwrap();
            list.join().unwrap();
            let _ = tx.send(());
        });
        rx.recv_timeout(std::time::Duration::from_secs(60))
            .expect("growing a file while listing its directory deadlocked");
    }

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
            vec![DirEntry::new("g", 0), DirEntry::directory("sub")]
        );
        let dir = std::env::temp_dir().join(format!("kvik-dirs-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        assert_eq!(
            StdFs.read_dir(&dir).unwrap(),
            vec![DirEntry::directory("sub")]
        );
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
        fs.sync_dir(p.parent().unwrap()).unwrap();
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
    fn power_loss_undoes_every_name_change_not_made_durable_by_a_directory_sync() {
        let fs = MemFs::new();
        let (kept, lost) = (Path::new("d/kept"), Path::new("d/lost"));
        for p in [kept, lost] {
            let f = fs.open(p, OpenMode::CreateNew).unwrap();
            f.write_at(b"x", 0).unwrap();
            f.sync_all().unwrap();
        }
        fs.sync_dir(Path::new("d")).unwrap();
        fs.rename(kept, Path::new("d/renamed")).unwrap();
        fs.open(Path::new("d/new"), OpenMode::CreateNew).unwrap();
        fs.remove_file(lost).unwrap();
        fs.power_loss();
        assert!(fs.read(kept).is_some() && fs.read(lost).is_some());
        assert!(fs.read(Path::new("d/renamed")).is_none() && fs.read(Path::new("d/new")).is_none());
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

    #[test]
    fn an_undone_rename_over_a_file_puts_the_replaced_file_back() {
        let mut seen = std::collections::BTreeSet::new();
        for seed in 0..64 {
            let fs = MemFs::new();
            let (old, new) = (Path::new("d/m"), Path::new("d/m.new"));
            for (p, b) in [(old, 1u8), (new, 2u8)] {
                let f = fs.open(p, OpenMode::CreateNew).unwrap();
                f.write_at(&[b; 8], 0).unwrap();
                f.sync_all().unwrap();
            }
            fs.sync_dir(Path::new("d")).unwrap();
            fs.rename(new, old).unwrap();
            fs.power_loss_partial(seed);
            // Either the rename stands, or both files are as they were: never the new name alone.
            let state = (fs.read(old), fs.read(new));
            assert!(
                state == (Some(vec![2; 8]), None) || state == (Some(vec![1; 8]), Some(vec![2; 8])),
                "{state:?}"
            );
            seen.insert(state);
        }
        assert_eq!(seen.len(), 2);
    }
}
