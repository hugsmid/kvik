//! Batches: data blocks followed by one commit block (`format.md` A.4, A.7).
//!
//! The batch layer (roadmap 4.3) produces the bodies of facts and entries blocks; this module frames them, counts
//! their sequences and computes the commit block, and on reading decides whether a batch is complete.

use crate::block::{
    BLOCK_HEADER_LEN, BlockFault, BlockHeader, RawBlock, block_type, encode_block, peek_header,
    peek_raw, read_block,
};
use crate::body::{COMMIT_LEN, Commit, DataPrefix};
use crate::content::check_block;
use crate::content_error::BodyError;
use crate::crc::Crc32c;
use crate::error::{Element, EncodeError, FormatError};

/// The most bytes of data blocks in one batch (A, common rules: a batch is at most 8 MiB of data blocks plus its
/// commit block).
pub const MAX_BATCH_DATA: u64 = 8 << 20;

/// What a segment's batches are checked against: the store's id (from the store descriptor) and the file id and
/// generation (from the file name, `format.md` A.12). They are taken from outside the file because the header may be
/// invalid while complete batches follow it (A.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SegmentIdentity {
    /// The store's id.
    pub store_id: [u8; 16],
    /// The segment's file id.
    pub file_id: u64,
    /// The generation in which the segment was created.
    pub generation: u32,
}

/// A data block of a complete batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataBlock<'a> {
    /// The block.
    pub block: RawBlock<'a>,
    /// The prefix of a facts or entries block; `None` for a skipped block of unknown type.
    pub prefix: Option<DataPrefix>,
}

/// A complete batch (`format.md` A.7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch<'a> {
    /// File offset of the first block header (`o` in A.7).
    pub offset: u64,
    /// The data blocks, in order.
    pub blocks: Vec<DataBlock<'a>>,
    /// The commit block body.
    pub commit: Commit,
}

impl Batch<'_> {
    /// File offset of the commit block header.
    pub fn commit_offset(&self) -> u64 {
        self.offset + self.commit.batch_bytes
    }

    /// File offset just past the commit block: where the next batch starts.
    pub fn end(&self) -> u64 {
        self.commit_offset() + (BLOCK_HEADER_LEN + COMMIT_LEN) as u64
    }

    /// Entries in the batch.
    pub fn entry_count(&self) -> u64 {
        self.blocks
            .iter()
            .filter_map(|b| b.prefix)
            .map(|p| u64::from(p.count))
            .sum()
    }
}

/// Why no complete batch starts at an offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BatchFault {
    /// A block that is not checksum-valid, or does not fit below the limit.
    Block(BlockFault),
    /// A checksum-valid data block whose body fails the checks of its type (A.2: cause 4, invalid content).
    Content,
    /// Checksum-valid blocks that do not form a complete batch (A.7: a disagreement with valid checksums).
    Disagreement,
}

pub(crate) enum BatchRead<'a> {
    Complete(Batch<'a>),
    Incomplete { fault: BatchFault },
    Refused(FormatError),
}

/// How an unknown critical block is treated while reading a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// At a block boundary reached from a valid position: refused at once (`format.md` A, common rules).
    AtOnce,
    /// At a candidate offset while resynchronising: refused only if the rest of the batch checks out (its commit
    /// block, `block_count`, `batch_bytes`, `batch_crc32c` and generation); otherwise the offset is not a batch
    /// (Q-105 (c)).
    IfBatchVerifies,
}

