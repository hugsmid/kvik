//! Backup and restore (spec §8; `docs/api.md` Backup and restore). Not
//! supported by the model backend yet: every call returns
//! [`BackupError::NotSupportedYet`](crate::BackupError::NotSupportedYet).
//! The sink's signatures were settled with roadmap 8.2; an S3-compatible
//! implementation and an in-memory fake are in the `kvik-sink` crate.

use std::future::Future;
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;

/// What the bucket reports for a stored object.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Stored {
    /// Size in bytes.
    pub size: u64,
    /// SHA-256 of the stored bytes, as the bucket reports it.
    pub sha256: [u8; 32],
}

impl Stored {
    /// What the bucket reports, for implementations of [`BackupSink`].
    pub fn new(size: u64, sha256: [u8; 32]) -> Self {
        Stored { size, sha256 }
    }
}

/// One page of keys from [`BackupSink::list`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ListPage {
    /// Keys in ascending byte order, each starting with the prefix asked for.
    pub keys: Vec<String>,
    /// Whether keys after the last one may follow: ask again with the last
    /// key as `start_after`.
    pub more: bool,
}

impl ListPage {
    /// A page, for implementations of [`BackupSink`].
    pub fn new(keys: Vec<String>, more: bool) -> Self {
        ListPage { keys, more }
    }
}

/// Why a [`BackupSink`] call failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SinkError {
    /// `put` found an object under the key with other content (size or
    /// reported SHA-256). Nothing was overwritten.
    #[error("an object with other content exists under {key}")]
    Exists {
        /// The key.
        key: String,
    },
    /// `get` found no object under the key.
    #[error("no object under {key}")]
    NotFound {
        /// The key.
        key: String,
    },
    /// The bucket rejected an upload whose bytes do not match the SHA-256
    /// sent with it, or reported a SHA-256 other than the one sent.
    #[error("the bucket rejected or misreported the SHA-256 of {key}")]
    HashMismatch {
        /// The key.
        key: String,
    },
    /// A range that does not lie within the object.
    #[error("range not within the object under {key}")]
    InvalidRange {
        /// The key.
        key: String,
    },
    /// An object larger than one single-part upload allows (spec §8 Hashes).
    #[error("object of {size} bytes is above the single-part limit of {limit}")]
    TooLarge {
        /// The object's size.
        size: u64,
        /// The limit.
        limit: u64,
    },
    /// A fault that a later attempt may not meet: a timeout, a lost
    /// connection, a server error, throttling, or a download whose bytes do
    /// not match what the bucket reports. A `put` that fails this way may
    /// have stored the object; repeating it is safe.
    #[error("transient: {reason}")]
    Transient {
        /// What happened.
        reason: String,
    },
    /// A fault that repeating will not cure: refused credentials, a missing
    /// bucket, a reply the sink cannot read.
    #[error("failed: {reason}")]
    Failed {
        /// What happened.
        reason: String,
    },
}

impl SinkError {
    /// Whether repeating the call may succeed.
    pub fn is_transient(&self) -> bool {
        matches!(self, SinkError::Transient { .. })
    }
}

/// The future a [`BackupSink`] call returns: boxed, `Send` and independent
/// of any async runtime, so that `dyn BackupSink` works.
pub type SinkFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, SinkError>> + Send + 'a>>;

