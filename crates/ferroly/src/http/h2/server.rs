//! The server side of HTTP/2: turning streams into [`Request`]s for the same
//! [`HttpHandler`] that serves HTTP/1.1, and writing its [`HttpResponse`]
//! back as HEADERS, DATA and trailing HEADERS.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::SystemTime;

use tokio::io::BufReader;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::AbortHandle;

use super::conn::{
    self, is_connection_specific, trim_value, valid_name, valid_value, Cmd, Options, Shared,
};
use super::error::Reason;
use super::hpack::Field;
use super::Http2Config;
use crate::http::core::{parse_authority, split_target};
use crate::http::date::format_http_date;
use crate::http::form::normalize_path;
use crate::http::message::ConnInfo;
use crate::http::server::{run_handler, ResponseBody};
use crate::http::transport::Conn;
use crate::http::{
    HeaderMap, HttpError, HttpHandler, HttpResponse, Method, Protocol, Request, ServerConfig,
    StatusCode, Uri,
};

/// What the connection engine needs to dispatch requests.
pub(crate) struct ServerCtx {
    pub(crate) handler: Arc<dyn HttpHandler>,
    pub(crate) config: Arc<ServerConfig>,
    pub(crate) info: ConnInfo,
    pub(crate) cmd_tx: mpsc::UnboundedSender<Cmd>,
}

impl ServerCtx {
    pub(crate) fn report(&self, error: &HttpError) {
        self.config.report(self.info.peer_addr, error);
    }

    /// Asks the handler whether it reads this request's body as a stream
    /// (a panic counts as "no").
    pub(crate) fn wants_body_stream(&self, req: &Request) -> bool {
        std::panic::catch_unwind(AssertUnwindSafe(|| self.handler.wants_body_stream(req)))
            .unwrap_or(false)
    }

    /// Runs the handler for stream `id` on its own task and writes the
    /// response. The task always reports back with `TaskDone`, even when it
    /// is aborted — including before its first poll, when the future is
    /// dropped without running: the guard is created here and moved into
    /// the future, so dropping the future drops it.
    pub(crate) fn spawn(&self, id: u32, req: Request, head_request: bool) -> AbortHandle {
        let handler = self.handler.clone();
        let config = self.config.clone();
        let peer = self.info.peer_addr;
        let out = StreamOut {
            id,
            tx: self.cmd_tx.clone(),
        };
        let done = TaskDone(out.tx.clone(), id);
        tokio::spawn(async move {
            let _done = done;
            let resp = match run_handler(&handler, req, config.handler_timeout).await {
                Ok(resp) => resp,
                Err(e) => {
                    config.report(peer, &e);
                    match e {
                        HttpError::Timeout => {
                            HttpResponse::text(StatusCode::SERVICE_UNAVAILABLE, "handler timed out")
                        }
                        _ => HttpResponse::text(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "internal server error",
                        ),
                    }
                }
            };
            // A failure here means the stream is gone (reset or the
            // connection closed); there is no one left to tell.
            let _ = respond(&out, resp, head_request).await;
        })
        .abort_handle()
    }
}

/// Tells the connection that a stream's handler task has ended.
struct TaskDone(mpsc::UnboundedSender<Cmd>, u32);

impl Drop for TaskDone {
    fn drop(&mut self) {
        let _ = self.0.send(Cmd::TaskDone { id: self.1 });
    }
}

