//! The block reads of one scan, and their count (`format.md` A, common rules: scan bound; Q-110).

use crate::batch::{DataBlock, MAX_BATCH_DATA};
use crate::block::{
    BLOCK_HEADER_LEN, BlockFault, BlockHeader, MAX_BODY_LEN, RawBlock, peek_raw, read_block_with,
    read_body,
};
use crate::body::{COMMIT_LEN, DataPrefix};
use crate::bytes::{slice, u32_at};
use crate::content::{DataBody, check_block};
use crate::content_error::BodyError;
use crate::crc::{PrefixCrc, crc32c};

/// Block headers a scan may visit per 8 bytes of the file: the sum of the passes over each block start (Q-110).
pub const HEADERS_PER_WORD: u64 = 16;

/// Body bytes that failed reads starting in one window may check; once reached, the rest of the window is passed as
/// damaged (`format.md` A, common rules: scan bound; Q-110). Twice a segment window: failed reads from damage check
/// disjoint bodies, so those starting in one window check less than the window and one batch beyond it.
pub const FAILED_READ_BUDGET: u64 = 16 << 20;

/// Body bytes checked by failed reads, per window of file offsets `[k · w, (k + 1) · w)` (`format.md` A, common
/// rules: scan bound): w is a batch's data in a log segment, a body in the manifest.
#[derive(Clone, Copy, Debug)]
pub struct FailedReads {
    window: u64,
    current: u64,
    charged: u64,
    peak: u64,
}

impl FailedReads {
    /// No failed read yet, in windows of `window` bytes (at least 1).
    pub fn new(window: u64) -> Self {
        Self {
            window: window.max(1),
            current: 0,
            charged: 0,
            peak: 0,
        }
    }

    /// Charges a failed read of the candidate at `offset` that checked `bytes` of body; offsets come in rising order.
    pub fn charge(&mut self, offset: u64, bytes: u64) {
        let w = offset / self.window;
        if w != self.current {
            self.current = w;
            self.charged = 0;
        }
        self.charged = self.charged.saturating_add(bytes);
        self.peak = self.peak.max(self.charged);
    }

    /// Where the search goes on from `offset`: `offset` itself, or the end of its window once failed reads starting
    /// in it have checked [`FAILED_READ_BUDGET`] bytes of body.
    pub fn next_from(&self, offset: u64) -> u64 {
        if self.charged < FAILED_READ_BUDGET {
            return offset;
        }
        let w = offset / self.window;
        if w == self.current {
            (w + 1).saturating_mul(self.window)
        } else {
            offset
        }
    }

    /// The most body bytes checked by failed reads starting in one window.
    pub fn peak(&self) -> u64 {
        self.peak
    }
}

/// The work one scan did on a file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ScanStats {
    /// Bytes run through CRC32C, block header checksums excluded.
    pub bytes_checksummed: u64,
    /// Block headers read and checked.
    pub headers_visited: u64,
    /// Of those, the headers whose 12-byte checksum was computed; one read again from the scan's memory takes none.
    pub header_checksums: u64,
    /// Body bytes passed to the checks of their type (A.5, A.6; a manifest record's fields, A.14).
    pub bytes_body_checked: u64,
    /// The most body bytes checked by failed reads starting in one window ([`FailedReads`]).
    pub failed_bytes_peak: u64,
}

impl ScanStats {
    /// Whether the counts keep the scan bound for a file of `len` bytes (`format.md` A, common rules: scan bound):
    /// at most `2 · len` bytes checksummed, plus once more the bytes of the one direct read that found damage (at
    /// most a batch); [`HEADERS_PER_WORD`] headers per 8 bytes; failed reads per window checking at most
    /// [`FAILED_READ_BUDGET`] plus the read that reached it (a batch); and body checks of at most `len` (each kept
    /// body once, Q-651), that direct read, and those failed reads.
    pub fn within_bound(&self, len: u64) -> bool {
        let first_read = len.min(MAX_BATCH_DATA + (BLOCK_HEADER_LEN + COMMIT_LEN) as u64);
        self.within(len, MAX_BATCH_DATA, first_read)
    }

