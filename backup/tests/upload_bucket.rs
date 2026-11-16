//! The upload driver against the S3 sink over the fake bucket, and its attach checks (Q-162, Q-283).

mod upload_support;

use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use kvik_backup::envelope::Envelope;
use kvik_backup::fake::{MemSink, TestEnvelope, block_on};
use kvik_backup::keys::Keys;
use kvik_backup::upload::{BackupDriver, Refused, Step, Uploader};
use kvik_sink::fake::{Checksums, FakeBucket, FakeOp, Fault};
use kvik_sink_api::{BackupSink, ListPage, SinkError, SinkFuture, Stored};
use upload_support::*;

fn driver_over<S: BackupSink>(h: &Harness, sink: S) -> Uploader<S, TestEnvelope, Port> {
    Uploader::new(
        sink,
        TestEnvelope::new(KEY),
        Keys::new(ID),
        h.local(),
        h.port(),
        cfg(),
    )
}

fn idle<S: BackupSink>(d: &mut Uploader<S, TestEnvelope, Port>) {
    let mut now = Duration::ZERO;
    for _ in 0..1_000 {
        match block_on(d.step(now)) {
            Step::Progress => {}
            Step::Retry { after, .. } => now += after,
            Step::Idle => return,
            s => panic!("{s:?}"),
        }
    }
    panic!("not idle");
}

fn sealed_store() -> Harness {
    let mut h = Harness::new();
    for len in [40, 70_000, 9] {
        h.seal(len);
    }
    h
}

fn assert_backed_up(h: &Harness, object: impl Fn(&str) -> Option<Vec<u8>>) {
    let keys = Keys::new(ID);
    let env = TestEnvelope::new(KEY);
    for (d, content) in &h.segments {
        let key = keys.file(d);
        assert_eq!(object(&key), Some(env.seal(&key, content).unwrap()));
    }
    let records = h.records();
    for (i, r) in records[..records.len() - 1].iter().enumerate() {
        let key = keys.record(r.generation, r.epoch);
        assert_eq!(
            object(&key),
            Some(env.seal(&key, &uploaded_form(&records, i)).unwrap())
        );
    }
}

#[test]
fn a_store_is_backed_up_through_the_s3_sink() {
    let h = sealed_store();
    let bucket = FakeBucket::new();
    let mut d = driver_over(&h, bucket.sink());
    assert_eq!(block_on(d.probe()), Ok(()));
    idle(&mut d);
    assert_backed_up(&h, |k| bucket.object(k));
}

#[test]
fn every_request_fault_of_the_fake_bucket_in_turn_ends_backed_up() {
    let total = {
        let h = sealed_store();
        let bucket = FakeBucket::new();
        idle(&mut driver_over(&h, bucket.sink()));
        bucket.count(FakeOp::Any)
    };
    for fault in [
        Fault::Status(503),
        Fault::TimeoutAfter,
        Fault::TimeoutBefore,
    ] {
        for k in 0..total {
            let h = sealed_store();
            let bucket = FakeBucket::new();
            let mut d = driver_over(&h, bucket.sink());
            // Run k requests, then fault the next.
            let mut now = Duration::ZERO;
            for _ in 0..1_000 {
                if bucket.count(FakeOp::Any) >= k {
                    break;
                }
                match block_on(d.step(now)) {
                    Step::Idle => break,
                    Step::Retry { after, .. } => now += after,
                    _ => {}
                }
            }
            bucket.inject(FakeOp::Any, fault);
            idle(&mut d);
            assert_backed_up(&h, |key| bucket.object(key));
        }
    }
}

#[test]
fn a_bucket_that_delays_each_request_in_turn_ends_backed_up() {
    let h = sealed_store();
    let bucket = FakeBucket::new();
    let mut d = driver_over(&h, bucket.sink());
    let run = std::thread::spawn(move || {
        idle(&mut d);
    });
    while !run.is_finished() {
        bucket.hold();
        while bucket.waiting() == 0 && !run.is_finished() {
            std::thread::yield_now();
        }
        bucket.release();
    }
    run.join().unwrap();
    assert_backed_up(&h, |k| bucket.object(k));
}

