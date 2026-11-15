//! [`S3Sink`]: the backup sink over S3-compatible object storage.

use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use kvik::{BackupSink, ListPage, SinkError, SinkFuture, Stored};
use sha2::{Digest, Sha256};

use crate::retry::{Jitter, Pause, RetryPolicy, SystemWallClock, ThreadSleep, WallClock};
use crate::sigv4::{self, Credentials, EMPTY_SHA256, ToSign};
use crate::transport::{HttpTransport, Method, Request, Response, Transport, TransportError};
use crate::worker::Pool;
use crate::xml;

/// The largest object one single-part upload may hold: 5 GiB.
pub const SINGLE_PART_LIMIT: u64 = 5 * 1024 * 1024 * 1024;

/// The longest key the protocol allows, in bytes.
const MAX_KEY_LEN: usize = 1024;

/// How the bucket is named in a request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Addressing {
    /// `https://endpoint/bucket/key`.
    #[default]
    Path,
    /// `https://bucket.endpoint/key`.
    VirtualHost,
}

/// How to reach a bucket, and how hard to try.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct S3Config {
    /// `https://host[:port]` (or `http://` for a local endpoint), no path.
    pub endpoint: String,
    /// The region the signature names.
    pub region: String,
    /// The bucket. It must have versioning or object lock (spec §8 Bucket).
    pub bucket: String,
    /// Access credentials.
    pub credentials: Credentials,
    /// Path-style (the default) or virtual-host addressing.
    pub addressing: Addressing,
    /// Connection set-up limit, TLS included. Default 10 s.
    pub connect_timeout: Duration,
    /// Limit on one whole exchange, body included. Default 10 minutes, enough
    /// for a 64 MiB segment at about 110 kB/s.
    pub exchange_timeout: Duration,
    /// Retries of transient faults.
    pub retry: RetryPolicy,
    /// Worker threads, that is, calls in flight at once. Default 2.
    pub workers: usize,
    /// The largest object `put` accepts; at most [`SINGLE_PART_LIMIT`].
    pub max_object_size: u64,
    /// Keys asked for per listing page, 1 to 1,000. Default 1,000.
    pub list_page: u32,
}

impl S3Config {
    /// A configuration with the defaults above.
    pub fn new(
        endpoint: impl Into<String>,
        region: impl Into<String>,
        bucket: impl Into<String>,
        credentials: Credentials,
    ) -> Self {
        S3Config {
            endpoint: endpoint.into(),
            region: region.into(),
            bucket: bucket.into(),
            credentials,
            addressing: Addressing::Path,
            connect_timeout: Duration::from_secs(10),
            exchange_timeout: Duration::from_secs(600),
            retry: RetryPolicy::default(),
            workers: 2,
            max_object_size: SINGLE_PART_LIMIT,
            list_page: 1000,
        }
    }

    /// Sets the addressing style.
    pub fn with_addressing(mut self, addressing: Addressing) -> Self {
        self.addressing = addressing;
        self
    }

    /// Sets the connect and exchange timeouts.
    pub fn with_timeouts(mut self, connect: Duration, exchange: Duration) -> Self {
        self.connect_timeout = connect;
        self.exchange_timeout = exchange;
        self
    }

    /// Sets the retry policy.
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Sets the number of worker threads.
    pub fn with_workers(mut self, workers: usize) -> Self {
        self.workers = workers;
        self
    }

    /// Sets the largest object `put` accepts (capped at
    /// [`SINGLE_PART_LIMIT`]).
    pub fn with_max_object_size(mut self, size: u64) -> Self {
        self.max_object_size = size.min(SINGLE_PART_LIMIT);
        self
    }

    /// Sets the listing page size (1 to 1,000).
    pub fn with_list_page(mut self, keys: u32) -> Self {
        self.list_page = keys.clamp(1, 1000);
        self
    }
}

fn failed(reason: impl Into<String>) -> SinkError {
    SinkError::Failed {
        reason: reason.into(),
    }
}

fn transient(reason: impl Into<String>) -> SinkError {
    SinkError::Transient {
        reason: reason.into(),
    }
}

