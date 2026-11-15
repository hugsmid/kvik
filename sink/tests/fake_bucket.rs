//! The S3 client against the in-memory fake: the rules the spec relies on
//! (spec §8 Hashes, Bucket, Generations), retries and faults.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Waker};
use std::time::Duration;

use kvik::{BackupSink, ListPage, SinkError, Stored};
use kvik_sink::fake::{Checksums, FakeBucket, FakeOp, Fault};
use kvik_sink::transport::{Request, Response, Transport, TransportError};
use kvik_sink::verify::{self, VerifyError, sha256};
use kvik_sink::{Addressing, RetryPolicy, S3Sink, SystemWallClock, ThreadSleep, block_on};

fn bytes(s: &[u8]) -> Arc<[u8]> {
    Arc::from(s)
}

fn put(sink: &S3Sink, key: &str, b: &[u8]) -> Result<Stored, SinkError> {
    block_on(sink.put(key, bytes(b), sha256(b)))
}

fn list_all(sink: &dyn BackupSink, prefix: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page: ListPage = block_on(sink.list(prefix, after.as_deref())).unwrap();
        keys.extend(page.keys.iter().cloned());
        if !page.more {
            return keys;
        }
        after = page.keys.last().cloned();
    }
}

#[test]
fn put_stat_get_round_trip() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink();
    let data = b"0123456789abcdef";
    let stored = put(&sink, "log/00000000-0000000000000001.kvl", data).unwrap();
    assert_eq!(stored, Stored::new(16, sha256(data)));
    assert_eq!(
        block_on(sink.stat("log/00000000-0000000000000001.kvl")).unwrap(),
        Some(stored)
    );
    assert_eq!(
        block_on(sink.get("log/00000000-0000000000000001.kvl", None)).unwrap(),
        data
    );
    assert_eq!(block_on(sink.stat("log/absent")).unwrap(), None);
    assert_eq!(
        block_on(sink.get("log/absent", None)),
        Err(SinkError::NotFound {
            key: "log/absent".into()
        })
    );
}

#[test]
fn put_never_overwrites_and_a_repeat_is_idempotent() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink();
    let first = put(&sink, "claims/00000001", b"restore A").unwrap();
    // The same content again (a repeat after a restart): accepted.
    assert_eq!(put(&sink, "claims/00000001", b"restore A").unwrap(), first);
    // Other content: refused, and the object is unchanged.
    assert_eq!(
        put(&sink, "claims/00000001", b"restore B"),
        Err(SinkError::Exists {
            key: "claims/00000001".into()
        })
    );
    // Same size, other bytes: still refused.
    assert!(matches!(
        put(&sink, "claims/00000001", b"restore C"),
        Err(SinkError::Exists { .. })
    ));
    assert_eq!(bucket.object("claims/00000001").unwrap(), b"restore A");
    assert_eq!(bucket.count(FakeOp::Put), 4);
}

#[test]
fn a_wrong_hash_is_rejected_and_nothing_is_stored() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink();
    let r = block_on(sink.put("data/x", bytes(b"payload"), sha256(b"other")));
    assert_eq!(
        r,
        Err(SinkError::HashMismatch {
            key: "data/x".into()
        })
    );
    assert_eq!(bucket.object("data/x"), None);
    // Not retried: a mismatch is not transient.
    assert_eq!(bucket.count(FakeOp::Put), 1);
}

#[test]
fn a_bucket_that_stores_other_bytes_fails_the_upload() {
    let bucket = FakeBucket::new();
    bucket.set_corrupt_on_store(true);
    let sink = bucket.sink();
    assert_eq!(
        put(&sink, "data/y", b"abc"),
        Err(SinkError::HashMismatch {
            key: "data/y".into()
        })
    );
    let r = block_on(verify::upload(&sink, "data/z", bytes(b"abc")));
    assert!(matches!(
        r,
        Err(VerifyError::Sink(SinkError::HashMismatch { .. }))
    ));
}

#[test]
fn a_bucket_that_does_not_check_is_caught_by_the_sampled_download() {
    // A bucket that trusts the sent hash reports it without checking. The
    // bytes arrive damaged; the upload looks verified by reported hash, and
    // only the download check (spec §8 Hashes) finds the damage.
    let bucket = FakeBucket::new();
    bucket.set_checksums(Checksums::Trusted);
    let sink = bucket.sink();
    let good = b"the real ciphertext";
    let copy = block_on(verify::upload(&sink, "data/t", bytes(good))).unwrap();
    assert!(bucket.damage("data/t"));
    // The reported hash still matches: the scheduled check alone passes.
    assert!(block_on(verify::check(&sink, "data/t", copy.stored())).is_ok());
    // A whole download is checked against the reported hash by the client
    // and fails transiently after every attempt; the sampled check reports it.
    let r = block_on(verify::download(&sink, "data/t", sha256(good)));
    assert!(matches!(
        r,
        Err(VerifyError::Sink(SinkError::Transient { .. }))
    ));
    // A ranged read carries no whole-object hash; the engine's plaintext
    // check is what catches damage there.
    assert_eq!(
        block_on(sink.get("data/t", Some(1..4))).unwrap(),
        good[1..4].to_vec()
    );
}