#[test]
fn q283_a_bucket_with_both_checks_is_accepted_at_every_attach() {
    let h = Harness::new();
    let mem = Arc::new(MemSink::new());
    for _ in 0..2 {
        assert_eq!(block_on(driver_over(&h, mem.clone()).probe()), Ok(()));
    }
    let bucket = FakeBucket::new();
    for _ in 0..2 {
        assert_eq!(block_on(driver_over(&h, bucket.sink()).probe()), Ok(()));
    }
}

/// A bucket that stores whatever arrives and reports the hash it was sent.
struct Unchecked(MemSink);

impl BackupSink for Unchecked {
    fn put<'a>(&'a self, key: &'a str, bytes: Arc<[u8]>, sha: [u8; 32]) -> SinkFuture<'a, Stored> {
        if self.0.objects().contains_key(key) {
            let sha = sha256(&bytes);
            return self.0.put(key, bytes, sha);
        }
        let size = bytes.len() as u64;
        self.0.set(key, &bytes);
        Box::pin(async move { Ok(Stored::new(size, sha)) })
    }
    fn stat<'a>(&'a self, key: &'a str) -> SinkFuture<'a, Option<Stored>> {
        self.0.stat(key)
    }
    fn get<'a>(&'a self, key: &'a str, range: Option<Range<u64>>) -> SinkFuture<'a, Vec<u8>> {
        self.0.get(key, range)
    }
    fn list<'a>(&'a self, prefix: &'a str, after: Option<&'a str>) -> SinkFuture<'a, ListPage> {
        self.0.list(prefix, after)
    }
}

#[test]
fn q162_a_bucket_that_does_not_check_checksums_is_refused() {
    let h = Harness::new();
    let sink = Arc::new(Unchecked(MemSink::new()));
    assert_eq!(
        block_on(driver_over(&h, sink.clone()).probe()),
        Err(Refused::NoChecksums)
    );
}

#[test]
fn q283_a_bucket_trusting_the_sent_checksum_behind_signed_payloads_is_accepted() {
    // The signed payload hash already refuses a body that does not match: the probe's wrong hash never lands.
    let h = Harness::new();
    let bucket = FakeBucket::new();
    bucket.set_checksums(Checksums::Trusted);
    assert_eq!(block_on(driver_over(&h, bucket.sink()).probe()), Ok(()));
}

#[test]
fn q162_a_bucket_that_reports_no_checksum_is_refused() {
    let h = Harness::new();
    let bucket = FakeBucket::new();
    bucket.set_checksums(Checksums::NotReported);
    let r = block_on(driver_over(&h, bucket.sink()).probe());
    assert!(
        matches!(r, Err(Refused::Sink(SinkError::Failed { .. }))),
        "{r:?}"
    );
}

/// A bucket that ignores conditional create: a `put` replaces what the key holds.
struct Overwrites(MemSink);

impl BackupSink for Overwrites {
    fn put<'a>(&'a self, key: &'a str, bytes: Arc<[u8]>, sha: [u8; 32]) -> SinkFuture<'a, Stored> {
        if sha256(&bytes) == sha {
            self.0.remove(key);
        }
        self.0.put(key, bytes, sha)
    }
    fn stat<'a>(&'a self, key: &'a str) -> SinkFuture<'a, Option<Stored>> {
        self.0.stat(key)
    }
    fn get<'a>(&'a self, key: &'a str, range: Option<Range<u64>>) -> SinkFuture<'a, Vec<u8>> {
        self.0.get(key, range)
    }
    fn list<'a>(&'a self, prefix: &'a str, after: Option<&'a str>) -> SinkFuture<'a, ListPage> {
        self.0.list(prefix, after)
    }
}

