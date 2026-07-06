//! HTTP/1.1 wire encoding and decoding.

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::message::{Framing, Request};
use super::{HeaderMap, HttpError, Method, StatusCode};

/// Maximum bytes in the request/status line or a single header line. Bounds the
/// memory a hostile peer can force by never terminating a line.
const MAX_LINE_BYTES: usize = 16 * 1024;
/// Maximum number of header lines accepted.
const MAX_HEADERS: usize = 128;
/// Maximum total bytes across all header lines.
const MAX_HEADER_TOTAL_BYTES: usize = 64 * 1024;

/// Reads one `\n`-terminated line into `out`, failing if it exceeds `cap` bytes
/// (so a peer cannot force an unbounded allocation with an endless line).
/// Returns the number of bytes read; `0` means EOF with no data.
async fn read_line_capped<R: AsyncBufReadExt + Unpin>(
    r: &mut R,
    out: &mut String,
    cap: usize,
) -> Result<usize, HttpError> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let byte = match r.read_u8().await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        };
        buf.push(byte);
        if buf.len() > cap {
            return Err(HttpError::Protocol(
                "request or header line too long".into(),
            ));
        }
        if byte == b'\n' {
            break;
        }
    }
    out.push_str(&String::from_utf8_lossy(&buf));
    Ok(buf.len())
}

/// Writes a request in HTTP/1.1 origin form, adding `Host` and `Content-Length`
/// when the caller did not. Leaves `Connection` unset (HTTP/1.1 keep-alive) so
/// the client can pool the connection.
pub(crate) async fn write_request<W: AsyncWrite + Unpin>(
    w: &mut W,
    req: &Request,
) -> Result<(), HttpError> {
    let mut head = String::new();
    head.push_str(&format!(
        "{} {} HTTP/1.1\r\n",
        req.method.as_str(),
        req.uri.request_target()
    ));
    if !req.headers.contains("host") {
        head.push_str(&format!("Host: {}\r\n", req.uri.authority()));
    }
    for (k, v) in req.headers.iter() {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if !req.body.is_empty() && !req.headers.contains("content-length") {
        head.push_str(&format!("Content-Length: {}\r\n", req.body.len()));
    }
    // No forced `Connection: close`: HTTP/1.1 defaults to keep-alive, which lets
    // the client pool and reuse the connection. A caller can still opt out by
    // setting `Connection: close` explicitly.
    head.push_str("\r\n");

    w.write_all(head.as_bytes()).await?;
    if !req.body.is_empty() {
        w.write_all(&req.body).await?;
    }
    w.flush().await?;
    Ok(())
}

/// Reads a response status line and headers from a buffered reader.
pub(crate) async fn read_response_head<R: AsyncBufReadExt + Unpin>(
    r: &mut R,
) -> Result<(StatusCode, HeaderMap), HttpError> {
    let mut status_line = String::new();
    let n = read_line_capped(r, &mut status_line, MAX_LINE_BYTES).await?;
    if n == 0 {
        return Err(HttpError::Protocol("empty response".into()));
    }
    let mut parts = status_line.trim_end().splitn(3, ' ');
    let _version = parts.next();
    let code = parts
        .next()
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| HttpError::Protocol(format!("bad status line: {}", status_line.trim())))?;

    let headers = read_headers(r).await?;
    Ok((StatusCode(code), headers))
}

/// Reads a request line and headers (server side).
pub(crate) async fn read_request_head<R: AsyncBufReadExt + Unpin>(
    r: &mut R,
) -> Result<(Method, String, HeaderMap), HttpError> {
    let mut request_line = String::new();
    let n = read_line_capped(r, &mut request_line, MAX_LINE_BYTES).await?;
    if n == 0 {
        return Err(HttpError::Protocol("empty request".into()));
    }
    let mut parts = request_line.trim_end().split(' ');
    let method = parts
        .next()
        .map(Method::parse)
        .ok_or_else(|| HttpError::Protocol("no method".into()))?;
    let target = parts
        .next()
        .ok_or_else(|| HttpError::Protocol("no target".into()))?
        .to_string();
    let headers = read_headers(r).await?;
    Ok((method, target, headers))
}

async fn read_headers<R: AsyncBufReadExt + Unpin>(r: &mut R) -> Result<HeaderMap, HttpError> {
    let mut headers = HeaderMap::new();
    let mut total = 0usize;
    let mut count = 0usize;
    loop {
        let mut line = String::new();
        let n = read_line_capped(r, &mut line, MAX_LINE_BYTES).await?;
        if n == 0 {
            break;
        }
        total += n;
        if total > MAX_HEADER_TOTAL_BYTES {
            return Err(HttpError::Protocol("header block too large".into()));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        count += 1;
        if count > MAX_HEADERS {
            return Err(HttpError::Protocol("too many headers".into()));
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            headers.append(k.trim(), v.trim());
        }
    }
    Ok(headers)
}

/// Chooses the body framing for a response based on status and headers.
pub(crate) fn response_framing(status: StatusCode, headers: &HeaderMap) -> Framing {
    let code = status.as_u16();
    if code == 204 || code == 304 || (100..200).contains(&code) {
        return Framing::Done;
    }
    if headers.is_chunked() {
        return Framing::Chunked;
    }
    match headers.content_length() {
        Some(len) => Framing::Length(len),
        None => Framing::Eof,
    }
}
