//! Errors of the facts and entries body decoders (`format.md` A.5, A.6).

use crate::error::Element;

/// A check of A.5 or A.6 that a checksum-valid body fails. Such a block is invalid: an invalid region with cause 4,
/// invalid content (A.2). Columns are named by their A.5 id, entries by their index in the block, fields by their
/// A.6 id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ContentError {
    /// The body is shorter than its 24-byte prefix.
    Prefix,
    /// A facts block whose `row_count` is not 1 to 65,536.
    RowCount(u32),
    /// A column directory entry shorter than 12 bytes.
    DirEntrySize(u8),
    /// The column directory does not fit in the body.
    Directory,
    /// A column id twice, or out of ascending order.
    ColumnOrder {
        /// The column.
        column: u8,
    },
    /// A mandatory column (1, 2, 5, 6) is missing.
    MissingColumn {
        /// The column.
        column: u8,
    },
    /// A payload outside the body, or over the prefix or directory.
    PayloadOutside {
        /// The column.
        column: u8,
    },
    /// Two payloads overlap.
    Overlap {
        /// The column.
        column: u8,
    },
    /// A payload length that does not match `row_count` and the width.
    Length {
        /// The column.
        column: u8,
    },
    /// An encoding not allowed for the column.
    Encoding {
        /// The column.
        column: u8,
    },
    /// A run-length payload that is not canonical: a zero run, a non-minimal or over-long run length, equal adjacent
    /// runs, or runs that do not sum to `row_count`.
    RunLength {
        /// The column.
        column: u8,
    },
    /// A value out of its range.
    Value {
        /// The column.
        column: u8,
        /// The row.
        row: u32,
    },
    /// A presence rule broken: column 7 present exactly when a row is text, 8 exactly when a row has flag bit 6, 9
    /// exactly when a row is decimal; or the column holds a non-zero value on a row it does not apply to.
    Presence {
        /// The column.
        column: u8,
    },
    /// A text value outside the heap, longer than 255 bytes or not UTF-8.
    Text {
        /// The row.
        row: u32,
    },
    /// An entry header or its fields overrun the body, or an entry's `length` does not hold whole fields.
    EntryOverrun {
        /// The entry.
        entry: u32,
    },
    /// A critical field out of ascending order, or repeated when not marked repeated.
    FieldOrder {
        /// The entry.
        entry: u32,
        /// The field.
        field: u16,
    },
    /// A field value of the wrong length or out of its range.
    Field {
        /// The entry.
        entry: u32,
        /// The field.
        field: u16,
    },
    /// A field missing where it applies, or present where it does not (`model/OPEN-QUESTIONS.md`, Q-106 (d)).
    FieldPresence {
        /// The entry.
        entry: u32,
        /// The field.
        field: u16,
    },
    /// A field a parameter change may not carry (A.6, entry type 2).
    FieldNotAllowed {
        /// The entry.
        entry: u32,
        /// The field.
        field: u16,
    },
    /// A recovery entry that is not the only entry of its batch.
    RecoveryNotAlone,
}

/// Why a body could not be decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BodyError {
    /// It fails a check of A.5 or A.6: invalid content.
    Invalid(ContentError),
    /// It holds an unknown critical element: a version refusal.
    Unsupported(Element),
    /// It passes every check but is not what a v1 writer produces for its content (A, canonical writing). Only the
    /// canonical decoders report this.
    NonCanonical,
}

impl From<ContentError> for BodyError {
    fn from(e: ContentError) -> Self {
        Self::Invalid(e)
    }
}
