//! The work of the `kvik-maint` thread for the log: creating a segment ahead of use and sealing one
//! (`format.md` A.4). Each function only performs file operations; the writer decides when they run and what their
//! results mean.

use std::io;
use std::path::{Path, PathBuf};

use crate::batch::SegmentIdentity;
use crate::body::{INDEX_HAS_ENTRIES, IndexRecord, SegmentIndex};
use crate::fs::{Fs, FsFile, OpenMode};
use crate::header::{FileHeader, HEADER_LEN, kind};
use crate::name::SegmentName;
use crate::segment::{SealSummary, encode_seal};

use super::{Fault, FaultOp, Halt};

/// Bytes written per call while zero-filling a new segment.
const FILL_CHUNK: usize = 1 << 20;

/// A segment the writer wants created ahead of use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentRequest {
    /// Its identity.
    pub id: SegmentIdentity,
    /// The log directory.
    pub log_dir: PathBuf,
    /// Its length: `segment_size`.
    pub len: u64,
    /// Its header's `created_at`.
    pub created_at: i64,
}

impl SegmentRequest {
    /// Its path: `<log>/<generation>-<file_id>.open`.
    pub fn path(&self) -> PathBuf {
        segment_path(&self.log_dir, &self.id, false)
    }
}

pub(crate) fn segment_path(log_dir: &Path, id: &SegmentIdentity, sealed: bool) -> PathBuf {
    log_dir.join(
        SegmentName {
            generation: id.generation,
            file_id: id.file_id,
            sealed,
        }
        .to_string(),
    )
}

/// A segment created ahead of use: zero-filled, its header written with `base_sequence = 0`, synced, and its
/// directory synced.
#[derive(Debug)]
pub struct AheadSegment {
    pub(crate) file: Box<dyn FsFile>,
    pub(crate) id: SegmentIdentity,
    pub(crate) path: PathBuf,
    pub(crate) len: u64,
    pub(crate) created_at: i64,
}

impl AheadSegment {
    /// Its identity.
    pub fn id(&self) -> SegmentIdentity {
        self.id
    }
}

/// Why a segment could not be created.
#[derive(Debug)]
pub enum CreateError {
    /// Lack of space: the partial file was deleted; the writer retries when a free-space check passes
    /// (`spec.md` §3 Segments).
    NoSpace,
    /// Any other failure, a failed sync included: a write or sync failure on the log (`spec.md` §3).
    Fault(Fault),
    /// Stopped before its next file operation because the store became read-only.
    Halted,
}

fn fault(op: FaultOp, path: &Path, e: &io::Error) -> Fault {
    Fault {
        op,
        path: path.to_path_buf(),
        kind: e.kind(),
        message: e.to_string(),
    }
}

/// Creates the segment `req` names (`format.md` A.4 Creation): created new under its `.open` name, zero-filled to
/// its length by ordinary writes, header written with `base_sequence = 0`, then `sync_all` and a directory sync.
///
/// `halt` is the writer's read-only flag ([`super::LogWriter::halt`]): checked before every file operation,
/// and set here on a write or sync failure, so no file operation follows a fault on either thread.
pub fn create_segment(
    fs: &dyn Fs,
    req: &SegmentRequest,
    halt: &Halt,
) -> Result<AheadSegment, CreateError> {
    let r = create_inner(fs, req, halt);
    if let Err(CreateError::Fault(f)) = &r {
        halt.set(f.clone());
    }
    r
}

fn create_inner(
    fs: &dyn Fs,
    req: &SegmentRequest,
    halt: &Halt,
) -> Result<AheadSegment, CreateError> {
    let halted = || halt.is_set();
    if halted() {
        return Err(CreateError::Halted);
    }
    let path = req.path();
    let no_space = |e: &io::Error| e.kind() == io::ErrorKind::StorageFull;
    let file = match fs.open(&path, OpenMode::CreateNew) {
        Ok(f) => f,
        Err(e) if no_space(&e) => return Err(CreateError::NoSpace),
        Err(e) => return Err(CreateError::Fault(fault(FaultOp::Create, &path, &e))),
    };
    let header = FileHeader::new(
        kind::LOG_SEGMENT,
        req.id.generation,
        req.id.file_id,
        req.created_at,
        req.id.store_id,
    )
    .encode();
    let mut chunk = vec![0u8; FILL_CHUNK.min(req.len as usize)];
    chunk[..HEADER_LEN].copy_from_slice(&header);
    let mut at = 0u64;
    while at < req.len {
        if halted() {
            return Err(CreateError::Halted);
        }
        let n = (req.len - at).min(chunk.len() as u64) as usize;
        if let Err(e) = file.write_at(&chunk[..n], at) {
            if no_space(&e) {
                drop(file);
                // The partial file is deleted; if even that fails, the next start removes it (A.4 table).
                let _ = fs.remove_file(&path);
                return Err(CreateError::NoSpace);
            }
            return Err(CreateError::Fault(fault(FaultOp::Write, &path, &e)));
        }
        if at == 0 {
            chunk[..HEADER_LEN].fill(0);
        }
        at += n as u64;
    }
    if halted() {
        return Err(CreateError::Halted);
    }
    if let Err(e) = file.sync_all() {
        return Err(CreateError::Fault(fault(FaultOp::Sync, &path, &e)));
    }
    if halted() {
        return Err(CreateError::Halted);
    }
    if let Err(e) = fs.sync_dir(&req.log_dir) {
        return Err(CreateError::Fault(fault(FaultOp::Sync, &req.log_dir, &e)));
    }
    Ok(AheadSegment {
        file,
        id: req.id,
        path,
        len: req.len,
        created_at: req.created_at,
    })
}

