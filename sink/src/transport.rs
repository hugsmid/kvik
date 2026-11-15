//! The seam under the S3 client: one HTTP exchange, request to response.
//! [`HttpTransport`] is the production implementation;
//! [`FakeBucket`](crate::fake::FakeBucket) answers in memory.

use std::sync::Arc;
use std::time::Duration;

/// An HTTP method the client uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Method {
    /// `GET`.
    Get,
    /// `HEAD`.
    Head,
    /// `PUT`.
    Put,
}

impl Method {
    /// The method's name on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Head => "HEAD",
            Method::Put => "PUT",
        }
    }
}

/// The largest response body read for anything but an object's bytes
/// (errors, listings, replies to uploads).
pub const META_BODY_LIMIT: u64 = 16 * 1024 * 1024;

/// One signed request. `Debug` hides credentials.
#[derive(Clone)]
#[non_exhaustive]
pub struct Request {
    /// The method.
    pub method: Method,
    /// `scheme://authority`, with no path.
    pub origin: String,
    /// The request path, already percent-encoded.
    pub path: String,
    /// Query pairs, not encoded.
    pub query: Vec<(String, String)>,
    /// Headers, names in lower case; `host` is among them.
    pub headers: Vec<(String, String)>,
    /// The body of a `PUT`.
    pub body: Option<Arc<[u8]>>,
    /// The largest body of a 2xx response; a longer one is
    /// [`TransportError::TooLarge`]. Other statuses are read up to
    /// [`META_BODY_LIMIT`]; a longer one is [`TransportError::Fatal`].
    pub max_response: u64,
}

const SECRET_HEADERS: [&str; 2] = ["authorization", "x-amz-security-token"];

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(k, v)| {
                let hidden = SECRET_HEADERS.iter().any(|s| k.eq_ignore_ascii_case(s));
                (k.as_str(), if hidden { ".." } else { v.as_str() })
            })
            .collect();
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("origin", &self.origin)
            .field("path", &self.path)
            .field("query", &self.query)
            .field("headers", &headers)
            .field("body_len", &self.body.as_ref().map(|b| b.len()))
            .field("max_response", &self.max_response)
            .finish()
    }
}

impl Request {
    /// A request, for transports that receive one off the wire (such as a
    /// test server in front of the fake).
    pub fn new(
        method: Method,
        origin: impl Into<String>,
        path: impl Into<String>,
        query: Vec<(String, String)>,
        headers: Vec<(String, String)>,
        body: Option<Arc<[u8]>>,
    ) -> Self {
        Request {
            method,
            origin: origin.into(),
            path: path.into(),
            query,
            headers,
            body,
            max_response: META_BODY_LIMIT,
        }
    }

    /// Sets the largest body of a 2xx response.
    pub fn with_max_response(mut self, limit: u64) -> Self {
        self.max_response = limit;
        self
    }

    /// The value of a header, by its lower-case name.
    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
    }

    /// The largest body read for a response of `status`.
    pub fn body_limit(&self, status: u16) -> u64 {
        if (200..300).contains(&status) {
            self.max_response
        } else {
            META_BODY_LIMIT
        }
    }

    /// The full URL, query encoded.
    pub fn url(&self) -> String {
        let mut url = format!("{}{}", self.origin, self.path);
        if !self.query.is_empty() {
            url.push('?');
            let q: Vec<String> = self
                .query
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}={}",
                        crate::sigv4::uri_encode(k, false),
                        crate::sigv4::uri_encode(v, false)
                    )
                })
                .collect();
            url.push_str(&q.join("&"));
        }
        url
    }
}

/// One response.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct Response {
    /// The status code.
    pub status: u16,
    /// Headers, names in lower case.
    pub headers: Vec<(String, String)>,
    /// The body (empty for `HEAD`).
    pub body: Vec<u8>,
}

impl Response {
    /// A response, for implementations of [`Transport`].
    pub fn new(status: u16, headers: Vec<(String, String)>, body: Vec<u8>) -> Self {
        Response {
            status,
            headers,
            body,
        }
    }

