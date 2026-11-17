//! The chain ends of the bounded scan's resynchronisation, read a batch ahead of the candidates (Q-873).
//!
//! A candidate `o` is read further only when the chain of block headers from `o` ends in a commit block whose
//! `batch_bytes` is the distance from `o` (A.2), that is, a commit that names `o` as its batch's start. Where a chain
//! ends is a function of the file: a start's end is its successor's. Each start's header is read once, in blocks of
//! [`SPAN`] bytes from the highest start down, so a start whose successor lies in its block takes the successor's end
//! with one table read, and one that leaves the block keeps its exit; the starts of earlier blocks whose exits lie in
//! the new block then take the ends found there. A start is then a candidate when its own entry names it.

use crate::batch::MAX_BATCH_DATA;
use crate::block::{BLOCK_HEADER_LEN, block_type};
use crate::body::COMMIT_LEN;
use crate::bytes::{slice, u64_at};

use super::Reads;

/// Bytes of starts whose ends are found in one pass.
pub(crate) const SPAN: u64 = MAX_BATCH_DATA / 2;

/// The chain ends in no commit block that names a start: at a header that is not valid or does not fit, a commit
/// block of the wrong length or whose `batch_bytes` is not a multiple of 8 from 8 to a batch, a seal block or a block
/// too long for a batch.
const NONE: u32 = 1;
/// The chain ends in a commit block that names the entry's target as its batch's start.
const NAMES: u32 = 2;
/// The chain leaves the block at the entry's target, not yet read when the block was.
const EXIT: u32 = 3;

/// What the chain from each start in `[base, ahead)` comes to, a word each in a ring indexed by offset: a tag and the
/// signed distance to its target in 8-byte steps; 0 for a start not read.
#[derive(Clone, Debug)]
pub(crate) struct Ends {
    /// [`SPAN`], or less for the differential tests, so small files cross blocks.
    span: u64,
    /// The ring's length less one, a power of two less one: it holds at least a batch and a block beyond a candidate
    /// and the 64 KiB it lags behind one.
    mask: u64,
    slots: Vec<u32>,
    /// A bit per start: a commit block of the commit length that fits below the limit starts there.
    commits: Vec<u64>,
    base: u64,
    ahead: u64,
}

/// Targets lie within two batches of their start, so the steps fit in the 30 bits above the tag.
fn entry(tag: u32, steps: i64) -> u32 {
    ((steps as i32 as u32) << 2) | tag
}

fn target(p: u64, w: u32) -> u64 {
    p.wrapping_add_signed(8 * i64::from((w as i32) >> 2))
}

/// The entry of a start `d` steps before the one whose entry is `w`, on the same chain.
fn rebase(w: u32, d: u64) -> u32 {
    if w & 3 == NONE {
        NONE
    } else {
        w.wrapping_add((d as u32) << 2)
    }
}

impl Ends {
    /// Ends found `span` bytes of starts at a time (a multiple of 8, at least 8).
    pub(crate) fn new(span: u64) -> Self {
        let span = span.clamp(8, SPAN) & !7;
        let len = ((MAX_BATCH_DATA + span + (1 << 16)) / 8 + 1).next_power_of_two();
        Self {
            span,
            mask: len - 1,
            slots: Vec::new(),
            commits: Vec::new(),
            base: 0,
            ahead: 0,
        }
    }

    fn index(&self, p: u64) -> usize {
        ((p >> 3) & self.mask) as usize
    }

    /// The entry of `p`, 0 if it was not read.
    #[inline]
    fn word(&self, p: u64) -> u32 {
        if p < self.base || p >= self.ahead {
            return 0;
        }
        self.slots[self.index(p)]
    }

    /// Forgets the starts below `o`.
    pub(crate) fn move_to(&mut self, o: u64) {
        if !self.slots.is_empty() {
            let n = (o.saturating_sub(self.base) / 8).min(self.mask + 1);
            for k in 0..n {
                let i = self.index(self.base + 8 * k);
                self.slots[i] = 0;
                self.commits[i / 64] &= !(1 << (i % 64));
            }
        }
        self.base = self.base.max(o);
        self.ahead = self.ahead.max(self.base);
    }

