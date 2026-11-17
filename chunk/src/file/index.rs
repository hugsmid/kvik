//! The chunk index object (`kind = 14`, `docs/format-compacted.md` B.2): a compacted segment's description,
//! trailer, length, hash, commit-time directory and chunk index, as its own small file.

use kvik_log::block::encode_block;
use kvik_log::body::encode_end_marker;
use kvik_log::crc::crc32c;
use kvik_log::header::{FileHeader, HEADER_LEN};

use super::desc::Description;
use super::error::{Fault, FileError};
use super::parts::{
    IndexEntry, IndexTrailer, RootRecord, TRAILER_LEN, TimesRecord, Trailer, decode_root,
    decode_times_directory, u64_at,
};
use super::read::{
    CompactedFile, Identity, Index, Layout, check_entry, read_bound, read_header, read_tail,
};
use super::source::ReadAt;
use super::{KIND_INDEX, block_type};

/// Bytes after the segment's trailer body in the `0x38` block: the segment's length and plaintext SHA-256.
const SEGMENT_TAIL: usize = 8 + 32;

/// The chunk index object of a compacted segment whose plaintext SHA-256 is `sha256`: the segment's description,
/// trailer, commit-time directory, index blocks and root, unchanged, under a header with the segment's identity
/// and creation time. A function of the segment and its hash.
pub fn extract<S: ReadAt>(file: &CompactedFile<S>, sha256: [u8; 32]) -> Result<Vec<u8>, FileError> {
    let src = file.source();
    let t = *file.trailer();
    let limit = file.trailer_offset();
    file.index()?;
    let h = file.header();
    let mut out = FileHeader::new(
        KIND_INDEX,
        h.generation,
        h.file_id,
        h.created_at,
        h.store_id,
    )
    .encode()
    .to_vec();
    let put = |out: &mut Vec<u8>, ty: u8, body: &[u8]| -> Result<u64, FileError> {
        let at = out.len() as u64;
        encode_block(ty, 0, body, out).map_err(|_| FileError::invalid(at, Fault::BlockLength))?;
        Ok(at)
    };
    let (_, desc) = read_bound(
        src,
        HEADER_LEN as u64,
        block_type::DESCRIPTION,
        Some(t.description_crc32c),
        limit,
    )?;
    put(&mut out, block_type::DESCRIPTION, &desc)?;
    let (_, mut segment) = read_tail(src)?;
    segment.extend_from_slice(&src.len().to_le_bytes());
    segment.extend_from_slice(&sha256);
    put(&mut out, block_type::SEGMENT, &segment)?;
    let (_, dir) = read_bound(
        src,
        t.times_dir_offset,
        block_type::TIMES_DIRECTORY,
        Some(t.times_dir_crc32c),
        limit,
    )?;
    let times_dir_offset = put(&mut out, block_type::TIMES_DIRECTORY, &dir)?;
    let index_offset = if t.chunk_count == 0 {
        0
    } else {
        out.len() as u64
    };
    for r in file.root() {
        let (_, body) = read_bound(
            src,
            t.index_offset + r.offset,
            block_type::INDEX,
            Some(r.body_crc32c),
            t.index_root_offset,
        )?;
        put(&mut out, block_type::INDEX, &body)?;
    }
    let (_, root) = read_bound(
        src,
        t.index_root_offset,
        block_type::ROOT,
        Some(t.root_crc32c),
        limit,
    )?;
    let index_root_offset = put(&mut out, block_type::ROOT, &root)?;
    let trailer = IndexTrailer {
        index_offset,
        index_root_offset,
        times_dir_offset,
        file_crc32c: crc32c(&out),
        root_crc32c: t.root_crc32c,
    };
    let trailer_offset = put(&mut out, block_type::TRAILER, &trailer.encode())?;
    out.extend_from_slice(&encode_end_marker(trailer_offset));
    Ok(out)
}

/// An open chunk index object.
pub struct IndexObject<S: ReadAt> {
    src: S,
    description: Description,
    segment: Trailer,
    segment_len: u64,
    sha256: [u8; 32],
    times: Vec<TimesRecord>,
    index: Index,
    description_end: u64,
    layout: Layout,
}

