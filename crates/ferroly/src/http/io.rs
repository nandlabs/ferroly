//! HTTP/1.1 wire encoding and decoding.
//!
//! The parsers here are deliberately strict: anything two HTTP implementations
//! could read differently (and so use to smuggle or split a message) is
//! rejected rather than repaired. See the "Security hardening" section of
//! `docs/http.md` for the full list of rules.

use std::time::Duration;

use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
};

use super::core::{is_field_value_byte, is_tchar, is_token, TransferCoding};
use super::message::{Framing, Request};
use super::{HeaderMap, HttpError, Method, StatusCode};

/// Default maximum bytes in the request/status line or a single header line.
/// Bounds the memory a hostile peer can force by never terminating a line.
pub(crate) const DEFAULT_MAX_LINE_BYTES: usize = 16 * 1024;
/// Default maximum number of header lines accepted.
pub(crate) const DEFAULT_MAX_HEADERS: usize = 128;
/// Default maximum total bytes across all header lines.
pub(crate) const DEFAULT_MAX_HEADER_BYTES: usize = 64 * 1024;
/// Maximum bytes in a chunk-size line (hex size plus any chunk extensions).
pub(crate) const MAX_CHUNK_LINE_BYTES: usize = 1024;
/// Largest piece of body data handed out (or written) at a time.
pub(crate) const BODY_PIECE_BYTES: usize = 16 * 1024;

/// Limits applied while reading a message head, chunk-size lines and trailers.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HeadLimits {
    /// Maximum bytes in the start line or one header/trailer line.
    pub(crate) max_line_bytes: usize,
    /// Maximum number of header (or trailer) lines.
    pub(crate) max_headers: usize,
    /// Maximum total bytes of the header (or trailer) section.
    pub(crate) max_header_bytes: usize,
    /// Accept a bare LF as a line terminator. The server never does; the
    /// client does, because RFC 9112 §2.2 lets a recipient tolerate it and a
    /// response cannot be used to smuggle a request.
    pub(crate) allow_bare_lf: bool,
}

impl HeadLimits {
    /// The strict limits a server applies by default.
    #[cfg_attr(not(any(feature = "ws", test)), allow(dead_code))]
    pub(crate) const fn server() -> Self {
        Self {
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
            max_headers: DEFAULT_MAX_HEADERS,
            max_header_bytes: DEFAULT_MAX_HEADER_BYTES,
            allow_bare_lf: false,
        }
    }

    /// The limits the client applies to a response.
    pub(crate) const fn client() -> Self {
        Self {
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
            max_headers: DEFAULT_MAX_HEADERS,
            max_header_bytes: DEFAULT_MAX_HEADER_BYTES,
            allow_bare_lf: true,
        }
    }
}

/// The HTTP version named on a start line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Version {
    /// `HTTP/1.0`: connections close by default.
    Http10,
    /// `HTTP/1.1`: connections persist by default.
    Http11,
}

/// Why a message head could not be read.
#[derive(Debug)]
pub(crate) enum HeadError {
    /// The peer closed the connection cleanly before sending any byte.
    Closed,
    /// The transport failed (including EOF in the middle of the head).
    Io(std::io::Error),
    /// The head is malformed or over a limit; `status` is what a server should
    /// answer before closing.
    Bad {
        /// The response status a server sends.
        status: StatusCode,
        /// A short, fixed description (never echoes peer input).
        reason: &'static str,
    },
}

impl HeadError {
    fn bad(reason: &'static str) -> Self {
        HeadError::Bad {
            status: StatusCode::BAD_REQUEST,
            reason,
        }
    }

    fn too_large(reason: &'static str) -> Self {
        HeadError::Bad {
            status: StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            reason,
        }
    }
}

impl From<std::io::Error> for HeadError {
    fn from(e: std::io::Error) -> Self {
        HeadError::Io(e)
    }
}

impl From<HeadError> for HttpError {
    fn from(e: HeadError) -> Self {
        match e {
            HeadError::Closed => HttpError::Protocol("connection closed before a message".into()),
            HeadError::Io(e) => HttpError::Io(e),
            HeadError::Bad { reason, .. } => HttpError::Protocol(reason.into()),
        }
    }
}

/// Reads one line and returns it without its terminator, or `None` on a clean
/// EOF before any byte of the line.
///
/// The line must end in CRLF (or a bare LF when `allow_bare_lf`); a CR that is
/// not part of the terminator is an error, as is a line longer than `cap`
/// bytes (so a peer cannot force an unbounded allocation with an endless
/// line). `started` is set as soon as any byte has been consumed.
async fn read_line<R: AsyncBufRead + Unpin>(
    r: &mut R,
    cap: usize,
    allow_bare_lf: bool,
    started: &mut bool,
) -> Result<Option<Vec<u8>>, HeadError> {
    let mut line: Vec<u8> = Vec::new();
    loop {
        let (used, complete) = {
            let available = r.fill_buf().await?;
            if available.is_empty() {
                if line.is_empty() {
                    return Ok(None);
                }
                return Err(HeadError::Io(std::io::ErrorKind::UnexpectedEof.into()));
            }
            match available.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    // +2 leaves room for the CR that is stripped below.
                    if line.len() + i > cap + 1 {
                        return Err(HeadError::too_large("line too long"));
                    }
                    line.extend_from_slice(&available[..i]);
                    (i + 1, true)
                }
                None => {
                    if line.len() + available.len() > cap + 1 {
                        return Err(HeadError::too_large("line too long"));
                    }
                    line.extend_from_slice(available);
                    (available.len(), false)
                }
            }
        };
        r.consume(used);
        *started = true;
        if complete {
            break;
        }
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    } else if !allow_bare_lf {
        return Err(HeadError::bad("line not terminated by CRLF"));
    }
    if line.len() > cap {
        return Err(HeadError::too_large("line too long"));
    }
    if line.contains(&b'\r') {
        return Err(HeadError::bad("bare CR in line"));
    }
    Ok(Some(line))
}

