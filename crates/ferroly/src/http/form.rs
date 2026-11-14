//! URL and form encoding helpers: percent-encoding, request-path
//! normalisation, `application/x-www-form-urlencoded` bodies and a
//! `multipart/form-data` builder.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use super::core::remove_dot_segments;
use super::HttpError;

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

/// Percent-encodes `input` for use as one URL component (a path segment, a
/// query key or a query value): every byte except the RFC 3986 unreserved set
/// (`A-Z a-z 0-9 - . _ ~`) becomes `%XX`. So `/`, `?`, `#`, `%`, spaces and
/// CR/LF can never change the structure of the URL the value is placed in.
///
/// ```
/// use ferroly::http::percent_encode;
///
/// assert_eq!(percent_encode("a b/c?d#e"), "a%20b%2Fc%3Fd%23e");
/// assert_eq!(percent_encode("../x\r\n"), "..%2Fx%0D%0A");
/// ```
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        if is_unreserved(b) {
            out.push(b as char);
        } else {
            push_escape(&mut out, b);
        }
    }
    out
}

fn push_escape(out: &mut String, b: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push('%');
    out.push(HEX[usize::from(b >> 4)] as char);
    out.push(HEX[usize::from(b & 0x0f)] as char);
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decodes `%XX` escapes. A `%` that is not followed by two hex digits is an
/// error (never passed through), so a decoded value cannot hide a second
/// layer of encoding behind a malformed escape. `+` is left as it is.
///
/// ```
/// use ferroly::http::percent_decode;
///
/// assert_eq!(percent_decode("a%20b%2Fc").unwrap(), b"a b/c");
/// assert!(percent_decode("100%").is_err());
/// ```
pub fn percent_decode(input: &str) -> Result<Vec<u8>, HttpError> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = bytes.get(i + 1).copied().and_then(hex_value);
            let lo = bytes.get(i + 2).copied().and_then(hex_value);
            match (hi, lo) {
                (Some(hi), Some(lo)) => out.push(hi << 4 | lo),
                _ => return Err(HttpError::InvalidUrl("malformed percent-escape".into())),
            }
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(out)
}

/// Normalises the path of a request target so that routing, authorisation and
/// handlers all see the same thing.
///
/// - the path must begin with `/`;
/// - a malformed percent-escape is an error;
/// - escapes of unreserved characters are decoded (`%41` → `A`, `%2e` → `.`)
///   and the remaining escapes are upper-cased (`%2f` → `%2F`), so `%2F` stays
///   distinct from a real `/`;
/// - an escape that decodes to a control character (including NUL) is an
///   error;
/// - `.` and `..` segments are removed (after the decoding above, so `%2e%2e`
///   cannot slip through); `..` never climbs above the root;
/// - an empty segment (`//`) is an error, except for one trailing `/`.
///
/// ```
/// use ferroly::http::normalize_path;
///
/// assert_eq!(normalize_path("/a/./b/../c").unwrap(), "/a/c");
/// assert_eq!(normalize_path("/%61pi/%2e%2e/x%2fy").unwrap(), "/x%2Fy");
/// assert!(normalize_path("//api/secret").is_err());
/// assert!(normalize_path("api/secret").is_err());
/// ```
pub fn normalize_path(path: &str) -> Result<String, HttpError> {
    let bad = |what: &str| HttpError::InvalidUrl(format!("invalid request path: {what}"));
    if !path.starts_with('/') {
        return Err(bad("must start with '/'"));
    }
    let bytes = path.as_bytes();
    let mut decoded = String::with_capacity(path.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            let hi = bytes.get(i + 1).copied().and_then(hex_value);
            let lo = bytes.get(i + 2).copied().and_then(hex_value);
            let (Some(hi), Some(lo)) = (hi, lo) else {
                return Err(bad("malformed percent-escape"));
            };
            let value = hi << 4 | lo;
            if value < 0x20 || value == 0x7f {
                return Err(bad("encoded control character"));
            }
            if is_unreserved(value) {
                decoded.push(value as char);
            } else {
                push_escape(&mut decoded, value);
            }
            i += 3;
        } else if b <= 0x20 || b >= 0x7f || b == b'?' || b == b'#' {
            return Err(bad("unexpected character"));
        } else {
            decoded.push(b as char);
            i += 1;
        }
    }
    let normalized = remove_dot_segments(&decoded);
    // After dot-segment removal an empty segment can only come from `//`.
    let inner = normalized.strip_suffix('/').unwrap_or(&normalized);
    if inner.contains("//") || normalized.ends_with("//") {
        return Err(bad("empty path segment"));
    }
    Ok(normalized)
}

