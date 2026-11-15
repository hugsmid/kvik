//! Backup sinks for the kvik time-series store (roadmap 8.2).
//!
//! - [`S3Sink`] implements [`kvik_sink_api::BackupSink`] (re-exported as
//!   `kvik::BackupSink`) over S3-compatible object storage: Signature
//!   Version 4, single-part conditional uploads carrying their SHA-256,
//!   read-back of the reported hash, ranged downloads, paged listings,
//!   timeouts, and retries with backoff on its own worker threads.
//! - [`fake::FakeBucket`] answers the same protocol subset in memory, with
//!   injected faults, for tests.
//! - [`verify`] turns uploads and checks into [`verify::Verified`] copies.
//!
//! The futures are runtime-neutral: they can be awaited on any executor or
//! blocked on with [`block_on`].
//!
//! ```
//! use std::sync::Arc;
//! use kvik_sink_api::BackupSink;
//! use kvik_sink::{block_on, fake::FakeBucket, verify};
//!
//! let bucket = FakeBucket::new();
//! let sink = bucket.sink();
//! let bytes: Arc<[u8]> = Arc::from(&b"sealed segment"[..]);
//! let copy = block_on(verify::upload(&sink, "log/0001", bytes.clone())).unwrap();
//! assert_eq!(copy.stored().size, 14);
//! // The key is taken: other content is refused, the same content accepted.
//! let other: Arc<[u8]> = Arc::from(&b"other"[..]);
//! assert!(block_on(sink.put("log/0001", other.clone(), verify::sha256(&other))).is_err());
//! assert!(block_on(sink.put("log/0001", bytes, copy.stored().sha256)).is_ok());
//! ```

#![forbid(unsafe_code)]

mod config;
pub mod fake;
mod retry;
mod s3;
mod sigv4;
pub mod transport;
pub mod verify;
mod worker;
mod xml;

pub use config::{Addressing, DEFAULT_MAX_OBJECT_SIZE, S3Config, SINGLE_PART_LIMIT};
pub use retry::{Pause, RetryPolicy, SystemWallClock, ThreadSleep, WallClock};
pub use s3::S3Sink;
pub use sigv4::Credentials;

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::Thread;

struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Runs a future to completion on the calling thread, parking it while the
/// future is pending.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(v) = future.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::park();
    }
}
