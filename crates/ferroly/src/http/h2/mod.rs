//! HTTP/2 (RFC 9113) for the server and the client, hand-rolled.
//!
//! - [`frame`]: the frame codec, every frame type, size caps and padding.
//! - [`hpack`] / [`huffman`]: header compression (RFC 7541).
//! - [`conn`]: the connection engine shared by both roles — the stream state
//!   machine, connection and stream flow control, SETTINGS / PING / GOAWAY /
//!   RST_STREAM handling and the flood defences.
//! - [`server`] / [`client`]: how the HTTP server and [`Client`](super::Client)
//!   drive that engine.
//!
//! Only [`Http2Config`] is public; every frame and connection type stays
//! inside the crate. Handlers, requests and responses are the same types as
//! for HTTP/1.1.

use std::time::Duration;

use tokio::sync::mpsc;

use super::{HeaderMap, Http2Cause, HttpError};

pub(crate) mod client;
pub(crate) mod conn;
pub(crate) mod error;
pub(crate) mod frame;
pub(crate) mod hpack;
pub(crate) mod huffman;
pub(crate) mod server;

use conn::Cmd;
use error::Reason;

/// HTTP/2 settings and limits for a server ([`ServerConfig::http2`]) or a
/// client ([`ClientBuilder::http2_config`]).
///
/// The defaults are safe for direct exposure. The struct is
/// `#[non_exhaustive]`: start from [`Http2Config::default`] and assign the
/// fields you want to change.
///
/// ```
/// use ferroly::http::{Http2Config, ServerConfig};
///
/// let mut h2 = Http2Config::default();
/// h2.max_concurrent_streams = 250;
/// let config = ServerConfig {
///     http2: Some(h2),
///     ..ServerConfig::default()
/// };
/// # let _ = config;
/// ```
///
/// [`ServerConfig::http2`]: super::ServerConfig::http2
/// [`ClientBuilder::http2_config`]: super::ClientBuilder::http2_config
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Http2Config {
    /// Most streams the peer may have open at once
    /// (`SETTINGS_MAX_CONCURRENT_STREAMS`). Further streams are refused with
    /// `REFUSED_STREAM`. A stream the peer resets keeps counting until its
    /// handler has actually stopped. Default 100.
    pub max_concurrent_streams: u32,
    /// The receive window of each stream (`SETTINGS_INITIAL_WINDOW_SIZE`),
    /// in bytes: how much body data the peer may send before this side has
    /// consumed it. Clamped to `65_535..=2^31-1`. Default 1 MiB.
    pub initial_stream_window_size: u32,
    /// The receive window of the whole connection, in bytes. Clamped to
    /// `65_535..=2^31-1`. Default 4 MiB.
    pub initial_connection_window_size: u32,
    /// Largest frame payload accepted (`SETTINGS_MAX_FRAME_SIZE`); a larger
    /// frame is a connection error. Clamped to `16_384..=16_777_215`.
    /// Default 16 384.
    pub max_frame_size: u32,
    /// Largest decoded header list accepted (`SETTINGS_MAX_HEADER_LIST_SIZE`,
    /// counted as name + value + 32 bytes per field). A larger request is
    /// answered with `431`; a server also applies
    /// [`ServerConfig::max_header_bytes`](super::ServerConfig::max_header_bytes)
    /// when that is smaller. The compressed header block (HEADERS plus all its
    /// CONTINUATION frames) is capped at twice this size and 64 frames, so a
    /// CONTINUATION flood cannot grow memory. Default 64 KiB.
    pub max_header_list_size: u32,
    /// Size of the HPACK dynamic table the peer may use to compress the
    /// headers it sends (`SETTINGS_HEADER_TABLE_SIZE`). Clamped to
    /// `4096..=1 MiB`: a peer may use the protocol default of 4096 until it
    /// has seen this side's SETTINGS, so less cannot be enforced. Default
    /// 4096.
    pub header_table_size: u32,
    /// Most stream resets that count as abuse within one
    /// [`flood_window`](Self::flood_window). Exceeding it closes the
    /// connection with `GOAWAY(ENHANCE_YOUR_CALM)` — the defence against
    /// "rapid reset" (CVE-2023-44487) and "MadeYouReset" (CVE-2025-8671).
    /// Default 200.
    ///
    /// What counts:
    ///
    /// - a server counts an RST_STREAM from the client only when it arrives
    ///   within [`rapid_reset_threshold`](Self::rapid_reset_threshold) of
    ///   the stream's opening — the rapid-reset pattern. Ordinary
    ///   cancellations (a gRPC deadline, a client that gives up on a slow or
    ///   long-lived stream) and resets of completed streams never count;
    /// - a client never counts the server's resets of its own streams
    ///   (`NO_ERROR` after an early response, `REFUSED_STREAM`, `CANCEL`):
    ///   it chooses how many streams it opens;
    /// - both sides count every stream they reset themselves because the
    ///   peer broke a rule on it (flow control, a malformed or truncated
    ///   message). A stream refused for exceeding
    ///   [`max_concurrent_streams`](Self::max_concurrent_streams) is not
    ///   counted here: before the client has acknowledged this side's
    ///   SETTINGS it is not counted at all (the client may not know the
    ///   limit yet), afterwards it counts as a control frame.
    pub max_resets_per_window: u32,
    /// How young a stream must be for the client's RST_STREAM of it to
    /// count toward [`max_resets_per_window`](Self::max_resets_per_window)
    /// (server side). `Duration::ZERO` counts no client reset there (the
    /// stream limit, which a reset stream keeps counting against until its
    /// handler has stopped, still applies). Resets of older streams are
    /// still bounded by
    /// [`max_cancels_per_window`](Self::max_cancels_per_window): a client
    /// that holds every stream just past this threshold could otherwise
    /// start `max_concurrent_streams / rapid_reset_threshold` handlers per
    /// second. Default 100 ms.
    pub rapid_reset_threshold: Duration,
    /// Server: most client resets of streams whose response is not yet
    /// complete, whatever their age, within one
    /// [`flood_window`](Self::flood_window) — or twice the streams completed
    /// normally in that window, when that is more. Exceeding it closes the
    /// connection with `GOAWAY(ENHANCE_YOUR_CALM)`.
    ///
    /// This is what keeps the stream limit a bound on the work a client
    /// can start: a handler stops when its stream is reset, but work it
    /// handed off (a spawned task, a blocking job, a query on another
    /// server) may not, so a client that opens and cancels streams in a
    /// loop could keep far more of it running than
    /// [`max_concurrent_streams`](Self::max_concurrent_streams). The default
    /// allows about `max_concurrent_streams` cancellations per second with
    /// no request completing, far more than deadline expiries and
    /// abandoned requests produce; a client that also completes requests
    /// may cancel twice as many as it completes. Default 1000.
    pub max_cancels_per_window: u32,
    /// Most control frames that make this side do work without carrying a
    /// request — SETTINGS, PING, PRIORITY, empty DATA, unknown frame types,
    /// HEADERS for already-closed streams, streams refused for exceeding the
    /// stream limit, and RST_STREAM or WINDOW_UPDATE for streams closed so
    /// long ago that they are forgotten — accepted within one
    /// [`flood_window`](Self::flood_window) before the connection is closed
    /// with `GOAWAY(ENHANCE_YOUR_CALM)`. RST_STREAM and WINDOW_UPDATE for a
    /// stream closed a moment ago (the last 256) are routine and free: a
    /// client returns window for response bytes it reads after the response
    /// has ended. Default 1000.
    pub max_control_frames_per_window: u32,
    /// The window over which the two flood budgets above are counted.
    /// Default 10 s.
    pub flood_window: Duration,
    /// Server: most request-body bytes one connection holds in memory for
    /// handlers that take a buffered body, across all its streams (at
    /// least `ServerConfig::max_body_bytes`).
    ///
    /// Bodies are admitted to this budget in arrival order by their
    /// declared `content-length` (or `max_body_bytes` when they have none).
    /// An admitted body gets flow-control credit as usual and always
    /// finishes; one that does not fit yet waits, with only the stream
    /// window the client started with, until earlier bodies are done.
    /// Beyond the budget the connection stops returning flow-control credit
    /// to the client, which can then send at most one connection window
    /// more, so one connection holds at most `max(max_buffered_body_bytes,
    /// ServerConfig::max_body_bytes) + initial_connection_window_size`
    /// bytes of bodies that are still arriving (about 20 MiB with the
    /// defaults, instead of `max_concurrent_streams × max_body_bytes`).
    /// Only a client that fills that window with waiting bodies gets the
    /// youngest of them refused (`REFUSED_STREAM`, which clients may retry),
    /// so the admitted ones can finish. Waiting is bounded by
    /// `ServerConfig::body_timeout`. Default 16 MiB.
    pub max_buffered_body_bytes: usize,
    /// Server: after this many streams on one connection, finish it with a
    /// graceful GOAWAY so the client moves to a new one. `None` (the
    /// default) never does. `ServerConfig::max_keep_alive_requests` applies
    /// only to HTTP/1.1.
    ///
    /// The GOAWAY is two-phase (RFC 9113 §6.8): first one with the largest
    /// stream id and a PING, then, once the PING is acknowledged (or after
    /// a second), the final one with the real last stream id. Requests the
    /// client sent in the meantime are still served, not refused.
    pub max_streams_per_connection: Option<u64>,
    /// Server: reset a stream (`CANCEL`) when its response output has been
    /// blocked by the client's flow control for this long — a client that
    /// stops reading a response. Blocked time adds up across short bursts of
    /// progress and is forgotten only once the response has sent 16 KiB
    /// more, so a client that grants a byte of window now and then cannot
    /// hold a stream for ever. `None` disables it: a stream then waits
    /// as long as the client keeps the connection alive (the connection's
    /// write timeout still applies when the whole connection stops
    /// draining). Long-lived streams to slow consumers (gRPC server or
    /// bidirectional streaming, server-sent events) may want `None` or a
    /// longer value. Default 30 s.
    pub stream_stall_timeout: Option<Duration>,
    /// Send a PING when the peer has shown no sign of life for this long
    /// while streams are open, and close the connection when it does not
    /// answer within [`keepalive_timeout`](Self::keepalive_timeout): the
    /// failure is [`Http2Cause::Timeout`](super::Http2Cause::Timeout). A
    /// sign of life is any frame from the peer, or output draining from a
    /// full socket (the peer is reading a large response), so a slow but
    /// live reader is not taken for a dead one. This detects a dead peer (a
    /// crashed host, a dropped NAT mapping) that would otherwise hang a
    /// stream with no deadline. `None` (the default) sends none.
    ///
    /// Some servers limit PING frequency; grpc-go, by default, closes
    /// connections that ping more often than every 5 minutes.
    pub keepalive_interval: Option<Duration>,
    /// How long to wait for the acknowledgement of a keep-alive PING, and
    /// for the peer to acknowledge this side's SETTINGS (a connection whose
    /// SETTINGS are not acknowledged in time is closed with
    /// `GOAWAY(SETTINGS_TIMEOUT)`). Default 20 s.
    pub keepalive_timeout: Duration,
    /// Client: most connections to one origin. When every connection to an
    /// origin has as many open streams as the server allows
    /// (`SETTINGS_MAX_CONCURRENT_STREAMS`), the client dials another one,
    /// up to this many; beyond that, requests wait for a free stream slot
    /// (bounded by the client's `timeout`). Default 4.
    pub max_connections_per_origin: usize,
}

