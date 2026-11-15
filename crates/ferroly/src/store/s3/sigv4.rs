//! AWS Signature Version 4 request signing (header-based).
//!
//! The four steps of the [AWS SigV4 process] on a [`Request`]:
//!
//! 1. the **canonical request** — method, URI path, sorted and encoded query,
//!    the lower-cased signed headers with trimmed values, the signed-header
//!    list and the payload hash;
//! 2. the **string to sign** — `AWS4-HMAC-SHA256`, the timestamp, the
//!    credential scope `<date>/<region>/<service>/aws4_request` and the
//!    SHA-256 of the canonical request;
//! 3. the **signing key** — HMAC-SHA256 chained over the date, region,
//!    service and `aws4_request`, starting from `"AWS4" + secret`;
//! 4. the **signature** — the hex HMAC of the string to sign, sent in the
//!    `Authorization` header.
//!
//! The URI path is encoded once, the S3 rule ([`canonical_path`]: each
//! segment decoded and re-encoded with only the unreserved characters kept,
//! so a caller's raw `+` or lower-case `%2f` signs the way the server will
//! see it); other AWS services encode it a second time, which this signer
//! does not do.
//!
//! [AWS SigV4 process]: https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use ferroly::hash::{self, hmac_sha256, Secret};
use ferroly::http::{percent_decode, percent_encode, Request};

use crate::store::StoreError;

/// The signing algorithm name.
pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// The `x-amz-content-sha256` value for a payload that is not signed.
pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

/// The SHA-256 of an empty payload.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Headers never included in the signature: hop-by-hop headers, and ones a
/// client or proxy may add or rewrite after signing.
const UNSIGNED_HEADERS: &[&str] = &[
    "authorization",
    "connection",
    "expect",
    "keep-alive",
    "proxy-authorization",
    "te",
    "transfer-encoding",
    "upgrade",
    "user-agent",
    "x-amzn-trace-id",
];

/// AWS credentials: an access key id, a secret access key and, for
/// temporary credentials, a session token.
///
/// The secret and the token are held in [`Secret`]s, and `Debug` shows only
/// the first four characters of the key id.
///
/// ```
/// use ferroly::store::s3::Credentials;
///
/// let creds = Credentials::new("AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY")
///     .with_session_token("FwoGZXIvYXdzEXAMPLE");
/// let shown = format!("{creds:?}");
/// assert!(!shown.contains("wJalr") && !shown.contains("FwoGZ") && !shown.contains("IOSFODNN"));
/// ```
#[derive(Clone)]
pub struct Credentials {
    access_key_id: String,
    secret_access_key: Secret,
    session_token: Option<Secret>,
}

impl Credentials {
    /// Long-term credentials.
    pub fn new(access_key_id: impl Into<String>, secret_access_key: impl Into<String>) -> Self {
        Credentials {
            access_key_id: access_key_id.into(),
            secret_access_key: Secret::from(secret_access_key.into()),
            session_token: None,
        }
    }

    /// Adds the session token of temporary (STS) credentials; it is sent as
    /// `x-amz-security-token`.
    pub fn with_session_token(mut self, token: impl Into<String>) -> Self {
        self.session_token = Some(Secret::from(token.into()));
        self
    }

    /// Reads `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and the optional
    /// `AWS_SESSION_TOKEN`. `None` unless both keys are set and non-empty.
    pub fn from_env() -> Option<Credentials> {
        Credentials::from_lookup(crate::store::env_lookup)
    }

