//! A reader's file access: the mark, the manifest, the lease and the segments.

use std::io;

use super::{Fingerprint, LogReader, MARK_RETRIES, ReaderError, io_err};
use crate::crc::crc32c;
use crate::fs::{FsFile, OpenMode};
use crate::header::{FileHeader, HEADER_LEN, kind};
use crate::lease::{LeaseSlot, SLOT_OFFSET};
use crate::manifest::{LostRange, read_manifest};
use crate::mark::{MARK_LEN, MarkSlot, read_mark};
use crate::name::SegmentName;
use crate::store::{MANIFEST, MARK, read_all};

/// A segment opened by name, under the extension it was found with.
pub(super) type OpenSegment = (SegmentName, Box<dyn FsFile>);

impl LogReader {
    pub(super) fn read_mark(&self) -> Result<MarkSlot, ReaderError> {
        let path = self.log_dir.join(MARK);
        for _ in 0..=MARK_RETRIES {
            let bytes = match self.cfg.fs.open(&path, OpenMode::Read) {
                Ok(f) => read_upto(&*f, MARK_LEN).map_err(io_err(&path))?,
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

    pub(super) fn read_manifest(&self) -> Result<(u64, Vec<LostRange>), ReaderError> {
        let path = self.cfg.dir.join(MANIFEST);
        let bytes = read_all(&*self.cfg.fs, &path).map_err(io_err(&path))?;
        let m = read_manifest(&bytes, &self.store_id).map_err(ReaderError::Manifest)?;
        Ok((m.last_epoch(), m.lost_ranges().cloned().collect()))
    }

    pub(super) fn renew(&mut self, epoch: u64, generation: u32) -> Result<(), ReaderError> {
        let s = self.renew_slot(epoch, generation)?;
        self.commit_lease(s);
        Ok(())
    }

    /// Writes the lease with the next counter, by path; creates it again if the writer deleted it.
    pub(super) fn renew_slot(&self, epoch: u64, generation: u32) -> Result<LeaseSlot, ReaderError> {
        let slot = LeaseSlot {
            epoch,
            refresh_counter: self.lease.refresh_counter + 1,
            ..self.lease
        };
        let fs = &*self.cfg.fs;
        let path = &self.lease_path;
        match fs.open(path, OpenMode::ReadWrite) {
            // The slot alone, in place, when the header is the lease's own; else the whole file.
            Ok(f) => {
                let mut h = [0u8; HEADER_LEN];
                let ok = read_at_full(&*f, &mut h, 0).is_ok_and(|n| n == HEADER_LEN)
                    && FileHeader::decode(&h)
                        .is_some_and(|h| h.kind == kind::LEASE && h.store_id == self.store_id);
                if ok {
                    f.write_at(&slot.encode(), SLOT_OFFSET as u64)
                } else {
                    f.write_at(&slot.encode_file(self.store_id, generation), 0)
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => fs
                .open(path, OpenMode::CreateNew)
                .and_then(|f| f.write_at(&slot.encode_file(self.store_id, generation), 0)),
            Err(e) => Err(e),
        }
        .map_err(io_err(path))?;
        Ok(slot)
    }

    pub(super) fn commit_lease(&mut self, slot: LeaseSlot) {
        self.lease = slot;
        self.renewed_at = self.cfg.clock.monotonic();
        self.state.lease_renewed_at = self.cfg.clock.wall();
        self.state.epoch = slot.epoch;
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

    /// Whether the batch at `fp` still reads the same: the same bytes at the same place.
    pub(super) fn still_reads(&self, fp: &Fingerprint) -> Result<bool, ReaderError> {
        let Some(name) = self.find_segment(fp.file_id)? else {
            return Ok(false);
        };
        let Some((n, f)) = self.open_segment(name)? else {
            return Ok(false);
        };
        let path = self.log_dir.join(n.to_string());
        let len =
            usize::try_from(fp.end - fp.offset).map_err(|e| io_err(&path)(io::Error::other(e)))?;
        let mut b = vec![0u8; len];
        let got = read_at_full(&*f, &mut b, fp.offset).map_err(io_err(&path))?;
        Ok(got == len && crc32c(&b) == fp.crc)
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
