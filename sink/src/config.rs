//! [`S3Config`] and its validation.

use std::net::IpAddr;
use std::time::Duration;

use kvik::SinkError;

use crate::retry::RetryPolicy;
use crate::sigv4::Credentials;

/// The largest object one single-part upload may hold: 5 GiB (spec §8 Hashes).
pub const SINGLE_PART_LIMIT: u64 = 5 * 1024 * 1024 * 1024;

/// The default largest object: 256 MiB, four 64 MiB segments.
pub const DEFAULT_MAX_OBJECT_SIZE: u64 = 256 * 1024 * 1024;

/// How the bucket is named in a request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Addressing {
    /// `https://endpoint/bucket/key`.
    #[default]
    Path,
    /// `https://bucket.endpoint/key`; the bucket name must be a DNS name.
    VirtualHost,
}

/// How to reach a bucket, and how hard to try. Checked when a sink is made.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct S3Config {
    /// `https://host[:port]`, no path; `http://` only for a loopback host or
    /// with [`allow_plain_http`](Self::allow_plain_http).
    pub endpoint: String,
    /// The region the signature names.
    pub region: String,
    /// The bucket; it must have versioning or object lock (spec §8 Bucket).
    pub bucket: String,
    /// Access credentials.
    pub credentials: Credentials,
    /// Path-style (the default) or virtual-host addressing.
    pub addressing: Addressing,
    /// Allows `http://` to a host that is not a loopback address.
    pub allow_plain_http: bool,
    /// Connection set-up limit, TLS included. Default 10 s.
    pub connect_timeout: Duration,
    /// Limit on one whole exchange, body included; it must cover
    /// `max_object_size` at the slowest uplink expected. Default 10 minutes:
    /// 256 MiB at about 450 kB/s.
    pub exchange_timeout: Duration,
    /// Retries of transient faults.
    pub retry: RetryPolicy,
    /// Worker threads, that is, calls in flight at once. Default 2.
    pub workers: usize,
    /// Calls waiting for a worker; more fail with `SinkError::Busy`. Dropping
    /// the sink waits for them: at worst `queue × attempts × exchange_timeout`.
    /// Default 64.
    pub queue: usize,
    /// The largest object `put` and `get` handle, held whole in memory;
    /// 1 to [`SINGLE_PART_LIMIT`]. Default [`DEFAULT_MAX_OBJECT_SIZE`].
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
            allow_plain_http: false,
            connect_timeout: Duration::from_secs(10),
            exchange_timeout: Duration::from_secs(600),
            retry: RetryPolicy::default(),
            workers: 2,
            queue: 64,
            max_object_size: DEFAULT_MAX_OBJECT_SIZE,
            list_page: 1000,
        }
    }

    /// Sets the addressing style.
    pub fn with_addressing(mut self, addressing: Addressing) -> Self {
        self.addressing = addressing;
        self
    }

    /// Allows `http://` to any host.
    pub fn with_plain_http(mut self, allow: bool) -> Self {
        self.allow_plain_http = allow;
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

    /// Sets the number of worker threads and the queue behind them.
    pub fn with_workers(mut self, workers: usize, queue: usize) -> Self {
        self.workers = workers;
        self.queue = queue;
        self
    }

    /// Sets the largest object, capped at [`SINGLE_PART_LIMIT`].
    pub fn with_max_object_size(mut self, size: u64) -> Self {
        self.max_object_size = size.min(SINGLE_PART_LIMIT);
        self
    }

    /// Sets the listing page size, clamped to 1–1,000.
    pub fn with_list_page(mut self, keys: u32) -> Self {
        self.list_page = keys.clamp(1, 1000);
        self
    }

    /// Checks every field; returns the endpoint's scheme and authority.
    pub(crate) fn validate(&self) -> Result<(&'static str, String), SinkError> {
        let (scheme, authority) = parse_endpoint(&self.endpoint)?;
        // With virtual-host addressing the host contacted is `bucket.<authority>`,
        // which is not the loopback host judged here.
        let loopback = self.addressing == Addressing::Path && is_loopback(host_of(&authority));
        if scheme == "http" && !self.allow_plain_http && !loopback {
            return Err(failed(format!(
                "plain http to {authority} needs allow_plain_http"
            )));
        }
        if self.region.is_empty()
            || !self
                .region
                .chars()
                .all(|c| c.is_ascii_graphic() && c != '/')
        {
            return Err(failed(format!("invalid region {:?}", self.region)));
        }
        let bucket_ok = match self.addressing {
            Addressing::Path => {
                !self.bucket.is_empty()
                    && self.bucket != "."
                    && self.bucket != ".."
                    && self.bucket.len() <= 255
                    && self
                        .bucket
                        .chars()
                        .all(|c| c.is_ascii_graphic() && !matches!(c, '/' | '?' | '#'))
            }
            Addressing::VirtualHost => is_dns_bucket(&self.bucket),
        };
        if !bucket_ok {
            return Err(failed(format!(
                "invalid bucket name {:?} for {:?} addressing",
                self.bucket, self.addressing
            )));
        }
        let checks = [
            (self.workers >= 1, "workers must be at least 1"),
            (self.queue >= 1, "queue must be at least 1"),
            (
                (1..=1000).contains(&self.list_page),
                "list_page must be 1 to 1,000",
            ),
            (
                (1..=SINGLE_PART_LIMIT).contains(&self.max_object_size),
                "max_object_size must be 1 byte to 5 GiB",
            ),
            (
                self.retry.attempts >= 1,
                "retry.attempts must be at least 1",
            ),
            (
                !self.connect_timeout.is_zero() && !self.exchange_timeout.is_zero(),
                "timeouts must not be zero",
            ),
        ];
        for (ok, why) in checks {
            if !ok {
                return Err(failed(why));
            }
        }
        Ok((scheme, authority))
    }
}

