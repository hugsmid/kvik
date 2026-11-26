//! What tree parts and roots share (`format-tree.md` T.8, T.12): refusals, the block walk, level tables and
//! positions. Tree files are derived (`format.md` A.18), so a reader accepts only canonical bytes: whatever decodes
//! encodes again to the same bytes.

use kvik_log::block::{BLOCK_HEADER_LEN, BlockHeader};
use kvik_log::crc::crc32c;
use kvik_log::header::{FileHeader, HEADER_LEN, MAGIC};
use kvik_log::writer::Position;

use crate::bytes::NotStorable;
use crate::level::{Level, MINUTE};

macro_rules! refusals {
    ($( $(#[$version:ident])? $name:ident => $text:literal, )*) => {
        /// Why bytes are not a tree part or root (`format-tree.md` T.11, T.12).
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Refusal {
            $(
                #[doc = $text]
                $name,
            )*
        }

        impl Refusal {
            /// Every refusal, so tests can show each has a vector.
            pub const ALL: &[Refusal] = &[$(Refusal::$name),*];

            /// What failed, in words.
            pub fn text(self) -> &'static str {
                match self { $(Refusal::$name => $text,)* }
            }

            /// A version refusal (`format.md` common rules): the file may be valid for a newer reader.
            pub fn is_version(self) -> bool {
                match self { $(Refusal::$name => refusals!(@v $($version)?),)* }
            }
        }
    };
    (@v version) => { true };
    (@v) => { false };
}

refusals! {
    Header => "the file header is short, its checksum fails or its magic is wrong",
    Kind => "the file is of another kind",
    #[version] NewerMajor => "a newer format major version",
    OlderMajor => "a format major version this reader never wrote",
    HeaderReserved => "header flags, base_sequence or reserved bytes are not zero",
    EndMarker => "the file does not end with a trailer offset and KVIKEND",
    TrailerOffset => "the trailer offset does not lead to a block that ends at the end marker",
    BlockHeader => "a block header is short or its checksum fails",
    BlockReserved => "block flags or reserved bytes are not zero",
    BodyLength => "a block body runs past its region or past 16 MiB",
    BodyChecksum => "a block body checksum fails",
    Padding => "padding after a block body is not zero",
    #[version] UnknownBlock => "a block type this version does not know",
    BlockOrder => "a block of a known type where another is due",
    Truncated => "a field runs past the end of its body",
    NotShortest => "an integer not in its shortest form",
    OutOfRange => "an integer out of range",
    Reserved => "reserved bytes are not zero",
    #[version] RecordSize => "records larger than this version writes",
    RecordSizeSmall => "records smaller than this version writes",
    ArrayLength => "the arrays do not fill the body exactly",
    LevelTableEmpty => "the level table is empty",
    LevelId => "a level id of zero, or level ids not ascending",
    LevelWidth => "a level width that is not a positive whole number of minutes",
    #[version] UnknownClass => "a part class this version does not know",
    Period => "a period that is empty or ends beyond i64",
    FileChecksum => "file_crc32c does not match",
    EmptyBlock => "a nodes block with no entries",
    UnknownLevel => "a level not in the file's level table",
    BaseStart => "the first entry of a nodes block is not at base_start",
    Start => "an entry start or end beyond i64",
    Alignment => "an entry or range not aligned to its level's width",
    #[version] EntryFlag => "an entry flag this version does not know",
    AsOf => "an as_of position out of range",
    BasePosition => "base_pos is not the least as_of of its series and level",
    #[version] NodeForm => "a node form this version does not know",
    Node => "node bytes that fail a check of T.6",
    NodeSpan => "a node whose span is not its entry's",
    NodeEmpty => "an empty node stored in full instead of as length 0",
    NodeKind => "nodes of one series of different forms",
    Order => "entries not in ascending series, level and start",
    OutsidePeriod => "an entry outside the part's period",
    Split => "nodes not split into blocks as the writer splits them",
    Directory => "the directory does not match the nodes blocks",
    DirectoryRoot => "the directory root does not match the directory",
    Summary => "the trailer's summary fields do not match the nodes",
    PartOrder => "parts not in ascending file_id, or two with one seq",
    FileId => "a file_id at or above next_file_id",
    CoveredRange => "covered ranges empty, out of order, overlapping or touching",
    FloorOrder => "floors not in ascending series",
    #[version] FloorFlag => "a floor flag this version does not know",
    RetiredOrder => "retired files not in ascending file_id, or also live",
    RootSeq => "the header's file_id is not the root's seq",
}

/// Bytes that are not a tree part or root: the refusal and the file offset where it was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("tree file refused at byte {at}: {}", refusal.text())]
pub struct FileError {
    /// File offset of the field or block.
    pub at: u64,
    /// The check that failed.
    pub refusal: Refusal,
}

/// A part or root that cannot be written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    /// A node has no stored form.
    #[error(transparent)]
    Node(#[from] NotStorable),
    /// A value the layout cannot hold: an unaligned start, a gap not in whole minutes, a body above 16 MiB.
    #[error("not encodable: {0}")]
    Shape(&'static str),
    /// The bytes would be refused by a reader.
    #[error(transparent)]
    Refused(#[from] FileError),
}

pub(crate) fn fail<T>(at: u64, refusal: Refusal) -> Result<T, FileError> {
    Err(FileError { at, refusal })
}

/// Largest block body (`format.md` A.2).
pub(crate) const MAX_BODY: usize = 16 << 20;
/// Size of a level record in a part trailer or a root.
pub(crate) const LEVEL_RECORD: usize = 16;

/// A cursor over one body; offsets it reports are file offsets.
pub(crate) struct Cursor<'a> {
    pub b: &'a [u8],
    pub pos: usize,
    pub base: u64,
}

impl<'a> Cursor<'a> {
    pub fn new(b: &'a [u8], base: u64) -> Self {
        Self { b, pos: 0, base }
    }

    pub fn at(&self) -> u64 {
        self.base + self.pos as u64
    }

    pub fn done(&self) -> bool {
        self.pos == self.b.len()
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8], FileError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.b.len())
            .ok_or(FileError {
                at: self.at(),
                refusal: Refusal::Truncated,
            })?;
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8, FileError> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, FileError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().expect("2")))
    }

    pub fn u32(&mut self) -> Result<u32, FileError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4")))
    }

    pub fn u64(&mut self) -> Result<u64, FileError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8")))
    }

    pub fn i64(&mut self) -> Result<i64, FileError> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().expect("8")))
    }

    pub fn position(&mut self) -> Result<Position, FileError> {
        Ok(Position {
            generation: self.u32()?,
            sequence: self.u64()?,
        })
    }

    pub fn reserved(&mut self, n: usize) -> Result<(), FileError> {
        let at = self.at();
        if self.take(n)?.iter().any(|b| *b != 0) {
            return fail(at, Refusal::Reserved);
        }
        Ok(())
    }

    /// Unsigned LEB128 in its shortest form, at most 64 bits (`format-tree.md` T.1 notation).
    pub fn uv64(&mut self) -> Result<u64, FileError> {
        let at = self.at();
        let mut v = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = self.u8()?;
            let low = u64::from(byte & 0x7F);
            if shift >= 64 || (shift == 63 && low > 1) {
                return fail(at, Refusal::OutOfRange);
            }
            v |= low << shift;
            if byte & 0x80 == 0 {
                if byte == 0 && shift > 0 {
                    return fail(at, Refusal::NotShortest);
                }
                return Ok(v);
            }
            shift += 7;
        }
    }
}

