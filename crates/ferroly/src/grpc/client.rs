//! [`Channel`]: the client side, over the [`http`](crate::http) client's
//! HTTP/2 connections.

use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use super::call::{Request, Response};
use super::frame::{
    decode_limits, encode_frame, Encoding, ACCEPT_ENCODING, DEFAULT_MAX_MESSAGE_SIZE,
};
use super::interceptor::{self, Interceptor};
use super::metadata::Metadata;
use super::status::{Code, Status};
use super::streaming::{Encoder, FrameSource, Receive, Source, Streaming, Wire};
use super::timeout::format_timeout;
use crate::http::{
    body_channel, BodySender, BodyStream, Client, ClientTlsConfig, Http2Cause, Http2Config,
    HttpError, Method, Protocol, Uri,
};
use crate::proto::Message;

type UploadError = Arc<Mutex<Option<Status>>>;

/// How a call the server refused without processing it (`REFUSED_STREAM`,
/// or a stream beyond a GOAWAY's last stream id) is sent again: once at once,
/// then once after a jittered pause (gRPC's "transparent retry"). For a
/// buffered call the at-once retry is the HTTP client's own, on a fresh
/// connection, so the gRPC layer adds only the paused one; every retry
/// carries the `grpc-timeout` left at that moment.
const RETRY_PAUSE_MIN: Duration = Duration::from_millis(25);
/// The pause before the last transparent retry is `RETRY_PAUSE_MIN` plus up
/// to this much, chosen at random, so refused clients do not come back in
/// lockstep.
const RETRY_PAUSE_JITTER: Duration = Duration::from_millis(50);

/// The most of a streamed upload kept for a transparent retry: a call that
/// is refused after sending more than this fails with `UNAVAILABLE`.
const REPLAY_BUFFER: usize = 256 * 1024;

struct Config {
    /// `scheme://authority[/prefix]`, without a trailing slash.
    base: String,
    max_decoding: usize,
    max_decoded_bytes: Option<usize>,
    max_encoding: usize,
    default_timeout: Option<Duration>,
    interceptors: Vec<Arc<dyn Interceptor>>,
}

impl Config {
    fn receive(&self, encoding: Encoding) -> Receive {
        Receive {
            max_size: self.max_decoding,
            limits: decode_limits(self.max_decoding, self.max_decoded_bytes),
            encoding,
        }
    }
}

/// A connection to a gRPC server, over which calls are made.
///
/// A channel is cheap to clone, and every clone shares one multiplexed
/// HTTP/2 connection per server: concurrent calls run side by side on it.
/// `http://` URLs speak cleartext h2c (prior knowledge), `https://` URLs TLS
/// with ALPN `h2`; a server that does not speak HTTP/2 fails the call with
/// `UNAVAILABLE` (gRPC does not work over HTTP/1.1).
///
/// ```no_run
/// use std::time::Duration;
/// use ferroly::grpc::{Channel, Request, Status};
///
/// #[derive(ferroly::proto::Message, Default)]
/// struct Ping {
///     #[proto(tag = 1)]
///     n: u32,
/// }
///
/// # async fn ex() -> Result<(), Status> {
/// let channel = Channel::new("http://127.0.0.1:50051")?;
/// let mut req = Request::new(Ping { n: 1 }).with_timeout(Duration::from_secs(5));
/// req.metadata_mut().insert("x-request-id", "r-1")?;
/// let resp = channel.unary::<Ping, Ping>("/demo.Echo/Ping", req).await?;
/// assert_eq!(resp.get_ref().n, 2);
/// # Ok(())
/// # }
/// ```
///
/// Each call method returns at the response **head** for the streaming
/// shapes (reading continues through the returned
/// [`Streaming`]), and after the whole exchange for the
/// single-response shapes. Dropping a call's future, or the returned
/// `Streaming`, cancels the call (the HTTP/2 stream is reset, which the
/// server sees as `CANCELLED`).
#[derive(Clone)]
pub struct Channel {
    client: Client,
    cfg: Arc<Config>,
}

