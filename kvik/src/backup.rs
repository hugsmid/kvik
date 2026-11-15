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
    /// reported SHA-256) before any attempt of the call could have reached the
    /// bucket. Nothing was overwritten.
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
    /// sent with it, or reported other content than the one sent. After an
    /// attempt that may have landed this includes another writer's object,
    /// which the sink cannot tell from its own damaged upload (`docs/api.md`).
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
    #[error("object of {size} bytes is above the limit of {limit}")]
    TooLarge {
        /// The object's size.
        size: u64,
        /// The limit.
        limit: u64,
    },
    /// The sink's queue of calls is full; the call was not started.
    #[error("the sink is busy: {queued} calls queued")]
    Busy {
        /// Calls queued.
        queued: usize,
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
        matches!(self, SinkError::Transient { .. } | SinkError::Busy { .. })
    }
}

/// The future a [`BackupSink`] call returns: boxed, `Send` and independent
/// of any async runtime, so that `dyn BackupSink` works.
pub type SinkFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, SinkError>> + Send + 'a>>;

/// The deep tier: an object store with no overwrite and no delete (spec §8).
/// The rules every implementation keeps are in `docs/api.md` Backup and restore.
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

/// Proof that the bucket reported the expected size and SHA-256 for a key
/// when asked (spec §8 Hashes: "verified in another tier"); made only by
/// [`Verified::check`]. It does not prove the bytes: a sampled download does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    key: String,
    stored: Stored,
}

impl Verified {
    /// Asks the sink for the key's reported size and SHA-256 and returns a
    /// proof only if they equal `expected`.
    pub async fn check<S: BackupSink + ?Sized>(
        sink: &S,
        key: &str,
        expected: &Stored,
    ) -> Result<Verified, VerifyError> {
        match sink.stat(key).await? {
            Some(s) if &s == expected => Ok(Verified {
                key: key.into(),
                stored: s,
            }),
            reported => Err(VerifyError::Reported {
                key: key.into(),
                expected: expected.clone(),
                reported,
            }),
        }
    }

    /// The key.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// What the bucket reported.
    pub fn stored(&self) -> &Stored {
        &self.stored
    }
}

/// Why a copy is not verified.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum VerifyError {
    /// The sink failed.
    #[error(transparent)]
    Sink(#[from] SinkError),
    /// The bucket reports something other than what was uploaded, or nothing.
    #[error("{key}: the bucket reports {reported:?}, expected {expected:?}")]
    Reported {
        /// The key.
        key: String,
        /// What was uploaded.
        expected: Stored,
        /// What the bucket reports.
        reported: Option<Stored>,
    },
    /// Downloaded bytes whose SHA-256 is not the expected one.
    #[error("{key}: downloaded bytes do not have the expected SHA-256")]
    Content {
        /// The key.
        key: String,
    },
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
        assert!(SinkError::Busy { queued: 1 }.is_transient());
        assert!(!SinkError::Exists { key: "k".into() }.is_transient());
    }

    #[test]
    fn verified_needs_the_reported_hash() {
        let expected = Stored::new(1, [1; 32]);
        let r = futures_ready(Verified::check(&Nothing, "k", &expected));
        assert_eq!(
            r,
            Err(VerifyError::Reported {
                key: "k".into(),
                expected,
                reported: None
            })
        );
    }

    fn futures_ready<F: Future>(f: F) -> F::Output {
        let mut f = std::pin::pin!(f);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        match f.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(v) => v,
            std::task::Poll::Pending => panic!("pending"),
        }
    }
}
