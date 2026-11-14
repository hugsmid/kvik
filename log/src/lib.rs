//! The on-disk log format of the kvik time-series store: log segment files, their blocks and checksums, and the
//! durable mark, as specified in `docs/format.md` Part A.
//!
//! - [`header`]: the 64-byte file header (A.1).
//! - [`block`]: block framing and checksums (A.2).
//! - [`body`]: the bodies the container reads itself: the data block prefix, commit, segment index, trailer and end
//!   marker (A.5 to A.9).
//! - [`batch`]: framing a batch and deciding whether one is complete (A.4, A.7).
//! - [`scan`]: reading a segment up to the durable mark or to its end, with resynchronisation past invalid regions,
//!   the tail and the seal (A.2, A.4, A.9).
//! - [`segment`]: the life cycle of a segment as bytes, and the states recovery may find (A.4).
//! - [`mark`]: the durable mark's slots (A.10).
//! - [`name`]: segment file names (A.12).
//!
//! The columns of facts blocks and the entries of entries blocks are encoded and checked by the batch layer
//! (roadmap 4.3); file operations go through the file-system seam (roadmap 4.4).
//!
//! ```
//! use kvik_log::batch::{BatchBuilder, SegmentIdentity};
//! use kvik_log::block::block_type;
//! use kvik_log::body::DataPrefix;
//! use kvik_log::scan::{ScanLimit, Tail, scan};
//! use kvik_log::segment::SegmentImage;
//!
//! let id = SegmentIdentity { store_id: [1; 16], file_id: 1, generation: 0 };
//! let mut seg = SegmentImage::create_ahead(id, 0, 4096).unwrap();
//! seg.take_into_use(1).unwrap();
//! // An entries block holding one alias entry (A.6): type 3, length 16, field 1 (series 7), field 2 ("a").
//! let mut body = vec![0u8; 24];
//! DataPrefix { first_sequence: 1, received_at: 10, count: 1 }.write(&mut body);
//! body.extend_from_slice(&[3, 0, 0, 0, 16, 0, 0, 0]);
//! body.extend_from_slice(&[1, 0, 4, 0, 7, 0, 0, 0, 2, 0, 1, 0, b'a', 0, 0, 0]);
//! let mut batch = BatchBuilder::new();
//! batch.push(block_type::ENTRIES, body).unwrap();
//! let at = seg.append(&batch, 11).unwrap();
//!
//! let report = scan(seg.bytes(), &id, ScanLimit::End).unwrap();
//! assert_eq!(report.batches.len(), 1);
//! assert_eq!(report.batches[0].offset, at);
//! assert_eq!(report.tail, Some(Tail::Clean { offset: report.batches[0].end() }));
//!
//! // A reader process reads only up to the mark's offset.
//! let report = scan(seg.bytes(), &id, ScanLimit::Mark(at)).unwrap();
//! assert!(report.batches.is_empty());
//! ```

#![forbid(unsafe_code)]

pub mod batch;
pub mod block;
pub mod body;
mod bytes;
pub mod crc;
mod error;
#[doc(hidden)]
pub mod fuzz;
pub mod header;
pub mod mark;
pub mod name;
pub mod scan;
pub mod segment;

pub use error::{Element, EncodeError, FormatError};

/// The major format version this crate reads and writes.
pub const FORMAT_MAJOR: u8 = 1;
/// The minor format version this crate writes.
pub const FORMAT_MINOR: u16 = 0;