/// Splits `scheme://authority` and checks it has no path or query.
fn parse_endpoint(endpoint: &str) -> Result<(&'static str, String), SinkError> {
    let (scheme, rest) = if let Some(r) = endpoint.strip_prefix("https://") {
        ("https", r)
    } else if let Some(r) = endpoint.strip_prefix("http://") {
        ("http", r)
    } else {
        return Err(failed(format!(
            "endpoint {endpoint:?} must start with https:// or http://"
        )));
    };
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.is_empty()
        || authority.contains(['/', '?', '#', '@'])
        || authority.chars().any(char::is_whitespace)
    {
        return Err(failed(format!(
            "endpoint {endpoint:?} must be scheme://host[:port] with no path"
        )));
    }
    Ok((scheme, authority.to_ascii_lowercase()))
}

/// The backup sink over S3-compatible object storage.
///
/// Every call runs on the sink's own worker threads (named `kvik-sink-N`),
/// which retry transient faults with backoff; the futures it returns only
/// wait for a worker's result, so they never block when polled and need no
/// async runtime. A call runs to the end even if its future is dropped.
/// Dropping the sink waits for the calls already queued.
///
/// Uploads are single-part, conditional (`If-None-Match: *`) and carry the
/// SHA-256 as `x-amz-checksum-sha256`, which the bucket must check; `put`
/// then asks for the object's reported size and SHA-256 and returns those.
/// The bucket must honour conditional writes and report whole-object
/// SHA-256 checksums; spec §13 lists checking this as a risk to measure.
pub struct S3Sink {
    inner: Arc<Inner>,
    pool: Pool,
}

impl std::fmt::Debug for S3Sink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Sink")
            .field("config", &self.inner.cfg)
            .finish_non_exhaustive()
    }
}

struct Inner {
    cfg: S3Config,
    scheme: &'static str,
    authority: String,
    transport: Arc<dyn Transport>,
    clock: Arc<dyn WallClock>,
    pause: Arc<dyn Pause>,
    jitter: Jitter,
}

impl S3Sink {
    /// A sink over HTTP, with the system clock and real pauses.
    pub fn new(config: S3Config) -> Result<Self, SinkError> {
        let max_body = config.max_object_size.max(16 * 1024 * 1024);
        let transport =
            HttpTransport::new(config.connect_timeout, config.exchange_timeout, max_body);
        Self::with_seams(
            config,
            Arc::new(transport),
            Arc::new(SystemWallClock),
            Arc::new(ThreadSleep),
        )
    }

    /// A sink over any transport (a fake, for tests), with the system clock
    /// and real pauses.
    pub fn with_transport(
        config: S3Config,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, SinkError> {
        Self::with_seams(
            config,
            transport,
            Arc::new(SystemWallClock),
            Arc::new(ThreadSleep),
        )
    }

    /// A sink with every seam given: transport, the clock that dates
    /// signatures, and the pause between attempts.
    pub fn with_seams(
        config: S3Config,
        transport: Arc<dyn Transport>,
        clock: Arc<dyn WallClock>,
        pause: Arc<dyn Pause>,
    ) -> Result<Self, SinkError> {
        let (scheme, authority) = parse_endpoint(&config.endpoint)?;
        if config.bucket.is_empty() || config.bucket.contains(['/', '?', '#']) {
            return Err(failed(format!("invalid bucket name {:?}", config.bucket)));
        }
        if config.region.is_empty() || config.region.contains('/') {
            return Err(failed(format!("invalid region {:?}", config.region)));
        }
        let pool = Pool::new(config.workers, "kvik-sink")?;
        let inner = Arc::new(Inner {
            jitter: Jitter::new(crate::retry::random_seed()),
            cfg: config,
            scheme,
            authority,
            transport,
            clock,
            pause,
        });
        Ok(S3Sink { inner, pool })
    }

    /// The configuration.
    pub fn config(&self) -> &S3Config {
        &self.inner.cfg
    }
}

fn check_key(key: &str) -> Result<(), SinkError> {
    if key.is_empty() || key.len() > MAX_KEY_LEN {
        return Err(failed(format!(
            "a key must have 1 to {MAX_KEY_LEN} bytes, not {}",
            key.len()
        )));
    }
    Ok(())
}

impl BackupSink for S3Sink {
    fn put<'a>(
        &'a self,
        key: &'a str,
        bytes: Arc<[u8]>,
        sha256: [u8; 32],
    ) -> SinkFuture<'a, Stored> {
        let size = bytes.len() as u64;
        let limit = self.inner.cfg.max_object_size;
        if size > limit {
            return Box::pin(async move { Err(SinkError::TooLarge { size, limit }) });
        }
        if let Err(e) = check_key(key) {
            return Box::pin(async move { Err(e) });
        }
        let inner = Arc::clone(&self.inner);
        let key = key.to_owned();
        Box::pin(
            self.pool
                .run(move || inner.retry(|| inner.put_attempt(&key, &bytes, sha256))),
        )
    }

    fn stat<'a>(&'a self, key: &'a str) -> SinkFuture<'a, Option<Stored>> {
        if let Err(e) = check_key(key) {
            return Box::pin(async move { Err(e) });
        }
        let inner = Arc::clone(&self.inner);
        let key = key.to_owned();
        Box::pin(self.pool.run(move || inner.retry(|| inner.head(&key))))
    }

    fn get<'a>(&'a self, key: &'a str, range: Option<Range<u64>>) -> SinkFuture<'a, Vec<u8>> {
        if let Err(e) = check_key(key) {
            return Box::pin(async move { Err(e) });
        }
        let inner = Arc::clone(&self.inner);
        let key = key.to_owned();
        Box::pin(
            self.pool
                .run(move || inner.retry(|| inner.get_attempt(&key, range.clone()))),
        )
    }

    fn list<'a>(
        &'a self,
        prefix: &'a str,
        start_after: Option<&'a str>,
    ) -> SinkFuture<'a, ListPage> {
        let inner = Arc::clone(&self.inner);
        let prefix = prefix.to_owned();
        let start_after = start_after.filter(|s| !s.is_empty()).map(str::to_owned);
        Box::pin(
            self.pool
                .run(move || inner.retry(|| inner.list_attempt(&prefix, start_after.as_deref()))),
        )
    }
}

