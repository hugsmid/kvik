//! Column encodings of kvik's compacted chunks (`docs/format-chunk.md`, roadmap 6.1), and the compacted segment and
//! chunk index object around them ([`file`], `docs/format-compacted.md`, roadmap 6.2a1).
//!
//! A compacted segment holds per-series chunks; each chunk is split into blocks of at most [`MAX_ROWS`] rows, and
//! each block body holds the rows' columns, each in its own encoding. This crate encodes and decodes one block
//! body ([`Block`]) and exposes the column codecs on their own ([`ints`], [`float`]), so that the chunk layout,
//! the index and the per-batch `received_at` table (roadmap 6.2) can reuse them.
//!
//! - `t`, `sequence` and `changed_at`: delta, frame-of-reference bit-packed per 128 values, or plain.
//! - `f64` values: scaled decimal with per-value exceptions, else XOR, compressed with zstd where that is smaller.
//! - decimal (`i64`), state, bool values and text lengths: delta, run-length or plain.
//! - quality, flags, value type, scale and generation: run-length or plain.
//!
//! Decoders never panic and never allocate from a declared count or length before checking it against the body;
//! decoded output is bounded by the row limit. A writer produces one byte sequence for given content
//! ([`Block::encode`]); [`Block::decode_canonical`] also requires it.
//!
//! ```
//! use kvik_chunk::{Block, value_type};
//!
//! let rows = 3;
//! let block = Block {
//!     t: vec![1_000_000_000, 2_000_000_000, 3_000_000_000],
//!     generation: vec![1; rows],
//!     sequence: vec![10, 14, 19],
//!     quality: vec![0; rows],
//!     flags: vec![0; rows],
//!     value_type: vec![value_type::F64; rows],
//!     value: [21.37f64, 21.38, 21.4].iter().map(|v| v.to_bits()).collect(),
//!     heap: vec![],
//!     changed_at: vec![0; rows],
//!     scale: vec![0; rows],
//! };
//! let body = block.encode().unwrap();
//! assert_eq!(Block::decode_canonical(&body).unwrap(), block);
//! ```

#![forbid(unsafe_code)]

mod block;
mod bytes;
mod error;
pub mod file;
pub mod float;
pub mod fuzz;
pub mod ints;

pub use block::Block;
pub use error::{ColumnError, DecodeError, Element, EncodeError};

/// The most rows a block holds.
pub const MAX_ROWS: usize = 4096;

/// The longest text value in bytes (`spec.md` §2.1).
pub const MAX_TEXT_LEN: usize = 255;

/// Column ids of a block body (`docs/format-chunk.md` C.2); ids 2–9 mean what they mean in a facts block
/// (`format.md` A.5).
pub mod column {
    /// `t`, `i64` nanoseconds.
    pub const T: u8 = 2;
    /// Adapter-supplied quality, `u8` 0–3; optional, absent means all good.
    pub const QUALITY: u8 = 3;
    /// Fact flags, `u16`; optional, absent means all zero.
    pub const FLAGS: u8 = 4;
    /// Value type, `u8` 1–5.
    pub const VALUE_TYPE: u8 = 5;
    /// Value words of the rows that have a value.
    pub const VALUE: u8 = 6;
    /// Text bytes, in row order; exactly when a row has value type 5.
    pub const TEXT_HEAP: u8 = 7;
    /// `changed_at` of the rows with flag bit 6; exactly when such a row exists.
    pub const CHANGED_AT: u8 = 8;
    /// Stored scale of the rows with value type 2; exactly when such a row exists.
    pub const SCALE: u8 = 9;
    /// Position generation, `u32`.
    pub const GENERATION: u8 = 10;
    /// Position sequence, `u64`.
    pub const SEQUENCE: u8 = 11;
}

/// Column encodings (`docs/format-chunk.md` C.3–C.8).
pub mod encoding {
    /// Fixed-width little-endian values.
    pub const PLAIN: u8 = 0;
    /// Runs of a LEB128 length and one fixed-width value.
    pub const RUN_LENGTH: u8 = 1;
    /// Delta, frame-of-reference bit-packed per 128 values.
    pub const DELTA: u8 = 2;
    /// Scaled decimal with per-value exceptions.
    pub const DECIMAL: u8 = 3;
    /// XOR with the previous value.
    pub const XOR: u8 = 4;
    /// XOR, compressed with zstd.
    pub const XOR_ZSTD: u8 = 5;
}

/// Value type codes (`format.md` A.5).
pub mod value_type {
    /// IEEE-754 `f64` bits.
    pub const F64: u8 = 1;
    /// Scaled `i64` (never `i64::MIN`).
    pub const DECIMAL: u8 = 2;
    /// State index, at most `u16::MAX`.
    pub const STATE: u8 = 3;
    /// Bool, 0 or 1.
    pub const BOOL: u8 = 4;
    /// UTF-8 text of at most 255 bytes.
    pub const TEXT: u8 = 5;
}

/// Fact flag bits that change what a block stores (`format.md` A.5).
pub mod flag {
    /// Bit 5 `no_value`: the row has no value word.
    pub const NO_VALUE: u16 = 1 << 5;
    /// Bit 6 `has_changed_at`: the row has a `changed_at`.
    pub const HAS_CHANGED_AT: u16 = 1 << 6;
    /// Bits 8–15: undefined and critical.
    pub const UNDEFINED: u16 = 0xFF00;
}
