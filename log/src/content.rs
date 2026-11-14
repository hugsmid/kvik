//! Arrival batches as typed content (`format.md` A.4, A.7): facts and entries in arrival order, split into blocks
//! in the canonical way, and decoded back from a complete batch.

pub use crate::batch::MAX_BATCH_DATA;
use crate::batch::{Batch, BatchBuilder, SegmentIdentity};
use crate::block::{BLOCK_HEADER_LEN, MAX_BODY_LEN, block_type};
use crate::body::{DATA_PREFIX_LEN, DataPrefix, MAX_ROWS};
use crate::content_error::{BodyError, ContentError};
use crate::entries::{EntriesBlock, Entry, entry_len};
use crate::error::EncodeError;
use crate::facts::{FactRow, FactsBlock, MAX_PLAIN_LEN};

/// One entry of a batch: a fact or another entry. Each takes one sequence.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Item {
    /// A reading.
    Fact(FactRow),
    /// Any other entry.
    Entry(Entry),
}

/// A batch's data blocks, typed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DataBody {
    /// A facts block.
    Facts(FactsBlock),
    /// An entries block.
    Entries(EntriesBlock),
}

impl DataBody {
    fn encode(&self) -> Result<(u8, Vec<u8>), EncodeError> {
        Ok(match self {
            Self::Facts(f) => (block_type::FACTS, f.encode()?),
            Self::Entries(e) => (block_type::ENTRIES, e.encode()?),
        })
    }
}

/// The content of one batch: its first sequence, its receive time and its entries in order.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BatchContent {
    /// Sequence of the first item.
    pub first_sequence: u64,
    /// The batch's receive time, carried by every data block.
    pub received_at: i64,
    /// Facts and entries in arrival order.
    pub items: Vec<Item>,
}

impl BatchContent {
    /// The last sequence.
    pub fn last_sequence(&self) -> u64 {
        self.first_sequence + self.items.len() as u64 - 1
    }