/// What failed attempts are made of, for the error of a status.
enum Op {
    Put,
    Head,
    Get,
    List,
}

impl Inner {
    /// Runs `attempt` until it succeeds, fails for good, or the policy's
    /// attempts are spent.
    fn retry<T>(&self, mut attempt: impl FnMut() -> Result<T, SinkError>) -> Result<T, SinkError> {
        let policy = &self.cfg.retry;
        let mut n = 1;
        loop {
            match attempt() {
                Err(e) if e.is_transient() && n < policy.attempts => {
                    self.pause.pause(self.jitter.apply(policy.backoff(n)));
                    n += 1;
                }
                other => return other,
            }
        }
    }

    fn request(
        &self,
        method: Method,
        key: Option<&str>,
        query: Vec<(String, String)>,
        extra: Vec<(String, String)>,
        body: Option<Arc<[u8]>>,
        payload_sha256: &str,
    ) -> Request {
        let cfg = &self.cfg;
        let encoded_key = key.map(|k| sigv4::uri_encode(k, true));
        let (host, path) = match cfg.addressing {
            Addressing::Path => (
                self.authority.clone(),
                match &encoded_key {
                    Some(k) => format!("/{}/{k}", sigv4::uri_encode(&cfg.bucket, false)),
                    None => format!("/{}", sigv4::uri_encode(&cfg.bucket, false)),
                },
            ),
            Addressing::VirtualHost => (
                format!("{}.{}", cfg.bucket, self.authority),
                match &encoded_key {
                    Some(k) => format!("/{k}"),
                    None => "/".to_owned(),
                },
            ),
        };
        let amz_date = sigv4::amz_date(self.clock.now());
        let mut headers = vec![
            ("host".to_owned(), host.clone()),
            ("x-amz-content-sha256".to_owned(), payload_sha256.to_owned()),
            ("x-amz-date".to_owned(), amz_date.clone()),
        ];
        if let Some(token) = &cfg.credentials.session_token {
            headers.push(("x-amz-security-token".to_owned(), token.clone()));
        }
        headers.extend(extra);
        let auth = sigv4::authorization(
            &ToSign {
                method: method.as_str(),
                path: &path,
                query: &query,
                headers: &headers,
                payload_sha256,
                amz_date: &amz_date,
                region: &cfg.region,
            },
            &cfg.credentials,
        );
        headers.push(("authorization".to_owned(), auth));
        Request {
            method,
            origin: format!("{}://{host}", self.scheme),
            path,
            query,
            headers,
            body,
        }
    }

    fn send(&self, request: &Request) -> Result<Response, SinkError> {
        self.transport.send(request).map_err(|e| match e {
            TransportError::Timeout(_) | TransportError::Connection(_) => transient(e.to_string()),
            TransportError::Fatal(r) => failed(r),
        })
    }

