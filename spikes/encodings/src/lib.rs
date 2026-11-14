//! Spike for roadmap item 2.1: candidate encodings for compacted per-series
//! chunks, measured against Parquet.
//!
//! This crate is not part of the engine or the model. It generates synthetic
//! per-second data ([`data`]), encodes per-series chunks with several
//! candidate encodings ([`own`] and [`parquet_codec`]), checks that every
//! candidate round-trips bit-exactly, and measures size and scan speed
//! ([`measure`]). The results are written up in
//! `docs/measurements/2.1-chunk-encodings.md`.
//!
//! Run the measurement with `cargo run --release -p encodings`.
//!
//! ```
//! use encodings::data::Generator;
//! use encodings::own::Own;
//! use encodings::Codec;
//!
//! let mut g = Generator::new(1, 5, 600);
//! let span = g.next_span();
//! let chunks: Vec<_> = span.iter().map(|s| &s.current).collect();
//! let codec = Own::DOD_XOR_RLE;
//! let day = codec.encode(&chunks);
//! assert_eq!(&codec.decode(&day, 0), chunks[0]);
//! ```

#![forbid(unsafe_code)]

pub mod bits;
pub mod data;
pub mod measure;
pub mod own;
pub mod parquet_codec;

/// Rows per block (own encodings) and per data page (Parquet). A range scan
/// decodes whole blocks or pages.
pub const BLOCK_ROWS: usize = 4096;

/// The zstd level used by every candidate that compresses.
pub const ZSTD_LEVEL: i32 = 3;

/// Flag bit for a fact recorded without a value (`docs/format.md` A.5).
pub const FLAG_NO_VALUE: u16 = 1 << 5;
/// Flag bit `late`.
pub const FLAG_LATE: u16 = 1 << 1;
/// Flag bit `host_time`.
pub const FLAG_HOST_TIME: u16 = 1 << 3;

/// Quality codes as in `docs/format.md` A.5.
pub mod quality {
    /// Good.
    pub const GOOD: u8 = 0;
    /// Suspect.
    pub const SUSPECT: u8 = 2;
    /// Bad.
    pub const BAD: u8 = 3;
}

/// How the value column of a chunk is interpreted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ValueKind {
    /// IEEE-754 bit patterns.
    #[default]
    F64,
    /// A state index (0 or 1 for a boolean; at most `u16::MAX`).
    State,
    /// A two's-complement integer with a declared decimal scale
    /// (`docs/spec.md` §2.1); the scale is in the series definition.
    I64,
}

/// One per-series chunk in column form: either the current facts of a time
/// range or its revisions (`docs/spec.md` §7).
///
/// Rows are ordered by `t` (then by position for revisions). A row whose
/// flags carry [`FLAG_NO_VALUE`] has quality bad and value bits 0; no
/// candidate stores a value for it.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Chunk {
    /// Interpretation of `value`.
    pub kind: ValueKind,
    /// Event time, UTC nanoseconds.
    pub t: Vec<i64>,
    /// Value bits: an f64 bit pattern or a state index.
    pub value: Vec<u64>,
    /// Quality code.
    pub quality: Vec<u8>,
    /// Fact flags.
    pub flags: Vec<u16>,
    /// Sequence part of the position; the generation is constant per chunk.
    pub position: Vec<u64>,
}

impl Chunk {
    /// An empty chunk of the given kind.
    pub fn new(kind: ValueKind) -> Self {
        Chunk {
            kind,
            ..Chunk::default()
        }
    }

    /// Number of rows.
    pub fn len(&self) -> usize {
        self.t.len()
    }

    /// Whether the chunk has no rows.
    pub fn is_empty(&self) -> bool {
        self.t.is_empty()
    }

    /// Appends one row.
    pub fn push(&mut self, t: i64, value: u64, quality: u8, flags: u16, position: u64) {
        self.t.push(t);
        self.value.push(value);
        self.quality.push(quality);
        self.flags.push(flags);
        self.position.push(position);
    }

    /// Appends rows `range` of `other`.
    pub fn extend_from(&mut self, other: &Chunk, range: std::ops::Range<usize>) {
        self.t.extend_from_slice(&other.t[range.clone()]);
        self.value.extend_from_slice(&other.value[range.clone()]);
        self.quality
            .extend_from_slice(&other.quality[range.clone()]);
        self.flags.extend_from_slice(&other.flags[range.clone()]);
        self.position.extend_from_slice(&other.position[range]);
    }

    /// The rows with `from <= t < to`, as a new chunk (rows are ordered by `t`).
    pub fn slice_t(&self, from: i64, to: i64) -> Chunk {
        let a = self.t.partition_point(|&x| x < from);
        let b = self.t.partition_point(|&x| x < to);
        let mut out = Chunk::new(self.kind);
        out.extend_from(self, a..b);
        out
    }
}

/// Size of an encoded chunk, split by column, in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColumnSizes {
    /// `t` column.
    pub t: u64,
    /// Value column.
    pub value: u64,
    /// Quality and flags columns together.
    pub quality_flags: u64,
    /// Position column.
    pub position: u64,
    /// Headers, block directories, footers and indexes.
    pub overhead: u64,
}

impl ColumnSizes {
    /// Sum of all parts.
    pub fn total(&self) -> u64 {
        self.t + self.value + self.quality_flags + self.position + self.overhead
    }

    /// Adds `other` to `self`.
    pub fn add(&mut self, other: &ColumnSizes) {
        self.t += other.t;
        self.value += other.value;
        self.quality_flags += other.quality_flags;
        self.position += other.position;
        self.overhead += other.overhead;
    }
}

/// A candidate encoding. A set of chunks (all chunks of one span, as one
/// compacted file would hold them) is encoded together; chunks are then
/// decoded one at a time, in full or for a time range.
pub trait Codec {
    /// The encoded form of a set of chunks.
    type Encoded;

    /// Short name for tables.
    fn name(&self) -> &'static str;

    /// Encodes `chunks`, in order, as one file.
    fn encode(&self, chunks: &[&Chunk]) -> Self::Encoded;

    /// Per-chunk sizes. File-level overhead is shared equally among chunks.
    fn sizes(&self, encoded: &Self::Encoded) -> Vec<ColumnSizes>;

    /// Total encoded size in bytes.
    fn total_size(&self, encoded: &Self::Encoded) -> u64 {
        self.sizes(encoded).iter().map(ColumnSizes::total).sum()
    }

    /// Checks the encoded layout (Parquet: encodings and page sizes); panics
    /// if it is not the intended one. Not timed.
    fn check(&self, encoded: &Self::Encoded) {
        let _ = encoded;
    }

    /// Does what a reader must do once per file before reading chunks, and
    /// throws the result away (Parquet: parse the footer and page index).
    fn open(&self, encoded: &Self::Encoded) {
        let _ = encoded;
    }

    /// Decodes chunk `index` in full.
    fn decode(&self, encoded: &Self::Encoded, index: usize) -> Chunk;

    /// Decodes the rows of chunk `index` with `from <= t < to`.
    fn decode_range(&self, encoded: &Self::Encoded, index: usize, from: i64, to: i64) -> Chunk;

    /// Size in bytes of one extra `i64` column (such as `received_at`) per
    /// chunk, encoded the way this candidate encodes `t`, for every chunk in
    /// `columns`.
    fn i64_column_size(&self, columns: &[&[i64]]) -> u64;
}