pub(crate) fn put_uv(w: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        w.push((v as u8 & 0x7F) | 0x80);
        v >>= 7;
    }
    w.push(v as u8);
}

pub(crate) fn put_position(w: &mut Vec<u8>, p: Position) {
    w.extend_from_slice(&p.generation.to_le_bytes());
    w.extend_from_slice(&p.sequence.to_le_bytes());
}

pub(crate) fn put_levels(w: &mut Vec<u8>, levels: &[Level]) {
    for l in levels {
        w.push(l.id);
        w.extend_from_slice(&[0; 7]);
        w.extend_from_slice(&l.width.to_le_bytes());
    }
}

/// `n` level records (T.11): ids ascending from 1, widths positive whole minutes.
pub(crate) fn levels(c: &mut Cursor<'_>, n: usize) -> Result<Vec<Level>, FileError> {
    if n == 0 {
        return fail(c.at(), Refusal::LevelTableEmpty);
    }
    let mut out: Vec<Level> = Vec::with_capacity(n.min(c.b.len() / LEVEL_RECORD));
    for _ in 0..n {
        let at = c.at();
        let id = c.u8()?;
        c.reserved(7)?;
        let width = c.i64()?;
        if id == 0 || out.last().is_some_and(|l| l.id >= id) {
            return fail(at, Refusal::LevelId);
        }
        if width <= 0 || width % MINUTE != 0 {
            return fail(at + 8, Refusal::LevelWidth);
        }
        out.push(Level { id, width });
    }
    Ok(out)
}

