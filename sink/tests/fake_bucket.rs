//! The S3 client against the in-memory fake: the rules the spec relies on
//! (spec §8 Hashes, Bucket, Generations), retries and faults.

use std::sync::Arc;
use std::task::{Context, Waker};
use std::time::Duration;

use kvik::{BackupSink, ListPage, SinkError, Stored};
use kvik_sink::fake::{Checksums, FakeBucket, FakeOp, Fault};
use kvik_sink::transport::{Request, Response, Transport, TransportError};
use kvik_sink::verify::{self, VerifyError, sha256};
use kvik_sink::{Addressing, RetryPolicy, S3Sink, block_on};

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
    assert_eq!(put(&sink, "claims/00000001", b"restore A").unwrap(), first);
    assert_eq!(
        put(&sink, "claims/00000001", b"restore B"),
        Err(SinkError::Exists {
            key: "claims/00000001".into()
        })
    );
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
    // spec §8 Hashes: only the sampled download finds damage a bucket that trusts the sent hash hides.
    let bucket = FakeBucket::new();
    bucket.set_checksums(Checksums::Trusted);
    let sink = bucket.sink();
    let good = b"the real ciphertext";
    let copy = block_on(verify::upload(&sink, "data/t", bytes(good))).unwrap();
    assert!(bucket.damage("data/t"));
    assert!(block_on(verify::check(&sink, "data/t", copy.stored())).is_ok());
    let r = block_on(verify::download(&sink, "data/t", sha256(good)));
    assert!(matches!(
        r,
        Err(VerifyError::Sink(SinkError::Transient { .. }))
    ));
    assert_eq!(
        block_on(sink.get("data/t", Some(1..4))).unwrap(),
        good[1..4].to_vec()
    );
}

#[test]
fn a_bucket_that_reports_no_checksum_is_refused() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink();
    put(&sink, "data/old", b"x").unwrap();
    bucket.set_checksums(Checksums::NotReported);
    assert!(matches!(
        block_on(sink.get("data/old", None)),
        Err(SinkError::Failed { .. })
    ));
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
    assert_eq!(bucket.count(FakeOp::Put), 1);
    assert_eq!(bucket.keys(), vec!["data/l"]);
}

#[test]
fn our_own_damaged_upload_found_on_a_retry_is_a_hash_mismatch_not_exists() {
    for fault in [
        (FakeOp::Put, Fault::TimeoutAfter),
        (FakeOp::Head, Fault::Status(503)),
    ] {
        let bucket = FakeBucket::new();
        bucket.set_corrupt_on_store(true);
        let sink = bucket.sink();
        bucket.inject(fault.0, fault.1);
        assert_eq!(
            put(&sink, "data/own", b"ours"),
            Err(SinkError::HashMismatch {
                key: "data/own".into()
            }),
            "{fault:?}"
        );
        assert_eq!(bucket.count(FakeOp::Put), 1);
    }
}

#[test]
fn other_content_found_before_any_upload_of_ours_is_exists() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink();
    put(&sink, "data/theirs", b"theirs").unwrap();
    bucket.inject(FakeOp::Put, Fault::Status(403));
    bucket.inject(FakeOp::Put, Fault::Coded(400, "RequestTimeout"));
    assert!(matches!(
        put(&sink, "data/theirs", b"ours"),
        Err(SinkError::Failed { .. })
    ));
    assert_eq!(
        put(&sink, "data/theirs", b"ours"),
        Err(SinkError::Exists {
            key: "data/theirs".into()
        })
    );
}

#[test]
fn a_failed_read_back_is_retried_without_sending_the_body_again() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink();
    bucket.inject(FakeOp::Head, Fault::Status(503));
    put(&sink, "data/rb", b"body").unwrap();
    assert_eq!(bucket.count(FakeOp::Put), 1);
}

#[test]
fn a_repeat_after_a_restart_is_accepted_by_the_conditional_upload() {
    let bucket = FakeBucket::new();
    put(&bucket.sink(), "data/again", b"same").unwrap();
    let sink = bucket.sink();
    put(&sink, "data/again", b"same").unwrap();
    assert_eq!(bucket.count(FakeOp::Put), 2);
}