/// Reads the batch starting at `o`, accepting it only if it ends at or below `limit` and its first sequence is at
/// least `min_first`.
pub(crate) fn read_batch<'a>(
    bytes: &'a [u8],
    id: &SegmentIdentity,
    o: u64,
    limit: u64,
    min_first: u64,
    refusal: Refusal,
) -> BatchRead<'a> {
    let incomplete = |fault| BatchRead::Incomplete { fault };
    let mut crc = Crc32c::new();
    crc.update(&id.store_id);
    crc.update(&id.file_id.to_le_bytes());
    crc.update(&o.to_le_bytes());
    let mut blocks = Vec::new();
    let mut unknown_critical = None;
    let mut recovery = false;
    let mut pos = o;
    let commit_block = loop {
        // Sizes are judged on the header, before the body is checksummed: a data block that would take the batch
        // past 8 MiB, or a commit block of another length, ends the batch here.
        if let Some(h) = peek_header(bytes, pos, limit) {
            let too_long = match h.block_type {
                block_type::COMMIT => h.body_len as usize != COMMIT_LEN,
                _ => pos - o + h.block_len() > MAX_BATCH_DATA,
            };
            if too_long {
                let fault = if h.block_type == block_type::COMMIT {
                    BatchFault::Content
                } else {
                    BatchFault::Disagreement
                };
                return incomplete(fault);
            }
        }
        let b = match read_block(bytes, pos, limit) {
            Ok(b) => b,
            Err(f) => return incomplete(BatchFault::Block(f)),
        };
        let t = b.header.block_type;
        let prefix = match t {
            block_type::COMMIT => break b,
            // The body checks of A.5 and A.6; an unknown critical element inside is refused like an unknown
            // critical block.
            block_type::FACTS | block_type::ENTRIES => match check_block(t, b.body) {
                Ok((p, r)) => {
                    recovery |= r;
                    Some(p)
                }
                Err(BodyError::Unsupported(element)) => {
                    let refused = FormatError::Unsupported {
                        offset: pos,
                        element,
                    };
                    if refusal == Refusal::AtOnce {
                        return BatchRead::Refused(refused);
                    }
                    unknown_critical.get_or_insert(refused);
                    None
                }
                Err(_) => return incomplete(BatchFault::Content),
            },
            // Seal blocks never belong to a batch.
            block_type::SEGMENT_INDEX | block_type::TRAILER => {
                return incomplete(BatchFault::Disagreement);
            }
            _ if b.header.skippable() => None,
            _ => {
                let refused = FormatError::Unsupported {
                    offset: pos,
                    element: Element::BlockType(t),
                };
                if refusal == Refusal::AtOnce {
                    return BatchRead::Refused(refused);
                }
                unknown_critical.get_or_insert(refused);
                None
            }
        };
        crc.update(&b.header_bytes);
        blocks.push(DataBlock { block: b, prefix });
        pos = b.next_offset();
    };
    let Ok(commit) = Commit::decode(commit_block.body) else {
        return incomplete(BatchFault::Content);
    };
    let framing_holds = !blocks.is_empty()
        && commit.block_count as usize == blocks.len()
        && commit.batch_bytes == commit_block.offset - o
        && commit.generation == id.generation
        && commit.batch_crc32c == crc.finish();
    if !framing_holds {
        return incomplete(BatchFault::Disagreement);
    }
    if let Some(refused) = unknown_critical {
        return BatchRead::Refused(refused);
    }
    // Sequences are counted from the commit's first sequence; a skipped block takes none (Q-105 (b)). A reader
    // accepts any gap above the last kept sequence (A.4 Gaps). That a batch starts higher only after an invalid
    // region or with a recovery entry (A.7) is checked by the writer's recovery, roadmap 4.5 (Q-105 (d)).
    if commit.first_sequence < min_first.max(1) {
        return incomplete(BatchFault::Disagreement);
    }
    let received_at = blocks.iter().find_map(|b| b.prefix).map(|p| p.received_at);
    let mut next = Some(commit.first_sequence);
    for p in blocks.iter().filter_map(|b| b.prefix) {
        if Some(p.first_sequence) != next || Some(p.received_at) != received_at {
            return incomplete(BatchFault::Disagreement);
        }
        next = p.first_sequence.checked_add(u64::from(p.count));
    }
    // `next` is one past the last counted sequence; `None` is an overflow. A batch takes at least one sequence.
    if next.map(|n| n - 1) != Some(commit.last_sequence)
        || commit.last_sequence < commit.first_sequence
    {
        return incomplete(BatchFault::Disagreement);
    }
    // A recovery entry is the only entry of its batch (A.6).
    if recovery && commit.last_sequence != commit.first_sequence {
        return incomplete(BatchFault::Content);
    }
    BatchRead::Complete(Batch {
        offset: o,
        blocks,
        commit,
    })
}

/// Whether the blocks from `o` frame a batch, judged on block headers and the 48-byte commit body only: the commit
/// block is reached within 8 MiB of data blocks, and its `block_count`, `batch_bytes`, generation and
/// `batch_crc32c` hold. No data block body is checksummed, so this costs O(blocks); a candidate that passes it is a
/// batch image for this store, file and offset, and only then is read in full.
pub(crate) fn framing_holds(bytes: &[u8], id: &SegmentIdentity, o: u64, limit: u64) -> bool {
    let mut crc = Crc32c::new();
    crc.update(&id.store_id);
    crc.update(&id.file_id.to_le_bytes());
    crc.update(&o.to_le_bytes());
    let mut pos = o;
    let mut count: u64 = 0;
    loop {
        let Some((h, raw)) = peek_raw(bytes, pos, limit) else {
            return false;
        };
        match h.block_type {
            block_type::COMMIT => {
                if h.body_len as usize != COMMIT_LEN {
                    return false;
                }
                let Ok(b) = read_block(bytes, pos, limit) else {
                    return false;
                };
                let Ok(c) = Commit::decode(b.body) else {
                    return false;
                };
                return count >= 1
                    && u64::from(c.block_count) == count
                    && c.batch_bytes == pos - o
                    && c.generation == id.generation
                    && c.batch_crc32c == crc.finish();
            }
            block_type::SEGMENT_INDEX | block_type::TRAILER => return false,
            _ => {
                if pos - o + h.block_len() > MAX_BATCH_DATA {
                    return false;
                }
                crc.update(&raw);
                count += 1;
                pos += h.block_len();
            }
        }
    }
}