impl fmt::Debug for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Channel")
            .field("target", &self.cfg.base)
            .field("max_decoding_message_size", &self.cfg.max_decoding)
            .field("max_encoding_message_size", &self.cfg.max_encoding)
            .field("timeout", &self.cfg.default_timeout)
            .finish_non_exhaustive()
    }
}

/// Configures a [`Channel`]; see [`Channel::builder`] and
/// [`Channel::from_client`].
///
/// | Setting | Default |
/// |---|---|
/// | [`tls`](Self::tls) | web PKI roots (for `https://`) |
/// | [`timeout`](Self::timeout) (per call, unless the request sets one) | none |
/// | [`connect_timeout`](Self::connect_timeout) | 30 s |
/// | [`max_decoding_message_size`](Self::max_decoding_message_size) | 4 MiB |
/// | [`max_decoded_bytes`](Self::max_decoded_bytes) | 16 × the decoding limit (64 MiB) |
/// | [`max_encoding_message_size`](Self::max_encoding_message_size) | 4 MiB |
/// | [`http2_config`](Self::http2_config) | [`Http2Config::default`] |
/// | [`user_agent`](Self::user_agent) | `grpc-ferroly/<version>` |
/// | [`interceptor`](Self::interceptor) | none |
///
/// A builder from [`Channel::from_client`] uses that client's connection
/// settings, so setting [`tls`](Self::tls), [`http2_config`](Self::http2_config),
/// [`connect_timeout`](Self::connect_timeout) or
/// [`user_agent`](Self::user_agent) on it makes [`build`](Self::build) fail.
#[must_use]
pub struct ChannelBuilder {
    base: String,
    tls: Option<ClientTlsConfig>,
    h2: Http2Config,
    connect_timeout: Option<Duration>,
    timeout: Option<Duration>,
    max_decoding: usize,
    max_decoded_bytes: Option<usize>,
    max_encoding: usize,
    user_agent: String,
    https: bool,
    interceptors: Vec<Arc<dyn Interceptor>>,
    /// The client of [`Channel::from_client`].
    client: Option<Client>,
    /// Whether a connection setting (which such a client already fixes) was
    /// set.
    transport_set: bool,
}

impl fmt::Debug for ChannelBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChannelBuilder")
            .field("target", &self.base)
            .field("from_client", &self.client.is_some())
            .finish_non_exhaustive()
    }
}

/// Parses a channel URL into its base (`scheme://authority[/prefix]`) and
/// whether it is TLS.
fn parse_target(url: &str) -> Result<(String, bool), Status> {
    let uri = Uri::parse(url)
        .map_err(|e| Status::invalid_argument(format!("invalid channel URL {url:?}: {e}")))?;
    let https = match uri.scheme.as_str() {
        "https" => true,
        "http" => false,
        other => {
            return Err(Status::invalid_argument(format!(
                "channel URL scheme must be http or https, not {other:?}"
            )))
        }
    };
    if uri.query.is_some() {
        return Err(Status::invalid_argument(
            "a channel URL cannot have a query",
        ));
    }
    let prefix = uri.path.trim_end_matches('/');
    let base = format!("{}://{}{}", uri.scheme, uri.authority(), prefix);
    Ok((base, https))
}

impl ChannelBuilder {
    fn new(base: String, https: bool, client: Option<Client>) -> Self {
        ChannelBuilder {
            base,
            tls: None,
            h2: Http2Config::default(),
            connect_timeout: Some(Duration::from_secs(30)),
            timeout: None,
            max_decoding: DEFAULT_MAX_MESSAGE_SIZE,
            max_decoded_bytes: None,
            max_encoding: DEFAULT_MAX_MESSAGE_SIZE,
            user_agent: format!("grpc-ferroly/{}", env!("CARGO_PKG_VERSION")),
            https,
            interceptors: Vec::new(),
            client,
            transport_set: false,
        }
    }

    /// TLS settings (trusted roots, client certificate) for an `https://`
    /// target.
    pub fn tls(mut self, tls: ClientTlsConfig) -> Self {
        self.tls = Some(tls);
        self.transport_set = true;
        self
    }

