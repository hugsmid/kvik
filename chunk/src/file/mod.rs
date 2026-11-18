//! Compacted segments (`kind = 2`) and chunk index objects (`kind = 14`), `docs/format-compacted.md`.
//!
//! - [`FileWriter`] writes one compacted segment from the rows of one series at a time, the entries and the batch
//!   table, in the canonical byte sequence (B.1 Canonical writing).
//! - [`CompactedFile`] is the positional reader: it reads the header, the end marker and the trailer at open, then
//!   only the blocks a caller follows a pointer to, each bound to its parent's recorded checksum (B.1 A reader's
//!   checks), so damage stays inside the block it hits.
//! - [`IndexObject`] reads a chunk index object; [`extract`] makes one from a compacted segment (B.2).
//! - [`check_canonical`] applies the canonical checks: the writer's read-back, golden files and the scrub.
//!
//! ```
//! use kvik_chunk::file::{
//!     Batch, Class, CompactedFile, Description, Fact, FileWriter, Identity, Input, Level, Reach, Series,
//! };
//! use kvik_chunk::value_type;
//!
//! let id = Identity { store_id: [1; 16], generation: 0, file_id: 9 };
//! let description = Description {
//!     class: Class::Current,
//!     level: Level::L0,
//!     period_start: 0,
//!     period_len: 86_400_000_000_000,
//!     run_id: 9,
//!     run_count: 1,
//!     inputs: vec![Input { kind: 1, file_id: 3 }],
//!     name_range: None,
//!     positions: vec![(0, 2)],
//!     has_entries: false,
//!     has_set_aside: false,
//!     complete: true,
//! };
//! let batches = vec![Batch {
//!     generation: 0,
//!     first_sequence: 1,
//!     last_sequence: 2,
//!     received_at: 50,
//!     commit_time: 60,
//! }];
//! let fact = |sequence, t, v: f64| Fact::value(7, (0, sequence), t, value_type::F64, v.to_bits());
//! let mut w = FileWriter::new(Vec::new(), id, 0, &description, batches).unwrap();
//! w.series(Series {
//!     series_id: 7,
//!     name: Some("room/temperature".into()),
//!     reach: Reach::Point,
//!     facts: vec![fact(1, 10, 21.5), fact(2, 20, 21.625)],
//!     set_aside: vec![],
//! })
//! .unwrap();
//! let (bytes, _) = w.finish().unwrap();
//!
//! let file = CompactedFile::open(bytes.as_slice(), &id, None).unwrap();
//! let entries = file.series(7).unwrap();
//! assert_eq!(entries[0].bounds().unwrap().f64_range(), Some((21.5, 21.625)));
//! let rows = file.read_chunk(&entries[0]).unwrap();
//! assert_eq!((rows[1].fact.t, rows[1].received_at, rows[1].commit_time), (20, 50, 60));
//! ```

mod access;
mod canonical;
mod desc;
mod error;
mod index;
mod layout;
mod parts;
mod read;
#[doc(hidden)]
pub mod reseal;
mod source;
mod stats;
mod times;
mod write;

pub use canonical::{CanonicalError, SeriesInfo, check_canonical};
pub use desc::{Class, Description, Input, Level};
pub use error::{Fault, FileElement, FileError};
pub use index::{IndexObject, extract};
pub use layout::{EntryRow, Identity, Ignored, Layout, TimedFact};
pub use parts::{BlockRecord, Directory, IndexEntry, RootRecord, TimesRecord, Trailer};
pub use read::CompactedFile;
pub use source::ReadAt;
pub use stats::{Bounds, Reach};
pub use times::Batch;
pub use write::{Fact, FileWriter, Series, WriteError, Written};

#[doc(hidden)]
pub use write::Tweaks;

/// A position: generation, then sequence (`format.md` A, common rules).
pub type Position = (u32, u64);

/// The kind of a compacted segment (`format.md` A.1).
pub const KIND_COMPACTED: u8 = 2;
/// The kind of a chunk index object (`format.md` A.1).
pub const KIND_INDEX: u8 = 14;

/// Block types of a compacted segment and a chunk index object (B.1, B.2).
pub mod block_type {
    /// An entries block (`format.md` A.6 body).
    pub const ENTRIES: u8 = 0x02;
    /// A chunk data block (`docs/format-chunk.md` C.2 body).
    pub const DATA: u8 = 0x30;
    /// A chunk directory.
    pub const DIRECTORY: u8 = 0x31;
    /// Commit times of whole batches.
    pub const TIMES: u8 = 0x32;
    /// The commit-time directory.
    pub const TIMES_DIRECTORY: u8 = 0x33;
    /// A chunk index block.
    pub const INDEX: u8 = 0x34;
    /// The index root.
    pub const ROOT: u8 = 0x35;
    /// The description.
    pub const DESCRIPTION: u8 = 0x36;
    /// A generation marker before that generation's entries blocks.
    pub const MARKER: u8 = 0x37;
    /// A chunk index object's copy of its segment's trailer, length and hash.
    pub const SEGMENT: u8 = 0x38;
    /// The trailer.
    pub const TRAILER: u8 = 0x7F;
}

/// Rows in every data block of a chunk but its last.
pub const BLOCK_ROWS: usize = crate::MAX_ROWS;
/// The most data blocks a chunk holds.
pub const MAX_CHUNK_BLOCKS: usize = 64;
/// Rows in every chunk of a series but its last.
pub const CHUNK_ROWS: usize = BLOCK_ROWS * MAX_CHUNK_BLOCKS;
/// Entries in every chunk index block but the last.
pub const INDEX_BLOCK_ENTRIES: usize = 1024;
/// The most inputs a description names.
pub const MAX_INPUTS: usize = 1024;
/// Rows in every commit-times block of a generation but its last.
pub const TIMES_ROWS: usize = crate::MAX_ROWS;

/// Feeds `data` to every body decoder of B.1 and B.2; whatever decodes must encode and decode again to itself.
pub(crate) fn fuzz_bodies(data: &[u8]) {
    if let Ok(d) = desc::Description::decode(data) {
        assert_eq!(desc::Description::decode(&d.encode()), Ok(d));
    }
    if let Ok(d) = parts::Directory::decode(data) {
        assert_eq!(parts::Directory::decode(&d.encode()), Ok(d));
    }
    if let Ok(e) = parts::decode_index_block(data) {
        assert_eq!(
            parts::decode_index_block(&parts::encode_index_block(&e)),
            Ok(e)
        );
    }
    if let Ok(r) = parts::decode_root(data) {
        assert_eq!(parts::decode_root(&parts::encode_root(&r)), Ok(r));
    }
    if let Ok(r) = parts::decode_times_directory(data) {
        assert_eq!(
            parts::decode_times_directory(&parts::encode_times_directory(&r)),
            Ok(r)
        );
    }
    if let Ok(t) = parts::Trailer::decode(data) {
        assert_eq!(parts::Trailer::decode(&t.encode()), Ok(t));
    }
    if let Ok(t) = parts::IndexTrailer::decode(data) {
        assert_eq!(parts::IndexTrailer::decode(&t.encode()), Ok(t));
    }
    if let Ok((g, rows, _)) = times::decode_times(data) {
        assert_eq!(
            times::decode_times(&times::encode_times(g, &rows, None)),
            Ok((g, rows, false))
        );
    }
}