impl Default for Http2Config {
    fn default() -> Self {
        Self {
            max_concurrent_streams: 100,
            initial_stream_window_size: 1024 * 1024,
            initial_connection_window_size: 4 * 1024 * 1024,
            max_frame_size: frame::DEFAULT_MAX_FRAME_SIZE,
            max_header_list_size: 64 * 1024,
            header_table_size: 4096,
            max_resets_per_window: 200,
            rapid_reset_threshold: Duration::from_millis(100),
            max_cancels_per_window: 1000,
            max_control_frames_per_window: 1000,
            flood_window: Duration::from_secs(10),
            max_buffered_body_bytes: 16 * 1024 * 1024,
            max_streams_per_connection: None,
            stream_stall_timeout: Some(Duration::from_secs(30)),
            keepalive_interval: None,
            keepalive_timeout: Duration::from_secs(20),
            max_connections_per_origin: 4,
        }
    }
}

impl Http2Config {
    /// The settings with every value brought into its legal range.
    pub(crate) fn normalized(&self) -> Http2Config {
        let window = |v: u32| v.clamp(frame::DEFAULT_WINDOW, frame::MAX_WINDOW);
        Http2Config {
            max_concurrent_streams: self.max_concurrent_streams.max(1),
            initial_stream_window_size: window(self.initial_stream_window_size),
            initial_connection_window_size: window(self.initial_connection_window_size),
            max_frame_size: self
                .max_frame_size
                .clamp(frame::DEFAULT_MAX_FRAME_SIZE, frame::MAX_MAX_FRAME_SIZE),
            max_header_list_size: self.max_header_list_size.max(1024),
            header_table_size: self.header_table_size.clamp(4096, 1 << 20),
            max_resets_per_window: self.max_resets_per_window.max(1),
            rapid_reset_threshold: self.rapid_reset_threshold,
            max_cancels_per_window: self.max_cancels_per_window.max(1),
            max_control_frames_per_window: self.max_control_frames_per_window.max(16),
            flood_window: self.flood_window.max(Duration::from_millis(100)),
            max_buffered_body_bytes: self.max_buffered_body_bytes,
            max_streams_per_connection: self.max_streams_per_connection.map(|n| n.max(1)),
            stream_stall_timeout: self
                .stream_stall_timeout
                .map(|d| d.max(Duration::from_millis(10))),
            keepalive_interval: self
                .keepalive_interval
                .map(|d| d.max(Duration::from_millis(10))),
            keepalive_timeout: self.keepalive_timeout.max(Duration::from_millis(10)),
            max_connections_per_origin: self.max_connections_per_origin.max(1),
        }
    }
}