    /// The error for a status the operation did not expect.
    fn status_error(&self, key: &str, resp: &Response, op: Op) -> SinkError {
        let body = String::from_utf8_lossy(&resp.body);
        let code = xml::element(&body, "Code")
            .ok()
            .flatten()
            .unwrap_or_default();
        let message = xml::element(&body, "Message")
            .ok()
            .flatten()
            .unwrap_or_default();
        let what = format!("status {} {code} {message}", resp.status);
        let what = what.trim_end();
        match (resp.status, code.as_str()) {
            (400, "BadDigest" | "XAmzContentSHA256Mismatch" | "InvalidDigest")
                if matches!(op, Op::Put) =>
            {
                SinkError::HashMismatch { key: key.into() }
            }
            (400, "RequestTimeout") | (408 | 429, _) => transient(what),
            (409, _) if matches!(op, Op::Put) => transient(what),
            (501, _) => failed(what),
            (500..=599, _) => transient(what),
            _ => failed(what),
        }
    }

    fn put_attempt(
        &self,
        key: &str,
        bytes: &Arc<[u8]>,
        sha256: [u8; 32],
    ) -> Result<Stored, SinkError> {
        let request = self.request(
            Method::Put,
            Some(key),
            Vec::new(),
            vec![
                ("if-none-match".to_owned(), "*".to_owned()),
                ("x-amz-checksum-sha256".to_owned(), B64.encode(sha256)),
            ],
            Some(Arc::clone(bytes)),
            &sigv4::hex(&sha256),
        );
        let resp = self.send(&request)?;
        let existed = match resp.status {
            200..=299 => false,
            412 => true,
            _ => return Err(self.status_error(key, &resp, Op::Put)),
        };
        // Ask the bucket what it holds now, rather than trust the reply.
        let size = bytes.len() as u64;
        match self.head(key)? {
            Some(s) if s.size == size && s.sha256 == sha256 => Ok(s),
            Some(_) if existed => Err(SinkError::Exists { key: key.into() }),
            Some(_) => Err(SinkError::HashMismatch { key: key.into() }),
            None => Err(transient(format!(
                "{key} not found just after its upload was accepted"
            ))),
        }
    }

    fn head(&self, key: &str) -> Result<Option<Stored>, SinkError> {
        let request = self.request(
            Method::Head,
            Some(key),
            Vec::new(),
            vec![("x-amz-checksum-mode".to_owned(), "ENABLED".to_owned())],
            None,
            EMPTY_SHA256,
        );
        let resp = self.send(&request)?;
        match resp.status {
            200 => {
                let size = resp
                    .header("content-length")
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .ok_or_else(|| failed(format!("no size reported for {key}")))?;
                let sha256 = reported_sha256(&resp).ok_or_else(|| {
                    failed(format!(
                        "the bucket reported no whole-object SHA-256 for {key}"
                    ))
                })?;
                Ok(Some(Stored::new(size, sha256)))
            }
            404 => Ok(None),
            _ => Err(self.status_error(key, &resp, Op::Head)),
        }
    }

    fn get_attempt(&self, key: &str, range: Option<Range<u64>>) -> Result<Vec<u8>, SinkError> {
        let invalid = || SinkError::InvalidRange { key: key.into() };
        let not_found = || SinkError::NotFound { key: key.into() };
        let mut extra = vec![("x-amz-checksum-mode".to_owned(), "ENABLED".to_owned())];
        if let Some(r) = &range {
            if r.start > r.end {
                return Err(invalid());
            }
            if r.start == r.end {
                // No bytes to ask for: only whether the range is within.
                return match self.head(key)? {
                    None => Err(not_found()),
                    Some(s) if r.start <= s.size => Ok(Vec::new()),
                    Some(_) => Err(invalid()),
                };
            }
            extra = vec![(
                "range".to_owned(),
                format!("bytes={}-{}", r.start, r.end - 1),
            )];
        }
        let request = self.request(
            Method::Get,
            Some(key),
            Vec::new(),
            extra,
            None,
            EMPTY_SHA256,
        );
        let resp = self.send(&request)?;
        let declared = resp
            .header("content-length")
            .and_then(|v| v.trim().parse::<u64>().ok());
        if let Some(n) = declared
            && n != resp.body.len() as u64
        {
            return Err(transient(format!(
                "{key}: {} of {n} bytes received",
                resp.body.len()
            )));
        }
        match (resp.status, range) {
            (200, None) => {
                if let Some(reported) = reported_sha256(&resp)
                    && <[u8; 32]>::from(Sha256::digest(&resp.body)) != reported
                {
                    return Err(transient(format!(
                        "{key}: downloaded bytes do not match the reported SHA-256"
                    )));
                }
                Ok(resp.body)
            }
            (206, Some(r)) => {
                let (first, last, total) = resp
                    .header("content-range")
                    .and_then(parse_content_range)
                    .ok_or_else(|| failed(format!("{key}: no readable Content-Range")))?;
                if first != r.start || last < first {
                    return Err(failed(format!(
                        "{key}: Content-Range does not match the request"
                    )));
                }
                if resp.body.len() as u64 != last - first + 1 {
                    return Err(transient(format!("{key}: ranged body cut short")));
                }
                if last + 1 < r.end {
                    return if total.is_some_and(|t| t <= last + 1) {
                        Err(invalid())
                    } else {
                        Err(transient(format!("{key}: fewer bytes than asked for")))
                    };
                }
                let mut body = resp.body;
                body.truncate((r.end - r.start) as usize);
                Ok(body)
            }
            // A server that ignores `Range` sends the whole object.
            (200, Some(r)) => {
                if r.end > resp.body.len() as u64 {
                    return Err(invalid());
                }
                Ok(resp.body[r.start as usize..r.end as usize].to_vec())
            }
            (416, _) => Err(invalid()),
            (404, _) => Err(not_found()),
            _ => Err(self.status_error(key, &resp, Op::Get)),
        }
    }

