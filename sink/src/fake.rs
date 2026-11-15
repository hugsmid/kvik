//! [`FakeBucket`]: an in-memory bucket that answers the subset of the S3
//! protocol the client uses, at the [`Transport`] seam, with faults that a
//! test injects. It checks signatures, payload hashes and checksums, honours
//! `If-None-Match: *`, serves ranges and pages listings, as a bucket must for
//! the sink's rules to hold.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use sha2::{Digest, Sha256};

use crate::retry::{Pause, RetryPolicy, SystemWallClock};
use crate::s3::{S3Config, S3Sink};
use crate::sigv4::{self, Credentials, ToSign};
use crate::transport::{Method, Request, Response, Transport, TransportError};
use crate::xml;

/// The endpoint a fake answers for (a reserved name: it never resolves).
pub const FAKE_ENDPOINT: &str = "http://objects.test";
/// The fake's bucket.
pub const FAKE_BUCKET: &str = "kvik-test";
/// The fake's region.
pub const FAKE_REGION: &str = "test-region-1";

/// The placeholder credentials a fake accepts.
pub fn fake_credentials() -> Credentials {
    Credentials::new(
        "TESTKEYID0000000",
        "test-secret-0000000000000000000000000000",
    )
}

/// Which requests a fault applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FakeOp {
    /// Uploads.
    Put,
    /// `HEAD` (stat, and the read-back after an upload).
    Head,
    /// Downloads.
    Get,
    /// Listings.
    List,
    /// Any request.
    Any,
}

/// A fault injected into the next request of a kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Fault {
    /// Answer with this status and an error body; nothing is applied.
    Status(u16),
    /// Time out before the request reaches the bucket; nothing is applied.
    TimeoutBefore,
    /// Apply the request, then lose the reply (a timeout).
    TimeoutAfter,
    /// Lose the connection part-way through an upload's body: the bucket
    /// never sees a whole body and stores nothing.
    CutUpload,
    /// Send half a download's body under its full `Content-Length`.
    TruncatedBody,
}

/// How the fake treats `x-amz-checksum-sha256`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Checksums {
    /// Reject a mismatch and report the hash of the stored bytes (what the
    /// spec requires of a bucket).
    #[default]
    Checked,
    /// Store whatever arrives and report the hash that was sent: a bucket
    /// that does not check.
    Trusted,
    /// Check, but never report a checksum.
    NotReported,
}

#[derive(Clone)]
struct Object {
    bytes: Vec<u8>,
    reported: [u8; 32],
    /// The listing call from which the object appears in listings.
    listed_from: u64,
}

#[derive(Default)]
struct State {
    objects: BTreeMap<String, Object>,
    faults: VecDeque<(FakeOp, Fault)>,
    checksums: Checksums,
    corrupt_on_store: bool,
    list_lag: u64,
    list_calls: u64,
    log: Vec<(Method, String)>,
    pauses: Vec<Duration>,
}

/// An in-memory bucket (see the module documentation). Clones share one
/// bucket, so a test keeps a handle to inject faults and inspect objects
/// while a sink uses another as its transport.
#[derive(Clone, Default)]
pub struct FakeBucket {
    state: Arc<Mutex<State>>,
}

impl std::fmt::Debug for FakeBucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeBucket")
            .field("objects", &self.lock().objects.len())
            .finish_non_exhaustive()
    }
}

/// Records pauses instead of sleeping.
struct RecordPause(Arc<Mutex<State>>);

impl Pause for RecordPause {
    fn pause(&self, d: Duration) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pauses
            .push(d);
    }
}

impl FakeBucket {
    /// An empty bucket.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A configuration for this fake: its endpoint, bucket, region and
    /// credentials, path-style, with a retry policy of six attempts.
    pub fn config(&self) -> S3Config {
        S3Config::new(FAKE_ENDPOINT, FAKE_REGION, FAKE_BUCKET, fake_credentials()).with_retry(
            RetryPolicy::new(6, Duration::from_millis(100), Duration::from_secs(5)),
        )
    }

    /// A sink over this fake with [`config`](Self::config) whose pauses are
    /// recorded ([`pauses`](Self::pauses)) instead of slept.
    pub fn sink(&self) -> S3Sink {
        self.sink_with(self.config())
    }