    /// HTTP/2 limits, windows and keep-alive pings for the connection. For
    /// a dead-peer check on long-lived streams set
    /// [`keepalive_interval`](Http2Config::keepalive_interval) — no more
    /// often than the server allows (grpc-go closes connections that ping
    /// more often than every 5 minutes by default).
    pub fn http2_config(mut self, config: Http2Config) -> Self {
        self.h2 = config;
        self.transport_set = true;
        self
    }

    /// Deadline for establishing the connection (TCP, TLS); `None` disables
    /// it. A connect failure is `UNAVAILABLE`.
    pub fn connect_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.connect_timeout = timeout;
        self.transport_set = true;
        self
    }

    /// The timeout of every call whose request sets none (see
    /// [`Request::set_timeout`]).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// The largest response message accepted (default 4 MiB); a larger one
    /// fails the call with `RESOURCE_EXHAUSTED` before it is buffered.
    pub fn max_decoding_message_size(mut self, bytes: usize) -> Self {
        self.max_decoding = bytes;
        self
    }

    /// The most memory a decoded response message may take, as the protobuf
    /// decoder estimates it
    /// ([`Limits::max_decoded_bytes`](crate::proto::Limits::max_decoded_bytes));
    /// beyond, the call fails with `RESOURCE_EXHAUSTED`. Default 16 × the
    /// [decoding limit](Self::max_decoding_message_size), at least 1 MiB:
    /// it stops a small message (a run of empty sub-messages, two bytes each
    /// on the wire) from expanding into gigabytes of structs. The budget is
    /// per message: concurrent calls each may hold it, so the worst case is
    /// that times the calls in flight.
    pub fn max_decoded_bytes(mut self, bytes: usize) -> Self {
        self.max_decoded_bytes = Some(bytes);
        self
    }

    /// The largest request message sent (default 4 MiB); a larger one fails
    /// the call with `RESOURCE_EXHAUSTED` before anything is sent.
    pub fn max_encoding_message_size(mut self, bytes: usize) -> Self {
        self.max_encoding = bytes;
        self
    }

    /// The `user-agent` sent with every call.
    pub fn user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = user_agent.into();
        self.transport_set = true;
        self
    }

    /// Adds an [`Interceptor`], run before every call (in the order added).
    pub fn interceptor(mut self, interceptor: impl Interceptor) -> Self {
        self.interceptors.push(Arc::new(interceptor));
        self
    }

    /// Builds the channel. It connects lazily, on the first call. Fails
    /// (`INVALID_ARGUMENT`) when the TLS settings are invalid, or — for
    /// [`Channel::from_client`] — when the client does not have HTTP/2
    /// enabled for the URL's scheme, or a connection setting was set.
    pub fn build(self) -> Result<Channel, Status> {
        let client = match self.client {
            Some(client) => {
                if self.transport_set {
                    return Err(Status::invalid_argument(
                        "a channel from an existing client uses that client's connection \
                         settings (TLS, HTTP/2, connect timeout, user agent)",
                    ));
                }
                let how = if self.https {
                    "https URLs (ClientBuilder::http2)"
                } else {
                    "http URLs (ClientBuilder::http2_prior_knowledge)"
                };
                client.requiring_http2(self.https).ok_or_else(|| {
                    Status::invalid_argument(format!(
                        "the client does not have HTTP/2 enabled for {how}, which gRPC requires"
                    ))
                })?
            }
            None => {
                let mut builder = Client::builder()
                    .http2(self.https)
                    .http2_prior_knowledge(!self.https)
                    .http2_config(self.h2)
                    .require_http2()
                    // Deadlines are the gRPC layer's: no HTTP timeout may
                    // cut a long-lived stream short.
                    .timeout(None)
                    .read_timeout(None)
                    .total_timeout(None)
                    .connect_timeout(self.connect_timeout)
                    .max_response_body_bytes(None)
                    .user_agent(self.user_agent);
                if let Some(tls) = self.tls {
                    builder = builder.tls(tls);
                }
                builder.build().map_err(|e| {
                    Status::invalid_argument(format!("invalid channel settings: {e}"))
                })?
            }
        };
        Ok(Channel {
            client,
            cfg: Arc::new(Config {
                base: self.base,
                max_decoding: self.max_decoding,
                max_decoded_bytes: self.max_decoded_bytes,
                max_encoding: self.max_encoding,
                default_timeout: self.timeout,
                interceptors: self.interceptors,
            }),
        })
    }
}