#[test]
fn a_bucket_that_reports_no_checksum_is_refused() {
    let bucket = FakeBucket::new();
    bucket.set_checksums(Checksums::NotReported);
    let sink = bucket.sink();
    assert!(matches!(
        put(&sink, "data/n", b"x"),
        Err(SinkError::Failed { .. })
    ));
    assert!(matches!(
        block_on(sink.stat("data/n")),
        Err(SinkError::Failed { .. })
    ));
}

#[test]
fn server_errors_are_retried_with_growing_pauses() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink();
    bucket.inject(FakeOp::Put, Fault::Status(503));
    bucket.inject(FakeOp::Put, Fault::Status(500));
    bucket.inject(FakeOp::Put, Fault::Status(429));
    put(&sink, "data/r", b"retry me").unwrap();
    assert_eq!(bucket.count(FakeOp::Put), 4);
    let p = bucket.pauses();
    assert_eq!(p.len(), 3);
    // Equal jitter around 100, 200 and 400 ms.
    for (d, base) in p.iter().zip([100u64, 200, 400]) {
        let base = Duration::from_millis(base);
        assert!(*d >= base / 2 && *d <= base, "{d:?} for {base:?}");
    }
}

#[test]
fn retries_end_after_the_policy_s_attempts() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink_with(bucket.config().with_retry(RetryPolicy::new(
        3,
        Duration::from_millis(1),
        Duration::from_millis(1),
    )));
    for _ in 0..5 {
        bucket.inject(FakeOp::Get, Fault::Status(503));
    }
    put(&sink, "data/g", b"x").unwrap();
    assert!(matches!(
        block_on(sink.get("data/g", None)),
        Err(SinkError::Transient { .. })
    ));
    assert_eq!(bucket.count(FakeOp::Get), 3);
    assert_eq!(bucket.pending_faults(), 2);
}

#[test]
fn permanent_errors_are_not_retried() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink();
    bucket.inject(FakeOp::Head, Fault::Status(403));
    assert!(matches!(
        block_on(sink.stat("data/p")),
        Err(SinkError::Failed { .. })
    ));
    assert_eq!(bucket.count(FakeOp::Head), 1);
    assert!(bucket.pauses().is_empty());
}

#[test]
fn wrong_credentials_are_refused_for_good() {
    let bucket = FakeBucket::new();
    let mut config = bucket.config();
    config.credentials = kvik_sink::Credentials::new("TESTKEYID0000000", "not-the-secret");
    let sink = bucket.sink_with(config);
    let r = put(&sink, "data/c", b"x");
    match r {
        Err(SinkError::Failed { reason }) => assert!(reason.contains("SignatureDoesNotMatch")),
        other => panic!("{other:?}"),
    }
    assert_eq!(bucket.count(FakeOp::Put), 1);
    assert!(bucket.keys().is_empty());
}

#[test]
fn a_lost_reply_after_the_upload_is_repeated_safely() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink();
    bucket.inject(FakeOp::Put, Fault::TimeoutAfter);
    let stored = put(&sink, "data/l", b"once").unwrap();
    assert_eq!(stored, Stored::new(4, sha256(b"once")));
    // The retry met the object the first attempt stored (412) and accepted it.
    assert_eq!(bucket.count(FakeOp::Put), 2);
    assert_eq!(bucket.keys(), vec!["data/l"]);
}

#[test]
fn a_lost_reply_then_a_lost_read_back_still_converges() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink();
    bucket.inject(FakeOp::Put, Fault::TimeoutAfter);
    bucket.inject(FakeOp::Head, Fault::Status(500));
    bucket.inject(FakeOp::Head, Fault::TimeoutBefore);
    put(&sink, "data/m", b"converge").unwrap();
    assert_eq!(bucket.object("data/m").unwrap(), b"converge");
}

#[test]
fn a_timeout_before_the_request_stores_nothing_until_the_retry() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink();
    bucket.inject(FakeOp::Put, Fault::TimeoutBefore);
    put(&sink, "data/b", b"later").unwrap();
    assert_eq!(bucket.count(FakeOp::Put), 2);
}