    /// A sink over this fake with a configuration of the caller's, pauses
    /// recorded.
    pub fn sink_with(&self, config: S3Config) -> S3Sink {
        S3Sink::with_seams(
            config,
            Arc::new(self.clone()),
            Arc::new(SystemWallClock),
            Arc::new(RecordPause(Arc::clone(&self.state))),
        )
        .expect("the fake's configuration is valid")
    }

    /// Queues a fault for the next request of a kind. Faults apply in the
    /// order queued, each once.
    pub fn inject(&self, op: FakeOp, fault: Fault) {
        self.lock().faults.push_back((op, fault));
    }

    /// Faults still queued.
    pub fn pending_faults(&self) -> usize {
        self.lock().faults.len()
    }

    /// Sets how checksums are treated.
    pub fn set_checksums(&self, c: Checksums) {
        self.lock().checksums = c;
    }

    /// Stores every later upload with its first byte changed, reporting the
    /// hash of what it stored.
    pub fn set_corrupt_on_store(&self, on: bool) {
        self.lock().corrupt_on_store = on;
    }

    /// Keeps each later upload out of the next `calls` listings, as a bucket
    /// whose listings lag behind its writes.
    pub fn set_list_lag(&self, calls: u64) {
        self.lock().list_lag = calls;
    }

    /// Changes a stored object's bytes without changing its reported hash,
    /// as damage inside the bucket would. Returns whether it exists.
    pub fn damage(&self, key: &str) -> bool {
        match self.lock().objects.get_mut(key) {
            Some(o) if !o.bytes.is_empty() => {
                o.bytes[0] ^= 0xff;
                true
            }
            _ => false,
        }
    }

    /// The stored bytes under a key.
    pub fn object(&self, key: &str) -> Option<Vec<u8>> {
        self.lock().objects.get(key).map(|o| o.bytes.clone())
    }

    /// Every stored key, ascending.
    pub fn keys(&self) -> Vec<String> {
        self.lock().objects.keys().cloned().collect()
    }

    /// Requests received of a kind (those a fault answered included).
    pub fn count(&self, op: FakeOp) -> usize {
        self.lock()
            .log
            .iter()
            .filter(|(m, path)| matches(op, *m, path))
            .count()
    }

    /// The pauses a [`sink`](Self::sink) took between attempts.
    pub fn pauses(&self) -> Vec<Duration> {
        self.lock().pauses.clone()
    }
}

fn matches(op: FakeOp, method: Method, target: &str) -> bool {
    let is_list = target.contains("list-type=");
    match op {
        FakeOp::Any => true,
        FakeOp::Put => method == Method::Put,
        FakeOp::Head => method == Method::Head,
        FakeOp::Get => method == Method::Get && !is_list,
        FakeOp::List => method == Method::Get && is_list,
    }
}

fn error(status: u16, code: &str, message: &str) -> Response {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>{}</Code><Message>{}</Message></Error>",
        xml::escape(code),
        xml::escape(message)
    );
    Response::new(
        status,
        vec![("content-type".into(), "application/xml".into())],
        body.into_bytes(),
    )
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Checks the signature against the fake's credentials, over the headers the
/// request says it signed.
fn signature_ok(req: &Request, body_sha: &str) -> Result<(), Response> {
    let auth = req
        .header("authorization")
        .ok_or_else(|| error(403, "AccessDenied", "no Authorization header"))?;
    let signed = auth
        .split("SignedHeaders=")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .ok_or_else(|| error(400, "AuthorizationHeaderMalformed", "no SignedHeaders"))?;
    let mut headers = Vec::new();
    for name in signed.split(';') {
        let v = req
            .header(name)
            .ok_or_else(|| error(403, "SignatureDoesNotMatch", "a signed header is missing"))?;
        headers.push((name.to_owned(), v.to_owned()));
    }
    for required in ["host", "x-amz-date", "x-amz-content-sha256"] {
        if !headers.iter().any(|(k, _)| k == required) {
            return Err(error(
                403,
                "AccessDenied",
                "a required header is not signed",
            ));
        }
    }
    let amz_date = req.header("x-amz-date").unwrap_or_default();
    if amz_date.len() != 16 {
        return Err(error(403, "AccessDenied", "malformed x-amz-date"));
    }
    let payload = req.header("x-amz-content-sha256").unwrap_or_default();
    let expected = sigv4::authorization(
        &ToSign {
            method: req.method.as_str(),
            path: &req.path,
            query: &req.query,
            headers: &headers,
            payload_sha256: payload,
            amz_date,
            region: FAKE_REGION,
        },
        &fake_credentials(),
    );
    if expected != auth {
        return Err(error(403, "SignatureDoesNotMatch", "signature mismatch"));
    }
    if payload != body_sha {
        return Err(error(
            400,
            "XAmzContentSHA256Mismatch",
            "the payload hash does not match the body",
        ));
    }
    Ok(())
}