/// How a call's response began.
enum Head {
    /// Response headers; messages and the status follow.
    Stream(Box<crate::http::Response>, Metadata, Encoding),
    /// A trailers-only response: the status, no messages.
    Finished(Status),
}

/// Runs `f` until `deadline`, failing with `DEADLINE_EXCEEDED` after it.
async fn within<T>(
    deadline: Option<Instant>,
    f: impl Future<Output = Result<T, Status>>,
) -> Result<T, Status> {
    match deadline {
        Some(deadline) => match tokio::time::timeout_at(deadline, f).await {
            Ok(result) => result,
            Err(_) => Err(Status::deadline_exceeded("the deadline passed")),
        },
        None => f.await,
    }
}

/// Whether a failed send was refused by the server before it processed
/// anything, so it can be sent again (gRPC "transparent retry").
fn refused(error: &HttpError) -> bool {
    matches!(error.http2_cause(), Some(Http2Cause::Refused))
}

/// Waits the jittered pause before a transparent retry (no longer than the
/// deadline leaves), then sets the request's `grpc-timeout` to what is left,
/// so the server is never told a later deadline than the client keeps.
async fn before_retry(
    req: &mut crate::http::Request,
    deadline: Option<Instant>,
) -> Result<(), Status> {
    use std::hash::{BuildHasher, Hasher};
    // `RandomState` is seeded per instance: a cheap random number.
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(0);
    let jitter = (RETRY_PAUSE_JITTER.as_micros() as u64).max(1);
    let pause = RETRY_PAUSE_MIN + Duration::from_micros(hasher.finish() % jitter);
    let Some(deadline) = deadline else {
        tokio::time::sleep(pause).await;
        return Ok(());
    };
    tokio::time::sleep_until(deadline.min(Instant::now() + pause)).await;
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Status::deadline_exceeded("the deadline passed"));
    }
    req.headers.set("grpc-timeout", format_timeout(left));
    Ok(())
}

/// One item of a streamed upload.
#[derive(Clone)]
enum UpItem {
    Frame(Vec<u8>),
    /// The request stream ended cleanly.
    End,
    /// The request stream failed (its status is in the upload's slot).
    Fail,
}

/// What of a streamed upload is kept for replay.
struct Replay {
    items: Vec<UpItem>,
    bytes: usize,
    /// Recording stopped: the response head arrived, or the buffer filled.
    stopped: bool,
    /// The buffer filled before the response head arrived: no replay.
    overflowed: bool,
}

/// A streamed upload: frames encoded by a background task, fed to one HTTP
/// request body per attempt. Until the response head arrives every item is
/// also recorded (up to [`REPLAY_BUFFER`] bytes), so an attempt the server
/// refuses can be replayed on a new stream.
struct Upload {
    frames: tokio::sync::Mutex<mpsc::Receiver<UpItem>>,
    replay: Mutex<Replay>,
    error: UploadError,
}

impl Upload {
    fn replay(&self) -> std::sync::MutexGuard<'_, Replay> {
        self.replay.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn record(&self, item: &UpItem) {
        let mut replay = self.replay();
        if replay.stopped {
            return;
        }
        replay.bytes += match item {
            UpItem::Frame(f) => f.len(),
            _ => 0,
        };
        if replay.bytes > REPLAY_BUFFER {
            replay.stopped = true;
            replay.overflowed = true;
            replay.items = Vec::new();
        } else {
            replay.items.push(item.clone());
        }
    }

    /// The response head arrived: nothing more is replayed.
    fn stop_recording(&self) {
        let mut replay = self.replay();
        replay.stopped = true;
        replay.items = Vec::new();
    }

    fn replayable(&self) -> bool {
        !self.replay().overflowed
    }