/// Serves one HTTP/2 connection (after ALPN `h2`, or a cleartext connection
/// that opened with the preface) until it closes or the server drains.
pub(crate) async fn serve(
    io: BufReader<Conn>,
    handler: Arc<dyn HttpHandler>,
    config: Arc<ServerConfig>,
    drain: watch::Receiver<bool>,
    info: ConnInfo,
    h2: &Http2Config,
) -> Result<(), HttpError> {
    let h2 = h2.normalized();
    let max_list_size = (h2.max_header_list_size as usize).min(config.max_header_bytes.max(1024));
    let opts = Options {
        server: true,
        max_list_size,
        // Room for the four request pseudo-headers.
        max_fields: config.max_headers.saturating_add(4),
        idle_timeout: Some(config.idle_timeout),
        write_timeout: Some(config.write_timeout),
        head_timeout: Some(config.head_timeout),
        body_timeout: Some(config.body_timeout),
        // Its own setting: `write_timeout` is about the socket, this about
        // one stream's consumer (HTTP/1.1's `max_keep_alive_requests` does
        // not apply either).
        stall_timeout: h2.stream_stall_timeout,
        max_requests: h2.max_streams_per_connection,
        // Below `max_body_bytes`, one upload of the largest allowed size
        // would wait for credit that never comes.
        body_budget: h2.max_buffered_body_bytes.max(config.max_body_bytes),
        h2,
    };
    let (tx, rx) = mpsc::unbounded_channel();
    let ctx = ServerCtx {
        handler,
        config,
        info,
        cmd_tx: tx,
    };
    let shared = Arc::new(Shared::new());
    let (conn, reader) = conn::start(io, Arc::new(opts), shared, Some(ctx));
    conn.run(reader, rx, Some(drain)).await
}