    /// [`ScanStats::within_bound`] for a manifest, whose windows are a body (16 MiB) and which has no direct read:
    /// each valid record checked once, and per window the budget and the read that reached it (a body).
    pub fn within_manifest_bound(&self, len: u64) -> bool {
        self.within(len, u64::from(MAX_BODY_LEN), 0)
    }

    fn within(&self, len: u64, window: u64, first_read: u64) -> bool {
        let per_window = FAILED_READ_BUDGET.saturating_add(len.min(window));
        let failed = per_window.saturating_mul(len.div_ceil(window));
        let twice = len.saturating_mul(2).saturating_add(first_read);
        let once = len.saturating_add(first_read);
        self.bytes_checksummed <= twice
            && self.headers_visited <= (len / 8 + 2).saturating_mul(HEADERS_PER_WORD)
            && self.failed_bytes_peak <= per_window
            && self.bytes_body_checked <= once.saturating_add(failed)
    }
}

/// The starts whose block header the scan read and found valid once damage was found, a bit each in a ring indexed by
/// offset (Q-873): a header read there again is taken without its checksum.
#[derive(Clone, Debug)]
pub(crate) struct Valid {
    bits: Vec<u64>,
    base: u64,
    mask: u64,
}

impl Valid {
    /// A ring of `mask + 1` starts, a power of two.
    pub(crate) fn new(mask: u64) -> Self {
        Self {
            bits: vec![0; (mask as usize + 1).div_ceil(64)],
            base: 0,
            mask,
        }
    }

    #[inline]
    fn index(&self, p: u64) -> Option<usize> {
        (p.is_multiple_of(8) && p >= self.base && (p - self.base) / 8 <= self.mask)
            .then_some(((p >> 3) & self.mask) as usize)
    }

    pub(crate) fn mark(&mut self, p: u64) {
        if let Some(i) = self.index(p) {
            self.bits[i / 64] |= 1 << (i % 64);
        }
    }

    #[inline]
    pub(crate) fn holds(&self, p: u64) -> bool {
        self.index(p)
            .is_some_and(|i| self.bits[i / 64] & (1 << (i % 64)) != 0)
    }

    /// Forgets the starts below `o`.
    pub(crate) fn move_to(&mut self, o: u64) {
        let n = (o.saturating_sub(self.base) / 8).min(self.mask + 1);
        for k in 0..n {
            let i = (((self.base >> 3) + k) & self.mask) as usize;
            self.bits[i / 64] &= !(1 << (i % 64));
        }
        self.base = self.base.max(o);
    }
}

/// Reads blocks of one file, taking body checksums from a prefix table, or directly for the reference scan.
#[derive(Clone, Debug)]
pub(crate) struct Reads<'a> {
    pub(crate) bytes: &'a [u8],
    sums: Option<PrefixCrc<'a>>,
    direct: u64,
    headers: u64,
    checksums: u64,
    body: u64,
    pub(crate) failed: FailedReads,
    /// The last valid header read, with its offset: a block read asks for the header its size test just read.
    recent: Option<(u64, BlockHeader, [u8; BLOCK_HEADER_LEN])>,
    pub(crate) valid: Option<Valid>,
    /// An empty list of blocks whose memory a batch read takes and gives back if it keeps no batch.
    pub(crate) spare: Vec<DataBlock<'a>>,
}