    /// The body of one attempt: what was recorded so far, then the rest of
    /// the stream as it is encoded. The returned handle stops the attempt.
    fn attempt(self: &Arc<Self>) -> (BodyStream, Attempt) {
        let (tx, body) = body_channel(4);
        let up = self.clone();
        let cancel = Arc::new(tokio::sync::Notify::new());
        let cancelled = cancel.clone();
        let task = tokio::spawn(async move {
            let feed = async move {
                let mut tx = Some(tx);
                // Take the stream first: the previous attempt has released it.
                let mut frames = up.frames.lock().await;
                let recorded = up.replay().items.clone();
                for item in recorded {
                    if !deliver(&mut tx, item).await {
                        return;
                    }
                }
                loop {
                    let Some(open) = tx.as_ref() else { return };
                    let item = tokio::select! {
                        item = frames.recv() => item,
                        () = open.closed() => return,
                    };
                    // The encoder always ends with `End` or `Fail`; without
                    // one, dropping the sender resets the stream.
                    let Some(item) = item else { return };
                    // Recorded before the next await, so an item taken from
                    // the stream is in the replay even if the attempt is
                    // stopped while delivering it.
                    up.record(&item);
                    if !deliver(&mut tx, item).await {
                        return;
                    }
                }
            };
            // Stopping drops the feed, and with it the lock on the stream.
            tokio::select! {
                () = feed => {}
                () = cancelled.notified() => {}
            }
        });
        (body, Attempt { cancel, task })
    }
}

/// The task feeding one attempt's body from an [`Upload`].
struct Attempt {
    cancel: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl Attempt {
    /// Stops the attempt and waits until it has released the request
    /// stream. A refused attempt's body may never be read again (the HTTP
    /// layer can be parked waiting for it), so the next attempt must not
    /// wait for the body to be dropped.
    async fn stop(self) {
        self.cancel.notify_one();
        let _ = self.task.await;
    }
}

/// Sends one upload item into an attempt's body; `false` once the attempt
/// is over.
async fn deliver(tx: &mut Option<BodySender>, item: UpItem) -> bool {
    let Some(sender) = tx.as_mut() else {
        return false;
    };
    match item {
        UpItem::Frame(frame) => sender.send(frame).await.is_ok(),
        UpItem::End => {
            if let Some(sender) = tx.take() {
                let _ = sender.finish().await;
            }
            false
        }
        // Dropping the sender unfinished resets the stream: the call ends
        // with the status left in the slot.
        UpItem::Fail => {
            tx.take();
            false
        }
    }
}

impl Channel {
    /// A channel to `url` (`http://host:port` or `https://host:port`,
    /// optionally with a path prefix) with the default settings.
    pub fn new(url: &str) -> Result<Channel, Status> {
        Channel::builder(url)?.build()
    }

    /// Starts configuring a channel to `url`.
    pub fn builder(url: &str) -> Result<ChannelBuilder, Status> {
        let (base, https) = parse_target(url)?;
        Ok(ChannelBuilder::new(base, https, None))
    }

    /// Starts configuring a channel that sends its calls with an existing
    /// HTTP `client` (for its interceptors, proxy or shared pool). The
    /// client must have HTTP/2 enabled for `url` — `ClientBuilder::http2`
    /// for `https`, `http2_prior_knowledge` for `http` — or
    /// [`build`](ChannelBuilder::build) fails; calls through the channel
    /// never fall back to HTTP/1.1. The client's own timeouts and body cap
    /// apply on top of the call deadlines, so build it without them.
    pub fn from_client(client: Client, url: &str) -> Result<ChannelBuilder, Status> {
        let (base, https) = parse_target(url)?;
        Ok(ChannelBuilder::new(base, https, Some(client)))
    }

    /// The target, `scheme://authority[/prefix]`.
    pub fn target(&self) -> &str {
        &self.cfg.base
    }

