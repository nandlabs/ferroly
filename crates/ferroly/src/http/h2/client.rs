//! The client side of HTTP/2: multiplexed connections per origin, shared
//! by every concurrent request to it. A second connection is dialed only
//! when every existing one has as many streams open as its server allows.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{mpsc, oneshot};

use super::conn::{self, is_connection_specific, trim_value, Cmd, Open, Options, Shared};
use super::error::{h2_error, Reason};
use super::server::StreamOut;
use super::{Event, Http2Config, OnDrop, RecvStream};
use crate::http::core::{is_field_value_byte, is_token};
use crate::http::io::{RequestWire, BODY_PIECE_BYTES};
use crate::http::pool::PoolKey;
use crate::http::transport::Conn;
use crate::http::{BodyStream, HeaderMap, Http2Cause, HttpError, Method, Request};

/// How long an origin that answered ALPN with HTTP/1.1 is remembered as
/// such, and how many such origins are remembered.
const H1_ONLY_TTL: Duration = Duration::from_secs(600);
const H1_ONLY_MAX: usize = 1024;

/// A handle on one HTTP/2 client connection.
#[derive(Clone)]
pub(crate) struct H2Conn {
    cmd: mpsc::UnboundedSender<Cmd>,
    shared: Arc<Shared>,
}

impl H2Conn {
    /// Whether new streams may be opened on it.
    pub(crate) fn usable(&self) -> bool {
        self.shared.usable.load(Ordering::Acquire) && !self.cmd.is_closed()
    }

    /// Whether every stream slot its server allows is taken. Until the
    /// server has announced a limit (its SETTINGS may not have arrived yet),
    /// [`UNKNOWN_CAPACITY`] streams count as a full connection, so a burst
    /// of first requests spreads over connections instead of piling onto
    /// one whose server will refuse most of them.
    fn saturated(&self) -> bool {
        self.shared.saturated() || (self.capacity().is_none() && self.load() >= UNKNOWN_CAPACITY)
    }

    fn load(&self) -> usize {
        self.shared.active.load(Ordering::Relaxed)
    }

    /// The server's stream limit, when it has announced one.
    fn capacity(&self) -> Option<usize> {
        let cap = self.shared.capacity.load(Ordering::Relaxed);
        (cap < u32::MAX as usize).then_some(cap)
    }

