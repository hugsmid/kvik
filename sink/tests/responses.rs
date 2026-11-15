//! Responses the sink must refuse: bodies above `max_object_size` and
//! malformed `Content-Range` headers.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use kvik::{BackupSink, SinkError};
use kvik_sink::fake::{FakeBucket, FakeOp};
use kvik_sink::transport::{Request, Response, Transport, TransportError};
use kvik_sink::verify::sha256;
use kvik_sink::{S3Sink, block_on};

fn put(sink: &S3Sink, key: &str, b: &[u8]) {
    block_on(sink.put(key, Arc::from(b), sha256(b))).unwrap();
}

#[test]
fn a_whole_download_above_max_object_size_is_too_large() {
    let bucket = FakeBucket::new();
    put(&bucket.sink(), "data/ten", b"0123456789");
    let small = bucket.sink_with(bucket.config().with_max_object_size(8));
    assert_eq!(
        block_on(small.get("data/ten", None)),
        Err(SinkError::TooLarge { size: 10, limit: 8 })
    );
    assert_eq!(bucket.count(FakeOp::Get), 1);
    assert_eq!(block_on(small.get("data/ten", Some(2..5))).unwrap(), b"234");
    assert_eq!(
        block_on(small.get("data/absent", None)),
        Err(SinkError::NotFound {
            key: "data/absent".into()
        })
    );
}

#[test]
fn a_whole_download_above_the_metadata_cap_and_max_object_size_is_too_large() {
    const MIB: u64 = 1024 * 1024;
    let bucket = FakeBucket::new();
    let big = vec![7u8; (17 * MIB) as usize];
    put(&bucket.sink(), "data/big", &big);
    let limit = 16 * MIB + 1;
    let capped = bucket.sink_with(bucket.config().with_max_object_size(limit));
    assert_eq!(
        block_on(capped.get("data/big", None)),
        Err(SinkError::TooLarge {
            size: 17 * MIB,
            limit
        })
    );
    assert_eq!(
        block_on(bucket.sink().get("data/big", None)).unwrap().len(),
        big.len()
    );
}

/// Answers every ranged `GET` with 206 and the given `Content-Range`.
struct Ranged {
    bucket: FakeBucket,
    content_range: &'static str,
    gets: AtomicUsize,
}

impl Transport for Ranged {
    fn send(&self, r: &Request) -> Result<Response, TransportError> {
        if r.header("range").is_some() {
            self.gets.fetch_add(1, Ordering::Relaxed);
            return Ok(Response::new(
                206,
                vec![("content-range".into(), self.content_range.into())],
                b"0123".to_vec(),
            ));
        }
        self.bucket.send(r)
    }
}

#[test]
fn a_malformed_content_range_is_a_protocol_error_not_retried() {
    let bucket = FakeBucket::new();
    put(&bucket.sink(), "data/r", b"0123456789");
    for content_range in [
        "bytes 0-18446744073709551615/*",
        "bytes 0-18446744073709551615/18446744073709551615",
        "bytes 0-3/3",
        "bytes 3-0/10",
    ] {
        let transport = Arc::new(Ranged {
            bucket: bucket.clone(),
            content_range,
            gets: AtomicUsize::new(0),
        });
        let sink = S3Sink::with_transport(bucket.config(), transport.clone()).unwrap();
        let r = block_on(sink.get("data/r", Some(0..4)));
        assert!(
            matches!(r, Err(SinkError::Failed { .. })),
            "{content_range}: {r:?}"
        );
        assert_eq!(transport.gets.load(Ordering::Relaxed), 1, "{content_range}");
    }
}
