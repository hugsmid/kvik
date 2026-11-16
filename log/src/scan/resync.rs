//! Resynchronisation after an invalid region (`format.md` A.2), and the readable commit blocks of a tail.

use std::collections::BTreeMap;

use crate::batch::{
    Batch, BatchRead, MAX_BATCH_DATA, Refusal, SegmentIdentity, framing_holds, read_batch,
};
use crate::block::{BLOCK_HEADER_LEN, block_type};
use crate::body::{COMMIT_LEN, Commit};
use crate::bytes::{slice, u64_at};
use crate::error::FormatError;

use super::{ReadableCommit, Reads};

/// Where a chain of block headers ends in a commit block: its offset and the `batch_bytes` it claims.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChainEnd {
    at: u64,
    batch_bytes: u64,
}

/// What is known of the chain through a block start.
#[derive(Clone, Copy, Debug)]
enum Link {
    End(Option<ChainEnd>),
    /// The chain runs on through this block start, not yet walked: a walk stops more than a batch beyond its
    /// candidate, so the remembered starts span at most one batch (`format.md` A, common rules: scan bound).
    Through(u64),
}

/// The chains of the block starts walked so far, from the current position on; kept across the regions of a
/// scan, so each block start is walked once per scan (Q-110).
#[derive(Clone, Debug, Default)]
pub(crate) struct Chains {
    links: BTreeMap<u64, Link>,
    pruned: u64,
    /// The reference scan's walks run to the chain's end, as before the scan bound.
    reference: bool,
    #[cfg(test)]
    pub(crate) peak: usize,
}

/// The readable commit blocks of a tail (`spec.md` §3 Recovery rule 3), collected alongside the candidates so the
/// prefix table is asked in rising order.
#[derive(Debug)]
pub(crate) struct CommitSearch {
    next: u64,
    end: u64,
    pub(crate) found: Vec<ReadableCommit>,
}

impl CommitSearch {
    /// A search of the steps in `[from, end)`, `end` being just past the last non-zero byte.
    pub(crate) fn new(from: u64, end: u64) -> Self {
        Self {
            next: from,
            end,
            found: Vec::new(),
        }
    }

    /// Visits every step below `to`.
    fn advance(&mut self, reads: &mut Reads<'_>, to: u64, limit: u64) {
        let to = to.min(self.end);
        while self.next < to && self.next + (BLOCK_HEADER_LEN + COMMIT_LEN) as u64 <= limit {
            self.next = commit_step(reads, self.next, limit, &mut self.found);
        }
    }

    /// The commits found below `end`.
    pub(crate) fn below(mut self, end: u64) -> Vec<ReadableCommit> {
        self.found.retain(|c| c.offset < end);
        self.found
    }
}

/// One step of the commit search at `o`: past the commit block if one is readable there, else 8 bytes on.
fn commit_step(reads: &mut Reads<'_>, o: u64, limit: u64, out: &mut Vec<ReadableCommit>) -> u64 {
    // Type and length are tested on the header before the body checksum is computed.
    if reads
        .peek(o, limit)
        .is_some_and(|h| h.block_type == block_type::COMMIT && h.body_len as usize == COMMIT_LEN)
        && let Ok(b) = reads.block(o, limit)
        && let Ok(commit) = Commit::decode(b.body)
    {
        out.push(ReadableCommit { offset: o, commit });
        return b.next_offset();
    }
    o + 8
}

/// Checksum-valid commit blocks starting at 8-byte steps in `[from, to)` and ending below `limit`.
pub(crate) fn readable_commits(
    reads: &mut Reads<'_>,
    from: u64,
    to: u64,
    limit: u64,
) -> Vec<ReadableCommit> {
    let mut out = Vec::new();
    let mut o = from;
    while o < to && o + (BLOCK_HEADER_LEN + COMMIT_LEN) as u64 <= limit {
        o = commit_step(reads, o, limit, &mut out);
    }
    out
}

impl Chains {
    /// The chains of one region of the reference scan.
    pub(crate) fn reference() -> Self {
        Self {
            reference: true,
            ..Self::default()
        }
    }

