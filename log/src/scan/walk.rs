//! The batch read at a block boundary, with each block judged once per scan (Q-110).
//!
//! A boundary read walks the chain of blocks from a valid position to its first block that is not a passing data
//! block. Positions just past a kept batch can lie on a chain an earlier boundary read already walked, so the
//! verdicts are remembered: a block is judged once, whatever the number of boundary reads that cross it.

use std::collections::BTreeMap;

use crate::batch::{BatchFault, BatchRead, MAX_BATCH_DATA, Refusal, SegmentIdentity, read_batch};
use crate::block::block_type;
use crate::body::{COMMIT_LEN, Commit};
use crate::content::check_block;
use crate::content_error::BodyError;
use crate::error::{Element, FormatError};

use super::Reads;

/// Why a walk stops at a block, as `read_batch` judges it reached from a valid position (`format.md` A.2).
#[derive(Clone, Copy, Debug)]
enum Stop {
    /// A checksum-valid commit block of the commit length; `None` if its body does not decode.
    Commit(Option<Commit>),
    Fault(BatchFault),
    Refuse(Element),
}

#[derive(Clone, Copy, Debug)]
struct Event {
    stop: Stop,
    /// For a checksum-valid header of a type other than commit: the block's end, for the batch-size test that
    /// depends on where the walk started.
    data_end: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
enum Link {
    /// Every block from here to the position passes and the last of them ends there.
    Jump(u64),
    At(Event),
}

/// The verdicts of the blocks walked by boundary reads, from the current position on.
#[derive(Clone, Debug, Default)]
pub(crate) struct Walks {
    links: BTreeMap<u64, Link>,
    pruned: u64,
}

enum Judged {
    Pass(u64),
    Stop(Event),
    /// A data block that takes the batch past 8 MiB from this start: not judged.
    TooLong,
}

fn judge(reads: &mut Reads<'_>, p: u64, o: u64, limit: u64) -> Judged {
    let header = reads.peek(p, limit);
    let mut data_end = None;
    if let Some(h) = header {
        if h.block_type == block_type::COMMIT {
            if h.body_len as usize != COMMIT_LEN {
                return Judged::Stop(Event {
                    stop: Stop::Fault(BatchFault::Content),
                    data_end: None,
                });
            }
        } else {
            let end = p + h.block_len();
            if end - o > MAX_BATCH_DATA {
                return Judged::TooLong;
            }
            data_end = Some(end);
        }
    }
    let stop = |stop| Judged::Stop(Event { stop, data_end });
    let b = match reads.block(p, limit) {
        Ok(b) => b,
        Err(f) => return stop(Stop::Fault(BatchFault::Block(f))),
    };
    let t = b.header.block_type;
    match t {
        block_type::COMMIT => stop(Stop::Commit(Commit::decode(b.body).ok())),
        block_type::FACTS | block_type::ENTRIES => match check_block(t, b.body) {
            Ok(_) => Judged::Pass(b.next_offset()),
            Err(BodyError::Unsupported(element)) => stop(Stop::Refuse(element)),
            Err(_) => stop(Stop::Fault(BatchFault::Content)),
        },
        block_type::SEGMENT_INDEX | block_type::TRAILER => {
            stop(Stop::Fault(BatchFault::Disagreement))
        }
        _ if b.header.skippable() => Judged::Pass(b.next_offset()),
        _ => stop(Stop::Refuse(Element::BlockType(t))),
    }
}

impl Walks {
    /// `read_batch(o, Refusal::AtOnce)`, with the same result.
    pub(crate) fn read<'a>(
        &mut self,
        reads: &mut Reads<'a>,
        id: &SegmentIdentity,
        o: u64,
        limit: u64,
        min_first: u64,
    ) -> BatchRead<'a> {
        if o - self.pruned >= 1 << 16 {
            self.links = self.links.split_off(&o);
            self.pruned = o;
        }
        let incomplete = |fault| BatchRead::Incomplete { fault };
        let mut walked = Vec::new();
        let mut p = o;
        let end = loop {
            match self.links.get(&p).copied() {
                Some(Link::At(e)) => break Some(e),
                Some(Link::Jump(f)) => {
                    walked.push(p);
                    if f - o > MAX_BATCH_DATA {
                        break None;
                    }
                    p = f;
                }
                None => match judge(reads, p, o, limit) {
                    Judged::Pass(next) => {
                        walked.push(p);
                        p = next;
                    }
                    Judged::Stop(e) => {
                        self.links.insert(p, Link::At(e));
                        break Some(e);
                    }
                    Judged::TooLong => break None,
                },
            }
        };
        for w in walked {
            if w != p {
                self.links.insert(w, Link::Jump(p));
            }
        }
        let Some(e) = end else {
            return incomplete(BatchFault::Disagreement);
        };
        if e.data_end.is_some_and(|d| d - o > MAX_BATCH_DATA) {
            return incomplete(BatchFault::Disagreement);
        }
        match e.stop {
            Stop::Fault(f) => incomplete(f),
            Stop::Refuse(element) => {
                BatchRead::Refused(FormatError::Unsupported { offset: p, element })
            }
            Stop::Commit(None) => incomplete(BatchFault::Content),
            Stop::Commit(Some(c)) if c.batch_bytes == p - o => {
                read_batch(reads, id, o, limit, min_first, Refusal::AtOnce)
            }
            Stop::Commit(Some(_)) => incomplete(BatchFault::Disagreement),
        }
    }
}
