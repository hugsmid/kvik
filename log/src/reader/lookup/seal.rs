//! A sealed segment's seal blocks as lookups read them: the trailer, the segment index (`format.md` A.8, A.9) and
//! where the series batches block lies (A.8a).

use super::*;

/// A sealed segment's index as the lookup uses it: `None` when the segment has none that reads.
pub(super) type SealIndex = Option<Sealed>;

/// A sealed segment's index records, with where its series batches block lies (A.8a), read again when a lookup uses
/// it rather than kept, so that the cache keeps every segment's records within its bound.
#[derive(Debug)]
pub(super) struct Sealed {
    pub(super) records: Vec<IndexRecord>,
    /// The block's offset and length, header included.
    pub(super) lists: Option<(u64, u64)>,
    /// The bytes of the segment's batches: from the header's end to the index.
    pub(super) batch_bytes: u64,
}

impl LogReader {
    /// A sealed segment's index, read from its seal blocks.
    pub(super) fn read_seal_index(&self, name: SegmentName) -> SealIndex {
        let (f, trailer, trailer_offset) = self.read_trailer(name)?;
        let at = trailer.index_offset;
        if at < HEADER_LEN as u64 || at >= trailer_offset {
            return None;
        }
        let head = read_header_at(&*f, at, trailer_offset)?;
        let mut b = vec![0u8; usize::try_from(head.block_len()).ok()?];
        if read_at_full(&*f, &mut b, at).ok()? != b.len() {
            return None;
        }
        let block = read_block(&b, 0, b.len() as u64).ok()?;
        if block.header.block_type != block_type::SEGMENT_INDEX {
            return None;
        }
        let mut records = SegmentIndex::decode(block.body).ok()?.records;
        records.sort_by_key(|r| r.series_id);
        let next = at + head.block_len();
        let lists = read_header_at(&*f, next, trailer_offset)
            .filter(|h| h.block_type == block_type::SERIES_BATCHES)
            .map(|h| (next, h.block_len()));
        Some(Sealed {
            records,
            lists,
            batch_bytes: at - HEADER_LEN as u64,
        })
    }

    /// A sealed segment's trailer with the open file and the trailer's offset; `None` if it does not read.
    pub(in crate::reader) fn read_trailer(
        &self,
        name: SegmentName,
    ) -> Option<(Box<dyn crate::fs::FsFile>, Trailer, u64)> {
        let (n, f) = self.open_segment(name).ok().flatten()?;
        if !n.sealed {
            return None;
        }
        let len = f.len().ok()?;
        let tail = (BLOCK_HEADER_LEN + TRAILER_LEN + END_MARKER_LEN) as u64;
        let start = len.checked_sub(tail)?;
        let mut b = vec![0u8; tail as usize];
        if read_at_full(&*f, &mut b, start).ok()? != b.len() {
            return None;
        }
        let trailer_offset = decode_end_marker(&b[b.len() - END_MARKER_LEN..])?;
        if trailer_offset != start {
            return None;
        }
        let block = read_block(&b, 0, (BLOCK_HEADER_LEN + TRAILER_LEN) as u64).ok()?;
        if block.header.block_type != block_type::TRAILER {
            return None;
        }
        let trailer = Trailer::decode(block.body).ok()?;
        Some((f, trailer, trailer_offset))
    }
}

/// The block header at `at`, if a block of that length ends by `limit`.
fn read_header_at(
    f: &dyn crate::fs::FsFile,
    at: u64,
    limit: u64,
) -> Option<crate::block::BlockHeader> {
    let mut h = [0u8; BLOCK_HEADER_LEN];
    if at.checked_add(BLOCK_HEADER_LEN as u64)? > limit
        || read_at_full(f, &mut h, at).ok()? != h.len()
    {
        return None;
    }
    let header = crate::block::BlockHeader::decode(&h)?;
    (at.checked_add(header.block_len())? <= limit).then_some(header)
}