#[test]
fn q162_a_bucket_without_conditional_writes_is_refused() {
    let h = Harness::new();
    let sink = Arc::new(Overwrites(MemSink::new()));
    for _ in 0..2 {
        assert_eq!(
            block_on(driver_over(&h, sink.clone()).probe()),
            Err(Refused::NoConditionalWrites)
        );
    }
}

#[test]
fn q283_a_bucket_holding_another_store_is_refused_before_any_write() {
    let h = Harness::new();
    for other in [[1u8; 16], [0xee; 16]] {
        let mem = Arc::new(MemSink::new());
        let own = Keys::new(ID);
        block_on(mem.put(&own.store(), b"s".as_slice().into(), sha256(b"s"))).unwrap();
        let theirs = Keys::new(other).store();
        block_on(mem.put(&theirs, b"t".as_slice().into(), sha256(b"t"))).unwrap();
        block_on(mem.put("unrelated/object", b"u".as_slice().into(), sha256(b"u"))).unwrap();
        let puts = mem.puts();
        assert_eq!(
            block_on(driver_over(&h, mem.clone()).probe()),
            Err(Refused::OtherStore { store_id: other })
        );
        assert_eq!(mem.puts(), puts);
    }
}

#[test]
fn q283_a_store_sealed_with_another_key_is_refused_before_any_write() {
    let h = Harness::new();
    let mem = Arc::new(MemSink::new());
    let key = Keys::new(ID).store();
    let other = TestEnvelope::new([1; 32]).seal(&key, STORE_BYTES).unwrap();
    block_on(mem.put(&key, other.clone().into(), sha256(&other))).unwrap();
    let puts = mem.puts();
    assert_eq!(
        block_on(driver_over(&h, mem.clone()).probe()),
        Err(Refused::WrongKey)
    );
    assert_eq!(mem.puts(), puts);
}

#[test]
fn q283_the_bucket_this_store_uploaded_to_passes_the_probe_again() {
    let h = sealed_store();
    let mem = Arc::new(MemSink::new());
    let mut d = driver_over(&h, mem.clone());
    assert_eq!(block_on(d.probe()), Ok(()));
    idle(&mut d);
    assert_eq!(block_on(driver_over(&h, mem.clone()).probe()), Ok(()));
}

#[test]
fn q162_a_bucket_without_conditional_writes_is_refused_through_the_s3_sink() {
    let h = Harness::new();
    let bucket = FakeBucket::new();
    bucket.set_ignores_conditions(true);
    for _ in 0..2 {
        assert_eq!(
            block_on(driver_over(&h, bucket.sink()).probe()),
            Err(Refused::NoConditionalWrites)
        );
    }
}

/// A bucket that refuses a wrong hash on upload but reports another hash for what it stores.
struct Misreports(MemSink);

impl BackupSink for Misreports {
    fn put<'a>(&'a self, key: &'a str, bytes: Arc<[u8]>, sha: [u8; 32]) -> SinkFuture<'a, Stored> {
        self.0.put(key, bytes, sha)
    }
    fn stat<'a>(&'a self, key: &'a str) -> SinkFuture<'a, Option<Stored>> {
        let r = block_on(self.0.stat(key)).map(|s| s.map(|s| Stored::new(s.size, [0; 32])));
        Box::pin(async move { r })
    }
    fn get<'a>(&'a self, key: &'a str, range: Option<Range<u64>>) -> SinkFuture<'a, Vec<u8>> {
        self.0.get(key, range)
    }
    fn list<'a>(&'a self, prefix: &'a str, after: Option<&'a str>) -> SinkFuture<'a, ListPage> {
        self.0.list(prefix, after)
    }
}

#[test]
fn q283_a_bucket_reporting_another_hash_for_the_probe_object_is_refused() {
    let h = Harness::new();
    let sink = Arc::new(Misreports(MemSink::new()));
    assert_eq!(
        block_on(driver_over(&h, sink).probe()),
        Err(Refused::NoChecksums)
    );
}
