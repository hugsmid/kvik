//! The manifest of the kvik time-series store (`docs/spec.md` §7 Manifest, `docs/format.md` A.14).
//!
//! - [`desc`]: file descriptions, what a record says about a file and what a self-describing file says about itself.
//! - [`record`]: typed manifest records, their canonical encoding and strict decoding.
//! - [`image`]: reading a manifest whole, every valid record on both sides of each corrupt one.
//! - [`catalogue`]: the fold of the records: live and retired files by epoch, tiers, verification marks, losses,
//!   acknowledgements, rebuilds and `file_id` floors.
//! - [`appender`]: the one run-time writer of `MANIFEST`: the start's repair, appends after the previous record's
//!   sync, and the rule that the last record is not acted on irreversibly until a successor is synced.
//! - [`rebuild`]: the manifest rebuild from the local files (`rebuild_manifest`), and the provisional rebuild with
//!   its holds while deep cannot say whether it holds a copy of the corrupt record.
//!
//! ```
//! use kvik_files::appender::{Appender, Opened};
//! use kvik_files::record::Body;
//! use kvik_log::fs::{Fs, MemFs, OpenMode};
//! use kvik_log::manifest::encode_initial;
//! use std::path::Path;
//!
//! let fs = MemFs::new();
//! let dir = Path::new("store");
//! let f = fs.open(&dir.join("MANIFEST"), OpenMode::CreateNew).unwrap();
//! f.write_at(&encode_initial([1; 16], 0), 0).unwrap();
//! f.sync_all().unwrap();
//! drop(f);
//!
//! let Opened::Ready(mut m) = Appender::open(&fs, dir, [1; 16], 0).unwrap() else { panic!() };
//! let first = m.append(Body::Empty, 10).unwrap();
//! assert_eq!(first, 2);
//! // The record just appended is the last one: it is not settled until a successor is synced.
//! assert!(m.settled(first).is_none());
//! let s = m.settle(first, 11).unwrap();
//! assert_eq!(s.epoch(), first);
//! assert_eq!(m.catalogue().last_epoch(), 3);
//! ```

#![forbid(unsafe_code)]

pub mod appender;
pub mod catalogue;
pub mod desc;
mod fields;
#[doc(hidden)]
pub mod fuzz;
pub mod image;
pub mod rebuild;
pub mod record;

/// The manifest's file name in the store directory (`format.md` A.12).
pub const MANIFEST: &str = "MANIFEST";
/// A rebuilt manifest before its rename (`format.md` A.12).
pub const MANIFEST_NEW: &str = "MANIFEST.new";