/// Why a request head cannot be dispatched.
pub(crate) enum Reject {
    /// The request is malformed (RFC 9113 §8.1.1): reset with PROTOCOL_ERROR.
    Malformed(&'static str),
    /// Answer with this status and text.
    Respond(StatusCode, &'static str),
}

/// Builds the [`Request`] for a decoded request header list. Returns it and
/// whether it is a `HEAD` request.
pub(crate) fn build_request(fields: &[Field], ctx: &ServerCtx) -> Result<(Request, bool), Reject> {
    let mut method = None;
    let mut scheme = None;
    let mut authority = None;
    let mut path = None;
    let mut regular = false;
    let mut headers = HeaderMap::new();
    let mut cookies: Vec<String> = Vec::new();
    for (name, value) in fields {
        if !valid_value(value) {
            return Err(Reject::Malformed("invalid header value"));
        }
        let text = || String::from_utf8_lossy(value).into_owned();
        if name.first() == Some(&b':') {
            if regular {
                return Err(Reject::Malformed("pseudo-header after a regular header"));
            }
            let slot = match name.as_slice() {
                b":method" => &mut method,
                b":scheme" => &mut scheme,
                b":authority" => &mut authority,
                b":path" => &mut path,
                _ => return Err(Reject::Malformed("unknown or response pseudo-header")),
            };
            if slot.is_some() {
                return Err(Reject::Malformed("duplicate pseudo-header"));
            }
            *slot = Some(text());
            continue;
        }
        regular = true;
        if !valid_name(name) {
            return Err(Reject::Malformed("invalid header name"));
        }
        let name = String::from_utf8_lossy(name).into_owned();
        if is_connection_specific(&name) {
            return Err(Reject::Malformed("connection-specific header field"));
        }
        if name == "te" && !value.eq_ignore_ascii_case(b"trailers") {
            return Err(Reject::Malformed("TE other than \"trailers\""));
        }
        if name == "cookie" {
            // HTTP/2 may split cookies across fields; HTTP/1.1 handlers
            // expect one header (RFC 9113 §8.2.3).
            cookies.push(text());
            continue;
        }
        headers.push_validated(name, text());
    }
    if !cookies.is_empty() {
        headers.push_validated("cookie".to_string(), cookies.join("; "));
    }
    let method = method.ok_or(Reject::Malformed("missing :method"))?;
    let method = Method::try_parse(&method).map_err(|_| Reject::Malformed("invalid :method"))?;
    if matches!(method, Method::Connect) {
        return Err(Reject::Respond(
            StatusCode::NOT_IMPLEMENTED,
            "CONNECT is not supported over HTTP/2",
        ));
    }
    let scheme = scheme.ok_or(Reject::Malformed("missing :scheme"))?;
    if !valid_scheme(&scheme) {
        return Err(Reject::Malformed("invalid :scheme"));
    }
    let target = path.ok_or(Reject::Malformed("missing :path"))?;
    let visible = target.bytes().all(|b| (0x21..0x7f).contains(&b));
    let origin_form = target.starts_with('/');
    let asterisk = target == "*" && matches!(method, Method::Options);
    if !visible || !(origin_form || asterisk) {
        return Err(Reject::Malformed("invalid :path"));
    }
    if let (Some(a), Some(h)) = (&authority, headers.get("host")) {
        if !a.eq_ignore_ascii_case(h) {
            return Err(Reject::Malformed(":authority and host disagree"));
        }
    }
    let host = match &authority {
        Some(a) => a.clone(),
        None => headers.get("host").unwrap_or("").to_string(),
    };
    // RFC 9113 §8.3.1: no userinfo; and the same host[:port] syntax
    // HTTP/1.1 accepts, so handlers see the same `Host` either way.
    if !host.is_empty() && (host.contains('@') || parse_authority(&host, 1).is_none()) {
        return Err(Reject::Malformed("invalid :authority"));
    }
    if headers.get_all("host").count() > 1 {
        return Err(Reject::Malformed("duplicate host header"));
    }
    if !headers.contains("host") && !host.is_empty() {
        headers.push_validated("host".to_string(), host.clone());
    }
    // Validated here; checked against `max_body_bytes` by the connection,
    // and only for buffered bodies.
    headers
        .content_length_checked()
        .map_err(|_| Reject::Malformed("invalid content-length"))?;
    let (mut path, query) = split_target(&target);
    if ctx.config.normalize_paths && origin_form {
        path = normalize_path(&path)
            .map_err(|_| Reject::Respond(StatusCode::BAD_REQUEST, "invalid request path"))?;
    }
    let head_request = matches!(method, Method::Head);
    let mut req = Request::new(
        method,
        Uri {
            scheme: String::new(),
            host,
            port: 0,
            path,
            query,
        },
    );
    req.headers = headers;
    req.set_conn_info(ctx.info.clone());
    req.set_protocol(Protocol::Http2);
    Ok((req, head_request))
}

/// `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )` (RFC 3986 §3.1).
fn valid_scheme(scheme: &str) -> bool {
    let mut bytes = scheme.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

/// A response writer for one stream.
pub(crate) struct StreamOut {
    pub(crate) id: u32,
    pub(crate) tx: mpsc::UnboundedSender<Cmd>,
}

use super::error::closed;

impl StreamOut {
    fn headers(&self, fields: Vec<(String, String)>, end_stream: bool) -> Result<(), HttpError> {
        self.tx
            .send(Cmd::Headers {
                id: self.id,
                fields,
                end_stream,
            })
            .map_err(|_| closed())
    }

    /// Sends data and waits until the connection has taken it.
    pub(crate) async fn data(&self, data: Vec<u8>, end_stream: bool) -> Result<(), HttpError> {
        let (ack, done) = oneshot::channel();
        self.tx
            .send(Cmd::Data {
                id: self.id,
                data,
                end_stream,
                ack: Some(ack),
            })
            .map_err(|_| closed())?;
        done.await.unwrap_or_else(|_| Err(closed()))
    }

    pub(crate) fn trailers(&self, trailers: &HeaderMap) -> Result<(), HttpError> {
        self.tx
            .send(Cmd::Trailers {
                id: self.id,
                fields: trailer_fields(trailers),
            })
            .map_err(|_| closed())
    }

    pub(crate) fn reset(&self, reason: Reason) {
        let _ = self.tx.send(Cmd::Reset {
            id: self.id,
            reason,
        });
    }
}

/// Regular fields for HTTP/2: lower-case names, without pseudo-headers or
/// connection-specific fields, and values without surrounding whitespace.
pub(crate) fn h2_fields<'a>(
    headers: &'a HeaderMap,
    skip: &'a [&'a str],
) -> impl Iterator<Item = (String, String)> + 'a {
    headers.iter().filter_map(move |(k, v)| {
        let name = k.to_ascii_lowercase();
        if name.starts_with(':') || is_connection_specific(&name) || skip.contains(&name.as_str()) {
            return None;
        }
        Some((name, trim_value(v)))
    })
}

pub(crate) fn trailer_fields(trailers: &HeaderMap) -> Vec<(String, String)> {
    h2_fields(trailers, &["content-length", "host"]).collect()
}