pub(crate) fn width_of(levels: &[Level], id: u8) -> Option<i64> {
    levels.iter().find(|l| l.id == id).map(|l| l.width)
}

/// The A.1 header of a tree file of `kind`; tree files keep flags, `base_sequence` and reserved bytes zero.
pub(crate) fn header(bytes: &[u8], kind: u8) -> Result<FileHeader, FileError> {
    let h = FileHeader::decode(bytes).ok_or(FileError {
        at: 0,
        refusal: Refusal::Header,
    })?;
    debug_assert_eq!(&bytes[..4], &MAGIC);
    if h.kind != kind {
        return fail(4, Refusal::Kind);
    }
    if h.format_major > kvik_log::FORMAT_MAJOR {
        return fail(5, Refusal::NewerMajor);
    }
    if h.format_major < kvik_log::FORMAT_MAJOR {
        return fail(5, Refusal::OlderMajor);
    }
    if h.flags != 0 || h.base_sequence != 0 || bytes[56..60] != [0; 4] {
        return fail(8, Refusal::HeaderReserved);
    }
    Ok(h)
}

pub(crate) fn check_header_fields(h: &FileHeader, kind: u8) -> Result<(), EncodeError> {
    if h.kind != kind
        || h.format_major != kvik_log::FORMAT_MAJOR
        || h.flags != 0
        || h.base_sequence != 0
    {
        return Err(EncodeError::Shape(
            "header kind, version, flags or base_sequence",
        ));
    }
    Ok(())
}

/// A block read at a file offset: its type and body.
pub(crate) struct Block<'a> {
    pub at: u64,
    pub block_type: u8,
    pub body: &'a [u8],
    /// File offset just past the block's padding.
    pub next: u64,
}

/// The block at `at` in `file`, which must end at or before `limit`: checksums, zero flags, reserved bytes and
/// padding. The type is left to the caller.
pub(crate) fn block(file: &[u8], at: u64, limit: u64) -> Result<Block<'_>, FileError> {
    let start = usize::try_from(at).expect("offset inside the file");
    let raw: &[u8; BLOCK_HEADER_LEN] = file
        .get(start..start + BLOCK_HEADER_LEN)
        .filter(|_| at + BLOCK_HEADER_LEN as u64 <= limit)
        .and_then(|s| s.try_into().ok())
        .ok_or(FileError {
            at,
            refusal: Refusal::BlockHeader,
        })?;
    let h = BlockHeader::decode(raw).ok_or(FileError {
        at,
        refusal: Refusal::BlockHeader,
    })?;
    if h.flags != 0 || raw[2..4] != [0, 0] {
        return fail(at, Refusal::BlockReserved);
    }
    let body_at = at + BLOCK_HEADER_LEN as u64;
    let next = h.block_len() + at;
    if h.body_len as usize > MAX_BODY || next > limit {
        return fail(at + 4, Refusal::BodyLength);
    }
    let body = &file[body_at as usize..body_at as usize + h.body_len as usize];
    if crc32c(body) != h.body_crc32c {
        return fail(at + 8, Refusal::BodyChecksum);
    }
    if file[body_at as usize + body.len()..next as usize]
        .iter()
        .any(|b| *b != 0)
    {
        return fail(body_at + body.len() as u64, Refusal::Padding);
    }
    Ok(Block {
        at,
        block_type: h.block_type,
        body,
        next,
    })
}

/// Refuses a block of a type other than `want`: one this version does not know is a version refusal.
pub(crate) fn expect_type(b: &Block<'_>, want: u8, known: &[u8]) -> Result<(), FileError> {
    if b.block_type == want {
        Ok(())
    } else if known.contains(&b.block_type) {
        fail(b.at, Refusal::BlockOrder)
    } else {
        fail(b.at, Refusal::UnknownBlock)
    }
}

pub(crate) fn header_len() -> u64 {
    HEADER_LEN as u64
}
