//! The work of the `kvik-maint` thread for the log: creating a segment ahead of use, sealing one (`format.md` A.4),
//! and describing a sealed one by its length and plaintext SHA-256 (`format.md` A.9, A.14). Each function only
//! performs file operations; the writer decides when they run and what their results mean.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

use crate::batch::SegmentIdentity;
use crate::block::{BLOCK_HEADER_LEN, block_type, read_block};
use crate::body::{
    END_MARKER_LEN, INDEX_HAS_ENTRIES, IndexRecord, SegmentIndex, TRAILER_LEN, Trailer,
    decode_end_marker,
};
use crate::crc::Crc32c;
use crate::fs::{Fs, FsFile, OpenMode, is_sharing_error};
use crate::header::{FileHeader, HEADER_LEN, kind};
use crate::name::SegmentName;
use crate::segment::{SealSummary, SealedFile, encode_seal_streamed};

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
    /// Kept across a blocked seal's retries, so the segment is read and hashed once per seal (Q-220 (c)).
    pub(crate) prepared: OnceLock<Prepared>,
}

#[derive(Debug)]
pub(crate) struct Prepared {
    seal: Vec<u8>,
    sha256: [u8; 32],
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
    /// A write, sync or read failed: a write or sync failure on the log (`spec.md` §3; a read, Q-221 (a)).
    Fault(Fault),
    /// Stopped before its next file operation because the store became read-only.
    Halted,
}

/// Seals a segment (`format.md` A.4 Sealing) as [`seal_segment_described`] does, without handing out its description.
pub fn seal_segment(fs: &dyn Fs, job: &SealJob, halt: &Halt) -> Result<(), SealError> {
    seal_segment_described(fs, job, halt).map(drop)
}

/// Seals a segment (`format.md` A.4 Sealing) and returns its description, the hash taken in the pass that computes
/// `file_crc32c`, on the calling thread (Q-220): (1) writes the segment index, the trailer and the end marker just
/// past the last complete batch; (2) sets the length to the end marker's end; (3) `sync_all`; (4) renames `.open` to
/// `.kvl`; (5) syncs the directory. Run again from step 1 after [`SealError::Blocked`]. `halt` is checked before
/// every step and set on a failure, as for [`create_segment`].
///
/// A failed length change is blocked only for a sharing error ([`crate::fs::is_sharing_error`]); a failed rename
/// for a sharing error or lack of space. Any other failure of either is a write failure on the log. A rename that
/// failed although the `.open` name is gone and the `.kvl` name exists took effect and then failed its flush
/// (`StdFs` on Windows): a failed sync, never retried (`format.md` A.13).
pub fn seal_segment_described(
    fs: &dyn Fs,
    job: &SealJob,
    halt: &Halt,
) -> Result<SealedFile, SealError> {
    let r = seal_inner(fs, job, halt);
    if let Err(SealError::Fault(f)) = &r {
        halt.set(f.clone());
    }
    r
}

/// Reads `file` from 0 to `end`, `SEAL_READ` bytes at a time, so memory does not grow with `segment_size`
/// (Q-115 (l)); `each` gets every piece with its offset.
fn read_through(
    file: &dyn FsFile,
    end: u64,
    halt: Option<&Halt>,
    mut each: impl FnMut(u64, &[u8]),
) -> Result<(), Option<io::Error>> {
    let mut buf = vec![0u8; SEAL_READ.min(usize::try_from(end).unwrap_or(SEAL_READ))];
    let mut at = 0u64;
    while at < end {
        if halt.is_some_and(|h| h.is_set()) {
            return Err(None);
        }
        let want = usize::try_from(end - at).map_or(buf.len(), |n| n.min(buf.len()));
        match file.read_at(&mut buf[..want], at) {
            Ok(0) => return Err(Some(io::ErrorKind::UnexpectedEof.into())),
            Ok(n) => {
                each(at, &buf[..n]);
                at += n as u64;
            }
            Err(e) => return Err(Some(e)),
        }
    }
    Ok(())
}

fn prepare(job: &SealJob, path: &Path, halt: &Halt) -> Result<Prepared, SealError> {
    let mut crc = Crc32c::new();
    let mut sha = Sha256::new();
    read_through(&*job.file, job.end, Some(halt), |_, b| {
        crc.update(b);
        sha.update(b);
    })
    .map_err(|e| match e {
        None => SealError::Halted,
        Some(e) => SealError::Fault(fault(FaultOp::Seal, path, &e)),
    })?;
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
            path: path.to_path_buf(),
            kind: io::ErrorKind::InvalidData,
            message: e.to_string(),
        })
    })?;
    sha.update(&seal);
    Ok(Prepared {
        seal,
        sha256: sha.finalize().into(),
    })
}