    /// The value of a header, by its lower-case name.
    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Why an exchange produced no response.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TransportError {
    /// No complete response in time. The request may have been applied.
    #[error("timed out: {0}")]
    Timeout(String),
    /// The connection failed or was lost, before or during the exchange.
    /// The request may have been applied.
    #[error("connection: {0}")]
    Connection(String),
    /// A fault that repeating will not cure (an invalid URL, a refused
    /// certificate, an error body above [`META_BODY_LIMIT`]).
    #[error("{0}")]
    Fatal(String),
    /// A 2xx body above [`Request::max_response`]; `size` is its declared
    /// length, or a lower bound when it declared none.
    #[error("response body of {size} bytes above the limit of {limit}")]
    TooLarge {
        /// The body's size, or a lower bound.
        size: u64,
        /// The limit.
        limit: u64,
    },
}

/// The error for a body of `size` bytes (or more) above the limit for `status`.
pub(crate) fn body_too_large(request: &Request, status: u16, size: u64) -> TransportError {
    let limit = request.body_limit(status);
    if (200..300).contains(&status) {
        TransportError::TooLarge { size, limit }
    } else {
        TransportError::Fatal(format!(
            "response body of {size} bytes above the limit of {limit}"
        ))
    }
}

/// One HTTP exchange. Called on the sink's worker threads, so it may block.
pub trait Transport: Send + Sync {
    /// Sends a request and reads the whole response.
    fn send(&self, request: &Request) -> Result<Response, TransportError>;
}

/// HTTP/1.1 over TCP, with TLS (rustls, the bundled web PKI roots) for
/// `https` endpoints. Proxies from the environment and redirects are not
/// followed: a signed request goes to its endpoint only.
pub struct HttpTransport {
    agent: ureq::Agent,
}

impl HttpTransport {
    /// A transport with a connect timeout and a timeout for each whole
    /// exchange.
    pub fn new(connect_timeout: Duration, exchange_timeout: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .proxy(None)
            .max_redirects(0)
            .timeout_connect(Some(connect_timeout))
            .timeout_global(Some(exchange_timeout))
            .build();
        HttpTransport {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

impl std::fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpTransport").finish_non_exhaustive()
    }
}

fn map_error(e: ureq::Error) -> TransportError {
    match e {
        ureq::Error::Timeout(t) => TransportError::Timeout(t.to_string()),
        ureq::Error::Io(e) => TransportError::Connection(e.to_string()),
        ureq::Error::ConnectionFailed => TransportError::Connection("connection failed".into()),
        ureq::Error::HostNotFound => TransportError::Connection("host not found".into()),
        ureq::Error::Protocol(e) => TransportError::Connection(e.to_string()),
        other => TransportError::Fatal(other.to_string()),
    }
}

impl Transport for HttpTransport {
    fn send(&self, request: &Request) -> Result<Response, TransportError> {
        let mut builder = ureq::http::Request::builder()
            .method(request.method.as_str())
            .uri(request.url());
        // A `host` header given here stops the client adding its own, so the
        // signed value is the one sent.
        for (k, v) in &request.headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        let result = match &request.body {
            Some(body) => {
                let req = builder
                    .body(&body[..])
                    .map_err(|e| TransportError::Fatal(e.to_string()))?;
                self.agent.run(req)
            }
            None => {
                let req = builder
                    .body(())
                    .map_err(|e| TransportError::Fatal(e.to_string()))?;
                self.agent.run(req)
            }
        };
        let mut response = result.map_err(map_error)?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_ascii_lowercase(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let body = if request.method == Method::Head {
            Vec::new()
        } else {
            let limit = request.body_limit(status);
            let declared = response
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok());
            if let Some(n) = declared.filter(|&n| n > limit) {
                return Err(body_too_large(request, status, n));
            }
            // ureq refuses any read once its limit is spent, even one that would meet the end of a body of exactly
            // `limit` bytes, so it is given one byte more and the limit is checked here.
            let body = response
                .body_mut()
                .with_config()
                .limit(limit.saturating_add(1))
                .read_to_vec()
                .map_err(|e| match e {
                    ureq::Error::BodyExceedsLimit(_) => {
                        body_too_large(request, status, limit.saturating_add(1))
                    }
                    e => map_error(e),
                })?;
            if body.len() as u64 > limit {
                return Err(body_too_large(request, status, body.len() as u64));
            }
            body
        };
        Ok(Response {
            status,
            headers,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_debug_hides_credentials() {
        let r = Request::new(
            Method::Get,
            "http://objects.test",
            "/b/k",
            Vec::new(),
            vec![
                ("host".into(), "objects.test".into()),
                ("authorization".into(), "SIGNATURE-VALUE".into()),
                ("x-amz-security-token".into(), "TOKEN-VALUE".into()),
            ],
            Some(Arc::from(&b"BODY-VALUE"[..])),
        );
        let s = format!("{r:?}");
        assert!(s.contains("objects.test"));
        for secret in ["SIGNATURE-VALUE", "TOKEN-VALUE", "BODY-VALUE"] {
            assert!(!s.contains(secret), "{s}");
        }
    }
}