#[test]
fn a_cut_upload_leaves_no_object() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink_with(bucket.config().with_retry(RetryPolicy::new(
        1,
        Duration::ZERO,
        Duration::ZERO,
    )));
    bucket.inject(FakeOp::Put, Fault::CutUpload);
    assert!(matches!(
        put(&sink, "data/cut", b"partial"),
        Err(SinkError::Transient { .. })
    ));
    assert_eq!(bucket.object("data/cut"), None);
    assert_eq!(block_on(sink.stat("data/cut")).unwrap(), None);
    // And with retries, the next attempt stores it whole.
    let sink = bucket.sink();
    bucket.inject(FakeOp::Put, Fault::CutUpload);
    put(&sink, "data/cut", b"partial").unwrap();
    assert_eq!(bucket.object("data/cut").unwrap(), b"partial");
}

#[test]
fn a_truncated_download_is_an_error_never_short_bytes() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink_with(bucket.config().with_retry(RetryPolicy::new(
        1,
        Duration::ZERO,
        Duration::ZERO,
    )));
    put(&sink, "data/tr", b"0123456789").unwrap();
    bucket.inject(FakeOp::Get, Fault::TruncatedBody);
    assert!(matches!(
        block_on(sink.get("data/tr", None)),
        Err(SinkError::Transient { .. })
    ));
    bucket.inject(FakeOp::Get, Fault::TruncatedBody);
    assert!(matches!(
        block_on(sink.get("data/tr", Some(2..8))),
        Err(SinkError::Transient { .. })
    ));
    // Retried, the download succeeds whole.
    let sink = bucket.sink();
    bucket.inject(FakeOp::Get, Fault::TruncatedBody);
    assert_eq!(block_on(sink.get("data/tr", None)).unwrap(), b"0123456789");
}

#[test]
fn ranges_are_half_open() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink();
    put(&sink, "data/range", b"0123456789").unwrap();
    let get = |r: std::ops::Range<u64>| block_on(sink.get("data/range", Some(r)));
    assert_eq!(get(2..5).unwrap(), b"234");
    assert_eq!(get(0..1).unwrap(), b"0");
    assert_eq!(get(9..10).unwrap(), b"9");
    assert_eq!(get(0..10).unwrap(), b"0123456789");
    // Empty ranges within the object, the end included.
    assert_eq!(get(4..4).unwrap(), b"");
    assert_eq!(get(10..10).unwrap(), b"");
    let invalid = Err(SinkError::InvalidRange {
        key: "data/range".into(),
    });
    assert_eq!(get(11..11), invalid);
    assert_eq!(get(5..11), invalid);
    assert_eq!(get(10..12), invalid);
    #[allow(clippy::reversed_empty_ranges)]
    let backwards = 6..5;
    assert_eq!(get(backwards), invalid);
    assert_eq!(
        block_on(sink.get("data/none", Some(0..1))),
        Err(SinkError::NotFound {
            key: "data/none".into()
        })
    );
    assert_eq!(
        block_on(sink.get("data/none", Some(0..0))),
        Err(SinkError::NotFound {
            key: "data/none".into()
        })
    );
}

#[test]
fn listings_are_paged_ordered_and_prefixed() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink_with(bucket.config().with_list_page(3));
    let mut expected = Vec::new();
    for i in (0..10).rev() {
        let key = format!("manifest/00000001/{i:016x}");
        put(&sink, &key, key.as_bytes()).unwrap();
        expected.push(key);
    }
    put(&sink, "manifest/00000002/0000000000000001", b"x").unwrap();
    put(&sink, "manifest/0000000", b"x").unwrap();
    expected.sort();
    assert_eq!(list_all(&sink, "manifest/00000001/"), expected);
    let first = block_on(sink.list("manifest/00000001/", None)).unwrap();
    assert_eq!((first.keys.len(), first.more), (3, true));
    let after = block_on(sink.list("manifest/00000001/", Some(&expected[8]))).unwrap();
    assert_eq!((after.keys, after.more), (vec![expected[9].clone()], false));
    assert_eq!(list_all(&sink, "absent/"), Vec::<String>::new());
    assert_eq!(list_all(&sink, "").len(), 12);
}

#[test]
fn awkward_keys_survive_encoding() {
    let bucket = FakeBucket::new();
    for addressing in [Addressing::Path, Addressing::VirtualHost] {
        let sink = bucket.sink_with(bucket.config().with_addressing(addressing));
        let keys = [
            format!("odd/{addressing:?}/a b+c&d<e>'\"%20~"),
            format!("odd/{addressing:?}/ünïcødé/π"),
            format!("odd/{addressing:?}//double//slash"),
        ];
        for k in &keys {
            put(&sink, k, k.as_bytes()).unwrap();
            assert_eq!(block_on(sink.get(k, None)).unwrap(), k.as_bytes());
        }
        let mut sorted = keys.to_vec();
        sorted.sort();
        assert_eq!(list_all(&sink, &format!("odd/{addressing:?}/")), sorted);
    }
}