fn failed(reason: impl Into<String>) -> SinkError {
    SinkError::Failed {
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
    if !is_authority(authority) {
        return Err(failed(format!(
            "endpoint {endpoint:?} must be scheme://host[:port] with no path"
        )));
    }
    Ok((scheme, authority.to_ascii_lowercase()))
}

/// `host[:port]`: a name of letters, digits, `.` and `-`, or `[v6]`; a port of
/// 1 to 65535.
fn is_authority(a: &str) -> bool {
    let (host, port) = if let Some(rest) = a.strip_prefix('[') {
        let Some((v6, after)) = rest.split_once(']') else {
            return false;
        };
        let v6_ok = !v6.is_empty()
            && v6
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.');
        if !v6_ok {
            return false;
        }
        match after {
            "" => return true,
            p => (None, p.strip_prefix(':')),
        }
    } else {
        match a.split_once(':') {
            Some((h, p)) => (Some(h), Some(p)),
            None => (Some(a), None),
        }
    };
    let host_ok = host.is_none_or(|h| {
        !h.is_empty()
            && h.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
    });
    let port_ok = match port {
        None => host.is_some(),
        Some(p) => {
            !p.is_empty()
                && p.len() <= 5
                && p.chars().all(|c| c.is_ascii_digit())
                && p.parse::<u32>().is_ok_and(|n| (1..=65535).contains(&n))
        }
    };
    host_ok && port_ok
}

/// The host of `host[:port]` or `[v6][:port]`.
fn host_of(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next().unwrap_or_default();
    }
    authority.split(':').next().unwrap_or_default()
}

fn is_loopback(host: &str) -> bool {
    host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// 3–63 characters of `a–z 0–9 . -`, starting and ending with a letter or
/// digit, with no empty label.
fn is_dns_bucket(b: &str) -> bool {
    let bytes = b.as_bytes();
    (3..=63).contains(&b.len())
        && bytes
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'-'))
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && !b.contains("..")
        && !b.contains(".-")
        && !b.contains("-.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(endpoint: &str) -> S3Config {
        S3Config::new(endpoint, "r-1", "kvik-test", Credentials::new("a", "b"))
    }

    #[test]
    fn endpoints_are_checked() {
        assert_eq!(
            parse_endpoint("https://Objects.test:9000/").unwrap(),
            ("https", "objects.test:9000".to_owned())
        );
        assert!(parse_endpoint("http://[::1]:9000").is_ok());
        assert!(parse_endpoint("http://[::1]").is_ok());
        for bad in [
            "objects.test",
            "https://",
            "https://a/b",
            "ftp://a",
            "https://u@a",
            "https://a_b",
            "https://a:",
            "https://a:0",
            "https://a:65536",
            "https://a:1:2",
            "https://a:x",
            "https://[::1",
            "https://[zz]",
            "https://[::1]x",
            "https://a%20b",
        ] {
            assert!(parse_endpoint(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn plain_http_is_for_loopback_unless_allowed() {
        for ok in [
            "http://127.0.0.1:9000",
            "http://localhost",
            "http://[::1]:9000",
            "https://objects.test",
        ] {
            assert!(config(ok).validate().is_ok(), "{ok}");
        }
        assert!(config("http://objects.test").validate().is_err());
        assert!(config("http://10.0.0.1:9000").validate().is_err());
        assert!(
            config("http://objects.test")
                .with_plain_http(true)
                .validate()
                .is_ok()
        );
        let virtual_host = config("http://127.0.0.1:9000").with_addressing(Addressing::VirtualHost);
        assert!(virtual_host.validate().is_err());
        assert!(virtual_host.with_plain_http(true).validate().is_ok());
    }

    #[test]
    fn public_fields_are_checked_again() {
        let base = config("https://objects.test");
        let mut bad = Vec::new();
        let mut c = base.clone();
        c.list_page = 0;
        bad.push(c);
        let mut c = base.clone();
        c.max_object_size = SINGLE_PART_LIMIT + 1;
        bad.push(c);
        let mut c = base.clone();
        c.max_object_size = 0;
        bad.push(c);
        let mut c = base.clone();
        c.workers = 0;
        bad.push(c);
        let mut c = base.clone();
        c.queue = 0;
        bad.push(c);
        let mut c = base.clone();
        c.retry.attempts = 0;
        bad.push(c);
        let mut c = base.clone();
        c.exchange_timeout = Duration::ZERO;
        bad.push(c);
        let mut c = base.clone();
        c.region = "a b".into();
        bad.push(c);
        for name in [".", ".."] {
            let mut c = base.clone();
            c.bucket = name.into();
            bad.push(c);
        }
        for c in bad {
            assert!(c.validate().is_err(), "{c:?}");
        }
        assert!(base.validate().is_ok());
    }

    #[test]
    fn virtual_host_buckets_are_dns_names() {
        for ok in ["kvik-test", "a.b.c", "abc", "0aa"] {
            assert!(is_dns_bucket(ok), "{ok}");
        }
        for bad in [
            "ab", "Kvik", "a_b", "-ab", "ab-", "a..b", "a.-b", "a b", "a/b", "a:b",
        ] {
            assert!(!is_dns_bucket(bad), "{bad}");
        }
        let c = config("https://objects.test").with_addressing(Addressing::VirtualHost);
        let mut bad = c.clone();
        bad.bucket = "evil.test/x".into();
        assert!(bad.validate().is_err());
        assert!(c.validate().is_ok());
    }
}
