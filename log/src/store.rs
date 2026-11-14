//! The store directory (`format.md` A.12): the store descriptor, the lock, the layout, and creating or opening a
//! store through the file-system seam.
//!
//! The log inside is opened, and recovered if it needs to be, by [`crate::writer::LogWriter::open`].

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::block::{encode_block, read_block};
use crate::error::{Element, FormatError};
use crate::fs::{Fs, FsFile, OpenMode};
use crate::header::{FileHeader, HEADER_LEN, kind};

/// The store descriptor's name.
pub const STORE: &str = "STORE";
/// The lock file's name.
pub const LOCK: &str = "LOCK";
/// The manifest's name.
pub const MANIFEST: &str = "MANIFEST";
/// The log directory's name.
pub const LOG_DIR: &str = "log";
/// The mark's name inside the log directory.
pub const MARK: &str = "MARK";
const STORE_TMP: &str = "STORE.tmp";
const MANIFEST_NEW: &str = "MANIFEST.new";
/// The lease directory's name (A.15).
pub const LEASES_DIR: &str = "leases";
const DIRS: [&str; 3] = [LOG_DIR, "cursors", LEASES_DIR];

/// Why a store directory cannot be opened or created.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// The directory holds other files and no store descriptor; it is never created over.
    #[error("the directory is not a store")]
    NotAStore,
    /// The store descriptor is invalid.
    #[error("the store descriptor is invalid: {0}")]
    Corrupt(&'static str),
    /// Another writer holds the lock.
    #[error("another writer holds the store's lock")]
    Locked,
    /// A version refusal in the store descriptor or the mark.
    #[error(transparent)]
    Unsupported(FormatError),
    /// A manifest record is corrupt: fetching it again or rebuilding the manifest is roadmap 6.3 (Q-116 (a)).
    #[error("a manifest record is corrupt")]
    ManifestCorrupt,
    /// A file operation failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// What a new store is created with (A.12).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewStore {
    /// The store's id: 16 random bytes.
    pub store_id: [u8; 16],
    /// Creation time, nanoseconds since the Unix epoch.
    pub created_at: i64,
    /// The site zone at creation (IANA name, at most 63 bytes).
    pub site_zone: String,
    /// The tzdata version at creation (ASCII, at most 15 bytes).
    pub tzdata: String,
    /// The creating software's version (an ignorable field).
    pub software: String,
}

/// The store descriptor (`STORE`, kind 7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreDescriptor {
    /// The store's id.
    pub store_id: [u8; 16],
    /// Creation time.
    pub created_at: i64,
    /// Field 1: the site zone at creation.
    pub site_zone: String,
    /// Field 2: the tzdata version at creation.
    pub tzdata: String,
    /// Field `0x8001`: the creating software's version, if present.
    pub software: Option<String>,
}

fn push_field(out: &mut Vec<u8>, id: u16, value: &[u8]) {
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&(value.len() as u16).to_le_bytes());
    out.extend_from_slice(value);
    out.resize(out.len().div_ceil(4) * 4, 0);
}

impl StoreDescriptor {
    /// The file's bytes: header, then one block of type `0x01` holding the field list.
    pub fn encode(&self) -> Result<Vec<u8>, StoreError> {
        if self.site_zone.is_empty() || self.site_zone.len() > 63 {
            return Err(StoreError::Corrupt("site zone of 1 to 63 bytes"));
        }
        if self.tzdata.is_empty() || self.tzdata.len() > 15 || !self.tzdata.is_ascii() {
            return Err(StoreError::Corrupt("tzdata version of 1 to 15 ASCII bytes"));
        }
        let mut body = Vec::new();
        push_field(&mut body, 1, self.site_zone.as_bytes());
        push_field(&mut body, 2, self.tzdata.as_bytes());
        if let Some(s) = &self.software {
            push_field(&mut body, 0x8001, &s.as_bytes()[..s.len().min(255)]);
        }
        let mut out = FileHeader::new(kind::STORE, 0, 0, self.created_at, self.store_id)
            .encode()
            .to_vec();
        encode_block(0x01, 0, &body, &mut out).map_err(|_| StoreError::Corrupt("body"))?;
        Ok(out)
    }

