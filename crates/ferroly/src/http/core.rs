//! Core HTTP value types: [`Method`], [`StatusCode`], [`HeaderMap`], [`Uri`].

use super::HttpError;

/// An HTTP request method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// GET.
    Get,
    /// POST.
    Post,
    /// PUT.
    Put,
    /// DELETE.
    Delete,
    /// PATCH.
    Patch,
    /// HEAD.
    Head,
    /// OPTIONS.
    Options,
    /// TRACE.
    Trace,
    /// CONNECT.
    Connect,
    /// Any other (extension) method.
    Other(String),
}

impl Method {
    /// The method's canonical uppercase token.
    pub fn as_str(&self) -> &str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
            Method::Patch => "PATCH",
            Method::Head => "HEAD",
            Method::Options => "OPTIONS",
            Method::Trace => "TRACE",
            Method::Connect => "CONNECT",
            Method::Other(s) => s,
        }
    }

    /// Parses a method token (case-sensitive per RFC, uppercase expected).
    ///
    /// This never fails, so an arbitrary string becomes [`Method::Other`]; use
    /// [`try_parse`](Method::try_parse) for untrusted input. The client refuses
    /// to send a request whose method is not a valid token.
    pub fn parse(s: &str) -> Method {
        match s {
            "GET" => Method::Get,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "DELETE" => Method::Delete,
            "PATCH" => Method::Patch,
            "HEAD" => Method::Head,
            "OPTIONS" => Method::Options,
            "TRACE" => Method::Trace,
            "CONNECT" => Method::Connect,
            other => Method::Other(other.to_string()),
        }
    }

    /// Parses a method token, rejecting anything that is not an RFC 9110
    /// `token` (so no spaces, control characters, CR/LF or separators) or that
    /// is longer than 32 bytes.
    pub fn try_parse(s: &str) -> Result<Method, HttpError> {
        if s.len() > MAX_METHOD_BYTES || !is_token(s) {
            return Err(HttpError::Protocol("invalid method token".into()));
        }
        Ok(Method::parse(s))
    }

    /// Whether the method is a valid RFC 9110 `token`. Always `true` for the
    /// named variants; [`Method::Other`] is checked character by character.
    pub fn is_valid(&self) -> bool {
        match self {
            Method::Other(s) => s.len() <= MAX_METHOD_BYTES && is_token(s),
            _ => true,
        }
    }

    /// Whether the method is idempotent per RFC 9110 §9.2.2 (`GET`, `HEAD`,
    /// `PUT`, `DELETE`, `OPTIONS`, `TRACE`) and so safe to re-send after a
    /// connection failure.
    pub fn is_idempotent(&self) -> bool {
        matches!(
            self,
            Method::Get
                | Method::Head
                | Method::Put
                | Method::Delete
                | Method::Options
                | Method::Trace
        )
    }

    /// Whether the method is safe (read-only) per RFC 9110 §9.2.1.
    pub fn is_safe(&self) -> bool {
        matches!(
            self,
            Method::Get | Method::Head | Method::Options | Method::Trace
        )
    }
}

/// Longest method token accepted from the wire or sent by the client.
const MAX_METHOD_BYTES: usize = 32;

/// Whether `b` is an RFC 9110 `tchar` (a byte allowed in a method or header
/// name).
pub(crate) fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Whether `s` is a non-empty RFC 9110 `token`.
pub(crate) fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(is_tchar)
}

/// Whether `b` may appear in a header field value: visible ASCII, space,
/// horizontal tab, or an obs-text byte (>= 0x80). Control characters
/// (including CR, LF and NUL) and DEL are not allowed.
pub(crate) fn is_field_value_byte(b: u8) -> bool {
    b == b'\t' || (b >= 0x20 && b != 0x7f)
}

/// An HTTP status code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusCode(pub u16);

