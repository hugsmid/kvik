//! [`S3Sink`]: the backup sink over S3-compatible object storage.

use std::ops::Range;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use kvik::{BackupSink, ListPage, SinkError, SinkFuture, Stored};
use sha2::{Digest, Sha256};

use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::{Addressing, S3Config};
use crate::retry::{Jitter, Pause, SystemWallClock, ThreadSleep, WallClock};
use crate::sigv4::{self, EMPTY_SHA256, ToSign};
use crate::transport::{
    HttpTransport, META_BODY_LIMIT, Method, Request, Response, Transport, TransportError,
};
use crate::worker::Pool;
use crate::xml;

/// The longest key the protocol allows, in bytes.
const MAX_KEY_LEN: usize = 1024;

/// The most of a server's error code and message copied into an error.
const MAX_ERROR_TEXT: usize = 200;

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

/// The backup sink over S3-compatible object storage; calls run, with retries,
/// on its worker threads (`kvik-sink-N`). Rules: `docs/api.md` Backup and restore.
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
    /// The bucket answered a `HEAD`, so a key's 404 means the key is absent.
    bucket_seen: AtomicBool,
}

impl S3Sink {
    /// A sink over HTTP, with the system clock and real pauses.
    pub fn new(config: S3Config) -> Result<Self, SinkError> {
        let transport = HttpTransport::new(config.connect_timeout, config.exchange_timeout);
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
        let (scheme, authority) = config.validate()?;
        let pool = Pool::new(config.workers, config.queue, "kvik-sink")?;
        let inner = Arc::new(Inner {
            jitter: Jitter::new(crate::retry::random_seed()),
            cfg: config,
            scheme,
            authority,
            transport,
            clock,
            pause,
            bucket_seen: AtomicBool::new(false),
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
        Box::pin(self.pool.run(move || inner.put(&key, &bytes, sha256)))
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

    /// The first attempt uploads; a later one asks first, so that a lost
    /// reply or a failed read-back does not send the body again.
    fn put(&self, key: &str, bytes: &Arc<[u8]>, sha256: [u8; 32]) -> Result<Stored, SinkError> {
        let mut first = true;
        self.retry(|| {
            if !std::mem::take(&mut first)
                && let Some(s) = self.head(key)?
            {
                return same_or_exists(key, s, bytes.len() as u64, sha256);
            }
            self.put_attempt(key, bytes, sha256)
        })
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
            max_response: META_BODY_LIMIT,
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
        let what = format!(
            "status {} {} {}",
            resp.status,
            clip(&code, MAX_ERROR_TEXT),
            clip(&message, MAX_ERROR_TEXT)
        );
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
        let size = bytes.len() as u64;
        match self.head(key)? {
            Some(s) if existed => same_or_exists(key, s, size, sha256),
            Some(s) if s.size == size && s.sha256 == sha256 => Ok(s),
            Some(_) => Err(SinkError::HashMismatch { key: key.into() }),
            None => Err(transient(format!(
                "{key} not found just after its upload was accepted"
            ))),
        }
    }

    /// Whether the bucket exists, asked once per sink; a missing bucket is
    /// `Failed`, never an absent key.
    fn check_bucket(&self) -> Result<(), SinkError> {
        if self.bucket_seen.load(Ordering::Acquire) {
            return Ok(());
        }
        let request = self.request(
            Method::Head,
            None,
            Vec::new(),
            Vec::new(),
            None,
            EMPTY_SHA256,
        );
        let resp = self.send(&request)?;
        match resp.status {
            200 => {
                self.bucket_seen.store(true, Ordering::Release);
                Ok(())
            }
            404 => Err(failed(format!("bucket {} not found", self.cfg.bucket))),
            _ => Err(self.status_error(&self.cfg.bucket, &resp, Op::Head)),
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
            404 => {
                self.check_bucket()?;
                Ok(None)
            }
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
        let limit = match &range {
            Some(r) => r.end - r.start,
            None => self.cfg.max_object_size,
        };
        let request = self
            .request(
                Method::Get,
                Some(key),
                Vec::new(),
                extra,
                None,
                EMPTY_SHA256,
            )
            .with_max_response(limit.max(META_BODY_LIMIT));
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
                let reported = reported_sha256(&resp).ok_or_else(|| {
                    failed(format!(
                        "the bucket reported no whole-object SHA-256 for {key}"
                    ))
                })?;
                if <[u8; 32]>::from(Sha256::digest(&resp.body)) != reported {
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
            (200, Some(r)) => {
                if r.end > resp.body.len() as u64 {
                    return Err(invalid());
                }
                Ok(resp.body[r.start as usize..r.end as usize].to_vec())
            }
            (416, _) => Err(invalid()),
            (404, _) if error_code(&resp).as_deref() == Some("NoSuchKey") => Err(not_found()),
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
        let rooted = (body.contains("<ListBucketResult>") || body.contains("<ListBucketResult "))
            && body.contains("</ListBucketResult>");
        if !rooted {
            return Err(failed("the reply to a listing is not a listing"));
        }
        let keys = xml::elements(body, "Key").map_err(|e| failed(format!("listing: {e}")))?;
        let more = match xml::element(body, "IsTruncated")
            .map_err(|e| failed(format!("listing: {e}")))?
            .as_deref()
            .map(str::trim)
        {
            Some("true") => true,
            Some("false") => false,
            _ => return Err(failed("listing: no IsTruncated")),
        };
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

fn same_or_exists(key: &str, s: Stored, size: u64, sha256: [u8; 32]) -> Result<Stored, SinkError> {
    if s.size == size && s.sha256 == sha256 {
        Ok(s)
    } else {
        Err(SinkError::Exists { key: key.into() })
    }
}

fn error_code(resp: &Response) -> Option<String> {
    xml::element(&String::from_utf8_lossy(&resp.body), "Code")
        .ok()
        .flatten()
}

/// At most `n` characters of `s`.
fn clip(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
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
    fn content_range_is_parsed() {
        assert_eq!(
            parse_content_range("bytes 0-9/100"),
            Some((0, 9, Some(100)))
        );
        assert_eq!(parse_content_range("bytes 5-6/*"), Some((5, 6, None)));
        assert_eq!(parse_content_range("items 0-9/100"), None);
    }

    #[test]
    fn error_text_is_clipped_on_a_character_boundary() {
        assert_eq!(clip("ab", 5), "ab");
        assert_eq!(clip("åbc", 2), "åb");
    }
}