    fn list_attempt(&self, prefix: &str, start_after: Option<&str>) -> Result<ListPage, SinkError> {
        let mut query = vec![
            ("list-type".to_owned(), "2".to_owned()),
            ("max-keys".to_owned(), self.cfg.list_page.to_string()),
            ("prefix".to_owned(), prefix.to_owned()),
        ];
        if let Some(s) = start_after {
            query.push(("start-after".to_owned(), s.to_owned()));
        }
        let request = self.request(Method::Get, None, query, Vec::new(), None, EMPTY_SHA256);
        let resp = self.send(&request)?;
        if resp.status != 200 {
            return Err(self.status_error(prefix, &resp, Op::List));
        }
        let body =
            std::str::from_utf8(&resp.body).map_err(|_| failed("a listing that is not UTF-8"))?;
        let keys = xml::elements(body, "Key").map_err(|e| failed(format!("listing: {e}")))?;
        let more = xml::element(body, "IsTruncated")
            .map_err(|e| failed(format!("listing: {e}")))?
            .is_some_and(|v| v.trim() == "true");
        let mut previous = start_after;
        for k in &keys {
            if !k.starts_with(prefix) || previous.is_some_and(|p| k.as_str() <= p) {
                return Err(failed(format!(
                    "listing: key {k:?} out of order or outside the prefix"
                )));
            }
            previous = Some(k);
        }
        if more && keys.is_empty() {
            return Err(failed("listing: truncated page with no keys"));
        }
        Ok(ListPage::new(keys, more))
    }
}

/// The whole-object SHA-256 a response reports, if any.
fn reported_sha256(resp: &Response) -> Option<[u8; 32]> {
    if let Some(t) = resp.header("x-amz-checksum-type")
        && !t.eq_ignore_ascii_case("FULL_OBJECT")
    {
        return None;
    }
    let v = resp.header("x-amz-checksum-sha256")?;
    B64.decode(v.trim()).ok()?.try_into().ok()
}

/// `bytes first-last/total` (total may be `*`).
fn parse_content_range(v: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = v.trim().strip_prefix("bytes ")?;
    let (span, total) = rest.split_once('/')?;
    let (a, b) = span.split_once('-')?;
    let total = if total == "*" {
        None
    } else {
        Some(total.parse().ok()?)
    };
    Some((a.parse().ok()?, b.parse().ok()?, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_checked() {
        assert_eq!(
            parse_endpoint("https://Objects.test:9000/").unwrap(),
            ("https", "objects.test:9000".to_owned())
        );
        assert_eq!(parse_endpoint("http://objects.test").unwrap().0, "http");
        for bad in [
            "objects.test",
            "https://",
            "https://a/b",
            "ftp://a",
            "https://u@a",
        ] {
            assert!(parse_endpoint(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn content_range_is_parsed() {
        assert_eq!(
            parse_content_range("bytes 0-9/100"),
            Some((0, 9, Some(100)))
        );
        assert_eq!(parse_content_range("bytes 5-6/*"), Some((5, 6, None)));
        assert_eq!(parse_content_range("items 0-9/100"), None);
    }
}