    /// [`from_env`](Credentials::from_env), reading variables through
    /// `lookup` (for tests, or another configuration source).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Option<Credentials> {
        let id = lookup("AWS_ACCESS_KEY_ID").filter(|v| !v.is_empty())?;
        let secret = lookup("AWS_SECRET_ACCESS_KEY").filter(|v| !v.is_empty())?;
        let mut creds = Credentials::new(id, secret);
        if let Some(token) = lookup("AWS_SESSION_TOKEN").filter(|v| !v.is_empty()) {
            creds = creds.with_session_token(token);
        }
        Some(creds)
    }

    /// The access key id.
    pub fn access_key_id(&self) -> &str {
        &self.access_key_id
    }

    /// Whether a session token is set.
    pub fn has_session_token(&self) -> bool {
        self.session_token.is_some()
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let shown: String = self.access_key_id.chars().take(4).collect();
        f.debug_struct("Credentials")
            .field("access_key_id", &format_args!("{shown}…"))
            .field("secret_access_key", &self.secret_access_key)
            .field("session_token", &self.session_token)
            .finish()
    }
}

/// The value of `x-amz-content-sha256`: the payload's SHA-256, or
/// `UNSIGNED-PAYLOAD`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PayloadHash {
    /// `UNSIGNED-PAYLOAD`: the body is not covered by the signature (allowed
    /// for S3 over HTTPS; common for downloads).
    Unsigned,
    /// The lower-case hex SHA-256 of the body.
    Sha256(String),
}

impl PayloadHash {
    /// The hash of an empty body (what a `GET` or `HEAD` sends).
    pub fn empty() -> PayloadHash {
        PayloadHash::Sha256(EMPTY_SHA256.to_string())
    }

    /// The hash of `body`.
    pub fn of(body: &[u8]) -> PayloadHash {
        PayloadHash::Sha256(hash::sha256(body).to_hex())
    }

    /// The header value.
    pub fn as_str(&self) -> &str {
        match self {
            PayloadHash::Unsigned => UNSIGNED_PAYLOAD,
            PayloadHash::Sha256(h) => h,
        }
    }
}

/// The intermediate values and the result of signing one request, for
/// inspection and debugging.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Signature {
    /// Step 1: the canonical request.
    pub canonical_request: String,
    /// Step 2: the string to sign.
    pub string_to_sign: String,
    /// The `;`-separated signed header names.
    pub signed_headers: String,
    /// Step 4: the hex signature.
    pub signature: String,
    /// The full `Authorization` header value.
    pub authorization: String,
}

/// Derives the SigV4 signing key for one day, region and service (step 3).
///
/// `date` is `YYYYMMDD`.
///
/// The intermediate keys (`kDate`, `kRegion`, `kService`) are overwritten
/// with zeros before this returns (best-effort, like
/// [`Secret`]). The returned key is derived from the
/// secret access key and valid for a whole day: wipe your copy when done
/// with it ([`sign`] does).
pub fn signing_key(secret_access_key: &[u8], date: &str, region: &str, service: &str) -> [u8; 32] {
    let mut start = Vec::with_capacity(4 + secret_access_key.len());
    start.extend_from_slice(b"AWS4");
    start.extend_from_slice(secret_access_key);
    let start = Secret::from(start);
    let mut k_date = hmac_sha256(start.expose(), date.as_bytes());
    let mut k_region = hmac_sha256(k_date.as_bytes(), region.as_bytes());
    crate::hash::wipe(k_date.as_mut());
    let mut k_service = hmac_sha256(k_region.as_bytes(), service.as_bytes());
    crate::hash::wipe(k_region.as_mut());
    let key = hmac_sha256(k_service.as_bytes(), b"aws4_request").into_bytes();
    crate::hash::wipe(k_service.as_mut());
    key
}

