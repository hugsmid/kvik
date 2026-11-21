//! A reader's file access: the mark, the lease and the segments.

use std::io;

use super::{Fingerprint, LogReader, MARK_RETRIES, ReaderError, io_err};
use crate::crc::crc32c;
use crate::fs::{FsFile, OpenMode};
use crate::header::HEADER_LEN;
use crate::mark::{MARK_LEN, MarkSlot, read_mark};
use crate::name::SegmentName;
use crate::store::MARK;

/// A segment opened by name, under the extension it was found with.
pub(super) type OpenSegment = (SegmentName, Box<dyn FsFile>);

impl LogReader {
    /// Reads the mark opened by path, and keeps the handle for [`LogReader::read_mark_held`].
    pub(super) fn read_mark(&self) -> Result<MarkSlot, ReaderError> {
        let path = self.log_dir.join(MARK);
        for _ in 0..=MARK_RETRIES {
            let bytes = match self.cfg.fs.open(&path, OpenMode::Read) {
                Ok(f) => {
                    let b = read_upto(&*f, MARK_LEN).map_err(io_err(&path))?;
                    *self.mark_file.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
                    b
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
                Err(e) => return Err(io_err(&path)(e)),
            };
            match read_mark(&bytes, &self.store_id) {
                Ok(Some((_, m))) => return Ok(m),
                Ok(None) => std::thread::yield_now(),
                Err(error) => return Err(ReaderError::Unsupported { path, error }),
            }
        }
        Err(ReaderError::MarkUnreadable)
    }

    /// Reads the mark through the handle the last [`LogReader::read_mark`] opened, one positioned read (the query
    /// design's section 3.1; the mark is written in place, `format.md` A.10); by path when there is none or it does
    /// not give a valid mark. A full refresh opens it by path again, so a replaced file is followed within one.
    pub(super) fn read_mark_held(&self) -> Result<MarkSlot, ReaderError> {
        let bytes = {
            let held = self.mark_file.lock().unwrap_or_else(|e| e.into_inner());
            held.as_deref().and_then(|f| read_upto(f, MARK_LEN).ok())
        };
        match bytes.map(|b| read_mark(&b, &self.store_id)) {
            Some(Ok(Some((_, m)))) => Ok(m),
            _ => self.read_mark(),
        }
    }

    /// The segments of the log by file id, under either extension; none for a missing directory.
    pub(super) fn segments(&self) -> Result<Vec<SegmentName>, ReaderError> {
        let mut v: Vec<SegmentName> = match self.cfg.fs.read_dir(&self.log_dir) {
            Ok(v) => v
                .into_iter()
                .filter_map(|e| SegmentName::parse(&e.name))
                .collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(io_err(&self.log_dir)(e)),
        };
        v.sort_by_key(|n| (n.file_id, n.sealed));
        v.dedup_by_key(|n| n.file_id);
        Ok(v)
    }

    /// Opens a segment by name; if it is not found, lists the directory and tries the other extension once
    /// (`format.md` A.13). `None` if it is gone.
    pub(super) fn open_segment(
        &self,
        name: SegmentName,
    ) -> Result<Option<OpenSegment>, ReaderError> {
        for n in [
            name,
            SegmentName {
                sealed: !name.sealed,
                ..name
            },
        ] {
            let path = self.log_dir.join(n.to_string());
            match self.cfg.fs.open(&path, OpenMode::Read) {
                Ok(f) => return Ok(Some((n, f))),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err(&path)(e)),
            }
        }
        Ok(None)
    }

    pub(super) fn find_segment(&self, file_id: u64) -> Result<Option<SegmentName>, ReaderError> {
        Ok(self.segments()?.into_iter().find(|n| n.file_id == file_id))
    }

    /// Whether the batch at `fp` still reads the same: the same bytes at the same place, in the segment it was read
    /// from under either extension.
    pub(super) fn still_reads(&self, fp: &Fingerprint) -> Result<bool, ReaderError> {
        let Some((n, f)) = self.open_segment(fp.segment)? else {
            return Ok(false);
        };
        let path = self.log_dir.join(n.to_string());
        let len =
            usize::try_from(fp.end - fp.offset).map_err(|e| io_err(&path)(io::Error::other(e)))?;
        let mut b = vec![0u8; len];
        let got = read_at_full(&*f, &mut b, fp.offset).map_err(io_err(&path))?;
        Ok(got == len && Some(crc32c(&b)) == fp.crc)
    }
}

/// Reads up to `n` bytes from the start of `f`.
pub(super) fn read_upto(f: &dyn FsFile, n: usize) -> io::Result<Vec<u8>> {
    let mut b = vec![0u8; n];
    let got = read_at_full(f, &mut b, 0)?;
    b.truncate(got);
    Ok(b)
}

/// Fills `buf` from `offset` until the end of the file; returns the bytes read.
pub(super) fn read_at_full(f: &dyn FsFile, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut at = 0;
    while at < buf.len() {
        let n = f.read_at(&mut buf[at..], offset + at as u64)?;
        if n == 0 {
            break;
        }
        at += n;
    }
    Ok(at)
}

/// Makes `b` the file's first `len` bytes as a scan resumed at `from` reads them: the header and the bytes from
/// `from`, zero past the end of the file. The bytes between keep what `b` held unless `whole`: a scan to the mark reads
/// only forward from `from`; one to the end looks for seal blocks backwards from the last non-zero byte.
pub(super) fn read_part(
    b: &mut Vec<u8>,
    f: &dyn FsFile,
    from: u64,
    len: u64,
    whole: bool,
) -> io::Result<()> {
    let len = usize::try_from(len).map_err(io::Error::other)?;
    let from = usize::try_from(from).map_err(io::Error::other)?.min(len);
    b.resize(len, 0);
    let head = len.min(HEADER_LEN);
    let got = read_at_full(f, &mut b[..head], 0)?;
    b[got..head].fill(0);
    if whole {
        b[head..from.max(head)].fill(0);
    }
    if from < len {
        let got = read_at_full(f, &mut b[from..], from as u64)?;
        b[from + got..].fill(0);
    }
    Ok(())
}