/// Writes a handler's response on its stream.
async fn respond(out: &StreamOut, resp: HttpResponse, head_request: bool) -> Result<(), HttpError> {
    let parts = resp.into_parts();
    let mut code = parts.status.as_u16();
    if (100..200).contains(&code) {
        // An informational status cannot be a final HTTP/2 response.
        code = 500;
    }
    let no_content = matches!(code, 204 | 304);
    let bodyless = head_request || no_content;
    let trailers = parts.trailers;
    let mut fields = vec![(":status".to_string(), code.to_string())];
    let skip: &[&str] = if head_request {
        &[]
    } else {
        &["content-length"]
    };
    fields.extend(h2_fields(&parts.headers, skip));
    if !parts.headers.contains("date") {
        fields.push(("date".to_string(), format_http_date(SystemTime::now())));
    }
    if bodyless {
        return out.headers(fields, true);
    }
    match parts.body {
        ResponseBody::Fixed(body) => {
            if trailers.is_empty() {
                fields.push(("content-length".to_string(), body.len().to_string()));
            }
            if body.is_empty() && trailers.is_empty() {
                return out.headers(fields, true);
            }
            out.headers(fields, false)?;
            if !body.is_empty() {
                out.data(body, trailers.is_empty()).await?;
            }
            if !trailers.is_empty() {
                out.trailers(&trailers)?;
            }
            Ok(())
        }
        ResponseBody::Chunks(mut rx) => {
            out.headers(fields, false)?;
            while let Some(chunk) = rx.recv().await {
                if !chunk.is_empty() {
                    out.data(chunk, false).await?;
                }
            }
            finish(out, trailers).await
        }
        ResponseBody::Stream(mut stream) => {
            out.headers(fields, false)?;
            loop {
                match stream.chunk().await {
                    Ok(Some(chunk)) => out.data(chunk, false).await?,
                    Ok(None) => break,
                    Err(e) => {
                        out.reset(Reason::INTERNAL_ERROR);
                        return Err(e);
                    }
                }
            }
            let mut all = trailers;
            if let Some(more) = stream.take_trailers() {
                for (k, v) in more.iter() {
                    all.append(k, v);
                }
            }
            finish(out, all).await
        }
    }
}