impl StatusCode {
    /// 100 Continue.
    pub const CONTINUE: StatusCode = StatusCode(100);
    /// 101 Switching Protocols.
    pub const SWITCHING_PROTOCOLS: StatusCode = StatusCode(101);
    /// 200 OK.
    pub const OK: StatusCode = StatusCode(200);
    /// 201 Created.
    pub const CREATED: StatusCode = StatusCode(201);
    /// 204 No Content.
    pub const NO_CONTENT: StatusCode = StatusCode(204);
    /// 206 Partial Content (a satisfied `Range` request).
    pub const PARTIAL_CONTENT: StatusCode = StatusCode(206);
    /// 301 Moved Permanently.
    pub const MOVED_PERMANENTLY: StatusCode = StatusCode(301);
    /// 302 Found.
    pub const FOUND: StatusCode = StatusCode(302);
    /// 303 See Other.
    pub const SEE_OTHER: StatusCode = StatusCode(303);
    /// 304 Not Modified.
    pub const NOT_MODIFIED: StatusCode = StatusCode(304);
    /// 307 Temporary Redirect.
    pub const TEMPORARY_REDIRECT: StatusCode = StatusCode(307);
    /// 308 Permanent Redirect.
    pub const PERMANENT_REDIRECT: StatusCode = StatusCode(308);
    /// 400 Bad Request.
    pub const BAD_REQUEST: StatusCode = StatusCode(400);
    /// 401 Unauthorized.
    pub const UNAUTHORIZED: StatusCode = StatusCode(401);
    /// 403 Forbidden.
    pub const FORBIDDEN: StatusCode = StatusCode(403);
    /// 404 Not Found.
    pub const NOT_FOUND: StatusCode = StatusCode(404);
    /// 405 Method Not Allowed.
    pub const METHOD_NOT_ALLOWED: StatusCode = StatusCode(405);
    /// 406 Not Acceptable.
    pub const NOT_ACCEPTABLE: StatusCode = StatusCode(406);
    /// 408 Request Timeout.
    pub const REQUEST_TIMEOUT: StatusCode = StatusCode(408);
    /// 413 Payload Too Large.
    pub const PAYLOAD_TOO_LARGE: StatusCode = StatusCode(413);
    /// 414 URI Too Long.
    pub const URI_TOO_LONG: StatusCode = StatusCode(414);
    /// 416 Range Not Satisfiable.
    pub const RANGE_NOT_SATISFIABLE: StatusCode = StatusCode(416);
    /// 417 Expectation Failed.
    pub const EXPECTATION_FAILED: StatusCode = StatusCode(417);
    /// 429 Too Many Requests.
    pub const TOO_MANY_REQUESTS: StatusCode = StatusCode(429);
    /// 431 Request Header Fields Too Large.
    pub const REQUEST_HEADER_FIELDS_TOO_LARGE: StatusCode = StatusCode(431);
    /// 500 Internal Server Error.
    pub const INTERNAL_SERVER_ERROR: StatusCode = StatusCode(500);
    /// 501 Not Implemented.
    pub const NOT_IMPLEMENTED: StatusCode = StatusCode(501);
    /// 503 Service Unavailable.
    pub const SERVICE_UNAVAILABLE: StatusCode = StatusCode(503);
    /// 504 Gateway Timeout.
    pub const GATEWAY_TIMEOUT: StatusCode = StatusCode(504);
    /// 505 HTTP Version Not Supported.
    pub const HTTP_VERSION_NOT_SUPPORTED: StatusCode = StatusCode(505);

    /// The numeric code.
    pub fn as_u16(self) -> u16 {
        self.0
    }

    /// Whether the code is in the 2xx success range.
    pub fn is_success(self) -> bool {
        (200..300).contains(&self.0)
    }

    /// Whether the code is a 1xx interim (informational) response.
    pub fn is_informational(self) -> bool {
        (100..200).contains(&self.0)
    }

    /// Whether the code is one of the redirects that carry a `Location`
    /// (`301`, `302`, `303`, `307`, `308`).
    pub fn is_redirection(self) -> bool {
        matches!(self.0, 301 | 302 | 303 | 307 | 308)
    }

    /// A canonical reason phrase for common codes.
    pub fn reason(self) -> &'static str {
        match self.0 {
            100 => "Continue",
            101 => "Switching Protocols",
            103 => "Early Hints",
            200 => "OK",
            201 => "Created",
            202 => "Accepted",
            204 => "No Content",
            206 => "Partial Content",
            301 => "Moved Permanently",
            302 => "Found",
            303 => "See Other",
            304 => "Not Modified",
            307 => "Temporary Redirect",
            308 => "Permanent Redirect",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            406 => "Not Acceptable",
            408 => "Request Timeout",
            409 => "Conflict",
            411 => "Length Required",
            413 => "Payload Too Large",
            414 => "URI Too Long",
            415 => "Unsupported Media Type",
            416 => "Range Not Satisfiable",
            417 => "Expectation Failed",
            422 => "Unprocessable Entity",
            426 => "Upgrade Required",
            429 => "Too Many Requests",
            431 => "Request Header Fields Too Large",
            500 => "Internal Server Error",
            501 => "Not Implemented",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            504 => "Gateway Timeout",
            505 => "HTTP Version Not Supported",
            _ => "",
        }
    }
}

/// How a message declares its body framing through `Transfer-Encoding`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransferCoding {
    /// No `Transfer-Encoding` header at all.
    Absent,
    /// The final (and only accepted) coding is exactly `chunked`.
    Chunked,
    /// A `Transfer-Encoding` is present but it is not a lone `chunked`: an
    /// unknown coding, `chunked` that is not last, or `chunked` applied twice.
    Unsupported,
}

