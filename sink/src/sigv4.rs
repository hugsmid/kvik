//! Request signing for S3-compatible object storage: Signature Version 4 with
//! the signature in the `Authorization` header, for the service name `s3`.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

/// The signing algorithm's name, as the protocol spells it.
pub(crate) const ALGORITHM: &str = "AWS4-HMAC-SHA256";
/// The hex SHA-256 of no bytes: the payload hash of a request without a body.
pub(crate) const EMPTY_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Access credentials for a bucket. `Debug` shows the access key id only.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Credentials {
    /// The access key id.
    pub access_key_id: String,
    /// The secret access key.
    pub secret_access_key: String,
    /// A session token for temporary credentials, sent as
    /// `x-amz-security-token`.
    pub session_token: Option<String>,
}

impl Credentials {
    /// Long-term credentials.
    pub fn new(access_key_id: impl Into<String>, secret_access_key: impl Into<String>) -> Self {
        Credentials {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
            session_token: None,
        }
    }

    /// Adds a session token (temporary credentials).
    pub fn with_session_token(mut self, token: impl Into<String>) -> Self {
        self.session_token = Some(token.into());
        self
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"..")
            .field("session_token", &self.session_token.as_ref().map(|_| ".."))
            .finish()
    }
}

/// `YYYYMMDDTHHMMSSZ` in UTC, the form of `x-amz-date`.
pub(crate) fn amz_date(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

/// The proleptic Gregorian date of a day count from 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Percent-encodes everything but the unreserved characters
/// (`A–Z a–z 0–9 - . _ ~`), and `/` too unless `keep_slash`.
pub(crate) fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
        if unreserved || (keep_slash && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Lower-case hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// The canonical query string: pairs encoded, then sorted by name and value.
pub(crate) fn canonical_query(pairs: &[(String, String)]) -> String {
    let mut enc: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    enc.sort();
    enc.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// What a signature covers. `path` is already encoded (the request line's
/// path); `query` holds the raw pairs; `headers` are the signed headers,
/// `host` included.
pub(crate) struct ToSign<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a [(String, String)],
    pub headers: &'a [(String, String)],
    pub payload_sha256: &'a str,
    pub amz_date: &'a str,
    pub region: &'a str,
}

fn canonical_headers(headers: &[(String, String)]) -> (String, String) {
    let mut h: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| {
            let value = v.split_whitespace().collect::<Vec<_>>().join(" ");
            (k.to_ascii_lowercase(), value)
        })
        .collect();
    h.sort();
    let canonical = h.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed = h
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    (canonical, signed)
}

/// The canonical request (the first step of signing).
pub(crate) fn canonical_request(r: &ToSign<'_>) -> String {
    let (headers, signed) = canonical_headers(r.headers);
    format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        r.method,
        r.path,
        canonical_query(r.query),
        headers,
        signed,
        r.payload_sha256
    )
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes any key");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// The `Authorization` header value for a request.
pub(crate) fn authorization(r: &ToSign<'_>, creds: &Credentials) -> String {
    let date = &r.amz_date[..8];
    let scope = format!("{date}/{}/s3/aws4_request", r.region);
    let creq = canonical_request(r);
    let string_to_sign = format!(
        "{ALGORITHM}\n{}\n{scope}\n{}",
        r.amz_date,
        hex(&Sha256::digest(creq.as_bytes()))
    );
    let k_date = hmac(
        format!("AWS4{}", creds.secret_access_key).as_bytes(),
        date.as_bytes(),
    );
    let k_region = hmac(&k_date, r.region.as_bytes());
    let k_service = hmac(&k_region, b"s3");
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex(&hmac(&k_signing, string_to_sign.as_bytes()));
    let (_, signed) = canonical_headers(r.headers);
    format!(
        "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
        creds.access_key_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn creds() -> Credentials {
        Credentials::new("TESTKEYID0000000", "test/secret+key/0000000000000000000000")
    }

    // Expected values from a separate implementation of the algorithm, not
    // this code; placeholder credentials, reserved host name.

    #[test]
    fn signs_a_conditional_put_with_a_checksum() {
        let headers = h(&[
            ("host", "objects.test:9000"),
            ("if-none-match", "*"),
            (
                "x-amz-checksum-sha256",
                "LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=",
            ),
            (
                "x-amz-content-sha256",
                "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            ),
            ("x-amz-date", "20261115T120000Z"),
        ]);
        let r = ToSign {
            method: "PUT",
            path: "/kvik-test/log/00000000-0000000000000001.kvl",
            query: &[],
            headers: &headers,
            payload_sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            amz_date: "20261115T120000Z",
            region: "test-region-1",
        };
        assert_eq!(
            authorization(&r, &creds()),
            "AWS4-HMAC-SHA256 Credential=TESTKEYID0000000/20261115/test-region-1/s3/aws4_request, \
             SignedHeaders=host;if-none-match;x-amz-checksum-sha256;x-amz-content-sha256;x-amz-date, \
             Signature=a1b887d4af8c61b3f68531dc177bf58d71a4f68fb94d31341c7fba8d7d70e9b1"
        );
    }

    #[test]
    fn signs_a_ranged_get() {
        let headers = h(&[
            ("host", "objects.test"),
            ("range", "bytes=10-19"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", "20261115T120000Z"),
        ]);
        let r = ToSign {
            method: "GET",
            path: "/kvik-test/a%20b/c%2Bd~e",
            query: &[],
            headers: &headers,
            payload_sha256: EMPTY_SHA256,
            amz_date: "20261115T120000Z",
            region: "test-region-1",
        };
        assert_eq!(
            authorization(&r, &creds()),
            "AWS4-HMAC-SHA256 Credential=TESTKEYID0000000/20261115/test-region-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=35270cdffa78b25f8653198f270c2dec9a0df90657bf127506f66893296dc092"
        );
    }

    #[test]
    fn signs_a_listing_with_a_session_token() {
        let headers = h(&[
            ("host", "kvik-test.objects.test"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", "20261115T235959Z"),
            ("x-amz-security-token", "token/with+chars="),
        ]);
        let query = h(&[
            ("list-type", "2"),
            ("prefix", "manifest/00000001/"),
            ("start-after", "manifest/00000001/0000000000000007"),
            ("max-keys", "1000"),
        ]);
        let r = ToSign {
            method: "GET",
            path: "/",
            query: &query,
            headers: &headers,
            payload_sha256: EMPTY_SHA256,
            amz_date: "20261115T235959Z",
            region: "test-region-1",
        };
        assert_eq!(
            canonical_query(&query),
            "list-type=2&max-keys=1000&prefix=manifest%2F00000001%2F\
             &start-after=manifest%2F00000001%2F0000000000000007"
        );
        assert_eq!(
            authorization(&r, &creds()),
            "AWS4-HMAC-SHA256 Credential=TESTKEYID0000000/20261115/test-region-1/s3/aws4_request, \
             SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token, \
             Signature=5482759f0a898f1f3299030bcede0361fa2ddaf26f437924b3f95cb860994546"
        );
    }

    #[test]
    fn dates_are_formatted_in_utc() {
        let t = UNIX_EPOCH + Duration::from_secs(1_794_744_000); // 2026-11-15 12:00:00 UTC
        assert_eq!(amz_date(t), "20261115T120000Z");
        assert_eq!(amz_date(UNIX_EPOCH), "19700101T000000Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400 + 86_399); // 2000-02-29 23:59:59
        assert_eq!(amz_date(leap), "20000229T235959Z");
    }

    #[test]
    fn encoding_keeps_only_unreserved_characters() {
        assert_eq!(uri_encode("a b/c+d~e_f.g-h", true), "a%20b/c%2Bd~e_f.g-h");
        assert_eq!(uri_encode("a/b", false), "a%2Fb");
        assert_eq!(uri_encode("ø", true), "%C3%B8");
    }

    #[test]
    fn credentials_hide_the_secret() {
        let c = creds().with_session_token("tok");
        let s = format!("{c:?}");
        assert!(s.contains("TESTKEYID0000000"));
        assert!(!s.contains("test/secret"));
        assert!(!s.contains("\"tok\""));
    }
}
