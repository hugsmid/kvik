//! Errors of the encoders and decoders.

/// Why one column payload could not be decoded (`docs/format-chunk.md` C.3–C.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum ColumnError {
    /// The payload ends before the values it must hold.
    #[error("payload cut short")]
    Truncated,
    /// Bytes are left after the last value.
    #[error("bytes after the last value")]
    TrailingBytes,
    /// A plain payload whose length is not the value count times the width.
    #[error("plain payload of the wrong length")]
    Length,
    /// Padding bits after packed values or a bit stream are not zero.
    #[error("non-zero padding bits")]
    Padding,
    /// A LEB128 integer that is not minimal, too long or too large.
    #[error("malformed LEB128 integer")]
    Leb128,
    /// A bit width above 64.
    #[error("bit width above 64")]
    Width,
    /// Run-length form broken: a zero run, equal adjacent runs, or runs not summing to the value count.
    #[error("malformed run-length payload")]
    RunLength,
    /// A decimal exponent above 18.
    #[error("decimal exponent above 18")]
    Exponent,
    /// More decimal exceptions than values.
    #[error("more exceptions than values")]
    ExceptionCount,
    /// Decimal exception indexes not strictly ascending, or not below the value count.
    #[error("exception index out of order or out of range")]
    ExceptionIndex,
    /// A scaled decimal integer of magnitude 2^53 or more.
    #[error("scaled integer out of range")]
    Mantissa,
    /// An XOR control code that reuses a window before one is set, or a window wider than 64 bits.
    #[error("malformed XOR control code")]
    XorWindow,
    /// A compressed payload whose declared length exceeds the largest XOR stream for its value count.
    #[error("declared raw length too large")]
    RawLength,
    /// A compressed payload that does not decompress to exactly its declared length.
    #[error("compressed payload does not decompress to its declared length")]
    Compressed,
}

/// An element a v1 reader does not know and may not ignore: a version refusal (`docs/format.md` common rules).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Element {
    /// A critical column id (`< 0x80`) not defined for a chunk block.
    Column(u8),
    /// Undefined fact flag bits (8–15).
    FactFlags(u16),
}

/// Why a block body could not be decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum DecodeError {
    /// `row_count` is 0 or above 4,096.
    #[error("row count {0} outside 1..=4096")]
    RowCount(u16),
    /// The column directory does not fit the body, or a length in it is malformed.
    #[error("malformed column directory")]
    Directory,
    /// Column ids not strictly ascending.
    #[error("column {column} out of order or repeated")]
    ColumnOrder {
        /// The column.
        column: u8,
    },
    /// The payloads do not end exactly at the end of the body.
    #[error("payloads do not end at the end of the body")]
    BodyLength,
    /// A mandatory column is missing.
    #[error("mandatory column {column} missing")]
    MissingColumn {
        /// The column.
        column: u8,
    },
    /// An encoding that is not allowed for the column (an unknown encoding included).
    #[error("encoding not allowed for column {column}")]
    Encoding {
        /// The column.
        column: u8,
    },
    /// A payload that fails the checks of its encoding.
    #[error("column {column}: {error}")]
    Column {
        /// The column.
        column: u8,
        /// What is wrong with it.
        error: ColumnError,
    },
    /// A value out of range for its column or value type.
    #[error("column {column}, row {row}: value out of range")]
    Value {
        /// The column.
        column: u8,
        /// The row.
        row: u16,
    },
    /// A text longer than 255 bytes, not UTF-8, or texts that do not fill the heap exactly.
    #[error("text of row {row} malformed")]
    Text {
        /// The row, or the row count when the heap holds bytes after the last text.
        row: u16,
    },
    /// An optional column present or absent against its presence rule.
    #[error("presence rule of column {column} broken")]
    Presence {
        /// The column.
        column: u8,
    },
    /// An unknown critical element: a version refusal, not invalid content.
    #[error("unsupported element {0:?}")]
    Unsupported(Element),
    /// The body passes every check but is not what a v1 writer produces for its content. Only
    /// [`Block::decode_canonical`](crate::Block::decode_canonical) reports this.
    #[error("not in canonical form")]
    NonCanonical,
}

/// Why a block could not be encoded: its content breaks a rule of `docs/format-chunk.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum EncodeError {
    /// No rows, or more than 4,096.
    #[error("row count {0} outside 1..=4096")]
    RowCount(usize),
    /// Column vectors of different lengths.
    #[error("column {column} has a length other than the row count")]
    Length {
        /// The column.
        column: u8,
    },
    /// A value out of range for its column or value type, or not zero where the format implies zero.
    #[error("column {column}, row {row}: value out of range")]
    Value {
        /// The column.
        column: u8,
        /// The row.
        row: usize,
    },
    /// A text longer than 255 bytes or not UTF-8, or text lengths that do not sum to the heap's length.
    #[error("text of row {row} malformed")]
    Text {
        /// The row, or the row count when the heap holds bytes after the last text.
        row: usize,
    },
}