/// Formats `time` as the SigV4 timestamp `YYYYMMDD'T'HHMMSS'Z'` (UTC).
pub fn amz_datetime(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Signs `req` in place for `service` in `region` at `time`.
///
/// Sets `x-amz-date`, `x-amz-content-sha256` (from `payload`),
/// `x-amz-security-token` (for temporary credentials) and `Authorization`.
/// Every header already on the request is signed, except hop-by-hop
/// headers and `User-Agent`; `host` is always signed (taken from a `Host`
/// header if one is set, otherwise from the URL, exactly as the client will
/// send it). Add every header you want covered — `Range` included —
/// **before** signing, and do not change the request afterwards.
pub fn sign(
    req: &mut Request,
    credentials: &Credentials,
    region: &str,
    service: &str,
    time: SystemTime,
    payload: &PayloadHash,
) -> Result<Signature, StoreError> {
    for (what, value) in [("region", region), ("service", service)] {
        if value.is_empty()
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(StoreError::Config(format!("invalid SigV4 {what}")));
        }
    }
    let datetime = amz_datetime(time);
    let date = &datetime[..8];
    req.headers.remove("authorization");
    req.headers.set("x-amz-date", datetime.clone());
    req.headers.set("x-amz-content-sha256", payload.as_str());
    match &credentials.session_token {
        Some(token) => req.headers.set(
            "x-amz-security-token",
            token.expose_str().unwrap_or_default(),
        ),
        None => {
            req.headers.remove("x-amz-security-token");
        }
    }

    let (canonical_headers, signed_headers) = canonical_headers(req);
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        req.method.as_str(),
        canonical_path(&req.uri.path)?,
        canonical_query(req.uri.query.as_deref())?,
        canonical_headers,
        signed_headers,
        payload.as_str()
    );
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "{ALGORITHM}\n{datetime}\n{scope}\n{}",
        hash::sha256(canonical_request.as_bytes()).to_hex()
    );
    let mut key = signing_key(
        credentials.secret_access_key.expose(),
        date,
        region,
        service,
    );
    let signature = hmac_sha256(&key, string_to_sign.as_bytes()).to_hex();
    crate::hash::wipe(&mut key);
    let authorization = format!(
        "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key_id
    );
    req.headers.set("authorization", authorization.clone());
    Ok(Signature {
        canonical_request,
        string_to_sign,
        signed_headers,
        signature,
        authorization,
    })
}