impl<'a> Reads<'a> {
    /// Reads that checksum bodies directly until [`Reads::use_table`].
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            sums: None,
            direct: 0,
            headers: 0,
            checksums: 0,
            body: 0,
            failed: FailedReads::new(MAX_BATCH_DATA),
            recent: None,
            valid: None,
            spare: Vec::new(),
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
            header_checksums: self.checksums,
            bytes_body_checked: self.body,
            failed_bytes_peak: self.failed.peak(),
        }
    }

    /// Body bytes checked so far.
    pub(crate) fn body_checked(&self) -> u64 {
        self.body
    }

    /// The checks of a facts or entries body (A.5, A.6), counted.
    /// With `keep`, also the body as the checks decoded it.
    pub(crate) fn check_body(
        &mut self,
        t: u8,
        body: &[u8],
        keep: bool,
    ) -> Result<(DataPrefix, bool, Option<DataBody>), BodyError> {
        self.body += body.len() as u64;
        check_block(t, body, keep)
    }

    /// Forgets the prefix table and the valid headers below `offset`, which the scan does not ask about again.
    pub(crate) fn forget_below(&mut self, offset: u64) {
        if let Some(t) = &mut self.sums {
            t.forget_below(offset);
        }
        if let Some(v) = &mut self.valid {
            v.move_to(offset);
        }
    }

    #[inline]
    pub(crate) fn peek(&mut self, offset: u64, limit: u64) -> Option<BlockHeader> {
        self.peek_raw(offset, limit).map(|(h, _)| h)
    }

    #[inline]
    pub(crate) fn peek_raw(
        &mut self,
        offset: u64,
        limit: u64,
    ) -> Option<(BlockHeader, [u8; BLOCK_HEADER_LEN])> {
        self.headers += 1;
        if offset.checked_add(BLOCK_HEADER_LEN as u64)? > limit {
            return None;
        }
        match self.recent_at(offset, limit) {
            Some(found) => Some(found),
            None => self.checksum_header(offset, limit),
        }
    }

    /// [`Reads::peek`] for the chain ends, which read each start once: a header found valid before, else checksummed
    /// and remembered.
    #[inline]
    pub(crate) fn peek_ahead(&mut self, offset: u64, limit: u64) -> Option<BlockHeader> {
        self.headers += 1;
        if offset.checked_add(BLOCK_HEADER_LEN as u64)? > limit {
            return None;
        }
        if self.valid.as_ref().is_some_and(|v| v.holds(offset)) {
            let raw = slice(self.bytes, offset, BLOCK_HEADER_LEN as u64)?;
            return Some(BlockHeader {
                block_type: raw[0],
                flags: raw[1],
                body_len: u32_at(raw, 4),
                body_crc32c: u32_at(raw, 8),
            });
        }
        self.checksums += 1;
        let (h, _) = peek_raw(self.bytes, offset, limit)?;
        if let Some(v) = &mut self.valid {
            v.mark(offset);
        }
        Some(h)
    }

    fn checksum_header(
        &mut self,
        offset: u64,
        limit: u64,
    ) -> Option<(BlockHeader, [u8; BLOCK_HEADER_LEN])> {
        self.checksums += 1;
        let found = peek_raw(self.bytes, offset, limit)?;
        self.recent = Some((offset, found.0, found.1));
        if let Some(v) = &mut self.valid {
            v.mark(offset);
        }
        Some(found)
    }

    /// A header read again: the last one, or one found valid since damage, taken without its checksum.
    #[inline]
    fn recent_at(&self, offset: u64, limit: u64) -> Option<(BlockHeader, [u8; BLOCK_HEADER_LEN])> {
        if offset.checked_add(BLOCK_HEADER_LEN as u64)? > limit {
            return None;
        }
        if let Some(r) = self.recent.filter(|r| r.0 == offset) {
            return Some((r.1, r.2));
        }
        if !self.valid.as_ref().is_some_and(|v| v.holds(offset)) {
            return None;
        }
        let raw: [u8; BLOCK_HEADER_LEN] = slice(self.bytes, offset, BLOCK_HEADER_LEN as u64)?
            .try_into()
            .ok()?;
        let header = BlockHeader {
            block_type: raw[0],
            flags: raw[1],
            body_len: u32_at(&raw, 4),
            body_crc32c: u32_at(&raw, 8),
        };
        Some((header, raw))
    }

    pub(crate) fn block(&mut self, offset: u64, limit: u64) -> Result<RawBlock<'a>, BlockFault> {
        self.headers += 1;
        let known = self.recent_at(offset, limit);
        let (bytes, sums, direct) = (self.bytes, &mut self.sums, &mut self.direct);
        let crc = |at, body: &[u8]| {
            let len = body.len() as u64;
            match sums.as_mut().and_then(|t| t.range(at, len)) {
                Some(c) => c,
                None => {
                    *direct += len;
                    crc32c(body)
                }
            }
        };
        match known {
            Some((h, raw)) => read_body(bytes, offset, limit, h, raw, crc),
            None => {
                self.checksums += 1;
                read_block_with(bytes, offset, limit, crc)
            }
        }
    }
}
