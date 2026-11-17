//! The work of the `kvik-maint` thread for the log: creating a segment ahead of use and sealing one
//! (`format.md` A.4). Each function only performs file operations; the writer decides when they run and what their
//! results mean.

use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::batch::SegmentIdentity;
use crate::block::BLOCK_HEADER_LEN;
use crate::body::{INDEX_HAS_ENTRIES, IndexRecord, SegmentIndex};
use crate::bytes::u64_at;
use crate::crc::Crc32c;
use crate::directory::logdir::SegmentRecord;
use crate::fs::{Fs, FsFile, OpenMode, is_sharing_error};
use crate::header::{FileHeader, HEADER_LEN, kind};
use crate::name::SegmentName;
use crate::segment::{SealSummary, encode_seal_streamed};

use super::{Fault, FaultOp, Halt};

/// Bytes written per call while zero-filling a new segment.
const FILL_CHUNK: usize = 1 << 20;
/// Bytes read per call while a seal computes `file_crc32c`.
const SEAL_READ: usize = 1 << 20;

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
    /// A length change or rename failed with a sharing error (another program holding the file, `format.md` A.13),
    /// or the rename for lack of space: retried later; the segment stays a complete `.open` file and health is
    /// degraded (`SealBlocked`).
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
///
/// A failed length change is blocked only for a sharing error ([`crate::fs::is_sharing_error`]); a failed rename
/// for a sharing error or lack of space. Any other failure of either is a write failure on the log. A rename that
/// failed although the `.open` name is gone and the `.kvl` name exists took effect and then failed its flush
/// (`StdFs` on Windows): a failed sync, never retried (`format.md` A.13).
pub fn seal_segment(fs: &dyn Fs, job: &SealJob, halt: &Halt) -> Result<(), SealError> {
    seal_described(fs, job, halt, false).map(|_| ())
}

/// [`seal_segment`], returning the sealed segment's log directory record (`format.md` A.18); with `hash`, its
/// SHA-256 is computed in the same pass as `file_crc32c` (zero otherwise).
pub fn seal_described(
    fs: &dyn Fs,
    job: &SealJob,
    halt: &Halt,
    hash: bool,
) -> Result<SegmentRecord, SealError> {
    let r = seal_inner(fs, job, halt, hash);
    if let Err(SealError::Fault(f)) = &r {
        halt.set(f.clone());
    }
    r
}

