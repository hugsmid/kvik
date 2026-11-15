//! The requests the client assembles, signed at a fixed time, against
//! signatures computed by a separate implementation of the signing algorithm
//! from the raw key, the body and the configuration.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kvik::BackupSink;
use kvik_sink::fake::{FAKE_BUCKET, FAKE_REGION, fake_credentials};
use kvik_sink::transport::{Method, Request, Response, Transport, TransportError};
use kvik_sink::verify::sha256;
use kvik_sink::{Addressing, RetryPolicy, S3Config, S3Sink, ThreadSleep, WallClock, block_on};

/// 2026-11-15 12:00:00 UTC.
struct Fixed;

impl WallClock for Fixed {
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_794_744_000)
    }
}

/// Keeps the request and answers nothing.
#[derive(Default)]
struct Capture(Mutex<Option<Request>>);

impl Transport for Capture {
    fn send(&self, r: &Request) -> Result<Response, TransportError> {
        *self.0.lock().unwrap() = Some(r.clone());
        Err(TransportError::Fatal("captured".into()))
    }
}

fn sink(config: S3Config) -> (S3Sink, Arc<Capture>) {
    let capture = Arc::new(Capture::default());
    let config = config.with_retry(RetryPolicy::new(1, Duration::ZERO, Duration::ZERO));
    let sink = S3Sink::with_seams(
        config,
        capture.clone(),
        Arc::new(Fixed),
        Arc::new(ThreadSleep),
    )
    .unwrap();
    (sink, capture)
}

fn config() -> S3Config {
    S3Config::new(
        "http://objects.test:9000",
        FAKE_REGION,
        FAKE_BUCKET,
        fake_credentials(),
    )
    .with_plain_http(true)
}

#[test]
fn a_path_style_upload_is_assembled_and_signed() {
    let (sink, capture) = sink(config());
    let body = b"hello";
    let _ = block_on(sink.put("log/a b+c", Arc::from(&body[..]), sha256(body)));
    let r = capture.0.lock().unwrap().take().unwrap();
    assert_eq!(r.method, Method::Put);
    assert_eq!(r.origin, "http://objects.test:9000");
    assert_eq!(r.path, "/kvik-test/log/a%20b%2Bc");
    assert_eq!(r.header("host"), Some("objects.test:9000"));
    assert_eq!(r.header("if-none-match"), Some("*"));
    assert_eq!(
        r.header("x-amz-checksum-sha256"),
        Some("LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=")
    );
    assert_eq!(r.header("x-amz-date"), Some("20261115T120000Z"));
    assert_eq!(
        r.header("authorization"),
        Some(
            "AWS4-HMAC-SHA256 Credential=TESTKEYID0000000/20261115/test-region-1/s3/aws4_request, \
             SignedHeaders=host;if-none-match;x-amz-checksum-sha256;x-amz-content-sha256;x-amz-date, \
             Signature=a75354ec57add1e1c53ef260c87d9642ab69e86d4e37c697ad599ef9be953777"
        )
    );
    assert_eq!(r.body.as_deref(), Some(&body[..]));
}

#[test]
fn a_virtual_host_listing_with_a_session_token_is_assembled_and_signed() {
    let mut config = config().with_addressing(Addressing::VirtualHost);
    config.credentials = fake_credentials().with_session_token("tok/en");
    let (sink, capture) = sink(config);
    let _ = block_on(sink.list("manifest/", Some("manifest/0001")));
    let r = capture.0.lock().unwrap().take().unwrap();
    assert_eq!(r.method, Method::Get);
    assert_eq!(r.path, "/");
    assert_eq!(r.header("host"), Some("kvik-test.objects.test:9000"));
    assert_eq!(
        r.url(),
        "http://kvik-test.objects.test:9000/?list-type=2&max-keys=1000&prefix=manifest%2F\
         &start-after=manifest%2F0001"
    );
    assert_eq!(
        r.header("authorization"),
        Some(
            "AWS4-HMAC-SHA256 Credential=TESTKEYID0000000/20261115/test-region-1/s3/aws4_request, \
             SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token, \
             Signature=02245e3c53d71d8e07c8a206345be647ac112704606fc01e06ebddb06efa18db"
        )
    );
    assert_eq!(r.body, None);
}