/// An ordered, case-insensitive collection of HTTP headers.
#[derive(Debug, Clone, Default)]
pub struct HeaderMap {
    entries: Vec<(String, String)>,
}

impl HeaderMap {
    /// Creates an empty header map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets a header, replacing any existing values with the same name.
    ///
    /// The name and value are sanitised so they can never split the message or
    /// inject another header: every byte that is not an RFC 9110 `tchar` is
    /// removed from the name, and CR, LF, NUL and the other control characters
    /// (except horizontal tab) are removed from the value. A name that is empty
    /// after sanitising is ignored. Use [`try_set`](Self::try_set) to reject
    /// such input instead of repairing it.
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = sanitize_name(name.into());
        if name.is_empty() {
            return;
        }
        self.entries.retain(|(k, _)| !k.eq_ignore_ascii_case(&name));
        self.entries.push((name, sanitize_value(value.into())));
    }

    /// Appends a header without removing existing same-named values. The name
    /// and value are sanitised exactly as in [`set`](Self::set).
    pub fn append(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = sanitize_name(name.into());
        if name.is_empty() {
            return;
        }
        self.entries.push((name, sanitize_value(value.into())));
    }

    /// Like [`set`](Self::set), but returns an error instead of repairing a
    /// name that is not a valid token or a value that contains CR, LF, NUL or
    /// another control character.
    pub fn try_set(
        &mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<(), HttpError> {
        let (name, value) = check_field(name.into(), value.into())?;
        self.entries.retain(|(k, _)| !k.eq_ignore_ascii_case(&name));
        self.entries.push((name, value));
        Ok(())
    }

    /// Like [`append`](Self::append), but returns an error instead of repairing
    /// an invalid name or value (see [`try_set`](Self::try_set)).
    pub fn try_append(
        &mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<(), HttpError> {
        let (name, value) = check_field(name.into(), value.into())?;
        self.entries.push((name, value));
        Ok(())
    }

    /// Appends a header that the wire parser has already validated.
    pub(crate) fn push_validated(&mut self, name: String, value: String) {
        self.entries.push((name, value));
    }

    /// Returns the first value for a header name (case-insensitive).
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Iterates over every value for a header name (case-insensitive), in
    /// insertion order.
    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.entries
            .iter()
            .filter(move |(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Removes every value for a header name (case-insensitive). Returns
    /// whether anything was removed.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
        self.entries.len() != before
    }

    /// Whether a header is present (case-insensitive).
    pub fn contains(&self, name: &str) -> bool {
        self.entries
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case(name))
    }

    /// The number of header lines (same-named headers count separately).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the map holds no headers.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The parsed `Content-Length`, if present and valid.
    ///
    /// Only a run of ASCII digits is accepted (no sign, no whitespace inside
    /// the number); when several `Content-Length` headers disagree, or any is
    /// invalid, this returns `None`.
    pub fn content_length(&self) -> Option<u64> {
        self.content_length_checked().ok().flatten()
    }

    /// The `Content-Length`, rejecting the request-smuggling-prone cases of an
    /// invalid value or multiple **conflicting** values. `Ok(None)` = absent;
    /// `Ok(Some(n))` = a single value (or several identical ones); `Err` =
    /// present but unparseable or contradictory.
    ///
    /// A value is valid only when it is one or more ASCII digits: a sign
    /// (`+5`), a list (`5, 5`), a decimal point or any other character is an
    /// error.
    pub fn content_length_checked(&self) -> Result<Option<u64>, HttpError> {
        let mut seen: Option<u64> = None;
        for (k, v) in &self.entries {
            if k.eq_ignore_ascii_case("content-length") {
                let n = parse_decimal(v.trim_matches([' ', '\t']))
                    .ok_or_else(|| HttpError::Protocol("invalid Content-Length".into()))?;
                match seen {
                    Some(prev) if prev != n => {
                        return Err(HttpError::Protocol(
                            "conflicting Content-Length values".into(),
                        ))
                    }
                    _ => seen = Some(n),
                }
            }
        }
        Ok(seen)
    }

    /// Whether `Transfer-Encoding` indicates chunked framing.
    ///
    /// Every `Transfer-Encoding` header line is considered. The result is
    /// `true` only when the combined coding list is exactly one `chunked`
    /// token (compared case-insensitively as a whole token, not as a
    /// substring). `gzip, chunked`, `chunked, chunked`, `xchunked` and
    /// `chunked, gzip` are all `false`.
    pub fn is_chunked(&self) -> bool {
        self.transfer_coding() == TransferCoding::Chunked
    }

    /// Classifies the message's `Transfer-Encoding` across all header lines.
    pub(crate) fn transfer_coding(&self) -> TransferCoding {
        let mut any = false;
        let mut codings = 0usize;
        let mut lone_chunked = false;
        for (k, v) in &self.entries {
            if !k.eq_ignore_ascii_case("transfer-encoding") {
                continue;
            }
            any = true;
            for token in v.split(',') {
                let token = token.trim_matches([' ', '\t']);
                codings += 1;
                lone_chunked = token.eq_ignore_ascii_case("chunked");
            }
        }
        if !any {
            TransferCoding::Absent
        } else if codings == 1 && lone_chunked {
            TransferCoding::Chunked
        } else {
            TransferCoding::Unsupported
        }
    }

    /// Whether any `Connection` header line lists `token` (case-insensitive).
    pub(crate) fn connection_has(&self, token: &str) -> bool {
        self.get_all("connection")
            .flat_map(|v| v.split(','))
            .any(|t| t.trim_matches([' ', '\t']).eq_ignore_ascii_case(token))
    }

    /// Iterates over `(name, value)` pairs in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

/// Parses a non-empty run of ASCII digits (no sign, no whitespace) as a `u64`.
pub(crate) fn parse_decimal(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Removes every byte that is not a `tchar`, so a header name cannot carry
/// CR/LF, a colon, whitespace or any control character onto the wire.
fn sanitize_name(s: String) -> String {
    if s.bytes().all(is_tchar) {
        s
    } else {
        s.chars()
            .filter(|c| c.is_ascii() && is_tchar(*c as u8))
            .collect()
    }
}

/// Removes CR, LF, NUL and the other control characters (except horizontal
/// tab) so a header value cannot inject extra header lines or split the
/// message (response/header injection).
fn sanitize_value(s: String) -> String {
    if s.chars().all(value_char_ok) {
        s
    } else {
        s.chars().filter(|c| value_char_ok(*c)).collect()
    }
}

fn value_char_ok(c: char) -> bool {
    c == '\t' || (c >= ' ' && c != '\u{7f}')
}

fn check_field(name: String, value: String) -> Result<(String, String), HttpError> {
    if !is_token(&name) {
        return Err(HttpError::Protocol("invalid header name".into()));
    }
    if !value.chars().all(value_char_ok) {
        return Err(HttpError::Protocol(
            "control character in header value".into(),
        ));
    }
    Ok((name, value))
}

/// A parsed absolute URL (client side) or request target (server side).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uri {
    /// `http` or `https` (empty for a bare request target).
    pub scheme: String,
    /// Host name (empty for a bare request target). An IPv6 literal is stored
    /// **without** its square brackets (`::1`), ready to connect to;
    /// [`authority`](Uri::authority) adds them back for the `Host` header.
    pub host: String,
    /// Port (defaulted from the scheme).
    pub port: u16,
    /// Absolute path (always begins with `/`).
    pub path: String,
    /// Raw query string (without the leading `?`).
    pub query: Option<String>,
}

impl Uri {
    /// Parses an absolute `http(s)` / `ws(s)` URL.
    ///
    /// The parser is strict about anything that could alter the request on the
    /// wire:
    ///
    /// - a control character (including CR, LF, NUL and DEL) or a space
    ///   anywhere in the URL is an error;
    /// - the `#fragment` is dropped (it is never sent to a server);
    /// - `user:password@` userinfo is an error — send credentials in a header;
    /// - the host must be a registered name / IPv4 address made of
    ///   `A-Z a-z 0-9 - . _ ~`, or an IPv6 literal in square brackets
    ///   (`http://[::1]:8080/`); the brackets are stripped from
    ///   [`host`](Uri::host);
    /// - the port, when present, must be 1–5 ASCII digits in `1..=65535`;
    /// - non-ASCII characters in the path or query are percent-encoded as
    ///   UTF-8.
    pub fn parse(url: &str) -> Result<Uri, HttpError> {
        if url.bytes().any(|b| b <= 0x20 || b == 0x7f) {
            return Err(HttpError::InvalidUrl(
                "control character or space in URL".into(),
            ));
        }
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| HttpError::InvalidUrl(format!("not an absolute URL: {url}")))?;
        let scheme = scheme.to_ascii_lowercase();
        let default_port = match scheme.as_str() {
            "http" | "ws" => 80,
            "https" | "wss" => 443,
            _ => {
                return Err(HttpError::InvalidUrl(format!(
                    "unsupported scheme: {scheme}"
                )))
            }
        };

        // The fragment is client-side only and must never reach the wire.
        let rest = match rest.find('#') {
            Some(i) => &rest[..i],
            None => rest,
        };

        // authority is up to the first '/', '?', or end.
        let auth_end = rest.find(['/', '?']).unwrap_or(rest.len());
        let authority = &rest[..auth_end];
        let remainder = &rest[auth_end..];

        if authority.contains('@') {
            // Deliberately does not echo the URL: it holds a credential.
            return Err(HttpError::InvalidUrl(
                "userinfo (user:password@) in a URL is not supported; \
                 send credentials in a header"
                    .into(),
            ));
        }
        let (host, port) = parse_authority(authority, default_port)
            .ok_or_else(|| HttpError::InvalidUrl(format!("invalid host or port: {authority}")))?;

        let (path, query) = split_target(remainder);
        Ok(Uri {
            scheme,
            host,
            port,
            path: encode_non_ascii(&path),
            query: query.map(|q| encode_non_ascii(&q)),
        })
    }

    /// Whether the scheme uses TLS (`https` or `wss`).
    pub fn is_tls(&self) -> bool {
        self.scheme == "https" || self.scheme == "wss"
    }

    /// The `Host` header value (`host` or `host:port` for non-default ports).
    /// An IPv6 literal is wrapped in square brackets (`[::1]:8080`).
    pub fn authority(&self) -> String {
        let default = if self.is_tls() { 443 } else { 80 };
        let host = self.bracketed_host();
        if self.port == default {
            host
        } else {
            format!("{}:{}", host, self.port)
        }
    }

    /// `host:port`, always with the port (the `CONNECT` request target).
    pub(crate) fn host_port(&self) -> String {
        format!("{}:{}", self.bracketed_host(), self.port)
    }

    fn bracketed_host(&self) -> String {
        if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        }
    }

    /// The origin-form request target (`/path?query`).
    pub fn request_target(&self) -> String {
        match &self.query {
            Some(q) => format!("{}?{}", self.path, q),
            None => self.path.clone(),
        }
    }

    /// The absolute form `scheme://authority/path?query` (what a forward proxy
    /// expects as the request target). Never includes a fragment.
    pub fn absolute(&self) -> String {
        format!(
            "{}://{}{}",
            self.scheme,
            self.authority(),
            self.request_target()
        )
    }

    /// Whether `other` has the same scheme, host (case-insensitive) and port.
    pub fn same_origin(&self, other: &Uri) -> bool {
        self.scheme == other.scheme
            && self.port == other.port
            && self.host.eq_ignore_ascii_case(&other.host)
    }

    /// Resolves a URI reference (for example a redirect `Location`) against
    /// this URL, following RFC 3986 §5: absolute URLs, scheme-relative
    /// (`//host/p`), absolute-path (`/p`), relative-path (`../p`) and
    /// query-only (`?q`) references are supported, and `.` / `..` segments are
    /// removed. The result is validated exactly like [`Uri::parse`].
    pub fn resolve(&self, reference: &str) -> Result<Uri, HttpError> {
        let reference = match reference.find('#') {
            Some(i) => &reference[..i],
            None => reference,
        };
        if has_scheme(reference) {
            return Uri::parse(reference);
        }
        if let Some(rest) = reference.strip_prefix("//") {
            return Uri::parse(&format!("{}://{}", self.scheme, rest));
        }
        let origin = format!("{}://{}", self.scheme, self.authority());
        if reference.is_empty() {
            return Uri::parse(&format!("{origin}{}", self.request_target()));
        }
        if let Some(query) = reference.strip_prefix('?') {
            return Uri::parse(&format!("{origin}{}?{query}", self.path));
        }
        let (ref_path, ref_query) = match reference.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (reference, None),
        };
        let merged = if ref_path.starts_with('/') {
            ref_path.to_string()
        } else {
            // Replace everything after the base path's last '/'.
            let dir_end = self.path.rfind('/').map(|i| i + 1).unwrap_or(0);
            format!("{}{}", &self.path[..dir_end], ref_path)
        };
        let path = remove_dot_segments(&merged);
        match ref_query {
            Some(q) => Uri::parse(&format!("{origin}{path}?{q}")),
            None => Uri::parse(&format!("{origin}{path}")),
        }
    }

    /// Checks that the parts that go on the wire (request target and `Host`)
    /// cannot split or corrupt the request. `Uri` has public fields, so a
    /// value may have been built or mutated without [`Uri::parse`]; the client
    /// calls this before encoding every request.
    pub(crate) fn validate_for_wire(&self) -> Result<(), HttpError> {
        let visible = |s: &str| s.bytes().all(|b| (0x21..0x7f).contains(&b));
        if !self.path.starts_with('/') || !visible(&self.path) || self.path.contains('#') {
            return Err(HttpError::InvalidUrl(
                "request path must start with '/' and contain no spaces, \
                 control characters, non-ASCII bytes or '#'"
                    .into(),
            ));
        }
        if let Some(q) = &self.query {
            if !visible(q) || q.contains('#') {
                return Err(HttpError::InvalidUrl(
                    "query must contain no spaces, control characters, \
                     non-ASCII bytes or '#'"
                        .into(),
                ));
            }
        }
        if self.host.is_empty() || !valid_host(&self.host) {
            return Err(HttpError::InvalidUrl("invalid host".into()));
        }
        Ok(())
    }
}