impl Transport for FakeBucket {
    fn send(&self, req: &Request) -> Result<Response, TransportError> {
        let target = req.url();
        let fault = {
            let mut s = self.lock();
            s.log.push((req.method, target.clone()));
            let i = s
                .faults
                .iter()
                .position(|(op, _)| matches(*op, req.method, &target));
            i.and_then(|i| s.faults.remove(i)).map(|(_, f)| f)
        };
        match fault {
            Some(Fault::Status(code)) => {
                return Ok(error(code, "InjectedFault", "fault injected by the test"));
            }
            Some(Fault::TimeoutBefore) => {
                return Err(TransportError::Timeout(
                    "injected, before the request".into(),
                ));
            }
            Some(Fault::CutUpload) if req.method == Method::Put => {
                return Err(TransportError::Connection(
                    "injected: connection lost during the upload".into(),
                ));
            }
            _ => {}
        }
        let mut resp = self.handle(req);
        match fault {
            Some(Fault::TimeoutAfter) => Err(TransportError::Timeout(
                "injected, after the request".into(),
            )),
            Some(Fault::TruncatedBody) => {
                let half = resp.body.len() / 2;
                resp.body.truncate(half);
                Ok(resp)
            }
            _ => Ok(resp),
        }
    }
}

impl FakeBucket {
    fn handle(&self, req: &Request) -> Response {
        let body: &[u8] = req.body.as_deref().unwrap_or(&[]);
        let body_sha = sigv4::hex(&Sha256::digest(body));
        if let Err(r) = signature_ok(req, &body_sha) {
            return r;
        }
        let host = req.header("host").unwrap_or_default();
        let virtual_prefix = format!("{FAKE_BUCKET}.");
        let raw_key = if host.starts_with(&virtual_prefix) {
            req.path.strip_prefix('/').unwrap_or_default()
        } else {
            let bucket_path = format!("/{FAKE_BUCKET}");
            match req.path.strip_prefix(&bucket_path) {
                Some("") => "",
                Some(rest) if rest.starts_with('/') => &rest[1..],
                _ => return error(404, "NoSuchBucket", "no such bucket"),
            }
        };
        let Some(key) = percent_decode(raw_key) else {
            return error(400, "InvalidURI", "bad percent-encoding");
        };
        match (req.method, key.is_empty()) {
            (Method::Put, false) => self.put(req, &key, body),
            (Method::Head, false) => self.head(req, &key),
            (Method::Get, false) => self.get(req, &key),
            (Method::Get, true) => self.list(req),
            _ => error(501, "NotImplemented", "not in the fake's subset"),
        }
    }

    fn put(&self, req: &Request, key: &str, body: &[u8]) -> Response {
        let mut s = self.lock();
        if req.header("if-none-match") == Some("*") && s.objects.contains_key(key) {
            return error(412, "PreconditionFailed", "the key exists");
        }
        let actual: [u8; 32] = Sha256::digest(body).into();
        let sent = match req.header("x-amz-checksum-sha256") {
            Some(v) => match B64
                .decode(v)
                .ok()
                .and_then(|b| <[u8; 32]>::try_from(b).ok())
            {
                Some(h) => Some(h),
                None => return error(400, "InvalidRequest", "malformed checksum"),
            },
            None => None,
        };
        let reported = match (s.checksums, sent) {
            (Checksums::Trusted, Some(h)) => h,
            (_, Some(h)) if h != actual => {
                return error(400, "BadDigest", "the checksum does not match the body");
            }
            _ => actual,
        };
        let mut bytes = body.to_vec();
        let mut reported = reported;
        if s.corrupt_on_store && !bytes.is_empty() {
            bytes[0] ^= 0xff;
            reported = Sha256::digest(&bytes).into();
        }
        let listed_from = s.list_calls + s.list_lag;
        s.objects.insert(
            key.to_owned(),
            Object {
                bytes,
                reported,
                listed_from,
            },
        );
        let mut headers = vec![(
            "etag".to_owned(),
            format!("\"{}\"", &sigv4::hex(&reported)[..32]),
        )];
        if s.checksums != Checksums::NotReported {
            headers.push(("x-amz-checksum-sha256".to_owned(), B64.encode(reported)));
        }
        Response::new(200, headers, Vec::new())
    }