fn seal_inner(
    fs: &dyn Fs,
    job: &SealJob,
    halt: &Halt,
    hash: bool,
) -> Result<SegmentRecord, SealError> {
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
    // `file_crc32c` is computed over the segment up to its last batch, read `SEAL_READ` bytes at a time, so memory
    // at seal does not grow with `segment_size` (Q-115 (l)).
    let mut crc = Crc32c::new();
    let mut sha = Sha256::new();
    let mut buf = vec![0u8; SEAL_READ.min(job.end as usize)];
    let mut at = 0u64;
    while at < job.end {
        go()?;
        let want = (job.end - at).min(buf.len() as u64) as usize;
        match job.file.read_at(&mut buf[..want], at) {
            Ok(0) => {
                let e = io::Error::from(io::ErrorKind::UnexpectedEof);
                return Err(f(FaultOp::Seal, &e));
            }
            Ok(n) => {
                crc.update(&buf[..n]);
                if hash {
                    sha.update(&buf[..n]);
                }
                at += n as u64;
            }
            Err(e) => return Err(f(FaultOp::Seal, &e)),
        }
    }
    drop(buf);
    let index = SegmentIndex {
        flags: if job.has_entries {
            INDEX_HAS_ENTRIES
        } else {
            0
        },
        records: job.records.clone(),
    };
    let seal = encode_seal_streamed(crc, job.end, &job.summary, &index).map_err(|e| {
        SealError::Fault(Fault {
            op: FaultOp::Seal,
            path: path.clone(),
            kind: io::ErrorKind::InvalidData,
            message: e.to_string(),
        })
    })?;
    sha.update(&seal);
    let trailer_offset = u64_at(&seal, seal.len() - 16);
    let at = (trailer_offset - job.end) as usize + BLOCK_HEADER_LEN;
    let file_crc32c = crate::body::Trailer::decode(&seal[at..at + crate::body::TRAILER_LEN])
        .map_or(0, |t| t.file_crc32c);
    let record = SegmentRecord {
        file_id: job.id.file_id,
        generation: job.id.generation,
        has_entries: job.has_entries,
        first_sequence: job.summary.first_sequence,
        last_sequence: job.summary.last_sequence,
        first_commit_time: job.summary.first_commit_time,
        last_commit_time: job.summary.last_commit_time,
        fact_count: job.records.iter().map(|r| u64::from(r.fact_count)).sum(),
        len: job.end + seal.len() as u64,
        file_crc32c,
        sha256: if hash { sha.finalize().into() } else { [0; 32] },
    };
    go()?;
    job.file
        .write_at(&seal, job.end)
        .map_err(|e| f(FaultOp::Write, &e))?;
    go()?;
    job.file.set_len(job.end + seal.len() as u64).map_err(|e| {
        if is_sharing_error(&e) {
            SealError::Blocked(e.kind())
        } else {
            f(FaultOp::Write, &e)
        }
    })?;
    go()?;
    job.file.sync_all().map_err(|e| f(FaultOp::Sync, &e))?;
    go()?;
    let sealed = segment_path(&job.log_dir, &job.id, true);
    if let Err(e) = fs.rename(&path, &sealed) {
        let exists = |p: &Path| fs.open(p, OpenMode::Read).is_ok();
        return Err(if !exists(&path) && exists(&sealed) {
            SealError::Fault(fault(FaultOp::Sync, &sealed, &e))
        } else if is_sharing_error(&e) || e.kind() == io::ErrorKind::StorageFull {
            SealError::Blocked(e.kind())
        } else {
            f(FaultOp::Write, &e)
        });
    }
    go()?;
    fs.sync_dir(&job.log_dir)
        .map_err(|e| SealError::Fault(fault(FaultOp::Sync, &job.log_dir, &e)))?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::MemFs;
    use crate::scan::{ScanLimit, scan};

    /// A file that refuses reads of more than 1 MiB.
    #[derive(Debug)]
    struct Small(Box<dyn FsFile>);

    impl FsFile for Small {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
            if buf.len() > SEAL_READ {
                return Err(io::Error::other("a read over 1 MiB"));
            }
            self.0.read_at(buf, offset)
        }
        fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
            self.0.write_at(buf, offset)
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
            self.0.len()
        }
        fn try_lock_exclusive(&self) -> io::Result<bool> {
            self.0.try_lock_exclusive()
        }
    }

    #[test]
    fn sealing_reads_at_most_1_mib_at_a_time() {
        let fs = MemFs::new();
        let log_dir = PathBuf::from("store/log");
        let id = SegmentIdentity {
            store_id: [7; 16],
            file_id: 1,
            generation: 0,
        };
        let req = SegmentRequest {
            id,
            log_dir: log_dir.clone(),
            len: 4 << 20,
            created_at: 0,
        };
        let halt = Halt::default();
        let ahead = create_segment(&fs, &req, &halt).unwrap();
        // Bytes past the header that the checksum must cover, over more than three reads.
        let end = (3 << 20) + 4096;
        let body: Vec<u8> = (0..end - HEADER_LEN as u64).map(|i| i as u8).collect();
        ahead.file.write_at(&body, HEADER_LEN as u64).unwrap();
        let summary = SealSummary {
            entry_count: 1,
            first_sequence: 1,
            last_sequence: 1,
            first_commit_time: 0,
            last_commit_time: 0,
            batch_count: 1,
        };
        let job = SealJob {
            file: Box::new(Small(ahead.file)),
            id,
            log_dir: log_dir.clone(),
            end,
            summary,
            records: Vec::new(),
            has_entries: false,
        };
        let record = seal_described(&fs, &job, &halt, true).unwrap();
        let bytes = fs.read(&segment_path(&log_dir, &id, true)).unwrap();
        let seal = scan(&bytes, &id, ScanLimit::End).unwrap().seal.unwrap();
        assert_eq!(seal.start, end);
        assert!(seal.file_crc_matches(&bytes));
        let described = SegmentRecord {
            sha256: Sha256::digest(&bytes).into(),
            len: bytes.len() as u64,
            file_crc32c: seal.trailer.file_crc32c,
            ..record.clone()
        };
        assert_eq!(
            record, described,
            "the record's hash and checksum are the file's"
        );
    }
}
