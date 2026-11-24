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

/// What [`BatchContent::take_wanted`] read of a batch besides the facts it kept.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Taken {
    /// The least and greatest event time of its facts; `(i64::MAX, i64::MIN)` without one.
    pub(crate) min_t: i64,
    pub(crate) max_t: i64,
    /// The `after` of the recovery entry it holds, alone (A.6).
    pub(crate) recovery_after: Option<u64>,
    /// Which series its facts are of.
    pub(crate) series: Sole,
}

/// Which series a batch's facts are of: none, one, or several.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Sole {
    #[default]
    None,
    One(u32),
    Several,
}

impl Sole {
    fn add(&mut self, series: u32) {
        *self = match *self {
            Sole::None => Sole::One(series),
            Sole::One(s) if s == series => Sole::One(s),
            _ => Sole::Several,
        };
    }
}

/// One entry of a batch: a fact or another entry. Each takes one sequence.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Item {
    /// A reading.
    Fact(FactRow),
    /// Any other entry, boxed so that an item is the size of a fact row.
    Entry(Box<Entry>),
}

/// An item by reference with its entry unboxed, for matching.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ItemRef<'a> {
    /// A reading.
    Fact(&'a FactRow),
    /// Any other entry.
    Entry(&'a Entry),
}

impl From<Entry> for Item {
    fn from(e: Entry) -> Self {
        Item::Entry(Box::new(e))
    }
}

impl Item {
    /// The item by reference, for matching an entry's variant.
    pub fn view(&self) -> ItemRef<'_> {
        match self {
            Self::Fact(r) => ItemRef::Fact(r),
            Self::Entry(e) => ItemRef::Entry(e),
        }
    }

    /// The sequences this item takes: 1, or the count of an [`Entry::Ignored`].
    pub fn sequences(&self) -> u64 {
        match self {
            Self::Fact(_) => 1,
            Self::Entry(e) => e.sequences(),
        }
    }
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
    /// The recovery entry this batch holds, if it is one (A.6: a recovery entry is alone in its batch).
    pub fn recovery(&self) -> Option<&crate::entries::Recovery> {
        match self.items.as_slice() {
            [Item::Entry(e)] => match &**e {
                Entry::Recovery(r) => Some(r),
                _ => None,
            },
            _ => None,
        }
    }

    /// The last sequence.
    pub fn last_sequence(&self) -> u64 {
        self.first_sequence + self.items.iter().map(Item::sequences).sum::<u64>() - 1
    }

    /// The data blocks in canonical form (A.4): consecutive facts fill a facts block until the next row would take it
    /// past 65,536 rows or 8 MiB plain-encoded; consecutive entries fill an entries block until the next entry would
    /// take its body past 16 MiB (Q-106 (e)).
    pub fn blocks(&self) -> Result<Vec<DataBody>, EncodeError> {
        if self.items.is_empty() || self.first_sequence == 0 {
            return Err(EncodeError::EmptyBatch);
        }
        // Ignored entries are read, never written.
        if let Some(k) = self
            .items
            .iter()
            .position(|i| matches!(i, Item::Entry(e) if matches!(**e, Entry::Ignored { .. })))
        {
            return Err(EncodeError::Content(ContentError::FieldPresence {
                entry: k as u32,
                field: 0,
            }));
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
                    let (e, len) = (&**e, entry_len(e));
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
    /// skipped; unknown ignorable entries are counted in [`Entry::Ignored`] so that sequences still count.
    pub fn decode(batch: &Batch<'_>) -> Result<Self, BodyError> {
        let mut bodies = Vec::with_capacity(batch.blocks.len());
        for d in &batch.blocks {
            let body = d.block.body;
            match d.block.header.block_type {
                block_type::FACTS => bodies.push(DataBody::Facts(FactsBlock::decode(body)?)),
                block_type::ENTRIES => bodies.push(DataBody::Entries(EntriesBlock::decode(body)?)),
                _ => {}
            }
        }
        Self::from_bodies(batch.commit.first_sequence, bodies)
    }

    /// The facts of a complete batch whose bodies were checked that `wants` names by series and event time, each
    /// given to `keep` with its sequence and the batch's receive time; no other fact's text is decoded (Q-1063).
    pub(crate) fn take_wanted(
        batch: &Batch<'_>,
        wants: &dyn Fn(u32, i64) -> bool,
        keep: &mut dyn FnMut(u64, i64, FactRow),
    ) -> Result<Taken, BodyError> {
        let mut out = Taken {
            min_t: i64::MAX,
            max_t: i64::MIN,
            recovery_after: None,
            series: Sole::None,
        };
        let sole = std::cell::Cell::new(Sole::None);
        let wants = &|series: u32, t: i64| {
            let mut s = sole.get();
            s.add(series);
            sole.set(s);
            wants(series, t)
        };
        let mut items = 0u64;
        for d in &batch.blocks {
            let body = d.block.body;
            match d.block.header.block_type {
                block_type::FACTS => {
                    let prefix = DataPrefix::decode(block_type::FACTS, body)
                        .map_err(|_| ContentError::Prefix)?;
                    let mut put = |k: u64, row: FactRow| {
                        keep(prefix.first_sequence + k, prefix.received_at, row);
                    };
                    let mut w = crate::facts::Wanted::new(wants, &mut put);
                    crate::facts::walk_wanted(body, &mut w)?;
                    out.min_t = out.min_t.min(w.min_t);
                    out.max_t = out.max_t.max(w.max_t);
                    items += u64::from(prefix.count);
                }
                block_type::ENTRIES => {
                    let e = EntriesBlock::decode(body)?;
                    for x in &e.entries {
                        if let Entry::Recovery(r) = x {
                            out.recovery_after = Some(r.after);
                        }
                    }
                    items += e.entries.len() as u64;
                }
                _ => {}
            }
        }
        if items > 1 && out.recovery_after.is_some() {
            return Err(ContentError::RecoveryNotAlone.into());
        }
        out.series = sole.get();
        Ok(out)
    }

    /// The content of a complete batch whose data blocks of known type decoded to `bodies`, in order.
    pub(crate) fn from_bodies(
        first_sequence: u64,
        bodies: Vec<DataBody>,
    ) -> Result<Self, BodyError> {
        let mut items = Vec::new();
        let mut received_at = None;
        for b in bodies {
            match b {
                DataBody::Facts(f) => {
                    received_at.get_or_insert(f.received_at);
                    items.extend(f.rows.into_iter().map(Item::Fact));
                }
                DataBody::Entries(e) => {
                    received_at.get_or_insert(e.received_at);
                    items.extend(e.entries.into_iter().map(Item::from));
                }
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
            first_sequence,
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

/// The checks a complete batch's data block must pass (A.2: "its body passes the checks of its type"), whether it
/// holds a recovery entry, and with `keep` the body as decoded by those checks.
pub(crate) fn check_block(
    t: u8,
    body: &[u8],
    keep: bool,
) -> Result<(DataPrefix, bool, Option<DataBody>), BodyError> {
    let prefix = DataPrefix::decode(t, body).map_err(|_| {
        if body.len() < DATA_PREFIX_LEN || t == block_type::ENTRIES {
            ContentError::Prefix
        } else {
            ContentError::RowCount(crate::bytes::u32_at(body, 16))
        }
    })?;
    Ok(match (t, keep) {
        (block_type::FACTS, false) => {
            FactsBlock::check(body)?;
            (prefix, false, None)
        }
        (_, false) => (prefix, crate::entries::check(body)?.1, None),
        (_, true) => {
            let body = decode_block(t, body)?;
            let recovery =
                matches!(&body, DataBody::Entries(e) if e.entries.iter().any(Entry::is_recovery));
            (prefix, recovery, Some(body))
        }
    })
}

/// A facts or entries body decoded, every check of its type applied (A.5, A.6).
pub(crate) fn decode_block(t: u8, body: &[u8]) -> Result<DataBody, BodyError> {
    Ok(match t {
        block_type::FACTS => DataBody::Facts(FactsBlock::decode(body)?),
        _ => DataBody::Entries(EntriesBlock::decode(body)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_item_is_the_size_of_a_fact_row_and_its_tag() {
        assert!(std::mem::size_of::<Item>() <= std::mem::size_of::<FactRow>() + 8);
    }
}