#[test]
fn an_absent_key_is_none_only_when_the_bucket_exists() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink();
    assert_eq!(block_on(sink.stat("data/none")).unwrap(), None);
    assert_eq!(block_on(sink.stat("data/none")).unwrap(), None);
    assert_eq!(bucket.count(FakeOp::Head), 3);

    let bucket = FakeBucket::new();
    bucket.set_bucket_missing(true);
    let sink = bucket.sink();
    assert!(matches!(
        block_on(sink.stat("data/none")),
        Err(SinkError::Failed { .. })
    ));
    assert!(matches!(
        block_on(sink.get("data/none", None)),
        Err(SinkError::Failed { .. })
    ));
    assert!(matches!(
        block_on(sink.list("data/", None)),
        Err(SinkError::Failed { .. })
    ));
    assert!(matches!(
        put(&sink, "data/none", b"x"),
        Err(SinkError::Failed { .. })
    ));
}

#[test]
fn a_404_without_no_such_key_is_not_an_absent_object() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink();
    put(&sink, "data/k", b"x").unwrap();
    bucket.inject(FakeOp::Get, Fault::Coded(404, "NoSuchBucket"));
    assert!(matches!(
        block_on(sink.get("data/k", None)),
        Err(SinkError::Failed { .. })
    ));
    bucket.inject(FakeOp::Get, Fault::Status(404));
    assert!(matches!(
        block_on(sink.get("data/k", None)),
        Err(SinkError::Failed { .. })
    ));
}

#[test]
fn a_full_queue_is_busy() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink_with(bucket.config().with_workers(1, 1));
    bucket.hold();
    let first = sink.put("q/1", bytes(b"1"), sha256(b"1"));
    while bucket.waiting() == 0 {
        std::thread::yield_now();
    }
    let second = sink.put("q/2", bytes(b"2"), sha256(b"2"));
    let third = block_on(sink.put("q/3", bytes(b"3"), sha256(b"3")));
    assert_eq!(third, Err(SinkError::Busy { queued: 1 }));
    bucket.release();
    block_on(first).unwrap();
    block_on(second).unwrap();
    assert_eq!(bucket.keys(), vec!["q/1", "q/2"]);
}

#[test]
fn a_reply_that_is_not_a_listing_fails() {
    struct Portal(FakeBucket);
    impl Transport for Portal {
        fn send(&self, r: &Request) -> Result<Response, TransportError> {
            if r.query.iter().any(|(k, _)| k == "list-type") {
                return Ok(Response::new(
                    200,
                    Vec::new(),
                    b"<html>sign in</html>".to_vec(),
                ));
            }
            self.0.send(r)
        }
    }
    struct NoFlag(FakeBucket);
    impl Transport for NoFlag {
        fn send(&self, r: &Request) -> Result<Response, TransportError> {
            let mut resp = self.0.send(r)?;
            let body = String::from_utf8(resp.body).unwrap();
            resp.body = body
                .replace("<IsTruncated>false</IsTruncated>", "")
                .into_bytes();
            Ok(resp)
        }
    }
    let bucket = FakeBucket::new();
    for transport in [
        Arc::new(Portal(bucket.clone())) as Arc<dyn Transport>,
        Arc::new(NoFlag(bucket.clone())),
    ] {
        let sink = S3Sink::with_transport(bucket.config(), transport).unwrap();
        assert!(matches!(
            block_on(sink.list("", None)),
            Err(SinkError::Failed { .. })
        ));
    }
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

#[test]
fn polling_never_blocks_on_the_network() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink();
    bucket.hold();
    let mut fut = sink.put("data/held", bytes(b"g"), sha256(b"g"));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(fut.as_mut().poll(&mut cx).is_pending());
    assert!(fut.as_mut().poll(&mut cx).is_pending());
    assert_eq!(bucket.object("data/held"), None);
    bucket.release();
    assert_eq!(block_on(fut).unwrap().size, 1);
}

#[test]
fn coded_errors_are_classified() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink();
    bucket.inject(FakeOp::Put, Fault::Coded(400, "BadDigest"));
    assert_eq!(
        put(&sink, "data/d", b"x"),
        Err(SinkError::HashMismatch {
            key: "data/d".into()
        })
    );
    assert_eq!(bucket.count(FakeOp::Put), 1);
    assert_eq!(bucket.object("data/d"), None);
    bucket.inject(FakeOp::Put, Fault::Coded(400, "RequestTimeout"));
    bucket.inject(FakeOp::Put, Fault::Coded(409, "ConditionalRequestConflict"));
    put(&sink, "data/d", b"x").unwrap();
    assert_eq!(bucket.count(FakeOp::Put), 4);
    bucket.inject(FakeOp::Head, Fault::Coded(403, "RequestTimeTooSkewed"));
    assert!(matches!(
        block_on(sink.stat("data/d")),
        Err(SinkError::Failed { .. })
    ));
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