    /// Runs the interceptors and builds the HTTP request of a call. The
    /// deadline is the earlier of the request's timeout (or the channel's
    /// default) and its deadline.
    fn prepare<T>(
        &self,
        path: &str,
        request: Request<T>,
    ) -> Result<(crate::http::Request, T, Option<Instant>), Status> {
        if !path.starts_with('/') {
            return Err(Status::invalid_argument(format!(
                "method path {path:?} must start with '/'"
            )));
        }
        let (mut head, message) = request.split();
        interceptor::run(&self.cfg.interceptors, path, &mut head)?;
        let now = Instant::now();
        let from_timeout = head
            .timeout()
            .or(self.cfg.default_timeout)
            .and_then(|t| now.checked_add(t));
        let from_deadline = head.deadline().map(Instant::from_std);
        let deadline = match (from_timeout, from_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let url = format!("{}{}", self.cfg.base, path);
        let mut req = crate::http::Request::builder(Method::Post, &url)
            .map_err(|e| Status::invalid_argument(format!("invalid method path {path:?}: {e}")))?
            .header("content-type", "application/grpc")
            .header("te", "trailers")
            .header("grpc-accept-encoding", ACCEPT_ENCODING)
            .build();
        if let Some(deadline) = deadline {
            let left = deadline.saturating_duration_since(now);
            if left.is_zero() {
                return Err(Status::deadline_exceeded(
                    "the deadline passed before the call started",
                ));
            }
            req.headers.set("grpc-timeout", format_timeout(left));
        }
        head.metadata().write_to(&mut req.headers);
        Ok((req, message, deadline))
    }

    /// Reads the response head of a call.
    fn head(&self, mut resp: crate::http::Response) -> Result<Head, Status> {
        if resp.protocol() != Protocol::Http2 {
            return Err(Status::unavailable(
                "the server answered over HTTP/1.1, but gRPC requires HTTP/2",
            ));
        }
        let http_status = resp.status().as_u16();
        let headers = resp.headers();
        if let Some(status) = Status::from_header_map(headers) {
            // Trailers-only: the status is in the headers.
            return Ok(Head::Finished(status));
        }
        if http_status != 200 {
            return Err(Status::from_http_status(http_status));
        }
        let content_type = headers.get("content-type").unwrap_or("");
        if !matches!(super::server::grpc_content_type(content_type), Some(Ok(_))) {
            return Err(Status::new(
                Code::Unknown,
                format!("the response is not gRPC (content-type {content_type:?})"),
            ));
        }
        let encoding = match headers.get("grpc-encoding") {
            None => Encoding::Identity,
            Some(value) => Encoding::parse(value).ok_or_else(|| {
                Status::internal(format!(
                    "the response uses an unsupported grpc-encoding {value:?}"
                ))
            })?,
        };
        let metadata = Metadata::from_headers(headers);
        resp.set_max_body_bytes(None);
        Ok(Head::Stream(Box::new(resp), metadata, encoding))
    }

    /// Sends a call with a single, buffered request message — replayable,
    /// so an attempt the server refused is sent again.
    async fn open_buffered(
        &self,
        mut req: crate::http::Request,
        frame: Vec<u8>,
        deadline: Option<Instant>,
    ) -> Result<Head, Status> {
        req.body = frame;
        // The HTTP client already retries a refused buffered request once,
        // at once, on another connection where it can: one more try after a
        // pause.
        let mut retried = false;
        loop {
            match self.client.send(req.clone()).await {
                Ok(resp) => return self.head(resp),
                Err(e) if refused(&e) && !retried => {
                    retried = true;
                    before_retry(&mut req, deadline).await?;
                }
                Err(e) => return Err(Status::from_http_error(&e, deadline)),
            }
        }
    }

    /// Sends a call whose request stream is uploaded as it is produced; an
    /// attempt the server refused is replayed while what was sent still fits
    /// in the replay buffer.
    async fn open_streamed(
        &self,
        mut req: crate::http::Request,
        upload: &Arc<Upload>,
        deadline: Option<Instant>,
    ) -> Result<Head, Status> {
        let taken = || {
            upload
                .error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
        };
        let mut retries = 0;
        loop {
            let (body, current) = upload.attempt();
            match self.client.send_body_stream(req.clone(), body).await {
                Ok(resp) => {
                    upload.stop_recording();
                    return self.head(resp);
                }
                Err(e) => {
                    // Release the request stream before anything else: the
                    // next attempt replays from it.
                    current.stop().await;
                    if let Some(status) = taken() {
                        return Err(status);
                    }
                    if refused(&e) && retries < 2 && upload.replayable() {
                        // Once at once, then once after a pause.
                        if retries == 1 {
                            before_retry(&mut req, deadline).await?;
                        }
                        retries += 1;
                        continue;
                    }
                    return Err(Status::from_http_error(&e, deadline));
                }
            }
        }
    }

    /// Reads a single-message response.
    async fn single<Resp: Message>(
        &self,
        head: Head,
        deadline: Option<Instant>,
        upload_error: Option<UploadError>,
    ) -> Result<Response<Resp>, Status> {
        let (resp, metadata, encoding) = match head {
            Head::Finished(status) if status.is_ok() => {
                return Err(Status::internal(
                    "the server sent no response message (a unary response needs one)",
                ))
            }
            Head::Finished(status) => return Err(status),
            Head::Stream(resp, metadata, encoding) => (resp, metadata, encoding),
        };
        let wire = Wire::new(
            Source::Client { resp, upload_error },
            self.cfg.receive(encoding),
            deadline,
        );
        let mut stream = Streaming::<Resp>::wire(wire);
        let first = stream.message().await?;
        if stream.message().await?.is_some() {
            return Err(Status::internal(
                "the server sent more than one response message (a unary response has one)",
            ));
        }
        let message = first.ok_or_else(|| {
            Status::internal("the server sent no response message (a unary response needs one)")
        })?;
        let trailers = stream.trailers().cloned().unwrap_or_default();
        Ok(Response::from_parts(metadata, message, trailers))
    }

    /// A response stream.
    fn streamed<Resp>(
        &self,
        head: Head,
        deadline: Option<Instant>,
        upload_error: Option<UploadError>,
    ) -> Result<Response<Streaming<Resp>>, Status> {
        match head {
            Head::Finished(status) if status.is_ok() => Ok(Response::new(Streaming::ended(status))),
            Head::Finished(status) => Err(status),
            Head::Stream(resp, metadata, encoding) => {
                let wire = Wire::new(
                    Source::Client { resp, upload_error },
                    self.cfg.receive(encoding),
                    deadline,
                );
                Ok(Response::from_parts(
                    metadata,
                    Streaming::wire(wire),
                    Metadata::new(),
                ))
            }
        }
    }

    /// Starts encoding a request stream in the background, for
    /// [`open_streamed`](Self::open_streamed).
    fn upload<Req: Message + Send + 'static>(&self, stream: Streaming<Req>) -> Arc<Upload> {
        let (tx, rx) = mpsc::channel(4);
        let upload = Arc::new(Upload {
            frames: tokio::sync::Mutex::new(rx),
            replay: Mutex::new(Replay {
                items: Vec::new(),
                bytes: 0,
                stopped: false,
                overflowed: false,
            }),
            error: Arc::new(Mutex::new(None)),
        });
        let failed = upload.error.clone();
        let mut source = Encoder {
            stream,
            max_size: self.cfg.max_encoding,
        };
        tokio::spawn(async move {
            loop {
                // An idle source is abandoned as soon as the call is over.
                let next = tokio::select! {
                    next = source.next_frame() => next,
                    () = tx.closed() => return,
                };
                let item = match next {
                    Some(Ok(frame)) => UpItem::Frame(frame),
                    None => UpItem::End,
                    Some(Err(status)) => {
                        *failed.lock().unwrap_or_else(|e| e.into_inner()) = Some(status);
                        UpItem::Fail
                    }
                };
                let last = !matches!(item, UpItem::Frame(_));
                if tx.send(item).await.is_err() || last {
                    return;
                }
            }
        });
        upload
    }