impl std::fmt::Display for Uri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.scheme.is_empty() {
            f.write_str(&self.request_target())
        } else {
            f.write_str(&self.absolute())
        }
    }
}

/// Whether `reference` starts with `scheme://`.
fn has_scheme(reference: &str) -> bool {
    match reference.split_once("://") {
        Some((scheme, _)) => {
            scheme
                .bytes()
                .next()
                .map(|b| b.is_ascii_alphabetic())
                .unwrap_or(false)
                && scheme
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
        }
        None => false,
    }
}

/// Whether `host` (already without brackets) is an IPv6 literal or a
/// registered name / IPv4 address made only of unreserved characters.
fn valid_host(host: &str) -> bool {
    if host.contains(':') {
        return host.parse::<std::net::Ipv6Addr>().is_ok();
    }
    !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// Splits `authority` into a bracket-free host and a port.
fn parse_authority(authority: &str, default_port: u16) -> Option<(String, u16)> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        // IPv6 literal: `[addr]` or `[addr]:port`.
        let (addr, after) = rest.split_once(']')?;
        let port = match after {
            "" => None,
            _ => Some(after.strip_prefix(':')?),
        };
        if addr.parse::<std::net::Ipv6Addr>().is_err() {
            return None;
        }
        (addr, port)
    } else {
        let (h, p) = match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        };
        // An IPv6 literal must be bracketed, or its colons are ambiguous.
        if h.contains(':') {
            return None;
        }
        (h, p)
    };
    if !valid_host(host) {
        return None;
    }
    let port = match port {
        Some(p) => {
            if p.len() > 5 {
                return None;
            }
            let n = parse_decimal(p)?;
            if n == 0 || n > u64::from(u16::MAX) {
                return None;
            }
            n as u16
        }
        None => default_port,
    };
    Some((host.to_string(), port))
}