/// Ends a streamed body: trailers when there are any, else an empty
/// END_STREAM DATA frame.
async fn finish(out: &StreamOut, trailers: HeaderMap) -> Result<(), HttpError> {
    if trailers.is_empty() {
        out.data(Vec::new(), true).await
    } else {
        out.trailers(&trailers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::BoxFuture;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct Ok200;
    impl HttpHandler for Ok200 {
        fn handle(&self, _req: Request) -> BoxFuture<'_, HttpResponse> {
            Box::pin(async { HttpResponse::ok() })
        }
    }

    /// Runs `serve` over an in-memory pipe (as after ALPN `h2`), writes
    /// `input`, and returns everything the server wrote before closing.
    async fn exchange(input: &[u8]) -> Vec<u8> {
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let info = ConnInfo {
            peer_addr: None,
            local_addr: None,
            tls: true,
            peer_certs: Arc::new(Vec::new()),
        };
        let (_drain_tx, drain) = watch::channel(false);
        let server = tokio::spawn(async move {
            let io = BufReader::new(Box::new(theirs) as Conn);
            let config = Arc::new(ServerConfig::default());
            serve(
                io,
                Arc::new(Ok200),
                config,
                drain,
                info,
                &Http2Config::default(),
            )
            .await
        });
        let (mut r, mut w) = tokio::io::split(ours);
        w.write_all(input).await.unwrap();
        let mut out = Vec::new();
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(5), r.read_to_end(&mut out)).await;
        drop(w);
        let _ = server.await;
        out
    }

    /// The error code of the first GOAWAY in `bytes`, if any.
    fn goaway_code(mut bytes: &[u8]) -> Option<u32> {
        while bytes.len() >= 9 {
            let len = (bytes[0] as usize) << 16 | (bytes[1] as usize) << 8 | bytes[2] as usize;
            let payload = bytes.get(9..9 + len)?;
            if bytes[3] == super::super::frame::kind::GOAWAY {
                return Some(u32::from_be_bytes(payload[4..8].try_into().ok()?));
            }
            bytes = &bytes[9 + len..];
        }
        None
    }

    #[tokio::test]
    async fn bad_preface_after_alpn_is_goaway() {
        let out = exchange(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await;
        // SETTINGS first, then GOAWAY(PROTOCOL_ERROR).
        assert_eq!(goaway_code(&out), Some(0x1));
    }

    #[tokio::test]
    async fn good_preface_then_settings_is_served() {
        let mut input = super::super::frame::PREFACE.to_vec();
        super::super::frame::encode_settings(&mut input, &[]);
        // GOAWAY(NO_ERROR) from the client side ends the connection.
        super::super::frame::encode_goaway(&mut input, 0, Reason::NO_ERROR, b"");
        let out = exchange(&input).await;
        // The server's SETTINGS came first, and it ended cleanly.
        assert_eq!(out[3], super::super::frame::kind::SETTINGS);
        assert_eq!(goaway_code(&out), Some(0x0));
    }

    /// A literal HPACK field without indexing, new name, no Huffman.
    fn lit(out: &mut Vec<u8>, name: &str, value: &str) {
        out.push(0x00);
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
        out.push(value.len() as u8);
        out.extend_from_slice(value.as_bytes());
    }

    fn raw_frame(out: &mut Vec<u8>, kind: u8, flags: u8, stream: u32, payload: &[u8]) {
        let len = payload.len();
        out.extend_from_slice(&[(len >> 16) as u8, (len >> 8) as u8, len as u8, kind, flags]);
        out.extend_from_slice(&stream.to_be_bytes());
        out.extend_from_slice(payload);
    }

    /// Random but mostly well-formed frame sequences, so the fuzzer's
    /// entry point is driven through stream states (open, half-closed,
    /// reset, closed) and not just the frame decoder.
    #[test]
    fn connection_fuzz_entry_point_survives_frame_sequences() {
        use super::super::frame::kind;
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = move |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n.max(1)
        };
        let started = std::time::Instant::now();
        for _ in 0..150 {
            let mut input = Vec::new();
            raw_frame(&mut input, kind::SETTINGS, 0, 0, &[]);
            for _ in 0..next(40) {
                let id = [1u32, 3, 5, 7, 9, 2, 0, 101][next(8) as usize];
                match next(11) {
                    0..=2 => {
                        let path = ["/", "/s", "/s/t", "/t"][next(4) as usize];
                        let method = ["GET", "POST", "HEAD"][next(3) as usize];
                        let mut block = Vec::new();
                        lit(&mut block, ":method", method);
                        lit(&mut block, ":scheme", "http");
                        lit(&mut block, ":authority", "fuzz");
                        lit(&mut block, ":path", path);
                        if next(3) == 0 {
                            lit(&mut block, "content-length", &next(5000).to_string());
                        }
                        let end_headers = if next(6) == 0 { 0 } else { 0x4 };
                        raw_frame(
                            &mut input,
                            kind::HEADERS,
                            end_headers | next(2) as u8,
                            id,
                            &block,
                        );
                    }
                    3 => raw_frame(&mut input, kind::CONTINUATION, 0x4, id, &[0x82]),
                    4..=5 => {
                        let data = vec![b'd'; next(3000) as usize];
                        raw_frame(&mut input, kind::DATA, next(2) as u8, id, &data);
                    }
                    6 => {
                        let code = (next(14) as u32).to_be_bytes();
                        raw_frame(&mut input, kind::RST_STREAM, 0, id, &code);
                    }
                    7 => {
                        let inc = (next(1 << 20) as u32).to_be_bytes();
                        raw_frame(&mut input, kind::WINDOW_UPDATE, 0, id % 3, &inc);
                    }
                    8 => {
                        let setting = [0, (next(7) + 1) as u8, 0, 0, next(256) as u8, 0];
                        raw_frame(&mut input, kind::SETTINGS, next(2) as u8, 0, &setting);
                    }
                    9 => raw_frame(&mut input, kind::PING, next(2) as u8, 0, &[7; 8]),
                    _ => {
                        let garbage: Vec<u8> = (0..next(20)).map(|_| next(256) as u8).collect();
                        input.extend_from_slice(&garbage);
                    }
                }
            }
            super::super::fuzz_connection(&input);
        }
        // Each run ends on its own (no hang waiting for timers).
        assert!(started.elapsed() < std::time::Duration::from_secs(60));
    }

    #[test]
    fn fuzz_entry_point_survives_garbage() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for round in 0..2000 {
            let len = (round % 200) + 9;
            let data: Vec<u8> = (0..len)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect();
            super::super::fuzz_frames(&data);
        }
    }
}