    /// A unary call: one request, one response.
    pub async fn unary<Req, Resp>(
        &self,
        path: &str,
        request: Request<Req>,
    ) -> Result<Response<Resp>, Status>
    where
        Req: Message,
        Resp: Message,
    {
        let (req, message, deadline) = self.prepare(path, request)?;
        let frame = encode_frame(&message, self.cfg.max_encoding)?;
        drop(message);
        let result = within(deadline, async {
            let head = self.open_buffered(req, frame, deadline).await?;
            self.single(head, deadline, None).await
        })
        .await;
        result.map_err(|status| status.after_deadline(deadline))
    }

    /// A server-streaming call: one request, a stream of responses. Returns
    /// once the response headers arrive; read the messages from the
    /// returned [`Streaming`] (its trailers follow the last one).
    pub async fn server_streaming<Req, Resp>(
        &self,
        path: &str,
        request: Request<Req>,
    ) -> Result<Response<Streaming<Resp>>, Status>
    where
        Req: Message,
        Resp: Message,
    {
        let (req, message, deadline) = self.prepare(path, request)?;
        let frame = encode_frame(&message, self.cfg.max_encoding)?;
        drop(message);
        let head = within(deadline, self.open_buffered(req, frame, deadline))
            .await
            .map_err(|status| status.after_deadline(deadline))?;
        self.streamed(head, deadline, None)
    }