    /// Decodes a store descriptor.
    pub fn decode(bytes: &[u8]) -> Result<Self, StoreError> {
        let h = FileHeader::decode(bytes).ok_or(StoreError::Corrupt("header"))?;
        if h.format_major > crate::FORMAT_MAJOR {
            return Err(StoreError::Unsupported(FormatError::Unsupported {
                offset: 0,
                element: Element::FormatMajor(h.format_major),
            }));
        }
        if h.kind != kind::STORE {
            return Err(StoreError::Corrupt("kind"));
        }
        let b = read_block(bytes, HEADER_LEN as u64, bytes.len() as u64)
            .map_err(|_| StoreError::Corrupt("block"))?;
        if b.header.block_type != 0x01 {
            return Err(StoreError::Corrupt("block type"));
        }
        let (mut zone, mut tz, mut software) = (None, None, None);
        let mut at = 0usize;
        let body = b.body;
        while at < body.len() {
            let f = body.get(at..at + 4).ok_or(StoreError::Corrupt("field"))?;
            let id = u16::from_le_bytes([f[0], f[1]]);
            let len = usize::from(u16::from_le_bytes([f[2], f[3]]));
            let v = body
                .get(at + 4..at + 4 + len)
                .ok_or(StoreError::Corrupt("field"))?;
            let text =
                || String::from_utf8(v.to_vec()).map_err(|_| StoreError::Corrupt("field text"));
            match id {
                1 => zone = Some(text()?),
                2 => tz = Some(text()?),
                0x8001 => software = Some(text()?),
                x if x < 0x8000 => {
                    return Err(StoreError::Unsupported(FormatError::Unsupported {
                        offset: HEADER_LEN as u64,
                        element: Element::Field {
                            entry_type: 0,
                            field: x,
                        },
                    }));
                }
                _ => {}
            }
            at = (at + 4 + len).div_ceil(4) * 4;
        }
        Ok(Self {
            store_id: h.store_id,
            created_at: h.created_at,
            site_zone: zone.ok_or(StoreError::Corrupt("no site zone"))?,
            tzdata: tz.ok_or(StoreError::Corrupt("no tzdata version"))?,
            software,
        })
    }
}

/// An open store directory whose lock this handle holds.
#[derive(Debug)]
pub struct StoreDir {
    fs: Arc<dyn Fs>,
    dir: PathBuf,
    descriptor: StoreDescriptor,
    _lock: Box<dyn FsFile>,
}