    fn object_headers(&self, req: &Request, o: &Object) -> Vec<(String, String)> {
        let mut h = vec![("content-length".to_owned(), o.bytes.len().to_string())];
        if req.header("x-amz-checksum-mode") == Some("ENABLED")
            && self.lock().checksums != Checksums::NotReported
        {
            h.push(("x-amz-checksum-sha256".to_owned(), B64.encode(o.reported)));
            h.push(("x-amz-checksum-type".to_owned(), "FULL_OBJECT".to_owned()));
        }
        h
    }

    fn head(&self, req: &Request, key: &str) -> Response {
        let o = self.lock().objects.get(key).cloned();
        match o {
            Some(o) => Response::new(200, self.object_headers(req, &o), Vec::new()),
            None => Response::new(404, Vec::new(), Vec::new()),
        }
    }

    fn get(&self, req: &Request, key: &str) -> Response {
        let Some(o) = self.lock().objects.get(key).cloned() else {
            return error(404, "NoSuchKey", "no such key");
        };
        let size = o.bytes.len() as u64;
        match req.header("range") {
            None => {
                let headers = self.object_headers(req, &o);
                Response::new(200, headers, o.bytes)
            }
            Some(r) => {
                let span = r.strip_prefix("bytes=").and_then(|s| s.split_once('-'));
                let Some((a, b)) = span else {
                    return error(400, "InvalidArgument", "malformed range");
                };
                let (Ok(a), Ok(b)) = (a.parse::<u64>(), b.parse::<u64>()) else {
                    return error(400, "InvalidArgument", "malformed range");
                };
                if a >= size || b < a {
                    return error(416, "InvalidRange", "range not satisfiable");
                }
                let b = b.min(size - 1);
                let part = o.bytes[a as usize..=b as usize].to_vec();
                Response::new(
                    206,
                    vec![
                        ("content-length".to_owned(), part.len().to_string()),
                        ("content-range".to_owned(), format!("bytes {a}-{b}/{size}")),
                    ],
                    part,
                )
            }
        }
    }

    fn list(&self, req: &Request) -> Response {
        let q = |name: &str| {
            req.query
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        if q("list-type") != Some("2") {
            return error(501, "NotImplemented", "only list-type=2");
        }
        let prefix = q("prefix").unwrap_or_default();
        let after = q("start-after").unwrap_or_default();
        let max: usize = q("max-keys").and_then(|v| v.parse().ok()).unwrap_or(1000);
        let mut s = self.lock();
        let call = s.list_calls;
        s.list_calls += 1;
        let mut keys = s
            .objects
            .range(prefix.to_owned()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .filter(|(k, o)| k.as_str() > after && o.listed_from <= call)
            .map(|(k, _)| k.clone());
        let page: Vec<String> = keys.by_ref().take(max).collect();
        let truncated = keys.next().is_some();
        let mut doc =
            String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ListBucketResult>");
        doc.push_str(&format!(
            "<Name>{FAKE_BUCKET}</Name><Prefix>{}</Prefix><KeyCount>{}</KeyCount><MaxKeys>{max}</MaxKeys><IsTruncated>{truncated}</IsTruncated>",
            xml::escape(prefix),
            page.len()
        ));
        for k in &page {
            let size = s.objects[k].bytes.len();
            doc.push_str(&format!(
                "<Contents><Key>{}</Key><Size>{size}</Size></Contents>",
                xml::escape(k)
            ));
        }
        doc.push_str("</ListBucketResult>");
        Response::new(
            200,
            vec![("content-type".into(), "application/xml".into())],
            doc.into_bytes(),
        )
    }
}