impl<S: ReadAt> IndexObject<S> {
    /// Opens a chunk index object whose segment has identity `id`. Its `file_crc32c` is checked at every open (the
    /// file is small), then its blocks in order, each equal to what the segment's trailer binds.
    pub fn open(src: S, id: &Identity) -> Result<Self, FileError> {
        read_header(&src, KIND_INDEX, id)?;
        let (trailer_offset, body) = read_tail(&src)?;
        let own = IndexTrailer::decode(&body).map_err(|b| b.at(trailer_offset))?;
        if crc32c(&src.read_at(0, trailer_offset as usize)?) != own.file_crc32c {
            return Err(FileError::invalid(trailer_offset, Fault::FileChecksum));
        }
        let at = HEADER_LEN as u64;
        let (dh, desc) = read_bound(&src, at, block_type::DESCRIPTION, None, trailer_offset)?;
        let description = Description::decode(&desc).map_err(|b| b.at(at))?;
        let seg_at = at + dh.block_len();
        let (sh, seg) = read_bound(&src, seg_at, block_type::SEGMENT, None, trailer_offset)?;
        let split = seg
            .len()
            .checked_sub(SEGMENT_TAIL)
            .filter(|&n| n >= TRAILER_LEN)
            .ok_or(FileError::invalid(seg_at, Fault::Body("segment record")))?;
        let segment = Trailer::decode(&seg[..split]).map_err(|b| b.at(seg_at))?;
        let segment_len = u64_at(&seg, split);
        let mut sha256 = [0u8; 32];
        sha256.copy_from_slice(&seg[split + 8..]);
        let mismatch = |at, w| FileError::invalid(at, Fault::Mismatch(w));
        if dh.body_crc32c != segment.description_crc32c
            || description.has_entries != (segment.entries_offset != 0)
            || segment.index_root_offset >= segment_len
        {
            return Err(mismatch(seg_at, "segment record"));
        }
        if own.times_dir_offset != seg_at + sh.block_len() {
            return Err(mismatch(trailer_offset, "trailer offsets"));
        }
        let (th, dir) = read_bound(
            &src,
            own.times_dir_offset,
            block_type::TIMES_DIRECTORY,
            Some(segment.times_dir_crc32c),
            trailer_offset,
        )?;
        let times = decode_times_directory(&dir).map_err(|b| b.at(own.times_dir_offset))?;
        let after_dir = own.times_dir_offset + th.block_len();
        let index_start = if segment.chunk_count == 0 {
            own.index_offset == 0
        } else {
            own.index_offset == after_dir
        };
        if !index_start || own.root_crc32c != segment.root_crc32c {
            return Err(mismatch(trailer_offset, "trailer offsets"));
        }
        let (_, root) = read_bound(
            &src,
            own.index_root_offset,
            block_type::ROOT,
            Some(segment.root_crc32c),
            trailer_offset,
        )?;
        let records = decode_root(&root).map_err(|b| b.at(own.index_root_offset))?;
        let index = Index::new(
            records,
            own.index_offset,
            own.index_root_offset,
            segment.chunk_count,
            own.index_root_offset,
        )?;
        let layout = Layout::of(id, &description, &segment);
        Ok(Self {
            src,
            description,
            segment,
            segment_len,
            sha256,
            times,
            index,
            description_end: at + dh.block_len(),
            layout,
        })
    }

    /// The segment's description.
    pub fn description(&self) -> &Description {
        &self.description
    }

    /// The segment's trailer.
    pub fn segment_trailer(&self) -> &Trailer {
        &self.segment
    }

    /// The segment's length.
    pub fn segment_len(&self) -> u64 {
        self.segment_len
    }

    /// The segment's plaintext SHA-256.
    pub fn sha256(&self) -> [u8; 32] {
        self.sha256
    }

    /// The segment's layout, as the manifest records it.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The segment's commit-time directory; its offsets are the segment's.
    pub fn times_directory(&self) -> &[TimesRecord] {
        &self.times
    }

    /// The index root's records.
    pub fn root(&self) -> &[RootRecord] {
        &self.index.records
    }

    fn block(&self, i: usize) -> Result<Vec<IndexEntry>, FileError> {
        let entries = self.index.block(&self.src, i)?;
        let at = self.index.base + self.index.records[i].offset;
        for e in &entries {
            check_entry(
                e,
                &self.segment,
                self.description_end,
                self.description.class,
                at,
            )?;
        }
        Ok(entries)
    }

    /// The index entries of one series; their offsets are the segment's.
    pub fn series(&self, series_id: u32) -> Result<Vec<IndexEntry>, FileError> {
        let mut out = Vec::new();
        for i in self.index.blocks_of(series_id) {
            out.extend(
                self.block(i)?
                    .into_iter()
                    .filter(|e| e.series_id == series_id),
            );
        }
        Ok(out)
    }

    /// Every index entry, in index order.
    pub fn index(&self) -> Result<Vec<IndexEntry>, FileError> {
        let mut out = Vec::new();
        for i in 0..self.index.records.len() {
            out.extend(self.block(i)?);
        }
        Ok(out)
    }
}