    fn same(&self, other: &H2Conn) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

/// How many streams a connection is given before its server has announced
/// a limit: the minimum RFC 9113 §6.5.2 recommends servers allow.
const UNKNOWN_CAPACITY: usize = 100;

/// Starts an HTTP/2 connection over `io` (TLS after ALPN `h2`, or cleartext
/// with prior knowledge). The connection runs on its own task until it is
/// idle for `idle_timeout`, the peer closes it, or every handle is gone.
pub(crate) fn start(
    io: Conn,
    h2: &Http2Config,
    idle_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
) -> H2Conn {
    let h2 = h2.normalized();
    let opts = Options {
        server: false,
        max_list_size: h2.max_header_list_size as usize,
        max_fields: crate::http::io::DEFAULT_MAX_HEADERS + 1,
        idle_timeout,
        write_timeout,
        head_timeout: None,
        body_timeout: None,
        stall_timeout: None,
        max_requests: None,
        body_budget: usize::MAX,
        h2,
    };
    let shared = Arc::new(Shared::new());
    let (tx, rx) = mpsc::unbounded_channel();
    let (actor, reader) = conn::start(io, Arc::new(opts), shared.clone(), None);
    tokio::spawn(async move {
        let _ = actor.run(reader, rx, None).await;
    });
    H2Conn { cmd: tx, shared }
}

/// The client's HTTP/2 connections, up to `max_connections_per_origin` per
/// origin.
#[derive(Default)]
pub(crate) struct H2Pool {
    conns: Mutex<HashMap<PoolKey, Vec<H2Conn>>>,
    /// Lets one task at a time set up a connection per origin, so concurrent
    /// first requests share one connection. An entry lives only while
    /// someone holds or waits for it.
    connecting: Mutex<HashMap<PoolKey, Arc<tokio::sync::Mutex<()>>>>,
    /// Origins whose ALPN answer was HTTP/1.1, and when they answered.
    h1_only: Mutex<HashMap<PoolKey, Instant>>,
    /// The ALPN copy of the TLS configuration, and the one it was made from.
    #[allow(clippy::type_complexity)]
    alpn: Mutex<Option<(Arc<rustls::ClientConfig>, Arc<rustls::ClientConfig>)>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What [`H2Pool::get`] found for an origin.
pub(crate) enum Pick {
    /// A connection with a free stream slot (or the least loaded one when
    /// the origin is at its connection limit).
    Use(H2Conn),
    /// Every connection is saturated and another may be dialed.
    Dial,
    /// No connection yet.
    None,
}

/// The right to set up a connection for one origin; dropping it removes
/// the origin's lock entry when nobody else waits for it.
pub(crate) struct SetupGuard<'a> {
    pool: &'a H2Pool,
    key: PoolKey,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl Drop for SetupGuard<'_> {
    fn drop(&mut self) {
        self.guard.take();
        let mut connecting = lock(&self.pool.connecting);
        // Ours and the map's: nobody else holds or waits for it.
        if Arc::strong_count(&self.lock) == 2 {
            connecting.remove(&self.key);
        }
    }
}

impl H2Pool {
    /// A connection for `key`: one with a free stream slot when there is
    /// one, otherwise [`Pick::Dial`] below `max` connections, otherwise the
    /// least loaded one.
    pub(crate) fn get(&self, key: &PoolKey, max: usize) -> Pick {
        let mut conns = lock(&self.conns);
        let Some(list) = conns.get_mut(key) else {
            return Pick::None;
        };
        list.retain(H2Conn::usable);
        if list.is_empty() {
            conns.remove(key);
            return Pick::None;
        }
        if let Some(free) = list.iter().find(|c| !c.saturated()) {
            // Count the stream we are about to open, so concurrent picks see
            // it before the connection republishes its real count.
            free.shared.active.fetch_add(1, Ordering::Relaxed);
            return Pick::Use(free.clone());
        }
        if list.len() < max {
            return Pick::Dial;
        }
        match list.iter().min_by_key(|c| c.load()) {
            Some(c) => Pick::Use(c.clone()),
            None => Pick::None,
        }
    }

    /// A connection for retrying a request that `failed` refused or lost.
    ///
    /// The retry goes to another connection with a free stream slot whose
    /// server has announced its stream limit, never back to `failed` while
    /// such a connection is there. (One whose server has not announced it
    /// yet may send the retry beyond that limit, to be refused again.)
    /// Otherwise this answers [`Pick::Dial`] below `max` connections (the
    /// caller dials under [`setup_lock`](Self::setup_lock), which keeps the
    /// origin at `max`). Only at the limit does it fall back to the least
    /// loaded connection, preferring one whose server has announced its
    /// limit, since such a connection queues streams beyond it instead of
    /// sending them. That one may be `failed` itself: with `max = 1` the
    /// retry goes back to the connection that refused it. Retries therefore
    /// never take the origin past `max` connections.
    pub(crate) fn get_retry(&self, key: &PoolKey, max: usize, failed: &H2Conn) -> Pick {
        let mut conns = lock(&self.conns);
        let Some(list) = conns.get_mut(key) else {
            return Pick::None;
        };
        list.retain(H2Conn::usable);
        if list.is_empty() {
            conns.remove(key);
            return Pick::None;
        }
        // `failed` (when still usable) has heard from its server, which
        // sends its SETTINGS first, even if it has not republished them yet.
        let known = |c: &&H2Conn| c.capacity().is_some() || c.same(failed);
        let free = list
            .iter()
            .filter(known)
            .find(|c| !c.saturated() && !c.same(failed));
        if let Some(free) = free {
            // Count the stream we are about to open, as `get` does.
            free.shared.active.fetch_add(1, Ordering::Relaxed);
            return Pick::Use(free.clone());
        }
        if list.len() < max {
            return Pick::Dial;
        }
        let known = list.iter().filter(known).min_by_key(|c| c.load());
        match known.or_else(|| list.iter().min_by_key(|c| c.load())) {
            Some(c) => Pick::Use(c.clone()),
            None => Pick::None,
        }
    }

