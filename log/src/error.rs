//! Errors.

use std::fmt;

/// An element of a file that this version of the format does not know and may not ignore.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Element {
    /// A file whose `format_major` is newer than the supported one.
    FormatMajor(u8),
    /// A block type that is not known and not `skippable` (`format.md` A.2).
    BlockType(u8),
    /// A mark slot of unknown version (`format.md` A.10).
    MarkVersion(u32),
    /// A facts column id below `0x80` that is not known (A.5).
    Column(u8),
    /// Fact flag bits not defined in A.5 (bits 8 to 15).
    FactFlags(u16),
    /// An entry type below `0x80` that is not known or not yet laid out (A.6).
    EntryType(u8),
    /// A field id below `0x8000` that the entry type does not define (A.6).
    Field {
        /// The entry type.
        entry_type: u8,
        /// The field id.
        field: u16,
    },
}

impl fmt::Display for Element {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FormatMajor(v) => write!(
                f,
                "format major version {v} (supported: {})",
                crate::FORMAT_MAJOR
            ),
            Self::BlockType(t) => write!(f, "block type {t:#04x}"),
            Self::MarkVersion(v) => write!(
                f,
                "mark version {v} (supported: {})",
                crate::mark::MARK_VERSION
            ),
            Self::Column(c) => write!(f, "facts column {c:#04x}"),
            Self::FactFlags(b) => write!(f, "fact flag bits {b:#06x}"),
            Self::EntryType(t) => write!(f, "entry type {t}"),
            Self::Field { entry_type, field } => {
                write!(f, "field {field} of entry type {entry_type}")
            }
        }
    }
}

/// A file that must not be read or written by this version (a version refusal), or that does not belong where it
/// was found. Damage is never reported this way: it is an invalid region or a torn tail ([`crate::scan`]).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FormatError {
    /// An unknown critical element in a checksum-valid structure (`format.md` A, common rules): a version refusal.
    #[error("unsupported format at offset {offset}: {element}")]
    Unsupported {
        /// File offset of the structure holding the element.
        offset: u64,
        /// The element.
        element: Element,
    },
    /// A checksum-valid header of another file kind.
    #[error("file kind {found}, expected {expected}")]
    WrongKind {
        /// Kind in the header.
        found: u8,
        /// Kind expected.
        expected: u8,
    },
    /// A checksum-valid header whose `store_id` is not the store's (`format.md` A.1).
    #[error("the file belongs to another store")]
    StoreIdMismatch,
    /// A scan limit or resume point that is not a multiple of 8, or a resume point below the header's end or
    /// above the limit.
    #[error("scan point {offset} is not a block boundary within the limit")]
    ScanPoint {
        /// The offset.
        offset: u64,
    },
    /// A checksum-valid header that disagrees with the file name: refused (`docs/format.md` A.12).
    #[error("header field {field} is {in_header}, the file name gives {expected}")]
    HeaderMismatch {
        /// The field.
        field: &'static str,
        /// Its value in the header.
        in_header: u64,
        /// Its value from the file name.
        expected: u64,
    },
}

/// A request to the writer side that would not produce a valid file.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    /// Block type 0, or a type that is not a data block where one is needed.
    #[error("block type {0:#04x} is not allowed here")]
    BlockType(u8),
    /// A body longer than 16 MiB.
    #[error("body of {0} bytes is longer than the maximum")]
    BodyTooLong(usize),
    /// A facts or entries body whose 24-byte prefix does not pass its checks.
    #[error("data block body {0} has an invalid prefix")]
    DataPrefix(usize),
    /// A batch with no data block whose sequences are known.
    #[error("a batch needs at least one facts or entries block")]
    EmptyBatch,
    /// Data blocks whose sequences are not consecutive, or whose `received_at` differ.
    #[error("data block {0} does not follow the previous one")]
    NotConsecutive(usize),
    /// A batch whose first sequence is not above the segment's last, or below `base_sequence`.
    #[error("batch starts at sequence {first}, the segment requires at least {min}")]
    SequenceNotAbove {
        /// The batch's first sequence.
        first: u64,
        /// The least first sequence allowed.
        min: u64,
    },
    /// An offset or length that is not a multiple of 8, or below the header.
    #[error("offset or length {0} is not allowed")]
    Misaligned(u64),
    /// The bytes do not fit in the segment's current length.
    #[error("{needed} bytes do not fit in the {left} bytes left")]
    DoesNotFit {
        /// Bytes needed.
        needed: u64,
        /// Bytes left.
        left: u64,
    },
    /// Content that would fail a check of A.5 or A.6.
    #[error("invalid content: {0:?}")]
    Content(crate::content_error::ContentError),
    /// An entry above 8 MiB, or a batch above 8 MiB of data blocks.
    #[error("too large")]
    TooLarge,
    /// A segment that is not in use yet (`base_sequence = 0`), already sealed, or empty where a batch is needed.
    #[error("operation not allowed in the segment's state: {0}")]
    State(&'static str),
}