/// Reads a header (or trailer) section up to and including its blank line.
///
/// Rejected: a line without a colon, an empty or non-token field name
/// (which covers whitespace before the colon), an obs-fold continuation line,
/// control characters in a value, and anything over the count/size limits.
async fn read_header_block<R: AsyncBufRead + Unpin>(
    r: &mut R,
    limits: &HeadLimits,
    started: &mut bool,
) -> Result<HeaderMap, HeadError> {
    let mut headers = HeaderMap::new();
    let mut total = 0usize;
    let mut count = 0usize;
    loop {
        let line = read_line(r, limits.max_line_bytes, limits.allow_bare_lf, started)
            .await?
            .ok_or_else(|| HeadError::Io(std::io::ErrorKind::UnexpectedEof.into()))?;
        total += line.len() + 2;
        if total > limits.max_header_bytes {
            return Err(HeadError::too_large("header block too large"));
        }
        if line.is_empty() {
            break;
        }
        count += 1;
        if count > limits.max_headers {
            return Err(HeadError::too_large("too many headers"));
        }
        if line[0] == b' ' || line[0] == b'\t' {
            return Err(HeadError::bad("obsolete header line folding"));
        }
        let colon = line
            .iter()
            .position(|&b| b == b':')
            .ok_or_else(|| HeadError::bad("header line without a colon"))?;
        let name = &line[..colon];
        if name.is_empty() || !name.iter().copied().all(is_tchar) {
            return Err(HeadError::bad("invalid header name"));
        }
        let value = trim_ows(&line[colon + 1..]);
        if !value.iter().copied().all(is_field_value_byte) {
            return Err(HeadError::bad("control character in header value"));
        }
        headers.push_validated(
            String::from_utf8_lossy(name).into_owned(),
            String::from_utf8_lossy(value).into_owned(),
        );
    }
    Ok(headers)
}

/// Trims optional whitespace (space / horizontal tab) from both ends.
fn trim_ows(mut s: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = s {
        s = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = s {
        s = rest;
    }
    s
}

/// A parsed request line plus headers.
#[derive(Debug)]
pub(crate) struct RequestHead {
    /// The request method (always a valid token).
    pub(crate) method: Method,
    /// The request target reduced to origin form (`/path?query`), `*`, or the
    /// authority for `CONNECT`.
    pub(crate) target: String,
    /// The authority taken from an absolute-form target, which overrides the
    /// `Host` header (RFC 9112 §3.2.2).
    pub(crate) authority: Option<String>,
    /// The HTTP version on the request line.
    pub(crate) version: Version,
    /// The request headers.
    pub(crate) headers: HeaderMap,
}

/// Reads a request line and headers (server side), strictly.
///
/// The request line must be `method SP target SP HTTP/1.x` with single
/// spaces, a token method and a target free of control characters, spaces,
/// non-ASCII bytes and `#`. One or two empty lines before the request line are
/// skipped (RFC 9112 §2.2).
pub(crate) async fn read_request_head_with<R: AsyncBufRead + Unpin>(
    r: &mut R,
    limits: &HeadLimits,
    started: &mut bool,
) -> Result<RequestHead, HeadError> {
    let mut skipped = 0;
    let line = loop {
        let line = match read_line(r, limits.max_line_bytes, limits.allow_bare_lf, started).await {
            Ok(Some(line)) => line,
            Ok(None) if skipped == 0 => return Err(HeadError::Closed),
            Ok(None) => return Err(HeadError::Io(std::io::ErrorKind::UnexpectedEof.into())),
            Err(HeadError::Bad { status, .. })
                if status == StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE =>
            {
                return Err(HeadError::Bad {
                    status: StatusCode::URI_TOO_LONG,
                    reason: "request line too long",
                })
            }
            Err(e) => return Err(e),
        };
        if !line.is_empty() {
            break line;
        }
        skipped += 1;
        if skipped > 2 {
            return Err(HeadError::bad("empty request line"));
        }
    };

    let mut parts = line.split(|&b| b == b' ');
    let (method, target, version) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(t), Some(v), None) => (m, t, v),
        _ => return Err(HeadError::bad("malformed request line")),
    };
    let version = parse_version(version).map_err(|unsupported| {
        if unsupported {
            HeadError::Bad {
                status: StatusCode::HTTP_VERSION_NOT_SUPPORTED,
                reason: "unsupported HTTP version",
            }
        } else {
            HeadError::bad("missing or malformed HTTP version")
        }
    })?;
    let method = std::str::from_utf8(method)
        .ok()
        .and_then(|m| Method::try_parse(m).ok())
        .ok_or_else(|| HeadError::bad("invalid method token"))?;
    let (target, authority) = normalize_request_target(&method, target)
        .ok_or_else(|| HeadError::bad("invalid request target"))?;

    let headers = read_header_block(r, limits, started).await?;
    Ok(RequestHead {
        method,
        target,
        authority,
        version,
        headers,
    })
}

