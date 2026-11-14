//! Backup and restore (spec §8; `docs/api.md` Backup and restore). Not
//! supported by the model backend yet: every call returns
//! [`Error::NotSupportedYet`](crate::Error::NotSupportedYet).

use std::future::Future;
use std::io;
use std::ops::Range;

/// What the bucket reports for a stored object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored {
    /// Size in bytes.
    pub size: u64,
    /// SHA-256 of the stored bytes, as the bucket reports it.
    pub sha256: [u8; 32],
}

/// The deep tier: an object store with no delete (spec §8). An S3-compatible
/// implementation lives in a separate crate.
pub trait BackupSink: Send + Sync {
    /// Stores an object; the bucket must reject a hash mismatch.
    fn put(
        &self,
        key: &str,
        bytes: Vec<u8>,
        sha256: [u8; 32],
    ) -> impl Future<Output = io::Result<Stored>> + Send;
    /// The reported size and hash of an object, if it exists.
    fn stat(&self, key: &str) -> impl Future<Output = io::Result<Option<Stored>>> + Send;
    /// An object's bytes, or a range of them.
    fn get(
        &self,
        key: &str,
        range: Option<Range<u64>>,
    ) -> impl Future<Output = io::Result<Vec<u8>>> + Send;
    /// The keys under a prefix.
    fn list(&self, prefix: &str) -> impl Future<Output = io::Result<Vec<String>>> + Send;
}

/// How files are encrypted before upload (spec §8 Encryption).
#[derive(Clone, PartialEq, Eq)]
pub enum Encryption {
    /// A 256-bit key supplied by the application.
    Key([u8; 32]),
}

impl std::fmt::Debug for Encryption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Encryption::Key(..)")
    }
}

/// How a restore brings detail back (spec §8 Restore).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RestoreMode {
    /// Serve from the checkpoint's summaries first, then bring detail back.
    #[default]
    SummariesFirst,
    /// A drill: restore read-only, claim and upload nothing.
    Drill,
}

/// What [`Store::rebuild`](crate::Store::rebuild) recomputes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Rebuild {
    /// Every derived structure, from the facts.
    #[default]
    Derived,
}