    /// The data blocks in canonical form (A.4): consecutive facts fill a facts block until the next row would take it
    /// past 65,536 rows or 8 MiB plain-encoded; consecutive entries fill an entries block until the next entry would
    /// take its body past 16 MiB (Q-106 (e)).
    pub fn blocks(&self) -> Result<Vec<DataBody>, EncodeError> {
        if self.items.is_empty() || self.first_sequence == 0 {
            return Err(EncodeError::EmptyBatch);
        }
        if self.items.len() > 1
            && self
                .items
                .iter()
                .any(|i| matches!(i, Item::Entry(e) if e.is_recovery()))
        {
            return Err(EncodeError::Content(ContentError::RecoveryNotAlone));
        }
        let mut out: Vec<DataBody> = Vec::new();
        let (mut plain, mut entries_len) = (0u64, 0u64);
        for (k, item) in self.items.iter().enumerate() {
            let sequence = self
                .first_sequence
                .checked_add(k as u64)
                .ok_or(EncodeError::NotConsecutive(k))?;
            let new_block = |out: &mut Vec<DataBody>, d| out.push(d);
            match item {
                Item::Fact(r) => {
                    let len = r.plain_len();
                    match out.last_mut() {
                        Some(DataBody::Facts(f))
                            if f.rows.len() < MAX_ROWS as usize && plain + len <= MAX_PLAIN_LEN =>
                        {
                            f.rows.push(r.clone());
                            plain += len;
                        }
                        _ => {
                            let rows = vec![r.clone()];
                            new_block(
                                &mut out,
                                DataBody::Facts(FactsBlock {
                                    first_sequence: sequence,
                                    received_at: self.received_at,
                                    rows,
                                }),
                            );
                            plain = len;
                        }
                    }
                }
                Item::Entry(e) => {
                    let len = entry_len(e);
                    match out.last_mut() {
                        Some(DataBody::Entries(b))
                            if entries_len + len <= u64::from(MAX_BODY_LEN) =>
                        {
                            b.entries.push(e.clone());
                            entries_len += len;
                        }
                        _ => {
                            let entries = vec![e.clone()];
                            new_block(
                                &mut out,
                                DataBody::Entries(EntriesBlock {
                                    first_sequence: sequence,
                                    received_at: self.received_at,
                                    entries,
                                }),
                            );
                            entries_len = DATA_PREFIX_LEN as u64 + len;
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// A builder holding the canonical data blocks; fails if they pass 8 MiB.
    pub fn builder(&self) -> Result<BatchBuilder, EncodeError> {
        let mut b = BatchBuilder::new();
        let mut total = 0u64;
        for d in self.blocks()? {
            let (t, body) = d.encode()?;
            total += BLOCK_HEADER_LEN as u64 + (body.len() as u64).div_ceil(8) * 8;
            if total > MAX_BATCH_DATA {
                return Err(EncodeError::TooLarge);
            }
            b.push(t, body)?;
        }
        Ok(b)
    }

    /// The bytes of the batch at `offset` of segment `id`, its commit block included.
    pub fn encode(
        &self,
        id: &SegmentIdentity,
        offset: u64,
        commit_time: i64,
    ) -> Result<Vec<u8>, EncodeError> {
        self.builder()?.encode(id, offset, commit_time)
    }

    /// The content of a complete batch, every body checked (A.5, A.6). Blocks of unknown, skippable type are
    /// skipped; unknown ignorable entries are kept as [`Entry::Ignored`] so that sequences still count.
    pub fn decode(batch: &Batch<'_>) -> Result<Self, BodyError> {
        let mut items = Vec::new();
        let mut received_at = None;
        for d in &batch.blocks {
            let body = d.block.body;
            match d.block.header.block_type {
                block_type::FACTS => {
                    let f = FactsBlock::decode(body)?;
                    received_at.get_or_insert(f.received_at);
                    items.extend(f.rows.into_iter().map(Item::Fact));
                }
                block_type::ENTRIES => {
                    let e = EntriesBlock::decode(body)?;
                    received_at.get_or_insert(e.received_at);
                    items.extend(e.entries.into_iter().map(Item::Entry));
                }
                _ => {}
            }
        }
        if items.len() > 1
            && items
                .iter()
                .any(|i| matches!(i, Item::Entry(e) if e.is_recovery()))
        {
            return Err(ContentError::RecoveryNotAlone.into());
        }
        Ok(Self {
            first_sequence: batch.commit.first_sequence,
            received_at: received_at.unwrap_or(0),
            items,
        })
    }

    /// [`BatchContent::decode`], also requiring the data blocks to be exactly the canonical ones for the content:
    /// their split, flags and bodies.
    pub fn decode_canonical(batch: &Batch<'_>) -> Result<Self, BodyError> {
        let c = Self::decode(batch)?;
        let canonical = c.builder().map_err(|_| BodyError::NonCanonical)?;
        let same = canonical.blocks_eq(batch.blocks.iter().map(|d| {
            (
                d.block.header.block_type,
                d.block.header.flags,
                d.block.body,
            )
        }));
        if same
            && batch
                .blocks
                .iter()
                .all(|d| d.block.header_bytes[2..4] == [0, 0])
        {
            Ok(c)
        } else {
            Err(BodyError::NonCanonical)
        }
    }
}

/// The checks a complete batch's data block must pass (A.2: "its body passes the checks of its type"), and whether
/// it holds a recovery entry.
pub(crate) fn check_block(t: u8, body: &[u8]) -> Result<(DataPrefix, bool), BodyError> {
    let prefix = DataPrefix::decode(t, body).map_err(|_| {
        if body.len() < DATA_PREFIX_LEN || t == block_type::ENTRIES {
            ContentError::Prefix
        } else {
            ContentError::RowCount(crate::bytes::u32_at(body, 16))
        }
    })?;
    let recovery = match t {
        block_type::FACTS => FactsBlock::decode(body).map(|_| false)?,
        _ => EntriesBlock::decode(body)?
            .entries
            .iter()
            .any(Entry::is_recovery),
    };
    Ok((prefix, recovery))
}