    pub(crate) fn insert(&self, key: PoolKey, conn: H2Conn) {
        let mut conns = lock(&self.conns);
        conns.retain(|_, list| {
            list.retain(H2Conn::usable);
            !list.is_empty()
        });
        conns.entry(key).or_default().push(conn);
    }

    /// Waits for the right to set up a connection for `key`.
    pub(crate) async fn setup_lock(&self, key: &PoolKey) -> SetupGuard<'_> {
        let m = lock(&self.connecting)
            .entry(key.clone())
            .or_default()
            .clone();
        let guard = m.clone().lock_owned().await;
        SetupGuard {
            pool: self,
            key: key.clone(),
            guard: Some(guard),
            lock: m,
        }
    }

    pub(crate) fn is_h1_only(&self, key: &PoolKey) -> bool {
        let mut h1 = lock(&self.h1_only);
        match h1.get(key) {
            Some(at) if at.elapsed() < H1_ONLY_TTL => true,
            Some(_) => {
                // The server may have been upgraded: offer h2 again.
                h1.remove(key);
                false
            }
            None => false,
        }
    }

    pub(crate) fn mark_h1_only(&self, key: PoolKey) {
        let mut h1 = lock(&self.h1_only);
        if h1.len() >= H1_ONLY_MAX {
            h1.retain(|_, at| at.elapsed() < H1_ONLY_TTL);
            if h1.len() >= H1_ONLY_MAX {
                if let Some(oldest) = h1.iter().min_by_key(|(_, at)| **at).map(|(k, _)| k.clone()) {
                    h1.remove(&oldest);
                }
            }
        }
        h1.insert(key, Instant::now());
    }

    #[cfg(test)]
    fn connecting_len(&self) -> usize {
        lock(&self.connecting).len()
    }

    /// `base` with `h2` + `http/1.1` ALPN, built once per base config.
    pub(crate) fn alpn_config(
        &self,
        base: &Arc<rustls::ClientConfig>,
    ) -> Arc<rustls::ClientConfig> {
        let mut cache = lock(&self.alpn);
        if let Some((b, alpn)) = cache.as_ref() {
            if Arc::ptr_eq(b, base) {
                return alpn.clone();
            }
        }
        let alpn = crate::http::transport::with_h2_alpn(base);
        *cache = Some((base.clone(), alpn.clone()));
        alpn
    }
}

/// The body to send with an HTTP/2 request.
pub(crate) enum Upload<'a> {
    /// The request's buffered body.
    Buffered(&'a [u8]),
    /// Read from a borrowed reader while the request is being sent.
    Reader(&'a mut (dyn AsyncRead + Send + Unpin)),
    /// Fed from a stream by a background task, concurrently with the
    /// response (taken when the stream is opened).
    Stream(Option<BodyStream>),
}

/// Why an HTTP/2 exchange failed.
pub(crate) enum Failure {
    /// The connection refused the stream before processing it: retry
    /// elsewhere. `sent` says whether body bytes were already consumed.
    Refused {
        sent: bool,
    },
    /// The connection died (closed, I/O error, unanswered keep-alive)
    /// before the response head arrived. The server may have processed the
    /// request, so only an idempotent one may be retried.
    Lost {
        sent: bool,
        error: HttpError,
    },
    Error(HttpError),
}

/// Whether `e` means the connection died under the stream.
fn connection_lost(e: &HttpError) -> bool {
    matches!(
        e.http2_cause(),
        Some(
            Http2Cause::ConnectionClosed
                | Http2Cause::Io(_)
                | Http2Cause::Timeout
                | Http2Cause::GoAway { .. }
        )
    )
}

/// Resets the stream if the exchange is abandoned before the response is
/// handed over.
struct CancelGuard<'a> {
    out: Option<&'a StreamOut>,
}

impl Drop for CancelGuard<'_> {
    fn drop(&mut self) {
        if let Some(out) = self.out {
            out.reset(Reason::CANCEL);
        }
    }
}