fn seal_inner(fs: &dyn Fs, job: &SealJob, halt: &Halt) -> Result<SealedFile, SealError> {
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
    let p = match job.prepared.get() {
        Some(p) => p,
        None => {
            let p = prepare(job, &path, halt)?;
            job.prepared.get_or_init(|| p)
        }
    };
    go()?;
    job.file
        .write_at(&p.seal, job.end)
        .map_err(|e| f(FaultOp::Write, &e))?;
    go()?;
    let len = job.end + p.seal.len() as u64;
    job.file.set_len(len).map_err(|e| {
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
    let s = &job.summary;
    Ok(SealedFile {
        id: job.id,
        len,
        sha256: p.sha256,
        sequences: (s.batch_count > 0).then_some((s.first_sequence, s.last_sequence)),
        commit_times: (s.batch_count > 0).then_some((s.first_commit_time, s.last_commit_time)),
    })
}

/// Why [`describe_sealed`] gave no description (Q-221 (c)).
#[derive(Debug, thiserror::Error)]
pub enum DescribeError {
    /// The file could not be opened or read whole.
    #[error("the segment could not be read: {0}")]
    Io(io::Error),
    /// Its last bytes are not a valid trailer block and end marker naming the trailer's offset.
    #[error("not a sealed segment")]
    NotSealed,
    /// It ends as a sealed segment but its header, trailer or `file_crc32c` does not agree.
    #[error("invalid sealed segment: {0}")]
    Invalid(&'static str),
}

/// The trailer block and the end marker: the last bytes of a sealed segment (A.9).
const TAIL_LEN: usize = BLOCK_HEADER_LEN + TRAILER_LEN + END_MARKER_LEN;

fn read_exact(file: &dyn FsFile, buf: &mut [u8], at: u64) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        match file.read_at(&mut buf[done..], at + done as u64)? {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => done += n,
        }
    }
    Ok(())
}

/// Describes the sealed segment `id` in `log_dir` from its own bytes, for a start's segments that no record lists
/// (`spec.md` §7 Manifest, Q-221): its header must agree with its name, its last bytes must be a valid trailer and end
/// marker, and `file_crc32c` must match, so a damaged file's hash is never taken as the hash at seal.
pub fn describe_sealed(
    fs: &dyn Fs,
    log_dir: &Path,
    id: &SegmentIdentity,
) -> Result<SealedFile, DescribeError> {
    let path = segment_path(log_dir, id, true);
    let file = fs.open(&path, OpenMode::Read).map_err(DescribeError::Io)?;
    let len = file.len().map_err(DescribeError::Io)?;
    let trailer_offset = len
        .checked_sub(TAIL_LEN as u64)
        .filter(|&o| o >= HEADER_LEN as u64)
        .ok_or(DescribeError::NotSealed)?;
    let mut head = [0u8; HEADER_LEN];
    let mut tail = [0u8; TAIL_LEN];
    read_exact(&*file, &mut head, 0).map_err(DescribeError::Io)?;
    read_exact(&*file, &mut tail, trailer_offset).map_err(DescribeError::Io)?;
    let marker_at = BLOCK_HEADER_LEN + TRAILER_LEN;
    if decode_end_marker(&tail[marker_at..]) != Some(trailer_offset) {
        return Err(DescribeError::NotSealed);
    }
    let trailer = read_block(&tail, 0, marker_at as u64)
        .ok()
        .filter(|b| b.header.block_type == block_type::TRAILER)
        .and_then(|b| Trailer::decode(b.body).ok())
        .ok_or(DescribeError::NotSealed)?;
    let header_agrees = FileHeader::decode(&head).is_some_and(|h| {
        h.kind == kind::LOG_SEGMENT
            && h.store_id == id.store_id
            && h.generation == id.generation
            && h.file_id == id.file_id
    });
    if !header_agrees {
        return Err(DescribeError::Invalid("header disagrees with the name"));
    }
    if trailer.batch_count == 0 || trailer.first_sequence > trailer.last_sequence {
        return Err(DescribeError::Invalid("trailer names no batches"));
    }
    let mut crc = Crc32c::new();
    let mut sha = Sha256::new();
    read_through(&*file, len, None, |at, b| {
        let below =
            usize::try_from(trailer_offset.saturating_sub(at)).map_or(b.len(), |n| n.min(b.len()));
        crc.update(&b[..below]);
        sha.update(b);
    })
    .map_err(|e| DescribeError::Io(e.unwrap_or_else(|| io::ErrorKind::Interrupted.into())))?;
    if crc.finish() != trailer.file_crc32c {
        return Err(DescribeError::Invalid("file_crc32c does not match"));
    }
    Ok(SealedFile {
        id: *id,
        len,
        sha256: sha.finalize().into(),
        sequences: Some((trailer.first_sequence, trailer.last_sequence)),
        commit_times: Some((trailer.first_commit_time, trailer.last_commit_time)),
    })
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
            prepared: Default::default(),
        };
        seal_segment(&fs, &job, &halt).unwrap();
        let bytes = fs.read(&segment_path(&log_dir, &id, true)).unwrap();
        let seal = scan(&bytes, &id, ScanLimit::End).unwrap().seal.unwrap();
        assert_eq!(seal.start, end);
        assert!(seal.file_crc_matches(&bytes));
    }
}