/// Builds the bytes of one batch from data block bodies.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BatchBuilder {
    blocks: Vec<(u8, u8, Vec<u8>)>,
}

impl BatchBuilder {
    /// An empty batch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a facts or entries block. `body` starts with its [`DataPrefix`].
    pub fn push(&mut self, block_type: u8, body: Vec<u8>) -> Result<&mut Self, EncodeError> {
        if !matches!(block_type, block_type::FACTS | block_type::ENTRIES) {
            return Err(EncodeError::BlockType(block_type));
        }
        DataPrefix::decode(block_type, &body)
            .map_err(|_| EncodeError::DataPrefix(self.blocks.len()))?;
        self.blocks.push((block_type, 0, body));
        Ok(self)
    }

    /// Adds a block of a type this version does not know, as a later minor version would write it. For tests of
    /// the critical and ignorable rule.
    pub fn push_unknown(
        &mut self,
        block_type: u8,
        flags: u8,
        body: Vec<u8>,
    ) -> Result<&mut Self, EncodeError> {
        if block_type == 0 || block_type::is_known(block_type) {
            return Err(EncodeError::BlockType(block_type));
        }
        self.blocks.push((block_type, flags, body));
        Ok(self)
    }

    /// A builder that writes the blocks of `batch` again, types, flags and bodies unchanged.
    pub fn from_batch(batch: &Batch<'_>) -> Self {
        let blocks = batch.blocks.iter().map(|b| {
            (
                b.block.header.block_type,
                b.block.header.flags,
                b.block.body.to_vec(),
            )
        });
        Self {
            blocks: blocks.collect(),
        }
    }

    /// Whether the builder holds exactly these blocks `(type, flags, body)`.
    pub fn blocks_eq<'b>(&self, other: impl Iterator<Item = (u8, u8, &'b [u8])>) -> bool {
        let mut n = 0;
        for (i, (t, f, b)) in other.enumerate() {
            match self.blocks.get(i) {
                Some((t2, f2, b2)) if *t2 == t && *f2 == f && b2.as_slice() == b => n += 1,
                _ => return false,
            }
        }
        n == self.blocks.len()
    }

    /// The types of the blocks added so far, in order.
    pub fn block_types(&self) -> impl Iterator<Item = u8> + '_ {
        self.blocks.iter().map(|b| b.0)
    }

    /// The first and last sequence of the batch, from its data block prefixes.
    pub fn sequences(&self) -> Result<(u64, u64), EncodeError> {
        let mut first = None;
        let mut next: Option<(u64, i64)> = None;
        for (i, (t, _, body)) in self.blocks.iter().enumerate() {
            let Ok(p) = DataPrefix::decode(*t, body) else {
                continue;
            };
            if let Some((n, r)) = next
                && (p.first_sequence != n || p.received_at != r)
            {
                return Err(EncodeError::NotConsecutive(i));
            }
            if p.first_sequence == 0 {
                return Err(EncodeError::DataPrefix(i));
            }
            first.get_or_insert(p.first_sequence);
            let n = p
                .first_sequence
                .checked_add(u64::from(p.count))
                .ok_or(EncodeError::NotConsecutive(i))?;
            next = Some((n, p.received_at));
        }
        match (first, next) {
            (Some(f), Some((n, _))) => Ok((f, n - 1)),
            _ => Err(EncodeError::EmptyBatch),
        }
    }

    /// The bytes of the batch at file offset `offset` of the segment `id`, its commit block included.
    pub fn encode(
        &self,
        id: &SegmentIdentity,
        offset: u64,
        commit_time: i64,
    ) -> Result<Vec<u8>, EncodeError> {
        if !offset.is_multiple_of(8) {
            return Err(EncodeError::Misaligned(offset));
        }
        let (first_sequence, last_sequence) = self.sequences()?;
        let mut out = Vec::new();
        let mut crc = Crc32c::new();
        crc.update(&id.store_id);
        crc.update(&id.file_id.to_le_bytes());
        crc.update(&offset.to_le_bytes());
        for (t, flags, body) in &self.blocks {
            let h: BlockHeader = encode_block(*t, *flags, body, &mut out)?;
            crc.update(&h.encode());
        }
        // A batch holds at most 8 MiB of data blocks (A, common rules: sizes).
        if out.len() as u64 > MAX_BATCH_DATA {
            return Err(EncodeError::TooLarge);
        }
        let commit = Commit {
            generation: id.generation,
            block_count: self.blocks.len() as u32,
            first_sequence,
            last_sequence,
            commit_time,
            batch_bytes: out.len() as u64,
            batch_crc32c: crc.finish(),
        };
        encode_block(block_type::COMMIT, 0, &commit.encode(), &mut out)?;
        Ok(out)
    }
}
