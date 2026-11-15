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

/// One signed request.
#[derive(Clone, Debug)]
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
        }
    }

    /// The value of a header, by its lower-case name.
    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
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
    /// certificate, a body above the read limit).
    #[error("{0}")]
    Fatal(String),
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
    max_body: u64,
}

impl HttpTransport {
    /// A transport with a connect timeout and a timeout for each whole
    /// exchange, reading bodies of at most `max_body` bytes.
    pub fn new(connect_timeout: Duration, exchange_timeout: Duration, max_body: u64) -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .proxy(None)
            .max_redirects(0)
            .timeout_connect(Some(connect_timeout))
            .timeout_global(Some(exchange_timeout))
            .build();
        HttpTransport {
            agent: ureq::Agent::new_with_config(config),
            max_body,
        }
    }
}

impl std::fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpTransport")
            .field("max_body", &self.max_body)
            .finish()
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
        // `host` is sent as signed; the client then adds none of its own.
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
            response
                .body_mut()
                .with_config()
                .limit(self.max_body)
                .read_to_vec()
                .map_err(map_error)?
        };
        Ok(Response {
            status,
            headers,
            body,
        })
    }
}