/// Something that happened on a stream, delivered to whoever consumes it
/// (a client response, or a server handler's streamed request body).
#[derive(Debug)]
pub(crate) enum Event {
    /// The final response head (client side only).
    Head {
        status: u16,
        headers: HeaderMap,
    },
    Data(Vec<u8>),
    Trailers(HeaderMap),
    /// The peer ended the stream cleanly.
    End,
    /// The stream was refused before the peer processed it (REFUSED_STREAM,
    /// or above the last stream id of a GOAWAY): safe to retry elsewhere.
    Refused,
    /// The stream failed.
    Error(HttpError),
}

/// What a [`RecvStream`] yields.
#[derive(Debug)]
pub(crate) enum Chunk {
    Data(Vec<u8>),
    End(Option<HeaderMap>),
}

/// What happens when a [`RecvStream`] is dropped before the end of the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnDrop {
    /// Reset the stream (a client that stops reading a response).
    Reset,
    /// Keep the stream, discarding the rest of the body (a server handler
    /// that does not read the whole request).
    Discard,
}

/// The receiving side of one stream: body chunks, then optional trailers.
/// Every chunk handed out is credited back to the peer's flow-control
/// window, so the peer can send at most a window ahead of the reader.
pub(crate) struct RecvStream {
    id: u32,
    events: mpsc::UnboundedReceiver<Event>,
    cmd: mpsc::UnboundedSender<Cmd>,
    trailers: Option<HeaderMap>,
    finished: bool,
    on_drop: OnDrop,
}