/// The canonical-headers block (each `name:value\n`) and the signed-header
/// list.
fn canonical_headers(req: &Request) -> (String, String) {
    let mut entries: Vec<(String, String)> = Vec::new();
    if !req.headers.contains("host") {
        entries.push(("host".into(), req.uri.authority()));
    }
    for (name, value) in req.headers.iter() {
        let name = name.to_ascii_lowercase();
        if UNSIGNED_HEADERS.contains(&name.as_str()) {
            continue;
        }
        let value = collapse_spaces(value);
        match entries.iter_mut().find(|(n, _)| *n == name) {
            Some((_, existing)) => {
                existing.push(',');
                existing.push_str(&value);
            }
            None => entries.push((name, value)),
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut block = String::new();
    for (name, value) in &entries {
        block.push_str(name);
        block.push(':');
        block.push_str(value);
        block.push('\n');
    }
    let signed = entries
        .iter()
        .map(|(n, _)| n.as_str())
        .collect::<Vec<_>>()
        .join(";");
    (block, signed)
}

/// Trims a header value and collapses runs of spaces to one.
fn collapse_spaces(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for word in value.split([' ', '\t']).filter(|w| !w.is_empty()) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// The canonical URI for S3: each `/`-separated segment of the request path
/// percent-decoded once and re-encoded with only the RFC 3986 unreserved
/// characters kept (upper-case hex), as S3 and S3-compatible servers
/// rebuild it from the decoded key. A path already encoded that way is
/// unchanged; raw sub-delimiters (`+`, `!`, `$`, …), `:` / `@` and
/// lower-case escapes are normalised, so the signature matches what the
/// server computes. An empty path is `/`. Not double-encoded, so this is the
/// S3 rule only.
///
/// ```
/// use ferroly::store::s3::sigv4::canonical_path;
///
/// assert_eq!(canonical_path("/b/a+b%2fc!").unwrap(), "/b/a%2Bb%2Fc%21");
/// assert_eq!(canonical_path("").unwrap(), "/");
/// ```
pub fn canonical_path(path: &str) -> Result<String, StoreError> {
    if path.is_empty() {
        return Ok("/".into());
    }
    let mut out = String::with_capacity(path.len() + 8);
    for (i, segment) in path.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        let raw = percent_decode(segment)
            .map_err(|_| StoreError::Config("path is not valid percent-encoding".into()))?;
        for b in raw {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                out.push(char::from(b));
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    Ok(out)
}

/// The canonical query string: every name and value URI-encoded (RFC 3986
/// unreserved characters kept), sorted by name then value, `name=value`
/// joined with `&` (a parameter without a value is `name=`).
pub fn canonical_query(query: Option<&str>) -> Result<String, StoreError> {
    let Some(query) = query.filter(|q| !q.is_empty()) else {
        return Ok(String::new());
    };
    let decode = |s: &str| {
        percent_decode(s)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .ok_or_else(|| StoreError::Config("query is not valid percent-encoded UTF-8".into()))
    };
    let mut pairs = Vec::new();
    for part in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once('=').unwrap_or((part, ""));
        pairs.push((percent_encode(&decode(k)?), percent_encode(&decode(v)?)));
    }
    pairs.sort();
    Ok(pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datetime_formatting() {
        assert_eq!(amz_datetime(UNIX_EPOCH), "19700101T000000Z");
        // 2013-05-24T00:00:00Z, the date of the AWS S3 signing examples.
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_369_353_600);
        assert_eq!(amz_datetime(t), "20130524T000000Z");
        // A leap day.
        let t = UNIX_EPOCH + std::time::Duration::from_secs(951_782_400 + 3_661);
        assert_eq!(amz_datetime(t), "20000229T010101Z");
    }

    #[test]
    fn query_canonicalisation() {
        assert_eq!(canonical_query(None).unwrap(), "");
        assert_eq!(
            canonical_query(Some("prefix=J&max-keys=2")).unwrap(),
            "max-keys=2&prefix=J"
        );
        assert_eq!(canonical_query(Some("lifecycle")).unwrap(), "lifecycle=");
        assert_eq!(
            canonical_query(Some("a=x%2fy&b=%7E")).unwrap(),
            "a=x%2Fy&b=~"
        );
        assert!(canonical_query(Some("a=%zz")).is_err());
    }

    #[test]
    fn path_canonicalisation() {
        assert_eq!(canonical_path("/").unwrap(), "/");
        assert_eq!(canonical_path("").unwrap(), "/");
        assert_eq!(
            canonical_path("/test%24file.text").unwrap(),
            "/test%24file.text"
        );
        // ST-16: raw sub-delimiters, `:`/`@`, `+` and lower-case escapes.
        assert_eq!(
            canonical_path("/b/a+b!$&'()*,;=:@%2fx%7e").unwrap(),
            "/b/a%2Bb%21%24%26%27%28%29%2A%2C%3B%3D%3A%40%2Fx~"
        );
        assert_eq!(canonical_path("/a//b/").unwrap(), "/a//b/");
        assert_eq!(canonical_path("/%E2%82%AC%ff").unwrap(), "/%E2%82%AC%FF");
        assert!(canonical_path("/bad%zz").is_err());

        // Two spellings of one key sign the same.
        let creds = Credentials::new("AKID", "SECRET");
        let sig = |url: &str| {
            let mut req = Request::builder(ferroly::http::Method::Get, url)
                .unwrap()
                .build();
            sign(
                &mut req,
                &creds,
                "us-east-1",
                "s3",
                UNIX_EPOCH,
                &PayloadHash::empty(),
            )
            .unwrap()
            .signature
        };
        assert_eq!(
            sig("https://h.example/b/a+b%2fc"),
            sig("https://h.example/b/a%2Bb%2Fc")
        );
    }

    #[test]
    fn header_values_are_collapsed() {
        assert_eq!(collapse_spaces("  a   b \t c  "), "a b c");
    }
}
