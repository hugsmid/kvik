//! Uploads a file to the in-memory fake bucket through the S3 client,
//! survives a lost reply, verifies the copy and lists it back.
//!
//! Run with `cargo run -p kvik-sink --example fake_bucket`. Against a real
//! bucket, build the sink with `S3Sink::new(S3Config::new(endpoint, region,
//! bucket, credentials))` instead.

use std::sync::Arc;

use kvik::BackupSink;
use kvik_sink::block_on;
use kvik_sink::fake::{FakeBucket, FakeOp, Fault};
use kvik_sink::verify;

fn main() {
    let bucket = FakeBucket::new();
    let sink = bucket.sink();

    // The first reply is lost after the bucket stored the object; the retry
    // finds it already there with the same hash and accepts it.
    bucket.inject(FakeOp::Put, Fault::TimeoutAfter);
    let bytes: Arc<[u8]> = Arc::from(&b"ciphertext of a sealed segment"[..]);
    let copy =
        block_on(verify::upload(&sink, "log/0000000000000001", bytes.clone())).expect("upload");
    println!(
        "verified {} ({} bytes) after {} upload attempts",
        copy.key(),
        copy.stored().size,
        bucket.count(FakeOp::Put)
    );

    let back =
        block_on(verify::download(&sink, copy.key(), copy.stored().sha256)).expect("download");
    assert_eq!(&back[..], &bytes[..]);

    let page = block_on(sink.list("log/", None)).expect("list");
    println!("listed: {:?}", page.keys);
}