/// A full or timed-out segment to seal; the next segment is in use and synced.
#[derive(Debug)]
pub struct SealJob {
    pub(crate) file: Box<dyn FsFile>,
    pub(crate) id: SegmentIdentity,
    pub(crate) log_dir: PathBuf,
    /// Just past the last complete batch.
    pub(crate) end: u64,
    pub(crate) summary: SealSummary,
    pub(crate) records: Vec<IndexRecord>,
    pub(crate) has_entries: bool,
}

impl SealJob {
    /// The segment's file id.
    pub fn file_id(&self) -> u64 {
        self.id.file_id
    }
}

/// Why a seal did not finish.
#[derive(Debug)]
pub enum SealError {
    /// A length change or rename failed (another program holding the file, `format.md` A.13): retried later; the
    /// segment stays a complete `.open` file and health is degraded (`SealBlocked`).
    Blocked(io::ErrorKind),
    /// A write or sync failed: a write or sync failure on the log (`spec.md` §3).
    Fault(Fault),
    /// Stopped before its next file operation because the store became read-only.
    Halted,
}

/// Seals a segment (`format.md` A.4 Sealing): (1) writes the segment index, the trailer and the end marker just
/// past the last complete batch; (2) sets the length to the end marker's end; (3) `sync_all`; (4) renames `.open`
/// to `.kvl`; (5) syncs the directory. Run again from step 1 after [`SealError::Blocked`]. `halt` is checked before
/// every step and set on a write or sync failure, as for [`create_segment`].
pub fn seal_segment(fs: &dyn Fs, job: &SealJob, halt: &Halt) -> Result<(), SealError> {
    let r = seal_inner(fs, job, halt);
    if let Err(SealError::Fault(f)) = &r {
        halt.set(f.clone());
    }
    r
}

fn seal_inner(fs: &dyn Fs, job: &SealJob, halt: &Halt) -> Result<(), SealError> {
    let go = || {
        if halt.is_set() {
            Err(SealError::Halted)
        } else {
            Ok(())
        }
    };
    go()?;
    let path = segment_path(&job.log_dir, &job.id, false);
    let f = |op, e: &io::Error| SealError::Fault(fault(op, &path, e));
    // The whole segment up to its last batch is read for `file_crc32c` (Q-115 (l)).
    let mut before = vec![0u8; job.end as usize];
    let mut at = 0usize;
    while at < before.len() {
        match job.file.read_at(&mut before[at..], at as u64) {
            Ok(0) => {
                let e = io::Error::from(io::ErrorKind::UnexpectedEof);
                return Err(f(FaultOp::Seal, &e));
            }
            Ok(n) => at += n,
            Err(e) => return Err(f(FaultOp::Seal, &e)),
        }
    }
    let index = SegmentIndex {
        flags: if job.has_entries {
            INDEX_HAS_ENTRIES
        } else {
            0
        },
        records: job.records.clone(),
    };
    let seal = encode_seal(&before, &job.summary, &index).map_err(|e| {
        SealError::Fault(Fault {
            op: FaultOp::Seal,
            path: path.clone(),
            kind: io::ErrorKind::InvalidData,
            message: e.to_string(),
        })
    })?;
    go()?;
    job.file
        .write_at(&seal, job.end)
        .map_err(|e| f(FaultOp::Write, &e))?;
    go()?;
    // A length change or rename that fails is retried later, whatever the error (Q-115 (m)).
    job.file
        .set_len(job.end + seal.len() as u64)
        .map_err(|e| SealError::Blocked(e.kind()))?;
    go()?;
    job.file.sync_all().map_err(|e| f(FaultOp::Sync, &e))?;
    go()?;
    fs.rename(&path, &segment_path(&job.log_dir, &job.id, true))
        .map_err(|e| SealError::Blocked(e.kind()))?;
    go()?;
    fs.sync_dir(&job.log_dir)
        .map_err(|e| SealError::Fault(fault(FaultOp::Sync, &job.log_dir, &e)))?;
    Ok(())
}
