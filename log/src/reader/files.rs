//! A reader's file access: the mark, the lease and the segments.

use std::io;

use super::{Fingerprint, LogReader, MARK_RETRIES, ReaderError, io_err};
use crate::crc::crc32c;
use crate::fs::{FsFile, OpenMode};
use crate::header::{FileHeader, HEADER_LEN, kind};
use crate::lease::{LEASE_LEN, LeaseSlot, SLOT_OFFSET};
use crate::mark::{MARK_LEN, MarkSlot, read_mark};
use crate::name::SegmentName;
use crate::store::MARK;

/// What a renewal wrote, and whether it found the lease gone or changed.
#[derive(Clone, Copy, Debug)]
pub(super) struct Renewal {
    slot: LeaseSlot,
    image: [u8; LEASE_LEN],
    lost: bool,
}

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

    /// The epoch the lease would name now: the lowest of the live cuts and `epoch`, the epoch of the pass about to
    /// read.
    pub(super) fn lease_epoch(&self, epoch: u64) -> u64 {
        let now = self.cfg.clock.monotonic();
        self.cuts
            .lowest(now, self.cfg.cut_max_age)
            .map_or(epoch, |l| l.min(epoch))
    }

    pub(super) fn renew(&mut self, epoch: u64, generation: u32) -> Result<(), ReaderError> {
        let s = self.renew_slot(epoch, generation)?;
        self.commit_lease(s);
        Ok(())
    }

    /// Writes the lease with the next counter, by path, naming the lowest epoch of the live cuts and `epoch`; creates
    /// it again if the writer deleted it. A lease found gone or changed since the reader last wrote it ends every
    /// cut, so it then names `epoch` (`spec.md` §7 Readers).
    pub(super) fn renew_slot(&self, epoch: u64, generation: u32) -> Result<Renewal, ReaderError> {
        let fs = &*self.cfg.fs;
        let path = &self.lease_path;
        let (f, found) = match fs.open(path, OpenMode::ReadWrite) {
            Ok(f) => {
                let b = read_upto(&*f, LEASE_LEN).map_err(io_err(path))?;
                (f, Some(b))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (
                fs.open(path, OpenMode::CreateNew).map_err(io_err(path))?,
                None,
            ),
            Err(e) => return Err(io_err(path)(e)),
        };
        let lost = self.image.is_some_and(|i| found.as_deref() != Some(&i[..]));
        let slot = LeaseSlot {
            epoch: if lost { epoch } else { self.lease_epoch(epoch) },
            refresh_counter: self.lease.refresh_counter + 1,
            ..self.lease
        };
        // The slot alone, in place, when the header is the lease's own; else the whole file.
        let header = found
            .as_deref()
            .and_then(|b| b.get(..HEADER_LEN))
            .filter(|h| {
                FileHeader::decode(h)
                    .is_some_and(|h| h.kind == kind::LEASE && h.store_id == self.store_id)
            });
        let image = match header {
            Some(h) => {
                let mut image = [0u8; LEASE_LEN];
                image[..HEADER_LEN].copy_from_slice(h);
                image[SLOT_OFFSET..].copy_from_slice(&slot.encode());
                f.write_at(&image[SLOT_OFFSET..], SLOT_OFFSET as u64)
                    .map(|()| image)
            }
            None => {
                let image = slot.encode_file(self.store_id, generation);
                f.write_at(&image, 0).map(|()| image)
            }
        }
        .map_err(io_err(path))?;
        Ok(Renewal { slot, image, lost })
    }

    pub(super) fn commit_lease(&mut self, r: Renewal) {
        if r.lost {
            self.cuts.lose_all();
        }
        self.lease = r.slot;
        self.image = Some(r.image);
        self.renewed_at = self.cfg.clock.monotonic();
        self.state.lease_renewed_at = self.cfg.clock.wall();
        self.state.epoch = r.slot.epoch;
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