/// Parses `HTTP/1.0` / `HTTP/1.1`. `Err(true)` = a well-formed version this
/// stack does not speak (505); `Err(false)` = not a version at all (400).
fn parse_version(v: &[u8]) -> Result<Version, bool> {
    match v {
        b"HTTP/1.1" => Ok(Version::Http11),
        b"HTTP/1.0" => Ok(Version::Http10),
        [b'H', b'T', b'T', b'P', b'/', major, b'.', minor]
            if major.is_ascii_digit() && minor.is_ascii_digit() =>
        {
            Err(true)
        }
        _ => Err(false),
    }
}

/// Validates a request target and reduces it to what a handler sees:
/// `(target, authority-from-absolute-form)`.
fn normalize_request_target(method: &Method, target: &[u8]) -> Option<(String, Option<String>)> {
    if target.is_empty() || !target.iter().all(|b| (0x21..0x7f).contains(b)) {
        return None;
    }
    if target.contains(&b'#') {
        return None;
    }
    let target = std::str::from_utf8(target).ok()?;
    if target.starts_with('/') {
        return Some((target.to_string(), None));
    }
    if matches!(method, Method::Connect) {
        // authority-form: `host:port`.
        if target.contains('/') || target.contains('?') || target.contains('@') {
            return None;
        }
        return Some((target.to_string(), None));
    }
    if target == "*" {
        return matches!(method, Method::Options).then(|| (target.to_string(), None));
    }
    // absolute-form, as sent to a proxy: keep only the path and query, and
    // remember the authority.
    let lower = target.to_ascii_lowercase();
    let rest = if lower.starts_with("http://") {
        &target[7..]
    } else if lower.starts_with("https://") {
        &target[8..]
    } else {
        return None;
    };
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let remainder = &rest[end..];
    let target = if remainder.starts_with('/') {
        remainder.to_string()
    } else {
        format!("/{remainder}")
    };
    Some((target, Some(authority.to_string())))
}

/// A parsed status line plus headers.
#[derive(Debug)]
pub(crate) struct ResponseHead {
    /// The HTTP version on the status line.
    pub(crate) version: Version,
    /// The status code.
    pub(crate) status: StatusCode,
    /// The response headers.
    pub(crate) headers: HeaderMap,
}

/// Reads a response status line and headers. `started` is set once any byte
/// of the response has been read (the client uses it to decide whether a
/// request on a reused connection may be re-sent).
pub(crate) async fn read_response_head_with<R: AsyncBufRead + Unpin>(
    r: &mut R,
    limits: &HeadLimits,
    started: &mut bool,
) -> Result<ResponseHead, HttpError> {
    let line = match read_line(r, limits.max_line_bytes, limits.allow_bare_lf, started).await {
        Ok(Some(line)) => line,
        Ok(None) => return Err(HttpError::Protocol("empty response".into())),
        Err(e) => return Err(e.into()),
    };
    let bad = || HttpError::Protocol("bad status line".into());
    // `HTTP/1.x SP 3DIGIT [SP reason-phrase]`
    if line.len() < 12 || line[8] != b' ' {
        return Err(bad());
    }
    let version = parse_version(&line[..8]).map_err(|_| bad())?;
    let code = &line[9..12];
    if !code.iter().all(u8::is_ascii_digit) || !(line.len() == 12 || line[12] == b' ') {
        return Err(bad());
    }
    let code = code
        .iter()
        .fold(0u16, |acc, d| acc * 10 + u16::from(d - b'0'));
    if code < 100 {
        return Err(bad());
    }
    let headers = read_header_block(r, limits, started).await?;
    Ok(ResponseHead {
        version,
        status: StatusCode(code),
        headers,
    })
}

/// Reads a response status line and headers from a buffered reader with the
/// default limits. Kept for the WebSocket client handshake.
#[cfg_attr(not(feature = "ws"), allow(dead_code))]
pub(crate) async fn read_response_head<R: AsyncBufRead + Unpin>(
    r: &mut R,
) -> Result<(StatusCode, HeaderMap), HttpError> {
    let mut started = false;
    let head = read_response_head_with(r, &HeadLimits::client(), &mut started).await?;
    Ok((head.status, head.headers))
}

/// Chooses the body framing for a response to a `method` request.
///
/// `HEAD` responses, `1xx`, `204`, `304` and a successful `CONNECT` have no
/// body whatever their headers say. Otherwise a lone `chunked`
/// `Transfer-Encoding` wins, then a (strictly validated) `Content-Length`,
/// then read-until-close. Any other `Transfer-Encoding` is an error.
pub(crate) fn response_framing(
    method: &Method,
    status: StatusCode,
    headers: &HeaderMap,
) -> Result<Framing, HttpError> {
    let code = status.as_u16();
    if matches!(method, Method::Head) || code == 204 || code == 304 || (100..200).contains(&code) {
        return Ok(Framing::Done);
    }
    if matches!(method, Method::Connect) && (200..300).contains(&code) {
        return Ok(Framing::Done);
    }
    match headers.transfer_coding() {
        TransferCoding::Chunked => Ok(Framing::Chunked),
        TransferCoding::Unsupported => Err(HttpError::Protocol(
            "unsupported Transfer-Encoding in response".into(),
        )),
        TransferCoding::Absent => Ok(match headers.content_length_checked()? {
            Some(len) => Framing::Length(len),
            None => Framing::Eof,
        }),
    }
}

