//! The block reads of one scan, and their count (`format.md` A, common rules: scan bound; Q-110).

use crate::batch::MAX_BATCH_DATA;
use crate::block::{
    BLOCK_HEADER_LEN, BlockFault, BlockHeader, RawBlock, peek_raw, read_block_with,
};
use crate::body::{COMMIT_LEN, DataPrefix};
use crate::content::check_block;
use crate::content_error::BodyError;
use crate::crc::{PrefixCrc, crc32c};

/// Block headers a scan may visit per 8 bytes of the file: the sum of the passes over each block start (Q-110).
pub const HEADERS_PER_WORD: u64 = 16;

/// Candidates read in full and rejected after a body check, allowed in one window; past them the rest of the
/// window is passed as damaged (`format.md` A, common rules: scan bound; Q-110).
pub const FAILED_READS_PER_WINDOW: u64 = 8;

/// Failed reads counted per window of file offsets `[k · w, (k + 1) · w)` (`format.md` A, common rules: scan
/// bound): w is a batch's data in a log segment, a body in the manifest.
#[derive(Clone, Copy, Debug)]
pub struct FailedReads {
    window: u64,
    current: u64,
    count: u64,
    peak: u64,
}

impl FailedReads {
    /// No failed read yet, in windows of `window` bytes.
    pub fn new(window: u64) -> Self {
        Self {
            window,
            current: 0,
            count: 0,
            peak: 0,
        }
    }

    /// Counts a failed read of the candidate at `offset`; offsets come in rising order.
    pub fn count(&mut self, offset: u64) {
        let w = offset / self.window;
        if w != self.current {
            self.current = w;
            self.count = 0;
        }
        self.count += 1;
        self.peak = self.peak.max(self.count);
    }

    /// Where the search goes on from `offset`: `offset` itself, or the end of its window once that window has had
    /// [`FAILED_READS_PER_WINDOW`] failed reads.
    pub fn next_from(&self, offset: u64) -> u64 {
        let w = offset / self.window;
        if w == self.current && self.count >= FAILED_READS_PER_WINDOW {
            (w + 1).saturating_mul(self.window)
        } else {
            offset
        }
    }

    /// The most failed reads in one window.
    pub fn peak(&self) -> u64 {
        self.peak
    }
}

/// The work one scan did on a file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ScanStats {
    /// Bytes run through CRC32C, block header checksums excluded.
    pub bytes_checksummed: u64,
    /// Block headers read and checked (each a 12-byte checksum).
    pub headers_visited: u64,
    /// Body bytes passed to the checks of their type (A.5, A.6; a manifest record's fields, A.14).
    pub bytes_body_checked: u64,
    /// The most candidates read in full and rejected in one window ([`FailedReads`]).
    pub failed_reads_peak: u64,
}

impl ScanStats {
    /// Whether the counts keep the scan bound for a file of `len` bytes (`format.md` A, common rules: scan bound):
    /// at most `2 · len` bytes checksummed, plus once more the bytes of the one direct read that found damage (at
    /// most a batch); [`HEADERS_PER_WORD`] headers per 8 bytes; [`FAILED_READS_PER_WINDOW`] failed reads per
    /// window; and body checks of at most `2 · len`, that direct read, and two batches per failed read.
    pub fn within_bound(&self, len: u64) -> bool {
        let first_read = len.min(MAX_BATCH_DATA + (BLOCK_HEADER_LEN + COMMIT_LEN) as u64);
        let failed = (2 * FAILED_READS_PER_WINDOW)
            .saturating_mul(len.min(MAX_BATCH_DATA))
            .saturating_mul(len.div_ceil(MAX_BATCH_DATA));
        let twice = len.saturating_mul(2).saturating_add(first_read);
        self.bytes_checksummed <= twice
            && self.headers_visited <= (len / 8 + 2).saturating_mul(HEADERS_PER_WORD)
            && self.failed_reads_peak <= FAILED_READS_PER_WINDOW
            && self.bytes_body_checked <= twice.saturating_add(failed)
    }
}

/// Reads blocks of one file, taking body checksums from a prefix table, or directly for the reference scan.
#[derive(Clone, Debug)]
pub(crate) struct Reads<'a> {
    pub(crate) bytes: &'a [u8],
    sums: Option<PrefixCrc<'a>>,
    direct: u64,
    headers: u64,
    body: u64,
    pub(crate) failed: FailedReads,
}

impl<'a> Reads<'a> {
    /// Reads that checksum bodies directly until [`Reads::use_table`].
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            sums: None,
            direct: 0,
            headers: 0,
            body: 0,
            failed: FailedReads::new(MAX_BATCH_DATA),
        }
    }

    /// Takes body checksums from a prefix table from here on.
    pub(crate) fn use_table(&mut self) {
        if self.sums.is_none() {
            self.sums = Some(PrefixCrc::new(self.bytes));
        }
    }

    pub(crate) fn stats(&self) -> ScanStats {
        ScanStats {
            bytes_checksummed: self.direct + self.sums.as_ref().map_or(0, PrefixCrc::checksummed),
            headers_visited: self.headers,
            bytes_body_checked: self.body,
            failed_reads_peak: self.failed.peak(),
        }
    }

    /// Body bytes checked so far.
    pub(crate) fn body_checked(&self) -> u64 {
        self.body
    }

    /// The checks of a facts or entries body (A.5, A.6), counted.
    pub(crate) fn check_body(
        &mut self,
        t: u8,
        body: &[u8],
    ) -> Result<(DataPrefix, bool), BodyError> {
        self.body += body.len() as u64;
        check_block(t, body)
    }

    pub(crate) fn forget_below(&mut self, offset: u64) {
        if let Some(t) = &mut self.sums {
            t.forget_below(offset);
        }
    }

    pub(crate) fn peek(&mut self, offset: u64, limit: u64) -> Option<BlockHeader> {
        self.peek_raw(offset, limit).map(|(h, _)| h)
    }

    pub(crate) fn peek_raw(
        &mut self,
        offset: u64,
        limit: u64,
    ) -> Option<(BlockHeader, [u8; BLOCK_HEADER_LEN])> {
        self.headers += 1;
        peek_raw(self.bytes, offset, limit)
    }

    pub(crate) fn block(&mut self, offset: u64, limit: u64) -> Result<RawBlock<'a>, BlockFault> {
        self.headers += 1;
        let (sums, direct) = (&mut self.sums, &mut self.direct);
        read_block_with(self.bytes, offset, limit, |at, body| {
            let len = body.len() as u64;
            match sums.as_mut().and_then(|t| t.range(at, len)) {
                Some(c) => c,
                None => {
                    *direct += len;
                    crc32c(body)
                }
            }
        })
    }
}