    /// A client-streaming call: a stream of requests (uploaded in the
    /// background as they come), one response.
    pub async fn client_streaming<Req, Resp>(
        &self,
        path: &str,
        request: Request<Streaming<Req>>,
    ) -> Result<Response<Resp>, Status>
    where
        Req: Message + Send + 'static,
        Resp: Message,
    {
        let (req, stream, deadline) = self.prepare(path, request)?;
        let upload = self.upload(stream);
        let result = within(deadline, async {
            let head = self.open_streamed(req, &upload, deadline).await?;
            self.single(head, deadline, Some(upload.error.clone()))
                .await
        })
        .await;
        result.map_err(|status| status.after_deadline(deadline))
    }

    /// A bidirectional-streaming call: requests are uploaded in the
    /// background while responses are read. Returns once the response
    /// headers arrive — a server may send those only with its first
    /// message, so queue the first request (or feed the request stream from
    /// another task) before awaiting this.
    pub async fn bidi_streaming<Req, Resp>(
        &self,
        path: &str,
        request: Request<Streaming<Req>>,
    ) -> Result<Response<Streaming<Resp>>, Status>
    where
        Req: Message + Send + 'static,
        Resp: Message,
    {
        let (req, stream, deadline) = self.prepare(path, request)?;
        let upload = self.upload(stream);
        let head = within(deadline, self.open_streamed(req, &upload, deadline))
            .await
            .map_err(|status| status.after_deadline(deadline))?;
        self.streamed(head, deadline, Some(upload.error.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(
            parse_target("http://127.0.0.1:50051").unwrap(),
            ("http://127.0.0.1:50051".to_string(), false)
        );
        assert_eq!(
            parse_target("https://example.com/prefix/").unwrap().0,
            "https://example.com/prefix"
        );
        assert!(parse_target("ftp://x").is_err());
        assert!(parse_target("http://x/?q=1").is_err());
        assert!(parse_target("not a url").is_err());
    }

    #[tokio::test]
    async fn a_retry_pauses_and_sends_the_time_left() {
        let mut req = crate::http::Request::builder(Method::Post, "http://x/t.S/M")
            .unwrap()
            .build();
        let deadline = Instant::now() + Duration::from_secs(30);
        let started = Instant::now();
        before_retry(&mut req, Some(deadline)).await.unwrap();
        let paused = started.elapsed();
        assert!(paused >= RETRY_PAUSE_MIN, "{paused:?}");
        let sent = req.headers.get("grpc-timeout").expect("grpc-timeout set");
        let left = super::super::timeout::parse_timeout(sent).unwrap();
        assert!(left < Duration::from_secs(30) - RETRY_PAUSE_MIN, "{sent}");
        assert!(left > Duration::from_secs(29), "{sent}");

        // A deadline shorter than the pause: the retry is not sent.
        let deadline = Instant::now() + Duration::from_millis(5);
        let status = before_retry(&mut req, Some(deadline)).await.unwrap_err();
        assert_eq!(status.code(), Code::DeadlineExceeded);
    }
}