/// Parses a chunk-size line (`1*HEXDIG [ chunk-ext ]`).
///
/// Only hex digits are accepted for the size (no sign, no `0x`, at most 16
/// digits). Whitespace between the size and a `;` extension is tolerated;
/// trailing whitespace without an extension only when `lenient`.
fn parse_chunk_size(line: &[u8], lenient: bool) -> Result<u64, HttpError> {
    let bad = || HttpError::Protocol("bad chunk size".into());
    let (mut digits, ext) = match line.iter().position(|&b| b == b';') {
        Some(i) => (&line[..i], Some(&line[i + 1..])),
        None => (line, None),
    };
    if ext.is_some() || lenient {
        while let [rest @ .., b' ' | b'\t'] = digits {
            digits = rest;
        }
    }
    if digits.is_empty() || digits.len() > 16 || !digits.iter().all(u8::is_ascii_hexdigit) {
        return Err(bad());
    }
    if let Some(ext) = ext {
        if !ext.iter().copied().all(is_field_value_byte) {
            return Err(bad());
        }
    }
    let text = std::str::from_utf8(digits).map_err(|_| bad())?;
    u64::from_str_radix(text, 16).map_err(|_| bad())
}

/// An incremental decoder for a `Transfer-Encoding: chunked` body, shared by
/// the server (request bodies) and the client (response bodies).
#[derive(Debug)]
pub(crate) struct ChunkedReader {
    /// Bytes still to read from the current chunk.
    remaining: u64,
    /// The current chunk's data is finished and its CRLF is still to be read.
    need_crlf: bool,
    /// The terminating chunk and trailer section have been consumed.
    done: bool,
    /// Sum of the chunk sizes declared so far.
    total: u64,
    /// Cap on `total`; exceeding it fails before the chunk is read.
    max: Option<u64>,
}

impl ChunkedReader {
    /// A decoder that fails with [`HttpError::BodyTooLarge`] once the declared
    /// chunk sizes add up to more than `max`.
    pub(crate) fn new(max: Option<u64>) -> Self {
        Self {
            remaining: 0,
            need_crlf: false,
            done: false,
            total: 0,
            max,
        }
    }

    /// Replaces the size cap.
    pub(crate) fn set_max(&mut self, max: Option<u64>) {
        self.max = max;
    }

    /// Returns the next piece of decoded data, or `None` once the terminating
    /// chunk and its trailer section have been read.
    ///
    /// Each chunk's data must be followed by exactly CRLF. Chunk-size lines
    /// are capped at [`MAX_CHUNK_LINE_BYTES`] (or the configured line limit,
    /// if that is smaller); the trailer section is bounded by `limits` (line
    /// length, count and total bytes) and then discarded.
    pub(crate) async fn next<R: AsyncBufRead + Unpin>(
        &mut self,
        r: &mut R,
        limits: &HeadLimits,
    ) -> Result<Option<Vec<u8>>, HttpError> {
        let truncated = || HttpError::Protocol("chunked body truncated".into());
        loop {
            if self.done {
                return Ok(None);
            }
            if self.remaining > 0 {
                let piece = {
                    let available = r.fill_buf().await?;
                    if available.is_empty() {
                        return Err(truncated());
                    }
                    let n = (available.len() as u64)
                        .min(self.remaining)
                        .min(BODY_PIECE_BYTES as u64) as usize;
                    available[..n].to_vec()
                };
                r.consume(piece.len());
                self.remaining -= piece.len() as u64;
                self.need_crlf = self.remaining == 0;
                return Ok(Some(piece));
            }
            if self.need_crlf {
                let mut crlf = [0u8; 2];
                r.read_exact(&mut crlf).await.map_err(eof_as(truncated))?;
                if crlf != *b"\r\n" {
                    return Err(HttpError::Protocol(
                        "chunk data not followed by CRLF".into(),
                    ));
                }
                self.need_crlf = false;
            }
            let mut started = false;
            // Never looser than the configured line limit.
            let cap = MAX_CHUNK_LINE_BYTES.min(limits.max_line_bytes);
            let line = read_line(r, cap, limits.allow_bare_lf, &mut started)
                .await
                .map_err(|e| match e {
                    HeadError::Bad { status, .. }
                        if status == StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE =>
                    {
                        HttpError::Protocol("chunk-size line too long".into())
                    }
                    HeadError::Io(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                        truncated()
                    }
                    e => e.into(),
                })?
                .ok_or_else(truncated)?;
            let size = parse_chunk_size(&line, limits.allow_bare_lf)?;
            if size == 0 {
                read_header_block(r, limits, &mut started)
                    .await
                    .map_err(|e| match e {
                        HeadError::Io(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                            truncated()
                        }
                        HeadError::Bad { status, .. }
                            if status == StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE =>
                        {
                            HttpError::Protocol("trailer section too large".into())
                        }
                        e => e.into(),
                    })?;
                self.done = true;
                return Ok(None);
            }
            self.total = self
                .total
                .checked_add(size)
                .ok_or_else(|| HttpError::Protocol("bad chunk size".into()))?;
            if let Some(max) = self.max {
                if self.total > max {
                    return Err(HttpError::BodyTooLarge(max));
                }
            }
            self.remaining = size;
        }
    }
}

