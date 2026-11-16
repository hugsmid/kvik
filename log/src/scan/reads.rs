//! The block reads of one scan, and their count (`format.md` A, common rules: scan bound; Q-110).

use crate::block::{
    BLOCK_HEADER_LEN, BlockFault, BlockHeader, RawBlock, peek_raw, read_block_with,
};
use crate::crc::{PrefixCrc, crc32c};

/// Block headers a scan may visit per 8 bytes of the file: the sum of the passes over each block start (Q-110).
pub const HEADERS_PER_WORD: u64 = 16;

/// The work one scan did on a file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ScanStats {
    /// Bytes run through CRC32C, block header checksums excluded.
    pub bytes_checksummed: u64,
    /// Block headers read and checked (each a 12-byte checksum).
    pub headers_visited: u64,
}

impl ScanStats {
    /// Whether the counts keep the scan bound for a file of `len` bytes: at most `2 · len` bytes checksummed and
    /// [`HEADERS_PER_WORD`] headers per 8 bytes (`format.md` A, common rules).
    pub fn within_bound(&self, len: u64) -> bool {
        self.bytes_checksummed <= len.saturating_mul(2)
            && self.headers_visited <= (len / 8 + 2).saturating_mul(HEADERS_PER_WORD)
    }
}

/// Reads blocks of one file, taking body checksums from a prefix table, or directly for the reference scan.
#[derive(Clone, Debug)]
pub(crate) struct Reads<'a> {
    pub(crate) bytes: &'a [u8],
    sums: Option<PrefixCrc<'a>>,
    direct: u64,
    headers: u64,
}

impl<'a> Reads<'a> {
    pub(crate) fn new(bytes: &'a [u8], table: bool) -> Self {
        Self {
            bytes,
            sums: table.then(|| PrefixCrc::new(bytes)),
            direct: 0,
            headers: 0,
        }
    }

    pub(crate) fn stats(&self) -> ScanStats {
        ScanStats {
            bytes_checksummed: self.direct + self.sums.as_ref().map_or(0, PrefixCrc::checksummed),
            headers_visited: self.headers,
        }
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