#[test]
fn a_lagging_listing_does_not_decide_whether_a_key_is_taken() {
    // A claim marker just written by another restore may be missing from a
    // listing; the create-if-absent upload still refuses to take it.
    let bucket = FakeBucket::new();
    bucket.set_list_lag(2);
    let sink = bucket.sink();
    put(&sink, "claims/00000002", b"restore A").unwrap();
    assert!(list_all(&sink, "claims/").is_empty());
    assert!(matches!(
        put(&sink, "claims/00000002", b"restore B"),
        Err(SinkError::Exists { .. })
    ));
    assert!(list_all(&sink, "claims/").is_empty());
    assert_eq!(list_all(&sink, "claims/"), vec!["claims/00000002"]);
}

#[test]
fn objects_above_the_single_part_limit_are_refused_without_a_request() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink_with(bucket.config().with_max_object_size(8));
    assert_eq!(
        put(&sink, "data/big", b"123456789"),
        Err(SinkError::TooLarge { size: 9, limit: 8 })
    );
    assert_eq!(bucket.count(FakeOp::Any), 0);
    put(&sink, "data/big", b"12345678").unwrap();
    assert_eq!(
        sink.config()
            .clone()
            .with_max_object_size(u64::MAX)
            .max_object_size,
        kvik_sink::SINGLE_PART_LIMIT
    );
}

#[test]
fn invalid_keys_are_refused() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink();
    assert!(matches!(
        put(&sink, "", b"x"),
        Err(SinkError::Failed { .. })
    ));
    let long = "k".repeat(1025);
    assert!(matches!(
        block_on(sink.stat(&long)),
        Err(SinkError::Failed { .. })
    ));
    assert_eq!(bucket.count(FakeOp::Any), 0);
}

#[test]
fn the_sink_works_as_a_trait_object() {
    let bucket = FakeBucket::new();
    let sink: Arc<dyn BackupSink> = Arc::new(bucket.sink());
    let b = bytes(b"dyn");
    let copy = block_on(verify::upload(&*sink, "data/dyn", b.clone())).unwrap();
    assert_eq!(copy.key(), "data/dyn");
    assert_eq!(list_all(&*sink, "data/"), vec!["data/dyn"]);
    let boxed: Box<dyn BackupSink> = Box::new(bucket.sink());
    assert_eq!(block_on(boxed.get("data/dyn", None)).unwrap(), b"dyn");
}

/// A transport that holds every request until released.
struct Gate {
    open: AtomicBool,
    inner: FakeBucket,
}

impl Transport for Gate {
    fn send(&self, r: &Request) -> Result<Response, TransportError> {
        while !self.open.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.inner.send(r)
    }
}

#[test]
fn polling_never_blocks_on_the_network() {
    let bucket = FakeBucket::new();
    let gate = Arc::new(Gate {
        open: AtomicBool::new(false),
        inner: bucket.clone(),
    });
    let sink = S3Sink::with_seams(
        bucket.config(),
        gate.clone(),
        Arc::new(SystemWallClock),
        Arc::new(ThreadSleep),
    )
    .unwrap();
    let mut fut = sink.put("data/gate", bytes(b"g"), sha256(b"g"));
    let mut cx = Context::from_waker(Waker::noop());
    // The exchange is stuck in the transport, yet the poll returns at once.
    assert!(fut.as_mut().poll(&mut cx).is_pending());
    assert!(fut.as_mut().poll(&mut cx).is_pending());
    gate.open.store(true, Ordering::Release);
    assert_eq!(block_on(fut).unwrap().size, 1);
}

#[test]
fn a_dropped_future_still_completes_its_call_and_drop_waits_for_it() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink();
    drop(sink.put("data/dropped", bytes(b"d"), sha256(b"d")));
    drop(sink);
    assert_eq!(bucket.object("data/dropped").unwrap(), b"d");
}

#[test]
fn invalid_configurations_are_refused() {
    let bucket = FakeBucket::new();
    for endpoint in [
        "objects.test",
        "https://objects.test/path",
        "ftp://objects.test",
    ] {
        let mut c = bucket.config();
        c.endpoint = endpoint.into();
        assert!(S3Sink::new(c).is_err(), "{endpoint}");
    }
    let mut c = bucket.config();
    c.bucket = "a/b".into();
    assert!(S3Sink::new(c).is_err());
}