/// Maps an `UnexpectedEof` I/O error to `make()`, leaving other errors as I/O.
fn eof_as(make: impl Fn() -> HttpError) -> impl Fn(std::io::Error) -> HttpError {
    move |e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            make()
        } else {
            HttpError::Io(e)
        }
    }
}

/// Per-request options for encoding a request onto the wire.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RequestWire<'a> {
    /// Use the absolute form (`http://host/path`) as the request target, as a
    /// forward proxy expects.
    pub(crate) absolute_form: bool,
    /// `User-Agent` to add when the request does not carry one.
    pub(crate) user_agent: Option<&'a str>,
    /// `Accept-Encoding` to add when the request does not carry one.
    pub(crate) accept_encoding: Option<&'a str>,
    /// `Proxy-Authorization` to add when the request does not carry one.
    pub(crate) proxy_authorization: Option<&'a str>,
}

/// How the body following an encoded request head is framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyEncoding {
    /// Exactly this many bytes follow, unframed.
    Length(u64),
    /// The body is sent with chunked transfer encoding.
    Chunked,
}

/// Encodes a request line and headers.
///
/// Adds `Host`, the configured `User-Agent` / `Accept-Encoding` /
/// `Proxy-Authorization`, and the body framing header when the caller did not
/// set them. Leaves `Connection` unset (HTTP/1.1 keep-alive) so the client can
/// pool the connection. `body_len` is the size of a buffered body, or `None`
/// when the body is streamed from a reader.
///
/// Refuses to encode anything that could split or desynchronise the request:
/// a method that is not a token, a target or host with control characters or
/// spaces, a header name that is not a token, a control character in a header
/// value, both `Content-Length` and `Transfer-Encoding`, a `Transfer-Encoding`
/// other than `chunked`, or a `Content-Length` that disagrees with the body.
pub(crate) fn encode_request_head(
    req: &Request,
    wire: &RequestWire<'_>,
    body_len: Option<usize>,
) -> Result<(Vec<u8>, BodyEncoding), HttpError> {
    if !req.method.is_valid() {
        return Err(HttpError::Protocol("invalid method token".into()));
    }
    req.uri.validate_for_wire()?;
    let target = if matches!(req.method, Method::Connect) {
        req.uri.host_port()
    } else if wire.absolute_form {
        req.uri.absolute()
    } else {
        req.uri.request_target()
    };

    let mut head = String::with_capacity(256);
    head.push_str(req.method.as_str());
    head.push(' ');
    head.push_str(&target);
    head.push_str(" HTTP/1.1\r\n");
    if !req.headers.contains("host") {
        push_header(&mut head, "Host", &req.uri.authority());
    }
    for (k, v) in req.headers.iter() {
        // `HeaderMap` sanitises on insert; this is defence in depth.
        if !is_token(k) {
            return Err(HttpError::Protocol("invalid header name".into()));
        }
        if !v.bytes().all(is_field_value_byte) {
            return Err(HttpError::Protocol(
                "control character in header value".into(),
            ));
        }
        push_header(&mut head, k, v);
    }
    if let Some(ua) = wire.user_agent {
        if !req.headers.contains("user-agent") {
            push_header(&mut head, "User-Agent", ua);
        }
    }
    if let Some(ae) = wire.accept_encoding {
        if !req.headers.contains("accept-encoding") {
            push_header(&mut head, "Accept-Encoding", ae);
        }
    }
    if let Some(pa) = wire.proxy_authorization {
        if !req.headers.contains("proxy-authorization") {
            push_header(&mut head, "Proxy-Authorization", pa);
        }
    }

    let declared = req.headers.content_length_checked()?;
    let coding = req.headers.transfer_coding();
    if coding == TransferCoding::Unsupported {
        return Err(HttpError::Protocol(
            "only a lone `chunked` Transfer-Encoding can be sent".into(),
        ));
    }
    if coding == TransferCoding::Chunked && declared.is_some() {
        return Err(HttpError::Protocol(
            "both Content-Length and Transfer-Encoding are set".into(),
        ));
    }
    let encoding = match (coding, declared, body_len) {
        (TransferCoding::Chunked, _, _) => BodyEncoding::Chunked,
        (_, Some(n), Some(len)) => {
            if n != len as u64 {
                return Err(HttpError::Protocol(
                    "Content-Length does not match the body length".into(),
                ));
            }
            BodyEncoding::Length(n)
        }
        (_, Some(n), None) => BodyEncoding::Length(n),
        (_, None, Some(len)) => {
            // Always declare the length of a non-empty body, and an explicit
            // zero for the methods that are expected to carry content.
            let expects_content = matches!(req.method, Method::Post | Method::Put | Method::Patch);
            if len > 0 || expects_content {
                push_header(&mut head, "Content-Length", &len.to_string());
            }
            BodyEncoding::Length(len as u64)
        }
        (_, None, None) => {
            push_header(&mut head, "Transfer-Encoding", "chunked");
            BodyEncoding::Chunked
        }
    };
    // No forced `Connection: close`: HTTP/1.1 defaults to keep-alive, which lets
    // the client pool and reuse the connection. A caller can still opt out by
    // setting `Connection: close` explicitly.
    head.push_str("\r\n");
    Ok((head.into_bytes(), encoding))
}