impl std::fmt::Debug for RecvStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecvStream")
            .field("id", &self.id)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl RecvStream {
    pub(crate) fn new(
        id: u32,
        events: mpsc::UnboundedReceiver<Event>,
        cmd: mpsc::UnboundedSender<Cmd>,
        on_drop: OnDrop,
    ) -> Self {
        Self {
            id,
            events,
            cmd,
            trailers: None,
            finished: false,
            on_drop,
        }
    }

    /// The next chunk of the body.
    pub(crate) async fn next(&mut self) -> Result<Chunk, HttpError> {
        if self.finished {
            return Ok(Chunk::End(self.trailers.take()));
        }
        loop {
            match self.events.recv().await {
                Some(Event::Data(data)) => {
                    if data.is_empty() {
                        continue;
                    }
                    let _ = self.cmd.send(Cmd::Consumed {
                        id: self.id,
                        n: data.len(),
                    });
                    return Ok(Chunk::Data(data));
                }
                Some(Event::Trailers(t)) => self.trailers = Some(t),
                Some(Event::End) => {
                    self.finished = true;
                    return Ok(Chunk::End(self.trailers.take()));
                }
                Some(Event::Head { .. }) => {
                    self.finished = true;
                    return Err(error::h2_error(
                        Http2Cause::Protocol(Reason::PROTOCOL_ERROR),
                        "unexpected second response head",
                    ));
                }
                Some(Event::Refused) => {
                    self.finished = true;
                    return Err(error::h2_error(Http2Cause::Refused, "stream refused"));
                }
                Some(Event::Error(e)) => {
                    self.finished = true;
                    return Err(e);
                }
                None => {
                    self.finished = true;
                    return Err(error::h2_error(
                        Http2Cause::ConnectionClosed,
                        "connection closed",
                    ));
                }
            }
        }
    }
}