    /// Steps forward 8 bytes at a time from an invalid region at `start` and returns the first complete batch
    /// (A.2). With `commits`, also runs the tail's commit search up to the limit.
    ///
    /// Candidates are judged on block headers first. Each chain of consecutive block headers is walked once,
    /// without checksumming any body: the commit block it ends in is remembered for every block start on it, and a
    /// block too long for a batch ends the chain. A candidate `o` goes further only when that commit's
    /// `batch_bytes` is the distance from `o`, which one candidate per commit block is; then its framing and
    /// `batch_crc32c` are checked on headers ([`framing_holds`]); only a candidate that passes is read in full,
    /// its body checksums taken from the prefix table (Q-110).
    pub(crate) fn resync<'a>(
        &mut self,
        reads: &mut Reads<'a>,
        id: &SegmentIdentity,
        start: u64,
        limit: u64,
        min_first: u64,
        mut commits: Option<&mut CommitSearch>,
    ) -> Result<Option<Batch<'a>>, FormatError> {
        let mut o = start + 8;
        while o + (2 * BLOCK_HEADER_LEN + COMMIT_LEN) as u64 <= limit {
            if let Some(c) = commits.as_deref_mut() {
                c.advance(reads, o, limit);
            }
            // Block starts below `o`, and table entries below the commit search, are never asked about again.
            if o.saturating_sub(self.pruned) >= 1 << 16 {
                self.links = self.links.split_off(&o);
                self.pruned = o;
                reads.forget_below(commits.as_deref().map_or(o, |c| c.next.min(o)));
            }
            // A zero word cannot start a block header (type 0 is never valid), and a header that fails its
            // checksum cannot start a batch: both are passed without reading further.
            if slice(reads.bytes, o, 8).is_none_or(|w| u64_at(w, 0) == 0)
                || reads.peek(o, limit).is_none()
            {
                o += 8;
                continue;
            }
            let candidate = self.chain_end(reads, o, limit).is_some_and(|c| {
                c.at > o && c.batch_bytes == c.at - o && c.at - o <= MAX_BATCH_DATA
            });
            if candidate && framing_holds(reads, id, o, limit) {
                match read_batch(reads, id, o, limit, min_first, Refusal::IfBatchVerifies) {
                    BatchRead::Complete(b) => return Ok(Some(b)),
                    BatchRead::Refused(e) => return Err(e),
                    BatchRead::Incomplete { .. } => {}
                }
            }
            o += 8;
        }
        if let Some(c) = commits {
            c.advance(reads, limit, limit);
        }
        Ok(None)
    }

    /// The commit block that the chain of block headers from `o` ends in, or `None` if the chain ends in anything
    /// else: a header that is not valid, a seal block, or a block too long for a batch, or if it runs on more than a
    /// batch beyond `o`. Bodies are not read, except the `batch_bytes` field of a commit body. Every block start
    /// walked, the failing one included, is remembered.
    fn chain_end(&mut self, reads: &mut Reads<'_>, o: u64, limit: u64) -> Option<ChainEnd> {
        let mut walked = Vec::new();
        let mut p = o;
        let end = loop {
            match self.links.get(&p) {
                Some(Link::End(known)) => break Some(*known),
                Some(Link::Through(q)) => {
                    walked.push(p);
                    p = *q;
                    continue;
                }
                None => {}
            }
            if !self.reference && p - o > MAX_BATCH_DATA {
                break None;
            }
            walked.push(p);
            let Some(h) = reads.peek(p, limit) else {
                break Some(None);
            };
            match h.block_type {
                block_type::COMMIT if h.body_len as usize == COMMIT_LEN => {
                    let field = p + (BLOCK_HEADER_LEN + 32) as u64;
                    break Some(
                        slice(reads.bytes, field, 8)
                            .filter(|_| p + (BLOCK_HEADER_LEN + COMMIT_LEN) as u64 <= limit)
                            .map(|b| ChainEnd {
                                at: p,
                                batch_bytes: u64_at(b, 0),
                            }),
                    );
                }
                block_type::COMMIT | block_type::SEGMENT_INDEX | block_type::TRAILER => {
                    break Some(None);
                }
                _ if h.block_len() > MAX_BATCH_DATA => break Some(None),
                _ => p += h.block_len(),
            }
        };
        let link = end.map_or(Link::Through(p), Link::End);
        for w in walked {
            self.links.insert(w, link);
        }
        #[cfg(test)]
        {
            self.peak = self.peak.max(self.links.len());
        }
        end.flatten()
    }
}