/// Encodes `(name, value)` pairs as `application/x-www-form-urlencoded`:
/// pairs joined by `&`, a space written as `+`, and everything outside the
/// unreserved set percent-encoded.
///
/// ```
/// use ferroly::http::form_urlencode;
///
/// assert_eq!(form_urlencode([("q", "a b&c"), ("lang", "en")]), "q=a+b%26c&lang=en");
/// ```
pub fn form_urlencode<I, K, V>(pairs: I) -> String
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut out = String::new();
    for (k, v) in pairs {
        if !out.is_empty() {
            out.push('&');
        }
        push_form_component(&mut out, k.as_ref());
        out.push('=');
        push_form_component(&mut out, v.as_ref());
    }
    out
}

fn push_form_component(out: &mut String, s: &str) {
    for b in s.bytes() {
        if b == b' ' {
            out.push('+');
        } else if is_unreserved(b) {
            out.push(b as char);
        } else {
            push_escape(out, b);
        }
    }
}

/// A `multipart/form-data` body builder (RFC 7578).
///
/// ```
/// use ferroly::http::{Method, Multipart, Request};
///
/// # fn ex() -> Result<(), ferroly::http::HttpError> {
/// let form = Multipart::new()
///     .text("title", "Quarterly report")
///     .file("report", "q3.csv", "text/csv", b"a,b\n1,2\n".to_vec());
/// let req = Request::builder(Method::Post, "https://example.com/upload")?
///     .multipart(&form)
///     .build();
/// assert!(req.headers.get("content-type").unwrap().starts_with("multipart/form-data; boundary="));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct Multipart {
    boundary: String,
    parts: Vec<Part>,
}

#[derive(Debug, Clone)]
struct Part {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    data: Vec<u8>,
}

impl Default for Multipart {
    fn default() -> Self {
        Self::new()
    }
}

impl Multipart {
    /// An empty form with a fresh random boundary.
    pub fn new() -> Self {
        Self {
            boundary: random_boundary(),
            parts: Vec::new(),
        }
    }

    /// Adds a plain text field.
    pub fn text(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.parts.push(Part {
            name: name.into(),
            filename: None,
            content_type: None,
            data: value.into().into_bytes(),
        });
        self
    }

    /// Adds a file field with a file name and content type.
    pub fn file(
        mut self,
        name: impl Into<String>,
        filename: impl Into<String>,
        content_type: impl Into<String>,
        data: impl Into<Vec<u8>>,
    ) -> Self {
        self.parts.push(Part {
            name: name.into(),
            filename: Some(filename.into()),
            content_type: Some(content_type.into()),
            data: data.into(),
        });
        self
    }

    /// The boundary string separating the parts.
    pub fn boundary(&self) -> &str {
        &self.boundary
    }

    /// The `Content-Type` header value for this form, including the boundary.
    pub fn content_type(&self) -> String {
        format!("multipart/form-data; boundary={}", self.boundary)
    }