impl Drop for RecvStream {
    fn drop(&mut self) {
        // Close the channel first, so the connection cannot queue more data
        // behind our back (it credits what it can no longer deliver), then
        // credit whatever is still queued so the connection window does not
        // shrink for good, and tell the connection the reader is gone.
        self.events.close();
        let mut unread = 0usize;
        while let Ok(event) = self.events.try_recv() {
            if let Event::Data(data) = event {
                unread += data.len();
            }
        }
        if unread > 0 {
            let _ = self.cmd.send(Cmd::Consumed {
                id: self.id,
                n: unread,
            });
        }
        if !self.finished {
            let _ = self.cmd.send(match self.on_drop {
                OnDrop::Reset => Cmd::Reset {
                    id: self.id,
                    reason: Reason::CANCEL,
                },
                OnDrop::Discard => Cmd::Discard { id: self.id },
            });
        }
    }
}

/// A fuzzing entry point: decodes `data` as a sequence of HTTP/2 frames
/// (each prefixed by its 9-byte header), feeding every header block through
/// an HPACK decoder. Never panics and never allocates more than the caps.
#[doc(hidden)]
pub fn fuzz_frames(data: &[u8]) {
    let mut decoder = hpack::Decoder::new(4096);
    let limits = hpack::DecodeLimits {
        max_list_size: 64 * 1024,
        max_fields: 256,
    };
    let mut rest = data;
    while rest.len() >= frame::HEADER_LEN {
        let mut head = [0u8; frame::HEADER_LEN];
        head.copy_from_slice(&rest[..frame::HEADER_LEN]);
        let head = frame::Head::parse(&head);
        rest = &rest[frame::HEADER_LEN..];
        let len = (head.len as usize).min(rest.len());
        let payload = rest[..len].to_vec();
        rest = &rest[len..];
        let head = frame::Head {
            len: len as u32,
            ..head
        };
        match frame::decode(head, payload) {
            Ok(frame::Frame::Headers { block, .. })
            | Ok(frame::Frame::Continuation { block, .. }) => {
                if decoder.decode(&block, &limits).is_err() {
                    decoder = hpack::Decoder::new(4096);
                }
            }
            Ok(_) | Err(_) => {}
        }
    }
    // The raw input as one header block too.
    let _ = hpack::Decoder::new(4096).decode(data, &limits);
}

/// A fuzzing entry point for the connection state machine: runs the HTTP/2
/// server engine over an in-memory pipe, writes the client preface and then
/// `data` as the client's bytes, closes the input, and reads everything the
/// server answers until it closes. Handlers answer buffered and streamed
/// requests, so frames reach streams in every state. Never panics, never
/// hangs, and leaves no task behind.
#[doc(hidden)]
pub fn fuzz_connection(data: &[u8]) {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    else {
        return;
    };
    rt.block_on(fuzz_connection_async(data));
}

