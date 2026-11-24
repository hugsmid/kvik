//! The batch read at a block boundary, with each block judged once per scan (Q-110, Q-873).
//!
//! A boundary read walks the chain of blocks from a valid position to its first block that is not a passing data
//! block. Positions just past a kept batch can lie on a chain an earlier boundary read already walked, so the
//! verdicts of a read that keeps no batch are remembered: a block is judged once, whatever the number of boundary
//! reads that cross it. Each block walked is remembered as a word in a ring indexed by offset, and each stop as an
//! event, so remembering costs one store a block.

use std::collections::BTreeMap;

use crate::batch::{BatchFault, BatchRead, MAX_BATCH_DATA, SegmentIdentity, read_judged_batch};
use crate::block::block_type;
use crate::body::{COMMIT_LEN, Commit};
use crate::bytes::u32_at;
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

/// The blocks from a candidate up to a stop where a boundary read walking them stopped, each passing.
pub(crate) enum Remembered {
    /// A read from the candidate fails at `stop`, having checked the facts and entries bodies below it and `extra`
    /// bytes of the stop's body.
    Fails { stop: u64, extra: u64 },
    /// The stop is a checksum-valid commit block.
    Commit,
}

/// Ring slots: the batch a walk reaches beyond its start and the 64 KiB the ring's base lags behind the start.
const SLOTS: u64 = (MAX_BATCH_DATA + (1 << 16)) / 8 + 1;
const _: () = assert!(8 * SLOTS > MAX_BATCH_DATA + (1 << 16));
/// A slot whose stop is in [`Walks::events`]; a block that passes is `steps << 1`, steps ≥ 1.
const AT: u32 = 1;

/// The verdicts of the blocks walked by boundary reads that kept no batch, from the ring's base on.
#[derive(Debug, Default)]
pub(crate) struct Walks {
    /// A word per 8-byte step in `[base, base + 8 · SLOTS)`, 0 for one not walked; allocated at the first verdict.
    slots: Vec<u32>,
    events: BTreeMap<u64, Event>,
    base: u64,
    walked: Vec<u64>,
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
        block_type::FACTS | block_type::ENTRIES => match reads.check_body(t, b.body, false) {
            Ok(_) => Judged::Pass(b.next_offset()),
            Err(BodyError::Unsupported(element)) => stop(Stop::Refuse(element)),
            Err(_) => stop(Stop::Fault(BatchFault::Content)),
        },
        block_type::SEGMENT_INDEX | block_type::SERIES_BATCHES | block_type::TRAILER => {
            stop(Stop::Fault(BatchFault::Disagreement))
        }
        _ if b.header.skippable() => Judged::Pass(b.next_offset()),
        _ => stop(Stop::Refuse(Element::BlockType(t))),
    }
}

impl Walks {
    #[cfg(test)]
    pub(crate) fn events(&self) -> usize {
        self.events.len()
    }

    fn slot(p: u64) -> usize {
        ((p / 8) % SLOTS) as usize
    }

    fn link(&self, p: u64) -> Option<Link> {
        if self.slots.is_empty() || p < self.base || (p - self.base) / 8 >= SLOTS {
            return None;
        }
        match self.slots[Self::slot(p)] {
            0 => None,
            AT => self.events.get(&p).map(|&e| Link::At(e)),
            w => Some(Link::Jump(p + 8 * u64::from(w >> 1))),
        }
    }

    /// Forgets the verdicts below `o`, once the base lags 64 KiB behind it.
    fn move_to(&mut self, o: u64) {
        if o - self.base < 1 << 16 {
            return;
        }
        if !self.slots.is_empty() {
            for k in 0..((o - self.base) / 8).min(SLOTS) {
                self.slots[Self::slot(self.base + 8 * k)] = 0;
            }
        }
        self.events = self.events.split_off(&o);
        self.base = o;
    }

    fn remember(&mut self, p: u64, link: Link) {
        if self.slots.is_empty() {
            self.slots = vec![0; SLOTS as usize];
        }
        let w = match link {
            Link::Jump(f) => ((f - p) / 8) as u32 * 2,
            Link::At(e) => {
                self.events.insert(p, e);
                AT
            }
        };
        self.slots[Self::slot(p)] = w;
    }

    /// What a boundary read found on the chain from a resynchronisation candidate `o`, when it walked it (Q-873).
    pub(crate) fn remembered(&self, reads: &Reads<'_>, o: u64) -> Option<Remembered> {
        let mut p = o;
        for _ in 0..4 {
            match self.link(p)? {
                Link::Jump(f) => p = f,
                Link::At(e) => {
                    let extra = match e.stop {
                        Stop::Refuse(_) => return None,
                        Stop::Commit(Some(_)) => return Some(Remembered::Commit),
                        Stop::Fault(BatchFault::Content) if e.data_end.is_some() => {
                            u64::from(u32_at(reads.bytes.get(p as usize..)?, 4))
                        }
                        Stop::Commit(None) | Stop::Fault(_) => 0,
                    };
                    return Some(Remembered::Fails { stop: p, extra });
                }
            }
        }
        None
    }

    /// `read_batch(o, Refusal::AtOnce)`, with the same result, each body checked once; a read that checked a body
    /// and kept no batch is charged the body bytes it checked against its window (`format.md` A, common rules: scan
    /// bound).
    pub(crate) fn read<'a>(
        &mut self,
        reads: &mut Reads<'a>,
        id: &SegmentIdentity,
        o: u64,
        limit: u64,
        min_first: u64,
        keep: bool,
    ) -> BatchRead<'a> {
        let before = reads.body_checked();
        let r = self.walk(reads, id, o, limit, min_first, keep);
        if matches!(r, BatchRead::Incomplete { .. }) && reads.body_checked() > before {
            reads.failed.charge(o, reads.body_checked() - before);
        }
        r
    }

    fn walk<'a>(
        &mut self,
        reads: &mut Reads<'a>,
        id: &SegmentIdentity,
        o: u64,
        limit: u64,
        min_first: u64,
        keep: bool,
    ) -> BatchRead<'a> {
        self.move_to(o);
        let incomplete = |fault| BatchRead::Incomplete { fault };
        let mut walked = std::mem::take(&mut self.walked);
        walked.clear();
        let mut judged = None;
        let mut p = o;
        let end = loop {
            match self.link(p) {
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
                        judged = Some(e);
                        break Some(e);
                    }
                    Judged::TooLong => break None,
                },
            }
        };
        let read = match end {
            None => incomplete(BatchFault::Disagreement),
            Some(e) if e.data_end.is_some_and(|d| d - o > MAX_BATCH_DATA) => {
                incomplete(BatchFault::Disagreement)
            }
            Some(e) => match e.stop {
                Stop::Fault(f) => incomplete(f),
                Stop::Refuse(element) => {
                    BatchRead::Refused(FormatError::Unsupported { offset: p, element })
                }
                Stop::Commit(None) => incomplete(BatchFault::Content),
                Stop::Commit(Some(c)) if c.batch_bytes == p - o => {
                    read_judged_batch(reads, id, o, limit, min_first, keep)
                }
                Stop::Commit(Some(_)) => incomplete(BatchFault::Disagreement),
            },
        };
        // A kept batch ends past every block walked, and later reads start at or past its end, so its verdicts are
        // never asked for again.
        if !matches!(read, BatchRead::Complete(..)) {
            for &w in &walked {
                if w != p {
                    self.remember(w, Link::Jump(p));
                }
            }
            if let Some(e) = judged {
                self.remember(p, Link::At(e));
            }
        }
        self.walked = walked;
        read
    }
}