    /// Encodes the form as a request body.
    ///
    /// Field and file names are escaped (`"` → `%22`, CR → `%0D`, LF → `%0A`)
    /// and control characters are stripped from part content types, so no
    /// field can inject a part header.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for part in &self.parts {
            out.extend_from_slice(b"--");
            out.extend_from_slice(self.boundary.as_bytes());
            out.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"");
            out.extend_from_slice(escape_quoted(&part.name).as_bytes());
            out.push(b'"');
            if let Some(filename) = &part.filename {
                out.extend_from_slice(b"; filename=\"");
                out.extend_from_slice(escape_quoted(filename).as_bytes());
                out.push(b'"');
            }
            out.extend_from_slice(b"\r\n");
            if let Some(ct) = &part.content_type {
                let ct: String = ct.chars().filter(|c| !c.is_control()).collect();
                out.extend_from_slice(b"Content-Type: ");
                out.extend_from_slice(ct.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            out.extend_from_slice(b"\r\n");
            out.extend_from_slice(&part.data);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"--");
        out.extend_from_slice(self.boundary.as_bytes());
        out.extend_from_slice(b"--\r\n");
        out
    }
}

/// Escapes a value placed inside a quoted `Content-Disposition` parameter.
fn escape_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("%22"),
            '\r' => out.push_str("%0D"),
            '\n' => out.push_str("%0A"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// A boundary that is unpredictable and vanishingly unlikely to occur in the
/// payload: 128 bits drawn from the standard library's randomly keyed hasher.
fn random_boundary() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut out = String::from("ferroly-");
    for _ in 0..2 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_round_trip() {
        let all: String = (1u8..=127).map(|b| b as char).collect();
        let enc = percent_encode(&all);
        assert!(enc.bytes().all(|b| is_unreserved(b) || b == b'%'));
        assert_eq!(percent_decode(&enc).unwrap(), all.as_bytes());
        assert_eq!(percent_encode("caf\u{e9}"), "caf%C3%A9");
        assert_eq!(percent_decode("%c3%A9").unwrap(), "\u{e9}".as_bytes());
        for bad in ["%", "%1", "%zz", "a%2"] {
            assert!(percent_decode(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn normalizes_paths() {
        let ok = |p: &str| normalize_path(p).unwrap();
        assert_eq!(ok("/"), "/");
        assert_eq!(ok("/a/b"), "/a/b");
        assert_eq!(ok("/a/b/"), "/a/b/");
        assert_eq!(ok("/a/./b"), "/a/b");
        assert_eq!(ok("/a/../b"), "/b");
        assert_eq!(ok("/../../b"), "/b");
        assert_eq!(ok("/a/%2e%2E/b"), "/b");
        assert_eq!(ok("/%41%7e"), "/A~");
        assert_eq!(ok("/a%2fb"), "/a%2Fb");
        assert_eq!(ok("/a%20b"), "/a%20b");
        assert_eq!(ok("/a/.."), "/");
        for bad in [
            "",
            "a/b",
            "//a",
            "/a//b",
            "/a//",
            "/a/%",
            "/a/%0",
            "/a/%00",
            "/a/%0d%0a",
            "/a b",
            "/a?b",
            "/a#b",
            "/a\u{e9}",
            "/a/..//b",
        ] {
            assert!(normalize_path(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn form_encoding() {
        assert_eq!(
            form_urlencode([("a b", "c&d=e"), ("x", "\u{e9}+~")]),
            "a+b=c%26d%3De&x=%C3%A9%2B~"
        );
        assert_eq!(form_urlencode(Vec::<(String, String)>::new()), "");
    }

    #[test]
    fn multipart_encoding() {
        let form = Multipart::new().text("field\"\r\nX: y", "value").file(
            "f",
            "a\"b.txt",
            "text/plain\r\nX: y",
            b"DATA".to_vec(),
        );
        let b = form.boundary().to_string();
        assert!(b.starts_with("ferroly-") && b.len() == 40);
        assert_ne!(Multipart::new().boundary(), b);
        let body = String::from_utf8(form.encode()).unwrap();
        let expect = format!(
            "--{b}\r\nContent-Disposition: form-data; name=\"field%22%0D%0AX: y\"\r\n\r\nvalue\r\n\
             --{b}\r\nContent-Disposition: form-data; name=\"f\"; filename=\"a%22b.txt\"\r\n\
             Content-Type: text/plainX: y\r\n\r\nDATA\r\n--{b}--\r\n"
        );
        assert_eq!(body, expect);
        assert_eq!(
            form.content_type(),
            format!("multipart/form-data; boundary={b}")
        );
    }
}