    /// Reads ahead until every start up to a batch beyond `o`, or up to `limit`, has its entry.
    fn fill(&mut self, reads: &mut Reads<'_>, o: u64, limit: u64) {
        let want = o.saturating_add(MAX_BATCH_DATA + 8).min(limit);
        if self.ahead >= want {
            return;
        }
        if self.slots.is_empty() {
            self.slots = vec![0; (self.mask + 1) as usize];
            self.commits = vec![0; (self.mask as usize + 1).div_ceil(64)];
        }
        while self.ahead < want {
            let start = self.ahead.max(o).next_multiple_of(8);
            let end = start.saturating_add(self.span).min(limit);
            let mut p = end.next_multiple_of(8);
            while p > start {
                p -= 8;
                let w = self.read(reads, p, end, limit);
                let i = self.index(p);
                self.slots[i] = w;
            }
            // Earlier starts whose chains left their block into this one take the ends found here, so every exit
            // lies at or past `ahead`.
            let mut p = o.max(self.base);
            while p < start {
                let i = self.index(p);
                let w = self.slots[i];
                if w & 3 == EXIT {
                    let e = target(p, w);
                    if e < end {
                        self.slots[i] = rebase(self.slots[self.index(e)], (e - p) / 8);
                    }
                }
                p += 8;
            }
            self.ahead = end.max(start);
        }
    }

    /// The entry of the start `p` in the block that ends at `end`, its successors in the block having theirs.
    fn read(&mut self, reads: &mut Reads<'_>, p: u64, end: u64, limit: u64) -> u32 {
        let header = slice(reads.bytes, p, 8)
            .is_some_and(|w| u64_at(w, 0) != 0)
            .then(|| reads.peek(p, limit))
            .flatten();
        let Some(h) = header else {
            return NONE;
        };
        match h.block_type {
            block_type::COMMIT
                if h.body_len as usize == COMMIT_LEN
                    && p + (BLOCK_HEADER_LEN + COMMIT_LEN) as u64 <= limit =>
            {
                let i = self.index(p);
                self.commits[i / 64] |= 1 << (i % 64);
                named(reads.bytes, p).map_or(NONE, |s| entry(NAMES, -(((p - s) / 8) as i64)))
            }
            block_type::COMMIT | block_type::SEGMENT_INDEX | block_type::TRAILER => NONE,
            _ if h.block_len() > MAX_BATCH_DATA => NONE,
            _ => {
                let q = p + h.block_len();
                if q < end {
                    rebase(self.slots[self.index(q)], (q - p) / 8)
                } else {
                    entry(EXIT, ((q - p) / 8) as i64)
                }
            }
        }
    }

    /// Whether a commit block of the commit length that fits below the limit starts at `p`, if its header was read.
    #[inline]
    pub(crate) fn commit_at(&self, p: u64) -> Option<bool> {
        (self.word(p) != 0).then(|| {
            let i = self.index(p);
            self.commits[i / 64] & (1 << (i % 64)) != 0
        })
    }

    /// The first start from `o` on, in steps of 8 bytes below `stop`, whose chain of block headers ends in a commit
    /// block whose `batch_bytes` is the distance from it, at most a batch: the candidates `Chains::chain_end` finds,
    /// and no others. Else the first step at or past `stop`.
    pub(crate) fn next_candidate(
        &mut self,
        reads: &mut Reads<'_>,
        mut o: u64,
        stop: u64,
        limit: u64,
    ) -> u64 {
        while o < stop {
            self.fill(reads, o, limit);
            // Every exit lies at or past `ahead`, so a start a batch below it, or any start once the limit is read,
            // is a candidate exactly when its chain ends in a commit that names it.
            let run = if self.ahead >= limit {
                stop
            } else {
                stop.min(self.ahead - MAX_BATCH_DATA - 7)
            };
            while o < run {
                if self.slots[self.index(o)] == NAMES {
                    return o;
                }
                o += 8;
            }
        }
        o
    }
}

/// The start the commit block at `commit` names: `batch_bytes` before it, a multiple of 8 from 8 to a batch.
fn named(bytes: &[u8], commit: u64) -> Option<u64> {
    let batch_bytes =
        slice(bytes, commit + (BLOCK_HEADER_LEN + 32) as u64, 8).map(|b| u64_at(b, 0))?;
    ((8..=MAX_BATCH_DATA).contains(&batch_bytes) && batch_bytes.is_multiple_of(8))
        .then(|| commit.checked_sub(batch_bytes))
        .flatten()
}