fn push_header(head: &mut String, name: &str, value: &str) {
    head.push_str(name);
    head.push_str(": ");
    head.push_str(value);
    head.push_str("\r\n");
}

/// Encodes a request with a buffered body in HTTP/1.1 form.
///
/// Returns the bytes to write first (the head, plus the body when it is small
/// or chunk-framed) and the rest of a large body, which is written straight
/// from the request without copying.
pub(crate) fn encode_request<'a>(
    req: &'a Request,
    wire: &RequestWire<'_>,
) -> Result<(Vec<u8>, &'a [u8]), HttpError> {
    let (mut head, encoding) = encode_request_head(req, wire, Some(req.body.len()))?;
    match encoding {
        BodyEncoding::Length(_) => {
            if req.body.len() <= 64 * 1024 {
                // One write for small requests: head and body in one segment.
                head.extend_from_slice(&req.body);
                Ok((head, &[]))
            } else {
                Ok((head, &req.body))
            }
        }
        BodyEncoding::Chunked => {
            // The caller asked for chunked framing of a buffered body.
            if !req.body.is_empty() {
                head.extend_from_slice(format!("{:X}\r\n", req.body.len()).as_bytes());
                head.extend_from_slice(&req.body);
                head.extend_from_slice(b"\r\n");
            }
            head.extend_from_slice(b"0\r\n\r\n");
            Ok((head, &[]))
        }
    }
}

/// Writes a request body read from `body`: chunked, or exactly the declared
/// number of bytes (a shorter stream is an error). Each read from `body` and
/// each write to `w` is bounded by `idle` when set.
pub(crate) async fn write_body_stream<W, B>(
    w: &mut W,
    encoding: BodyEncoding,
    body: &mut B,
    idle: Option<Duration>,
) -> Result<(), HttpError>
where
    W: AsyncWrite + Unpin,
    B: AsyncRead + Unpin + ?Sized,
{
    let mut buf = vec![0u8; BODY_PIECE_BYTES];
    match encoding {
        BodyEncoding::Chunked => loop {
            let n = bounded(idle, body.read(&mut buf)).await?;
            if n == 0 {
                bounded(idle, w.write_all(b"0\r\n\r\n")).await?;
                break;
            }
            let mut frame = Vec::with_capacity(n + 16);
            frame.extend_from_slice(format!("{n:X}\r\n").as_bytes());
            frame.extend_from_slice(&buf[..n]);
            frame.extend_from_slice(b"\r\n");
            bounded(idle, w.write_all(&frame)).await?;
        },
        BodyEncoding::Length(mut remaining) => {
            while remaining > 0 {
                let want = remaining.min(buf.len() as u64) as usize;
                let n = bounded(idle, body.read(&mut buf[..want])).await?;
                if n == 0 {
                    return Err(HttpError::Protocol(
                        "request body is shorter than its Content-Length".into(),
                    ));
                }
                bounded(idle, w.write_all(&buf[..n])).await?;
                remaining -= n as u64;
            }
        }
    }
    bounded(idle, w.flush()).await?;
    Ok(())
}