/// Percent-encodes every non-ASCII character as its UTF-8 bytes.
fn encode_non_ascii(s: &str) -> String {
    if s.is_ascii() {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        if b.is_ascii() {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Removes `.` and `..` segments from an absolute path (RFC 3986 §5.2.4).
/// `..` never climbs above the root.
pub(crate) fn remove_dot_segments(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let segments: Vec<&str> = path.split('/').collect();
    let last = segments.len() - 1;
    let mut trailing_slash = false;
    for (i, seg) in segments.iter().enumerate() {
        match *seg {
            "." => trailing_slash = i == last,
            ".." => {
                out.pop();
                trailing_slash = i == last;
            }
            "" if i == 0 => {}
            s => {
                out.push(s);
                trailing_slash = false;
            }
        }
    }
    let mut result = String::with_capacity(path.len());
    for seg in &out {
        result.push('/');
        result.push_str(seg);
    }
    if result.is_empty() || trailing_slash {
        result.push('/');
    }
    result
}

/// Splits a request target into path and query, defaulting an empty path to `/`.
pub fn split_target(target: &str) -> (String, Option<String>) {
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, Some(q.to_string())),
        None => (target, None),
    };
    let path = if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    };
    (path, query)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let u = Uri::parse("https://api.example.com/v1/chat?x=1").unwrap();
        assert_eq!(u.scheme, "https");
        assert_eq!(u.host, "api.example.com");
        assert_eq!(u.port, 443);
        assert_eq!(u.path, "/v1/chat");
        assert_eq!(u.query.as_deref(), Some("x=1"));
        assert!(u.is_tls());
        assert_eq!(u.authority(), "api.example.com");
        assert_eq!(u.request_target(), "/v1/chat?x=1");

        let u = Uri::parse("http://localhost:8080/").unwrap();
        assert_eq!(u.port, 8080);
        assert_eq!(u.authority(), "localhost:8080");
        assert_eq!(u.request_target(), "/");

        assert!(Uri::parse("ftp://x/").is_err());
        assert!(Uri::parse("not-a-url").is_err());
    }

    #[test]
    fn rejects_control_characters_and_spaces() {
        for bad in [
            "http://h/a b",
            "http://h/a\r\nHost: evil",
            "http://h/a\nb",
            "http://h/a\tb",
            "http://h/a\0b",
            "http://h/a\u{7f}b",
            "http://h /a",
            "http://h/?q=a b",
            "http://h/ HTTP/1.1\r\nHost: evil\r\n\r\nGET /admin",
        ] {
            assert!(Uri::parse(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn ipv6_literals_strip_brackets_for_connect_and_keep_them_for_host() {
        let u = Uri::parse("http://[::1]/x").unwrap();
        assert_eq!(u.host, "::1");
        assert_eq!(u.port, 80);
        assert_eq!(u.authority(), "[::1]");

        let u = Uri::parse("http://[::1]:8080/").unwrap();
        assert_eq!(u.host, "::1");
        assert_eq!(u.port, 8080);
        assert_eq!(u.authority(), "[::1]:8080");
        assert_eq!(u.absolute(), "http://[::1]:8080/");

        let u = Uri::parse("https://[2001:db8::2]:443/p?q").unwrap();
        assert_eq!(u.host, "2001:db8::2");
        assert_eq!(u.authority(), "[2001:db8::2]");

        assert!(Uri::parse("http://[::1/").is_err());
        assert!(Uri::parse("http://[::1]x/").is_err());
        assert!(Uri::parse("http://[not-v6]/").is_err());
        assert!(Uri::parse("http://::1/").is_err());
    }

    #[test]
    fn fragment_is_dropped_and_userinfo_rejected() {
        let u = Uri::parse("http://h/p?q=1#frag").unwrap();
        assert_eq!(u.request_target(), "/p?q=1");
        let u = Uri::parse("http://h#frag").unwrap();
        assert_eq!(u.request_target(), "/");
        assert_eq!(u.host, "h");

        let err = Uri::parse("http://user:secret@h/").unwrap_err();
        assert!(!err.to_string().contains("secret"));
        assert!(Uri::parse("http://user@h/").is_err());
    }

    #[test]
    fn rejects_bad_hosts_and_ports() {
        for bad in [
            "http:///p",
            "http://h:/p",
            "http://h:+80/",
            "http://h:0/",
            "http://h:65536/",
            "http://h:80a/",
            "http://h\\x/",
            "http://h%20x/",
            "http://ex<ample.com/",
        ] {
            assert!(Uri::parse(bad).is_err(), "accepted {bad:?}");
        }
        assert_eq!(Uri::parse("http://h:65535/").unwrap().port, 65535);
        assert_eq!(
            Uri::parse("http://my_svc.local-1/").unwrap().host,
            "my_svc.local-1"
        );
    }

    #[test]
    fn non_ascii_path_is_percent_encoded() {
        let u = Uri::parse("http://h/caf\u{e9}?n=\u{e9}").unwrap();
        assert_eq!(u.request_target(), "/caf%C3%A9?n=%C3%A9");
        assert!(Uri::parse("http://h\u{e9}/").is_err());
    }

    #[test]
    fn validate_for_wire_catches_hand_built_uris() {
        let mut u = Uri::parse("http://h/ok").unwrap();
        assert!(u.validate_for_wire().is_ok());
        u.path = "/a b".into();
        assert!(u.validate_for_wire().is_err());
        u.path = "/a\r\nX: y".into();
        assert!(u.validate_for_wire().is_err());
        u.path = "relative".into();
        assert!(u.validate_for_wire().is_err());
        u.path = "/ok".into();
        u.query = Some("a=1\r\n".into());
        assert!(u.validate_for_wire().is_err());
        u.query = None;
        u.host = "h\r\nX: y".into();
        assert!(u.validate_for_wire().is_err());
    }

    #[test]
    fn resolves_references() {
        let base = Uri::parse("http://a.example:8080/b/c/d?q=1").unwrap();
        let r = |s: &str| base.resolve(s).unwrap().absolute();
        assert_eq!(r("g"), "http://a.example:8080/b/c/g");
        assert_eq!(r("./g"), "http://a.example:8080/b/c/g");
        assert_eq!(r("g/"), "http://a.example:8080/b/c/g/");
        assert_eq!(r("/g"), "http://a.example:8080/g");
        assert_eq!(r("?y"), "http://a.example:8080/b/c/d?y");
        assert_eq!(r("g?y"), "http://a.example:8080/b/c/g?y");
        assert_eq!(r(""), "http://a.example:8080/b/c/d?q=1");
        assert_eq!(r("#frag"), "http://a.example:8080/b/c/d?q=1");
        assert_eq!(r(".."), "http://a.example:8080/b/");
        assert_eq!(r("../g"), "http://a.example:8080/b/g");
        assert_eq!(r("../../../../g"), "http://a.example:8080/g");
        assert_eq!(r("/a/./b/../c"), "http://a.example:8080/a/c");
        assert_eq!(r("//other.example/x"), "http://other.example/x");
        assert_eq!(r("https://s.example/y#f"), "https://s.example/y");
        assert!(base.resolve("/a b").is_err());
        assert!(base.resolve("ftp://x/").is_err());
    }

    #[test]
    fn dot_segments() {
        assert_eq!(remove_dot_segments("/a/b/../c"), "/a/c");
        assert_eq!(remove_dot_segments("/../.."), "/");
        assert_eq!(remove_dot_segments("/a/."), "/a/");
        assert_eq!(remove_dot_segments("/a/b/"), "/a/b/");
        assert_eq!(remove_dot_segments("/"), "/");
    }

    #[test]
    fn method_token_validation() {
        assert_eq!(Method::try_parse("GET").unwrap(), Method::Get);
        assert_eq!(
            Method::try_parse("PURGE").unwrap(),
            Method::Other("PURGE".into())
        );
        for bad in ["", "GE T", "GET\r\n", "G\0T", "GET/", "G(E)T"] {
            assert!(Method::try_parse(bad).is_err(), "accepted {bad:?}");
        }
        assert!(Method::try_parse(&"A".repeat(33)).is_err());
        assert!(!Method::Other("X Y".into()).is_valid());
        assert!(Method::Other("PURGE".into()).is_valid());
        assert!(Method::Put.is_idempotent());
        assert!(!Method::Post.is_idempotent());
        assert!(!Method::Patch.is_idempotent());
    }

    #[test]
    fn header_map_case_insensitive() {
        let mut h = HeaderMap::new();
        h.set("Content-Type", "application/json");
        assert_eq!(h.get("content-type"), Some("application/json"));
        h.set("content-type", "text/plain");
        assert_eq!(h.get("Content-Type"), Some("text/plain"));
        h.set("Content-Length", "10");
        assert_eq!(h.content_length(), Some(10));
        assert_eq!(h.len(), 2);
        assert!(h.remove("CONTENT-LENGTH"));
        assert!(!h.remove("content-length"));
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn header_set_strips_crlf_to_prevent_injection() {
        let mut h = HeaderMap::new();
        h.set("x-test", "value\r\nInjected: evil");
        // CR/LF removed, so no extra header line can be injected.
        assert_eq!(h.get("x-test"), Some("valueInjected: evil"));
        assert!(h.get("injected").is_none());
        h.append("na\rme", "v\nal");
        assert_eq!(h.get("name"), Some("val"));
    }

    #[test]
    fn header_names_are_reduced_to_tokens() {
        let mut h = HeaderMap::new();
        h.set("x bad: name\0", "a\0b\u{7f}c\td");
        assert_eq!(h.get("xbadname"), Some("abc\td"));
        // A name with no token characters at all is dropped.
        h.append(": \r\n", "v");
        assert_eq!(h.len(), 1);

        assert!(h.try_set("x-ok", "fine").is_ok());
        assert!(h.try_set("x bad", "v").is_err());
        assert!(h.try_set("", "v").is_err());
        assert!(h.try_append("x-ok", "a\r\nb").is_err());
        assert!(h.try_append("x-ok", "second").is_ok());
        assert_eq!(h.get_all("X-OK").collect::<Vec<_>>(), ["fine", "second"]);
    }

    #[test]
    fn content_length_is_strict() {
        let cl = |v: &str| {
            let mut h = HeaderMap::new();
            h.append("content-length", v);
            h.content_length_checked()
        };
        assert_eq!(cl("5").unwrap(), Some(5));
        assert_eq!(cl(" 5 ").unwrap(), Some(5));
        for bad in ["+5", "-5", "5, 5", "0x5", "5.0", "", "5 5", "five"] {
            assert!(cl(bad).is_err(), "accepted {bad:?}");
        }
        let mut h = HeaderMap::new();
        h.append("content-length", "5");
        h.append("Content-Length", "5");
        assert_eq!(h.content_length_checked().unwrap(), Some(5));
        h.append("content-length", "6");
        assert!(h.content_length_checked().is_err());
        assert_eq!(h.content_length(), None);
    }

    #[test]
    fn transfer_coding_considers_every_header_line() {
        let tc = |values: &[&str]| {
            let mut h = HeaderMap::new();
            for v in values {
                h.append("Transfer-Encoding", *v);
            }
            h.transfer_coding()
        };
        assert_eq!(tc(&[]), TransferCoding::Absent);
        assert_eq!(tc(&["chunked"]), TransferCoding::Chunked);
        assert_eq!(tc(&[" Chunked "]), TransferCoding::Chunked);
        for bad in [
            &["gzip", "chunked"][..],
            &["gzip, chunked"],
            &["chunked", "gzip"],
            &["xchunked"],
            &["chunked, chunked"],
            &["chunked", "chunked"],
            &["identity"],
            &[""],
            &["chunked,"],
        ] {
            assert_eq!(tc(bad), TransferCoding::Unsupported, "{bad:?}");
        }
    }
}