/// A wait for a stream slot. Abandoned (a timeout, or the caller dropped
/// the request), it gives up the slot request — the connection prunes
/// waiters that gave up — and, if the stream opened at the last moment,
/// cancels it so it does not linger half-sent.
struct OpenWait<'a> {
    rx: Option<oneshot::Receiver<Result<u32, conn::OpenError>>>,
    cmd: &'a mpsc::UnboundedSender<Cmd>,
}

impl Drop for OpenWait<'_> {
    fn drop(&mut self) {
        if let Some(mut rx) = self.rx.take() {
            rx.close();
            if let Ok(Ok(id)) = rx.try_recv() {
                let _ = self.cmd.send(Cmd::Reset {
                    id,
                    reason: Reason::CANCEL,
                });
            }
        }
    }
}

/// A response head and its body stream.
pub(crate) struct Head {
    pub(crate) status: u16,
    pub(crate) headers: HeaderMap,
    pub(crate) body: RecvStream,
    /// For a request body fed by [`pump_stream`]: kept with the response
    /// body, so the upload stops when the stream is over (see
    /// [`UploadWatch`]).
    pub(crate) upload: Option<UploadWatch>,
}

/// Tells a background upload ([`pump_stream`]) whether its stream is still
/// alive. Dropped (the exchange failed, the stream was refused or reset,
/// the response was dropped before its end), the upload resets the stream
/// and stops at once — even while it waits for the next piece of an idle
/// request body, which would otherwise be its first chance to notice.
/// [`release`](Self::release)d after the response ended cleanly, the upload
/// carries on to its end, as a server that answered early may still read.
pub(crate) struct UploadWatch(Option<oneshot::Sender<()>>);