/// The deep tier: an object store with no delete (spec §8). An S3-compatible
/// implementation lives in the `kvik-sink` crate.
///
/// Keys are opaque to the sink. Every implementation keeps these rules:
/// - **No overwrite.** `put` creates an object only where the key is absent.
///   Where an object exists with the same size and reported SHA-256, `put`
///   succeeds with what the bucket reports (an upload repeated after a lost
///   reply or a restart); with other content it fails with
///   [`SinkError::Exists`]. This is also the create-if-absent of generation
///   claims (spec §8 Generations).
/// - **Hashes.** `put` sends the SHA-256 with a single-part upload; the
///   bucket must reject a mismatch ([`SinkError::HashMismatch`]). The
///   returned [`Stored`] is what the bucket reports when asked after the
///   upload, never the arguments echoed, and `put` fails with
///   [`SinkError::HashMismatch`] where it differs from them.
/// - **No delete.** The trait has none.
pub trait BackupSink: Send + Sync {
    /// Stores an object if the key is absent (see the rules above).
    fn put<'a>(
        &'a self,
        key: &'a str,
        bytes: Arc<[u8]>,
        sha256: [u8; 32],
    ) -> SinkFuture<'a, Stored>;
    /// The reported size and hash of an object, if it exists.
    fn stat<'a>(&'a self, key: &'a str) -> SinkFuture<'a, Option<Stored>>;
    /// An object's bytes, or a half-open range of them. An empty range
    /// within the object returns no bytes.
    fn get<'a>(&'a self, key: &'a str, range: Option<Range<u64>>) -> SinkFuture<'a, Vec<u8>>;
    /// One page of the keys under a prefix, in ascending order, starting
    /// after `start_after` where given. A key just stored may be missing
    /// from a listing for a while on some buckets: absence from a listing
    /// proves nothing, and `put` alone decides whether a key is taken.
    fn list<'a>(
        &'a self,
        prefix: &'a str,
        start_after: Option<&'a str>,
    ) -> SinkFuture<'a, ListPage>;
}

macro_rules! forward_sink {
    ($ty:ty) => {
        impl<S: BackupSink + ?Sized> BackupSink for $ty {
            fn put<'a>(
                &'a self,
                key: &'a str,
                bytes: Arc<[u8]>,
                sha256: [u8; 32],
            ) -> SinkFuture<'a, Stored> {
                (**self).put(key, bytes, sha256)
            }
            fn stat<'a>(&'a self, key: &'a str) -> SinkFuture<'a, Option<Stored>> {
                (**self).stat(key)
            }
            fn get<'a>(
                &'a self,
                key: &'a str,
                range: Option<Range<u64>>,
            ) -> SinkFuture<'a, Vec<u8>> {
                (**self).get(key, range)
            }
            fn list<'a>(
                &'a self,
                prefix: &'a str,
                start_after: Option<&'a str>,
            ) -> SinkFuture<'a, ListPage> {
                (**self).list(prefix, start_after)
            }
        }
    };
}

forward_sink!(Arc<S>);
forward_sink!(Box<S>);

/// How files are encrypted before upload (spec §8 Encryption).
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
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
#[non_exhaustive]
pub enum RestoreMode {
    /// Serve from the checkpoint's summaries first, then bring detail back.
    #[default]
    SummariesFirst,
    /// A drill: restore read-only, claim and upload nothing.
    Drill,
}

/// What [`Store::rebuild`](crate::Store::rebuild) recomputes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Rebuild {
    /// Every derived structure, from the facts.
    #[default]
    Derived,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_is_constructible() {
        let s = Stored::new(3, [7; 32]);
        assert_eq!((s.size, s.sha256), (3, [7; 32]));
    }

    struct Nothing;

    impl BackupSink for Nothing {
        fn put<'a>(&'a self, key: &'a str, _: Arc<[u8]>, _: [u8; 32]) -> SinkFuture<'a, Stored> {
            Box::pin(async move { Err(SinkError::Exists { key: key.into() }) })
        }
        fn stat<'a>(&'a self, _: &'a str) -> SinkFuture<'a, Option<Stored>> {
            Box::pin(async { Ok(None) })
        }
        fn get<'a>(&'a self, key: &'a str, _: Option<Range<u64>>) -> SinkFuture<'a, Vec<u8>> {
            Box::pin(async move { Err(SinkError::NotFound { key: key.into() }) })
        }
        fn list<'a>(&'a self, _: &'a str, _: Option<&'a str>) -> SinkFuture<'a, ListPage> {
            Box::pin(async { Ok(ListPage::default()) })
        }
    }

    #[test]
    fn the_sink_is_usable_as_a_trait_object() {
        fn takes_dyn(_: &dyn BackupSink) {}
        fn takes_generic<S: BackupSink>(_: S) {}
        let sink: Arc<dyn BackupSink> = Arc::new(Nothing);
        takes_dyn(&*sink);
        takes_generic(sink.clone());
        takes_generic(Box::new(Nothing) as Box<dyn BackupSink>);
        assert!(SinkError::Transient { reason: "x".into() }.is_transient());
        assert!(!SinkError::Exists { key: "k".into() }.is_transient());
    }
}