async fn fuzz_connection_async(data: &[u8]) {
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::message::ConnInfo;
    use super::{BoxFuture, HttpHandler, HttpResponse, Request, ServerConfig, StatusCode};

    struct App;
    impl HttpHandler for App {
        fn wants_body_stream(&self, req: &Request) -> bool {
            req.uri.path.starts_with("/s")
        }

        fn handle(&self, mut req: Request) -> BoxFuture<'_, HttpResponse> {
            Box::pin(async move {
                let mut n = req.body.len();
                if let Some(mut body) = req.take_body_stream() {
                    while let Ok(Some(chunk)) = body.chunk().await {
                        n += chunk.len();
                    }
                }
                let resp = HttpResponse::text(StatusCode::OK, n.to_string());
                if req.uri.path.ends_with("/t") {
                    return resp.trailer("x-n", n.to_string());
                }
                resp
            })
        }
    }

    let (ours, theirs) = tokio::io::duplex(1 << 16);
    let info = ConnInfo {
        peer_addr: None,
        local_addr: None,
        tls: true,
        peer_certs: Arc::new(Vec::new()),
    };
    let short = Duration::from_millis(200);
    let config = Arc::new(ServerConfig {
        head_timeout: short,
        body_timeout: short,
        idle_timeout: short,
        write_timeout: short,
        handler_timeout: Some(short),
        max_body_bytes: 64 * 1024,
        ..ServerConfig::default()
    });
    let h2 = Http2Config {
        max_concurrent_streams: 8,
        max_resets_per_window: 32,
        max_control_frames_per_window: 64,
        max_buffered_body_bytes: 32 * 1024,
        keepalive_timeout: Duration::from_millis(500),
        ..Http2Config::default()
    };
    let (_drain_tx, drain) = tokio::sync::watch::channel(false);
    let server = async move {
        let io = tokio::io::BufReader::new(Box::new(theirs) as super::Conn);
        let _ = server::serve(io, Arc::new(App), config, drain, info, &h2).await;
    };
    let (mut r, mut w) = tokio::io::split(ours);
    let client = async move {
        let write = async {
            let _ = w.write_all(frame::PREFACE).await;
            let _ = w.write_all(data).await;
            let _ = w.shutdown().await;
            // Keep the write half alive (dropping it would not close the
            // pipe while `r` lives) until the server is done reading.
            w
        };
        let read = async {
            let mut sink = [0u8; 4096];
            while let Ok(n) = r.read(&mut sink).await {
                if n == 0 {
                    break;
                }
            }
        };
        let (_w, ()) = tokio::join!(write, read);
    };
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(server, client);
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_a_recv_stream_credits_everything_it_could_be_sent() {
        let (tx, rx) = mpsc::unbounded_channel();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let stream = RecvStream::new(1, rx, cmd_tx, OnDrop::Reset);
        tx.send(Event::Data(vec![0; 100])).unwrap();
        tx.send(Event::Data(vec![0; 23])).unwrap();
        drop(stream);
        // The channel is closed before the drain: the connection cannot
        // queue data nobody will read (it credits such data itself).
        assert!(tx.send(Event::Data(vec![0; 7])).is_err());
        let mut credited = 0;
        let mut reset = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                Cmd::Consumed { id: 1, n } => credited += n,
                Cmd::Reset { id: 1, reason } => reset = reason == Reason::CANCEL,
                _ => panic!("unexpected command"),
            }
        }
        assert_eq!(credited, 123);
        assert!(reset);
    }

    #[test]
    fn normalized_keeps_the_new_settings_sane() {
        let mut h2 = Http2Config::default();
        assert_eq!(h2.max_streams_per_connection, None);
        assert_eq!(h2.rapid_reset_threshold, Duration::from_millis(100));
        assert_eq!(h2.stream_stall_timeout, Some(Duration::from_secs(30)));
        h2.keepalive_interval = Some(Duration::ZERO);
        h2.keepalive_timeout = Duration::ZERO;
        h2.stream_stall_timeout = Some(Duration::ZERO);
        h2.max_streams_per_connection = Some(0);
        h2.max_connections_per_origin = 0;
        let h2 = h2.normalized();
        assert!(h2.keepalive_interval.unwrap() > Duration::ZERO);
        assert!(h2.keepalive_timeout > Duration::ZERO);
        assert!(h2.stream_stall_timeout.unwrap() > Duration::ZERO);
        assert_eq!(h2.max_streams_per_connection, Some(1));
        assert_eq!(h2.max_connections_per_origin, 1);
    }
}