fn names(fs: &dyn Fs, dir: &Path) -> io::Result<Vec<String>> {
    match fs.read_dir(dir) {
        Ok(v) => Ok(v.into_iter().map(|e| e.name).collect()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

pub(crate) fn read_all(fs: &dyn Fs, path: &Path) -> io::Result<Vec<u8>> {
    let f = fs.open(path, OpenMode::Read)?;
    let len = usize::try_from(f.len()?).map_err(io::Error::other)?;
    let mut buf = vec![0u8; len];
    let mut at = 0;
    while at < len {
        let n = f.read_at(&mut buf[at..], at as u64)?;
        if n == 0 {
            buf.truncate(at);
            break;
        }
        at += n;
    }
    Ok(buf)
}

fn take_lock(fs: &dyn Fs, dir: &Path) -> Result<Box<dyn FsFile>, StoreError> {
    let path = dir.join(LOCK);
    let f = match fs.open(&path, OpenMode::ReadWrite) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => fs.open(&path, OpenMode::CreateNew)?,
        Err(e) => return Err(e.into()),
    };
    if !f.try_lock_exclusive()? {
        return Err(StoreError::Locked);
    }
    Ok(f)
}

/// Reads the store descriptor of the store in `dir` without taking the lock, as a reader process does (A.12:
/// readers never take it). No descriptor is [`StoreError::NotAStore`].
pub fn read_descriptor(fs: &dyn Fs, dir: &Path) -> Result<StoreDescriptor, StoreError> {
    match read_all(fs, &dir.join(STORE)) {
        Ok(b) => StoreDescriptor::decode(&b),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(StoreError::NotAStore),
        Err(e) => Err(e.into()),
    }
}

impl StoreDir {
    /// Opens the store in `dir`, creating it from `new` if the directory does not exist, is empty, or holds only
    /// what a creation that did not finish leaves (A.12). Without `new`, such a directory is [`StoreError::NotAStore`].
    pub fn open(fs: Arc<dyn Fs>, dir: &Path, new: Option<&NewStore>) -> Result<Self, StoreError> {
        let top = names(&*fs, dir)?;
        if top.iter().any(|n| n == STORE) {
            let lock = take_lock(&*fs, dir)?;
            let descriptor = StoreDescriptor::decode(&read_all(&*fs, &dir.join(STORE))?)?;
            if top.iter().any(|n| n == MANIFEST_NEW) {
                fs.remove_file(&dir.join(MANIFEST_NEW))?;
            }
            return Ok(Self {
                fs,
                dir: dir.to_path_buf(),
                descriptor,
                _lock: lock,
            });
        }
        // No descriptor: only what creation makes may be present, with its directories empty.
        let unfinished = top.iter().all(|n| {
            [LOCK, MANIFEST, STORE_TMP].contains(&n.as_str()) || DIRS.contains(&n.as_str())
        });
        let mut empty_dirs = true;
        for d in DIRS {
            empty_dirs &= names(&*fs, &dir.join(d))?.is_empty();
        }
        let Some(new) = new.filter(|_| unfinished && empty_dirs) else {
            return Err(StoreError::NotAStore);
        };
        Self::create(fs, dir, &top, new)
    }

    fn create(
        fs: Arc<dyn Fs>,
        dir: &Path,
        left: &[String],
        new: &NewStore,
    ) -> Result<Self, StoreError> {
        let descriptor = StoreDescriptor {
            store_id: new.store_id,
            created_at: new.created_at,
            site_zone: new.site_zone.clone(),
            tzdata: new.tzdata.clone(),
            software: Some(new.software.clone()),
        };
        let store_bytes = descriptor.encode()?;
        fs.create_dir_all(dir)?;
        let lock = take_lock(&*fs, dir)?;
        for n in left
            .iter()
            .filter(|n| *n != LOCK && !DIRS.contains(&n.as_str()))
        {
            fs.remove_file(&dir.join(n))?;
        }
        for d in DIRS {
            fs.create_dir_all(&dir.join(d))?;
        }
        let m = fs.open(&dir.join(MANIFEST), OpenMode::CreateNew)?;
        m.write_at(
            &crate::manifest::encode_initial(new.store_id, new.created_at),
            0,
        )?;
        m.sync_all()?;
        drop(m);
        for d in DIRS {
            fs.sync_dir(&dir.join(d))?;
        }
        fs.sync_dir(dir)?;
        let t = fs.open(&dir.join(STORE_TMP), OpenMode::CreateNew)?;
        t.write_at(&store_bytes, 0)?;
        t.sync_all()?;
        drop(t);
        fs.rename(&dir.join(STORE_TMP), &dir.join(STORE))?;
        fs.sync_dir(dir)?;
        Ok(Self {
            fs,
            dir: dir.to_path_buf(),
            descriptor,
            _lock: lock,
        })
    }

    /// The store descriptor.
    pub fn descriptor(&self) -> &StoreDescriptor {
        &self.descriptor
    }

    /// The store directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The log directory.
    pub fn log_dir(&self) -> PathBuf {
        self.dir.join(LOG_DIR)
    }

    /// The file system.
    pub fn fs(&self) -> &Arc<dyn Fs> {
        &self.fs
    }

    /// Reads a whole file through the seam.
    pub fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        read_all(&*self.fs, path)
    }
}
