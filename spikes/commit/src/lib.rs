//! Spike for roadmap item 2.2: commit and sync latency, group commit, and behaviour on a sync failure and a
//! full disk. Not part of the engine; see `docs/measurements/2.2-commit-latency.md`.
//!
//! The crate holds just enough of the `format.md` Part A append path to measure it: preallocated segments,
//! checksummed blocks and commit blocks, a group-commit writer and the durable mark with the timed mark sync of
//! `spec.md` §3, behind a file-system seam with a simulated implementation for fault injection.

pub mod format;
pub mod fs;
pub mod recovery;
pub mod stats;
pub mod writer;