impl UploadWatch {
    /// The response ended cleanly: let the upload finish on its own.
    pub(crate) fn release(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

/// Sends one request on `conn` and waits for the response head.
/// `head_timeout` bounds the wait for a stream slot and the wait for the
/// head together (not the upload of a borrowed reader in between, which
/// `step_timeout` bounds per step).
pub(crate) async fn exchange(
    conn: &H2Conn,
    fields: Vec<(String, String)>,
    upload: &mut Upload<'_>,
    head_request: bool,
    step_timeout: Option<Duration>,
    head_timeout: Option<Duration>,
) -> Result<Head, Failure> {
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let (reply, opened) = oneshot::channel();
    let end_stream = matches!(upload, Upload::Buffered(b) if b.is_empty());
    let open = Open {
        fields,
        end_stream,
        head_request,
        events: events_tx,
        reply,
    };
    if conn.cmd.send(Cmd::Open(open)).is_err() {
        return Err(Failure::Refused { sent: false });
    }
    let started = tokio::time::Instant::now();
    let mut wait = OpenWait {
        rx: Some(opened),
        cmd: &conn.cmd,
    };
    let opened = {
        let rx = wait.rx.as_mut().expect("set above");
        match head_timeout {
            Some(limit) => match tokio::time::timeout(limit, rx).await {
                Ok(r) => r,
                Err(_) => return Err(Failure::Error(HttpError::Timeout)),
            },
            None => rx.await,
        }
    };
    wait.rx = None;
    let id = match opened {
        Ok(Ok(id)) => id,
        _ => return Err(Failure::Refused { sent: false }),
    };
    let head_timeout = head_timeout.map(|limit| limit.saturating_sub(started.elapsed()));
    let out = StreamOut {
        id,
        tx: conn.cmd.clone(),
    };
    let mut guard = CancelGuard { out: Some(&out) };
    let mut watch = None;
    match upload {
        Upload::Buffered(body) => {
            if !body.is_empty() {
                let _ = conn.cmd.send(Cmd::Data {
                    id,
                    data: body.to_vec(),
                    end_stream: true,
                    ack: None,
                });
            }
        }
        Upload::Reader(reader) => {
            // A failed send means the server reset the stream; its answer (if
            // any) is in the events below.
            if let Err(e) = pump_reader(&out, &mut **reader, step_timeout).await {
                if !matches!(e, HttpError::Protocol(_) | HttpError::Http2(_)) {
                    return Err(Failure::Error(e));
                }
            }
        }
        Upload::Stream(stream) => {
            if let Some(stream) = stream.take() {
                let out = StreamOut {
                    id,
                    tx: conn.cmd.clone(),
                };
                // Dropped with this exchange if it fails (a refusal, a reset,
                // a timeout), which stops the upload.
                let (alive, closed) = oneshot::channel();
                watch = Some(UploadWatch(Some(alive)));
                tokio::spawn(pump_stream(out, stream, closed));
            }
        }
    }
    let sent = !matches!(upload, Upload::Buffered(_));
    // The engine delivers the response head (interim responses are skipped
    // there) before anything else on the stream.
    let wait = async {
        match events.recv().await {
            Some(Event::Head { status, headers }) => Ok((status, headers)),
            Some(Event::Refused) => Err(Failure::Refused { sent }),
            Some(Event::Error(e)) if connection_lost(&e) => Err(Failure::Lost { sent, error: e }),
            Some(Event::Error(e)) => Err(Failure::Error(e)),
            Some(Event::Data(_) | Event::Trailers(_) | Event::End) => {
                Err(Failure::Error(h2_error(
                    Http2Cause::Protocol(Reason::PROTOCOL_ERROR),
                    "response body before its head",
                )))
            }
            None => Err(Failure::Lost {
                sent,
                error: h2_error(Http2Cause::ConnectionClosed, "connection closed"),
            }),
        }
    };
    let (status, headers) = match head_timeout {
        Some(limit) => match tokio::time::timeout(limit, wait).await {
            Ok(r) => r?,
            Err(_) => return Err(Failure::Error(HttpError::Timeout)),
        },
        None => wait.await?,
    };
    guard.out = None;
    Ok(Head {
        status,
        headers,
        body: RecvStream::new(id, events, conn.cmd.clone(), OnDrop::Reset),
        upload: watch,
    })
}

async fn timed<T>(
    limit: Option<Duration>,
    fut: impl std::future::Future<Output = Result<T, HttpError>>,
) -> Result<T, HttpError> {
    match limit {
        Some(d) => match tokio::time::timeout(d, fut).await {
            Ok(r) => r,
            Err(_) => Err(HttpError::Timeout),
        },
        None => fut.await,
    }
}

/// Uploads a borrowed reader's bytes, each read and send bounded by `step`.
async fn pump_reader(
    out: &StreamOut,
    reader: &mut (dyn AsyncRead + Send + Unpin),
    step: Option<Duration>,
) -> Result<(), HttpError> {
    let mut buf = vec![0u8; BODY_PIECE_BYTES];
    loop {
        let n = timed(step, async { Ok(reader.read(&mut buf).await?) }).await?;
        if n == 0 {
            return timed(step, out.data(Vec::new(), true)).await;
        }
        timed(step, out.data(buf[..n].to_vec(), false)).await?;
    }
}

/// Uploads a body stream in the background, ending with its trailers; a
/// failed stream resets the HTTP/2 stream. It also stops (resetting the
/// stream, which is a no-op for one already gone) as soon as `closed` says
/// the HTTP/2 stream is over (see [`UploadWatch`]), dropping `stream` so its
/// producer learns at once that nobody reads it.
async fn pump_stream(out: StreamOut, mut stream: BodyStream, mut closed: oneshot::Receiver<()>) {
    let mut watching = true;
    loop {
        let next = {
            let next = stream.chunk();
            tokio::pin!(next);
            loop {
                if !watching {
                    break next.await;
                }
                tokio::select! {
                    piece = &mut next => break piece,
                    alive = &mut closed => {
                        if alive.is_err() {
                            out.reset(Reason::CANCEL);
                            return;
                        }
                        // Released: the response ended cleanly; carry on.
                        watching = false;
                    }
                }
            }
        };
        match next {
            Ok(Some(chunk)) => {
                if out.data(chunk, false).await.is_err() {
                    return;
                }
            }
            Ok(None) => {
                match stream.take_trailers() {
                    Some(t) if !t.is_empty() => {
                        let _ = out.trailers(&t);
                    }
                    _ => {
                        let _ = out.data(Vec::new(), true).await;
                    }
                }
                return;
            }
            Err(_) => {
                out.reset(Reason::CANCEL);
                return;
            }
        }
    }
}

/// The request header list: pseudo-headers, then the request's headers with
/// lower-case names. Refuses anything that could not be sent safely, as the
/// HTTP/1.1 encoder does.
pub(crate) fn request_fields(
    req: &Request,
    wire: &RequestWire<'_>,
    body_len: Option<usize>,
) -> Result<Vec<(String, String)>, HttpError> {
    if !req.method.is_valid() {
        return Err(HttpError::Protocol("invalid method token".into()));
    }
    if matches!(req.method, Method::Connect) {
        return Err(HttpError::Protocol(
            "CONNECT is not supported over HTTP/2".into(),
        ));
    }
    req.uri.validate_for_wire()?;
    let authority = match req.headers.get("host") {
        Some(host) => host.to_string(),
        None => req.uri.authority(),
    };
    let scheme = if req.uri.is_tls() { "https" } else { "http" };
    let mut fields = vec![
        (":method".to_string(), req.method.as_str().to_string()),
        (":scheme".to_string(), scheme.to_string()),
        (":authority".to_string(), authority),
        (":path".to_string(), req.uri.request_target()),
    ];
    for (k, v) in req.headers.iter() {
        if !is_token(k) {
            return Err(HttpError::Protocol("invalid header name".into()));
        }
        if !v.bytes().all(is_field_value_byte) {
            return Err(HttpError::Protocol(
                "control character in header value".into(),
            ));
        }
        let name = k.to_ascii_lowercase();
        if name == "host" || is_connection_specific(&name) {
            continue;
        }
        if name == "te" && !v.trim().eq_ignore_ascii_case("trailers") {
            continue;
        }
        fields.push((name, trim_value(v)));
    }
    if let Some(ua) = wire.user_agent {
        if !req.headers.contains("user-agent") {
            fields.push(("user-agent".to_string(), ua.to_string()));
        }
    }
    if let Some(ae) = wire.accept_encoding {
        if !req.headers.contains("accept-encoding") {
            fields.push(("accept-encoding".to_string(), ae.to_string()));
        }
    }
    let declared = req.headers.content_length_checked()?;
    if let (Some(n), Some(len)) = (declared, body_len) {
        if n != len as u64 {
            return Err(HttpError::Protocol(
                "Content-Length does not match the body length".into(),
            ));
        }
    }
    if let (None, Some(len)) = (declared, body_len) {
        let expects_content = matches!(req.method, Method::Post | Method::Put | Method::Patch);
        if len > 0 || expects_content {
            fields.push(("content-length".to_string(), len.to_string()));
        }
    }
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(host: &str) -> PoolKey {
        PoolKey {
            host: host.to_string(),
            port: 443,
            tls: true,
            proxy: None,
        }
    }

    #[tokio::test]
    async fn setup_locks_are_pruned_after_use() {
        let pool = Arc::new(H2Pool::default());
        for i in 0..50 {
            let _guard = pool.setup_lock(&key(&format!("h{i}.test"))).await;
        }
        assert_eq!(pool.connecting_len(), 0);
        // Held by one and waited for by another: kept until both are done.
        let first = pool.setup_lock(&key("a.test")).await;
        let waiter = {
            let pool = pool.clone();
            tokio::spawn(async move {
                let _g = pool.setup_lock(&key("a.test")).await;
            })
        };
        tokio::task::yield_now().await;
        assert_eq!(pool.connecting_len(), 1);
        drop(first);
        waiter.await.unwrap();
        assert_eq!(pool.connecting_len(), 0);
    }

    #[test]
    fn h1_only_origins_are_bounded_and_expire() {
        let pool = H2Pool::default();
        for i in 0..(H1_ONLY_MAX + 100) {
            pool.mark_h1_only(key(&format!("h{i}.test")));
        }
        assert!(lock(&pool.h1_only).len() <= H1_ONLY_MAX);
        assert!(pool.is_h1_only(&key(&format!("h{}.test", H1_ONLY_MAX + 99))));
        // An entry past its TTL is forgotten, so h2 is offered again.
        if let Some(old) = Instant::now().checked_sub(H1_ONLY_TTL + Duration::from_secs(1)) {
            lock(&pool.h1_only).insert(key("old.test"), old);
            assert!(!pool.is_h1_only(&key("old.test")));
            assert!(!lock(&pool.h1_only).contains_key(&key("old.test")));
        }
    }
}
