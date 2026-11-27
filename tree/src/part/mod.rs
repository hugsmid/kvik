//! A tree part (`kind = 6`, `format-tree.md` T.8 to T.11): immutable nodes of many series, one period. Encoded and
//! decoded over byte buffers; files are the writer's concern.

mod decode;
mod encode;

pub(crate) use decode::record_size;

use kvik_log::header::FileHeader;
use kvik_log::writer::Position;

use crate::frame::{EncodeError, FileError};
use crate::level::Level;
use crate::node::Node;

/// File kind of a tree part (`format.md` A.1).
pub const KIND: u8 = 6;

/// Block types of a part (T.8).
pub mod block_type {
    /// One series, one level, entries in ascending start (T.9).
    pub const NODES: u8 = 0x60;
    /// One fixed record per nodes block (T.10).
    pub const DIRECTORY: u8 = 0x61;
    /// One fixed record per directory block (T.10).
    pub const DIRECTORY_ROOT: u8 = 0x62;
    /// Ends the part (T.11).
    pub const TRAILER: u8 = 0x63;
}

pub(crate) const KNOWN: &[u8] = &[
    block_type::NODES,
    block_type::DIRECTORY,
    block_type::DIRECTORY_ROOT,
    block_type::TRAILER,
];

/// A nodes block body stops taking entries before it would pass this many bytes (T.9).
pub const TARGET_BODY: usize = 64 << 10;
pub(crate) const DIRECTORY_RECORD: usize = 48;
pub(crate) const DIRECTORY_ROOT_RECORD: usize = 32;
/// Count, record size and reserved, before the records of a directory or directory root body.
pub(crate) const ARRAY_PREFIX: usize = 8;
pub(crate) const DIRECTORY_PER_BLOCK: usize = (TARGET_BODY - ARRAY_PREFIX) / DIRECTORY_RECORD;
pub(crate) const TRAILER_FIXED: usize = 104;
pub(crate) const END_MARKER: &[u8; 8] = b"KVIKEND\0";

/// Why a part was written (T.11).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Class {
    /// Written by a checkpoint.
    Delta = 0,
    /// The merge of older parts of its period.
    Merged = 1,
    /// Written by a rebuild from the log.
    Rebuilt = 2,
}

impl Class {
    pub(crate) fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Delta),
            1 => Some(Self::Merged),
            2 => Some(Self::Rebuilt),
            _ => None,
        }
    }
}

/// One stored entry (T.9): a node, an empty marker that hides an older entry, or a stale mark.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Span start; the width is the level's.
    pub start: i64,
    /// `summary_stale`: the reader takes facts for this span.
    pub stale: bool,
    /// The node version (Q-1422).
    pub as_of: Position,
    /// `None`: no node bytes (empty without `stale`).
    pub node: Option<Node>,
}

/// The entries of one series at one level, in ascending start.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    /// Series id.
    pub series: u64,
    /// Level id, from the part's level table.
    pub level: u8,
    /// At least one entry.
    pub entries: Vec<Entry>,
}

/// A decoded part: what its bytes say, less what the layout derives (directory, summary fields, checksums).
#[derive(Clone, Debug, PartialEq)]
pub struct Part {
    /// The A.1 header: kind 6, the tree's own `file_id`.
    pub header: FileHeader,
    /// The first listing root's `seq`, or for a merged part its inputs' highest (Q-1540); a higher one overlays.
    pub seq: u64,
    /// Why it was written.
    pub class: Class,
    /// Period start; every entry lies inside the period.
    pub period_start: i64,
    /// Period length in nanoseconds, positive.
    pub period_len: u64,
    /// The checkpoint K the part was written at (invariant 26).
    pub through: Position,
    /// The level table it was written under.
    pub levels: Vec<Level>,
    /// Runs in ascending `(series, level)`, one per pair.
    pub runs: Vec<Run>,
}

impl Part {
    /// The canonical bytes of this part (T.8 to T.11); refuses whatever a reader would refuse.
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        let bytes = encode::encode(self)?;
        let again = decode::decode(&bytes)?;
        if again != *self {
            return Err(EncodeError::Shape(
                "a part that does not decode to itself (runs not one per series and level, or empty)",
            ));
        }
        Ok(bytes)
    }

    /// The part in `bytes`, with every check of T.11; whatever is accepted encodes to the same bytes.
    pub fn decode(bytes: &[u8]) -> Result<Part, FileError> {
        decode::decode(bytes)
    }
}