/// Awaits an I/O future, failing with [`HttpError::Timeout`] after `limit`.
pub(crate) async fn bounded<T>(
    limit: Option<Duration>,
    fut: impl std::future::Future<Output = std::io::Result<T>>,
) -> Result<T, HttpError> {
    match limit {
        Some(d) => match tokio::time::timeout(d, fut).await {
            Ok(r) => r.map_err(HttpError::Io),
            Err(_) => Err(HttpError::Timeout),
        },
        None => fut.await.map_err(HttpError::Io),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn req_head(raw: &[u8]) -> Result<RequestHead, HeadError> {
        let mut r = tokio::io::BufReader::new(raw);
        let mut started = false;
        read_request_head_with(&mut r, &HeadLimits::server(), &mut started).await
    }

    fn status_of(r: Result<RequestHead, HeadError>) -> u16 {
        match r {
            Err(HeadError::Bad { status, .. }) => status.as_u16(),
            Err(HeadError::Io(_)) => 0,
            Err(HeadError::Closed) => 1,
            Ok(_) => 200,
        }
    }

    #[tokio::test]
    async fn parses_a_well_formed_request_head() {
        let h = req_head(b"\r\nGET /a?b=1 HTTP/1.1\r\nHost: x\r\nX-A:\t v \r\n\r\n")
            .await
            .unwrap();
        assert_eq!(h.method, Method::Get);
        assert_eq!(h.target, "/a?b=1");
        assert_eq!(h.version, Version::Http11);
        assert_eq!(h.headers.get("x-a"), Some("v"));
        let h = req_head(b"GET / HTTP/1.0\r\n\r\n").await.unwrap();
        assert_eq!(h.version, Version::Http10);
    }

    #[tokio::test]
    async fn rejects_malformed_request_lines() {
        for raw in [
            &b"GET /\r\n\r\n"[..],
            b"GET  / HTTP/1.1\r\n\r\n",
            b"GET / HTTP/1.1 \r\n\r\n",
            b" GET / HTTP/1.1\r\n\r\n",
            b"G(T / HTTP/1.1\r\n\r\n",
            b"GET / HTTP/1.1 extra\r\n\r\n",
            b"GET / http/1.1\r\n\r\n",
            b"GET / HTTP/1\r\n\r\n",
            b"GET /a#f HTTP/1.1\r\n\r\n",
            b"GET noslash HTTP/1.1\r\n\r\n",
            b"GET /a\x01b HTTP/1.1\r\n\r\n",
            b"GET /\xc3\xa9 HTTP/1.1\r\n\r\n",
            b"GET * HTTP/1.1\r\n\r\n",
            b"\r\n\r\n\r\nGET / HTTP/1.1\r\n\r\n",
        ] {
            assert_eq!(
                status_of(req_head(raw).await),
                400,
                "{:?}",
                String::from_utf8_lossy(raw)
            );
        }
        assert_eq!(status_of(req_head(b"GET / HTTP/2.0\r\n\r\n").await), 505);
        assert_eq!(status_of(req_head(b"").await), 1);
    }

    #[tokio::test]
    async fn rejects_malformed_header_lines() {
        for raw in [
            &b"GET / HTTP/1.1\r\nHost : x\r\n\r\n"[..],
            b"GET / HTTP/1.1\r\nHost\t: x\r\n\r\n",
            b"GET / HTTP/1.1\r\nno-colon\r\n\r\n",
            b"GET / HTTP/1.1\r\nA: b\r\n folded\r\n\r\n",
            b"GET / HTTP/1.1\r\nA: b\r\n\tfolded\r\n\r\n",
            b"GET / HTTP/1.1\r\nA: b\rc\r\n\r\n",
            b"GET / HTTP/1.1\r\nA: b\0c\r\n\r\n",
            b"GET / HTTP/1.1\r\n: empty-name\r\n\r\n",
            b"GET / HTTP/1.1\nHost: x\n\n",
            b"GET / HTTP/1.1\r\nHost: x\n\r\n",
        ] {
            assert_eq!(
                status_of(req_head(raw).await),
                400,
                "{:?}",
                String::from_utf8_lossy(raw)
            );
        }
    }

    #[tokio::test]
    async fn absolute_form_and_special_targets() {
        let h = req_head(b"GET http://example.com:8080/p?q HTTP/1.1\r\nHost: other\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(h.target, "/p?q");
        assert_eq!(h.authority.as_deref(), Some("example.com:8080"));
        let h = req_head(b"GET http://example.com HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(h.target, "/");
        let h = req_head(b"OPTIONS * HTTP/1.1\r\n\r\n").await.unwrap();
        assert_eq!(h.target, "*");
        let h = req_head(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(h.target, "example.com:443");
    }

    #[tokio::test]
    async fn head_limits_are_enforced() {
        let limits = HeadLimits {
            max_line_bytes: 32,
            max_headers: 2,
            max_header_bytes: 48,
            allow_bare_lf: false,
        };
        let run = |raw: Vec<u8>| async move {
            let mut r = tokio::io::BufReader::new(&raw[..]);
            let mut started = false;
            status_of(read_request_head_with(&mut r, &limits, &mut started).await)
        };
        assert_eq!(
            run(b"GET / HTTP/1.1\r\nA: 1\r\nB: 2\r\n\r\n".to_vec()).await,
            200
        );
        assert_eq!(
            run(format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(40)).into_bytes()).await,
            414
        );
        assert_eq!(
            run(format!("GET / HTTP/1.1\r\nA: {}\r\n\r\n", "a".repeat(40)).into_bytes()).await,
            431
        );
        assert_eq!(
            run(b"GET / HTTP/1.1\r\nA: 1\r\nB: 2\r\nC: 3\r\n\r\n".to_vec()).await,
            431
        );
        assert_eq!(
            run(format!(
                "GET / HTTP/1.1\r\nA: {}\r\nB: {}\r\n\r\n",
                "a".repeat(20),
                "b".repeat(20)
            )
            .into_bytes())
            .await,
            431
        );
    }

    #[tokio::test]
    async fn response_status_lines() {
        let parse = |raw: &'static [u8]| async move {
            let mut r = tokio::io::BufReader::new(raw);
            let mut started = false;
            read_response_head_with(&mut r, &HeadLimits::client(), &mut started).await
        };
        assert_eq!(
            parse(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap().status.0,
            200
        );
        assert_eq!(parse(b"HTTP/1.1 204\r\n\r\n").await.unwrap().status.0, 204);
        // The client tolerates bare-LF line endings.
        let h = parse(b"HTTP/1.0 404 Not Found\nA: b\n\n").await.unwrap();
        assert_eq!(h.version, Version::Http10);
        assert_eq!(h.headers.get("a"), Some("b"));
        for bad in [
            &b"HTTP/1.1 20 OK\r\n\r\n"[..],
            b"HTTP/1.1 2000 OK\r\n\r\n",
            b"HTTP/2 200 OK\r\n\r\n",
            b"ICY 200 OK\r\n\r\n",
            b"HTTP/1.1  200 OK\r\n\r\n",
            b"HTTP/1.1 +20 OK\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nA : b\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nnocolon\r\n\r\n",
            b"",
        ] {
            assert!(
                parse(bad).await.is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn chunk_sizes_are_strict() {
        assert_eq!(parse_chunk_size(b"0", false).unwrap(), 0);
        assert_eq!(parse_chunk_size(b"1aF", false).unwrap(), 0x1af);
        assert_eq!(parse_chunk_size(b"5;ext=1", false).unwrap(), 5);
        assert_eq!(parse_chunk_size(b"5 ;ext", false).unwrap(), 5);
        assert_eq!(parse_chunk_size(b"5 ", true).unwrap(), 5);
        for bad in [
            &b"+5"[..],
            b"-5",
            b"0x5",
            b" 5",
            b"5 ",
            b"",
            b";ext",
            b"5g",
            b"11111111111111111",
            b"5;e\x01",
        ] {
            assert!(
                parse_chunk_size(bad, false).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    async fn dechunk(raw: &[u8], max: Option<u64>) -> Result<Vec<u8>, HttpError> {
        let mut r = tokio::io::BufReader::new(raw);
        let mut dec = ChunkedReader::new(max);
        let mut out = Vec::new();
        while let Some(piece) = dec.next(&mut r, &HeadLimits::server()).await? {
            out.extend_from_slice(&piece);
        }
        Ok(out)
    }

    #[tokio::test]
    async fn chunked_decoding_is_strict_and_bounded() {
        assert_eq!(
            dechunk(b"5\r\nhello\r\n6;x=y\r\n world\r\n0\r\nT: v\r\n\r\n", None)
                .await
                .unwrap(),
            b"hello world"
        );
        for bad in [
            &b"5\r\nhelloXX0\r\n\r\n"[..],
            b"5\r\nhello\n\n0\r\n\r\n",
            b"+5\r\nhello\r\n0\r\n\r\n",
            b"5\r\nhel",
            b"5\r\nhello\r\n",
            b"5\r\nhello\r\n0\r\n",
            b"5\nhello\r\n0\r\n\r\n",
            b"0\r\nbad trailer\r\n\r\n",
        ] {
            assert!(
                dechunk(bad, None).await.is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        assert!(matches!(
            dechunk(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n", Some(10)).await,
            Err(HttpError::BodyTooLarge(10))
        ));
        // An over-long chunk-size line is refused however small the body is.
        let mut raw = b"5;".to_vec();
        raw.extend(std::iter::repeat(b'a').take(MAX_CHUNK_LINE_BYTES));
        raw.extend_from_slice(b"\r\nhello\r\n0\r\n\r\n");
        assert!(dechunk(&raw, None).await.is_err());
        // So is an over-long trailer section.
        let mut raw = b"0\r\n".to_vec();
        for i in 0..200 {
            raw.extend_from_slice(format!("T{i}: v\r\n").as_bytes());
        }
        raw.extend_from_slice(b"\r\n");
        assert!(dechunk(&raw, None).await.is_err());
    }

    #[test]
    fn request_encoding_refuses_unsafe_requests() {
        let wire = RequestWire::default();
        let ok = Request::builder(Method::Post, "http://h/p")
            .unwrap()
            .build();
        let (head, enc) = encode_request_head(&ok, &wire, Some(0)).unwrap();
        let head = String::from_utf8(head).unwrap();
        assert!(head.starts_with("POST /p HTTP/1.1\r\nHost: h\r\n"));
        assert!(head.contains("Content-Length: 0\r\n"));
        assert_eq!(enc, BodyEncoding::Length(0));

        let mut bad = ok.clone();
        bad.method = Method::Other("GET / HTTP/1.1\r\nX: y\r\n\r\nGET".into());
        assert!(encode_request_head(&bad, &wire, Some(0)).is_err());

        let mut bad = ok.clone();
        bad.uri.path = "/a HTTP/1.1\r\nHost: evil\r\n\r\nGET /admin".into();
        assert!(encode_request_head(&bad, &wire, Some(0)).is_err());

        let mut bad = ok.clone();
        bad.headers.set("content-length", "7");
        assert!(encode_request_head(&bad, &wire, Some(3)).is_err());

        let mut bad = ok.clone();
        bad.headers.set("transfer-encoding", "gzip");
        assert!(encode_request_head(&bad, &wire, Some(3)).is_err());

        let mut bad = ok.clone();
        bad.headers.set("transfer-encoding", "chunked");
        bad.headers.set("content-length", "3");
        assert!(encode_request_head(&bad, &wire, Some(3)).is_err());

        // A streamed body is chunked unless a length is declared.
        let (head, enc) = encode_request_head(&ok, &wire, None).unwrap();
        assert!(String::from_utf8(head)
            .unwrap()
            .contains("Transfer-Encoding: chunked\r\n"));
        assert_eq!(enc, BodyEncoding::Chunked);

        // GET without a body carries no framing header at all.
        let get = Request::builder(Method::Get, "http://h/p#frag")
            .unwrap()
            .build();
        let wire = RequestWire {
            absolute_form: true,
            user_agent: Some("ua/1"),
            ..RequestWire::default()
        };
        let (head, _) = encode_request_head(&get, &wire, Some(0)).unwrap();
        assert_eq!(
            String::from_utf8(head).unwrap(),
            "GET http://h/p HTTP/1.1\r\nHost: h\r\nUser-Agent: ua/1\r\n\r\n"
        );
    }
}
