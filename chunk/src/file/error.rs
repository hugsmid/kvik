//! Errors of the compacted segment and chunk index object readers (`docs/format-compacted.md` B.1, B.2).

use std::io;

/// An element a v1 reader does not know and may not ignore: a version refusal (`format.md` common rules).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileElement {
    /// A `format_major` above the supported one.
    FormatMajor(u8),
    /// A description field id below `0x8000` that v1 does not define.
    DescriptionField(u16),
    /// A file class v1 does not define.
    Class(u8),
    /// A level v1 does not define.
    Level(u8),
    /// Undefined bits of a chunk directory's flags.
    DirectoryFlags(u16),
    /// Undefined bits of a chunk index entry's flags.
    IndexFlags(u16),
    /// A commit-times column id below `0x80` that v1 does not define.
    TimesColumn(u8),
    /// An unknown critical element of a data block (`docs/format-chunk.md` C.11).
    Data(crate::Element),
    /// An unknown critical element of an entries block (`format.md` A.6).
    Entries(kvik_log::Element),
}

/// Why a structure on the read path is invalid: the file is invalid (`spec.md` §8 Corrupt block).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Fault {
    /// The file header is short, its checksum or magic is wrong.
    #[error("file header unreadable")]
    Header,
    /// The end marker is missing or points where no trailer can be.
    #[error("end marker missing or out of place")]
    EndMarker,
    /// A block header that does not fit or whose checksum does not match.
    #[error("block header unreadable")]
    BlockHeader,
    /// A block of another type than its pointer states.
    #[error("block of type {found:#04x} where {expected:#04x} was expected")]
    BlockType {
        /// The type the pointer states.
        expected: u8,
        /// The type found.
        found: u8,
    },
    /// A `body_len` above the limit, outside the file or other than its parent records.
    #[error("block length out of place")]
    BlockLength,
    /// A body whose checksum does not match its header's.
    #[error("block body checksum mismatch")]
    BlockChecksum,
    /// A checksum-valid block whose `body_crc32c` is not the one its parent records: a block from another place
    /// or an older file.
    #[error("block not the one its parent records")]
    Binding,
    /// A body that fails a check of its layout.
    #[error("malformed {0}")]
    Body(&'static str),
    /// A field that disagrees with its parent's or the trailer's record of it.
    #[error("{0} disagrees with its parent")]
    Mismatch(&'static str),
    /// A data block that fails `docs/format-chunk.md` C.11.
    #[error("data block: {0}")]
    Data(crate::DecodeError),
    /// An entries block that fails `format.md` A.6.
    #[error("entries block: {0:?}")]
    Entries(kvik_log::ContentError),
    /// A chunk index object whose `file_crc32c` does not match its bytes.
    #[error("file checksum mismatch")]
    FileChecksum,
    /// A position no commit-time row contains.
    #[error("position ({0}, {1}) has no commit-time row")]
    NoBatch(u32, u64),
}

/// Why a compacted segment or chunk index object cannot be read.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    /// A structure on the read path is invalid: the file is invalid.
    #[error("invalid file at offset {offset}: {fault}")]
    Invalid {
        /// File offset of the block, or of the file's structure, that failed.
        offset: u64,
        /// What failed.
        fault: Fault,
    },
    /// An unknown critical element: a version refusal, never damage.
    #[error("unsupported element at offset {offset}: {element:?}")]
    Unsupported {
        /// File offset of the block holding it.
        offset: u64,
        /// The element.
        element: FileElement,
    },
    /// A valid header of another kind or store, or one that disagrees with the file's name: refused
    /// (`format.md` A.12).
    #[error("header field {0} is not the expected one")]
    Refused(&'static str),
    /// The file disagrees with the layout the manifest records for it.
    #[error("the file disagrees with its recorded layout in {0}")]
    Layout(&'static str),
    /// The source could not be read.
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl FileError {
    pub(crate) fn invalid(offset: u64, fault: Fault) -> Self {
        Self::Invalid { offset, fault }
    }

    /// The fault, if the file is invalid.
    pub fn fault(&self) -> Option<Fault> {
        match self {
            Self::Invalid { fault, .. } => Some(*fault),
            _ => None,
        }
    }
}

/// A body that fails its checks, before the offset it was read at is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Bad {
    /// Invalid: a check of its layout fails.
    Invalid(Fault),
    /// A version refusal.
    Unsupported(FileElement),
}

impl Bad {
    pub(crate) fn body(what: &'static str) -> Self {
        Self::Invalid(Fault::Body(what))
    }

    pub(crate) fn at(self, offset: u64) -> FileError {
        match self {
            Self::Invalid(fault) => FileError::Invalid { offset, fault },
            Self::Unsupported(element) => FileError::Unsupported { offset, element },
        }
    }
}
