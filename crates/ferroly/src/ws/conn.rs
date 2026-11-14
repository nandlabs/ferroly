//! The connection core shared by the WebSocket client and server: the bounded
//! outgoing queue, the strict read side, the close handshake, keepalive and
//! the idle/write timeouts.
//!
//! One future ([`establish`] returns it) drives a connection. Inside it a
//! *reader* and a *writer* run concurrently:
//!
//! - the writer drains a bounded queue of application messages and a small
//!   priority slot of control frames (at most one pending pong, one pending
//!   ping and one close);
//! - the reader validates and reassembles frames, hands messages to the
//!   application through a bounded queue, owns every timer, and decides when
//!   the connection is over.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::io::{AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::{mpsc, Notify};
use tokio::time::Instant;

use ferroly::http::Conn;

use super::frame::{self, Frame, FrameError, FrameReader, Opcode};
use super::message::close_code;
use super::observer::{TimeoutKind, WsEvent, WsObserver};
use super::rand::MaskGen;
use super::{Message, WsError};

/// Allocates a process-wide unique connection id (clients and servers share
/// the sequence, so ids never collide inside a [`Hub`](super::Hub)).
fn next_conn_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Per-connection limits and timeouts, resolved from the client or server
/// configuration.
#[derive(Clone)]
pub(crate) struct ConnParams {
    /// `true` for the server side (expects masked frames, sends unmasked).
    pub server: bool,
    pub max_frame_size: usize,
    pub max_message_size: usize,
    pub idle_timeout: Option<Duration>,
    pub write_timeout: Option<Duration>,
    pub ping_interval: Option<Duration>,
    pub pong_timeout: Duration,
    pub close_timeout: Duration,
    pub send_timeout: Duration,
    pub send_queue: usize,
    pub recv_queue: usize,
    pub observer: Option<Arc<dyn WsObserver>>,
}

/// A close frame waiting to be written.
struct CloseRequest {
    /// `None` writes an empty close payload.
    code: Option<u16>,
    reason: String,
    /// Write the messages already queued before the close frame.
    flush: bool,
}

/// Control frames waiting for the writer, and the final close result.
#[derive(Default)]
struct Control {
    /// The payload of the most recent unanswered ping. A newer ping replaces
    /// an older one (RFC 6455 §5.5.3 allows answering only the latest), so a
    /// ping flood cannot queue more than one pong.
    pong: Option<Vec<u8>>,
    /// A keepalive ping to send.
    ping: Option<Vec<u8>>,
    close: Option<CloseRequest>,
    /// Why the connection ended, as reported to the application.
    outcome: Option<(u16, String)>,
    outcome_taken: bool,
}

/// State shared between the connection's driver, its senders and (on a
/// server) the shutdown path.
pub(crate) struct Shared {
    id: u64,
    ctl: Mutex<Control>,
    /// Wakes the writer when a control frame is queued.
    writer_wake: Notify,
    /// Wakes the reader when a close was requested, the close frame was
    /// written, or the writer failed.
    reader_wake: Notify,
    /// A close has been requested or received; nothing more may be sent.
    closing: AtomicBool,
    /// The writer has written the close frame.
    close_written: AtomicBool,
    /// The writer failed; the connection is unusable.
    dead: AtomicBool,
    /// The driver has finished.
    closed: AtomicBool,
    send_timeout: Duration,
    observer: Option<Arc<dyn WsObserver>>,
}

impl Shared {
    fn ctl(&self) -> MutexGuard<'_, Control> {
        self.ctl.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// Reports an event to the observer. Never called with the lock held.
    fn emit(&self, event: WsEvent) {
        if let Some(observer) = &self.observer {
            observer.on_event(&event);
        }
    }

    pub(crate) fn is_closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }

    /// Asks the writer to send a close frame. The first request wins; later
    /// ones return `false`.
    ///
    /// `flush` writes the messages already queued first. `record` makes
    /// `(code, reason)` the result the application sees — used for closes the
    /// library decides on (errors, timeouts, shutdown); a close the
    /// application asked for reports the peer's answer instead.
    pub(crate) fn request_close(
        &self,
        code: Option<u16>,
        reason: &str,
        flush: bool,
        record: bool,
    ) -> bool {
        {
            let mut ctl = self.ctl();
            if self.closing.swap(true, Ordering::SeqCst) {
                return false;
            }
            ctl.close = Some(CloseRequest {
                code,
                reason: reason.to_string(),
                flush,
            });
            if record && ctl.outcome.is_none() {
                ctl.outcome = Some((code.unwrap_or(close_code::NO_STATUS), reason.to_string()));
            }
        }
        self.writer_wake.notify_one();
        self.reader_wake.notify_one();
        true
    }

    /// Queues a pong for `payload`, replacing one that is still pending.
    /// Returns `true` when an older pending pong was replaced.
    fn queue_pong(&self, payload: Vec<u8>) -> bool {
        let replaced = self.ctl().pong.replace(payload).is_some();
        self.writer_wake.notify_one();
        replaced
    }

    fn queue_ping(&self, payload: Vec<u8>) {
        self.ctl().ping = Some(payload);
        self.writer_wake.notify_one();
    }

    /// Records why the connection ended, unless a reason is already known.
    fn set_outcome(&self, code: u16, reason: &str) {
        let mut ctl = self.ctl();
        if ctl.outcome.is_none() {
            ctl.outcome = Some((code, reason.to_string()));
        }
    }

    fn has_outcome(&self) -> bool {
        self.ctl().outcome.is_some()
    }

    fn outcome_code(&self) -> u16 {
        self.ctl()
            .outcome
            .as_ref()
            .map_or(close_code::ABNORMAL, |(code, _)| *code)
    }

    /// Hands the close result to the application, once.
    fn take_outcome(&self) -> Option<(u16, String)> {
        let mut ctl = self.ctl();
        if ctl.outcome_taken {
            return None;
        }
        let outcome = ctl.outcome.clone();
        ctl.outcome_taken = outcome.is_some();
        outcome
    }
}

/// A clonable handle for sending on a WebSocket connection from any task.
///
/// Obtained from [`WsClient::sender`](super::WsClient::sender) or
/// [`WsConn::sender`](super::WsConn::sender). Every clone feeds the same
/// **bounded** outgoing queue, so a peer that reads slowly applies
/// backpressure to the senders instead of growing memory:
///
/// - [`send`](Self::send) waits for queue space for up to the configured
///   `send_timeout`. If the queue is still full, the peer is treated as a
///   slow consumer: the connection is closed with code `1008` and `send`
///   returns [`WsError::QueueFull`].
/// - [`try_send`](Self::try_send) never waits and never closes; it returns
///   [`WsError::QueueFull`] and leaves the decision to the caller.
#[derive(Clone)]
pub struct WsSender {
    tx: mpsc::Sender<Message>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for WsSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsSender")
            .field("id", &self.shared.id)
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl WsSender {
    /// The id of the connection this handle sends on (unique in the
    /// process).
    pub fn id(&self) -> u64 {
        self.shared.id
    }

    /// Queues a message, waiting for space if the queue is full.
    ///
    /// Returns once the message is queued, not once it is on the wire. Fails
    /// with [`WsError::Closed`] if the connection is closed or closing, and
    /// with [`WsError::QueueFull`] if no space appeared within the send
    /// timeout (the connection is then closed with code `1008`).
    ///
    /// Sending a [`Message::Close`] is the same as calling
    /// [`close`](Self::close).
    pub async fn send(&self, msg: Message) -> Result<(), WsError> {
        if let Message::Close { code, reason } = msg {
            return self.close(code, &reason);
        }
        if self.shared.is_closing() {
            return Err(WsError::Closed);
        }
        match tokio::time::timeout(self.shared.send_timeout, self.tx.send(msg)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(WsError::Closed),
            Err(_) => {
                self.shared.emit(WsEvent::QueueFull {
                    conn_id: self.shared.id,
                });
                self.close_slow_consumer();
                Err(WsError::QueueFull)
            }
        }
    }

    /// Queues a message without waiting. Fails with [`WsError::QueueFull`]
    /// when the queue is full (the connection stays open) and with
    /// [`WsError::Closed`] when the connection is closed or closing.
    pub fn try_send(&self, msg: Message) -> Result<(), WsError> {
        if let Message::Close { code, reason } = msg {
            return self.close(code, &reason);
        }
        if self.shared.is_closing() {
            return Err(WsError::Closed);
        }
        match self.tx.try_send(msg) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(WsError::Closed),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.shared.emit(WsEvent::QueueFull {
                    conn_id: self.shared.id,
                });
                Err(WsError::QueueFull)
            }
        }
    }

    /// Starts the close handshake with `code` and `reason`.
    ///
    /// Messages already queued are written first, then the close frame; the
    /// connection then waits (up to the close timeout) for the peer's close
    /// frame. Returns immediately. Closing an already closing connection is
    /// a no-op.
    ///
    /// `code` must be one that may appear on the wire
    /// ([`close_code::is_sendable`]): `1000..=1003`, `1007..=1014` or
    /// `3000..=4999`; anything else is [`WsError::Protocol`]. A reason longer
    /// than 123 bytes is truncated at a character boundary.
    pub fn close(&self, code: u16, reason: &str) -> Result<(), WsError> {
        if !close_code::is_sendable(code) {
            return Err(WsError::Protocol(format!(
                "close code {code} may not be sent"
            )));
        }
        self.shared.request_close(Some(code), reason, true, false);
        Ok(())
    }

    /// Whether the connection is closed or closing (nothing more can be
    /// sent).
    pub fn is_closed(&self) -> bool {
        self.shared.is_closing() || self.shared.closed.load(Ordering::SeqCst)
    }

    /// Closes the connection as a slow consumer: code `1008`, queued
    /// messages dropped.
    pub(crate) fn close_slow_consumer(&self) {
        self.shared.request_close(
            Some(close_code::POLICY_VIOLATION),
            "send queue full",
            false,
            true,
        );
    }

    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }
}

/// The receiving side of a connection: a bounded queue of messages followed
/// by one final [`Message::Close`].
pub(crate) struct Incoming {
    rx: mpsc::Receiver<Message>,
    shared: Arc<Shared>,
}

impl Incoming {
    /// The next message; the last one is always a [`Message::Close`], after
    /// which this returns `None`.
    pub(crate) async fn recv(&mut self) -> Option<Message> {
        match self.rx.recv().await {
            Some(msg) => Some(msg),
            None => self
                .shared
                .take_outcome()
                .map(|(code, reason)| Message::Close { code, reason }),
        }
    }
}

/// Sets up the queues and shared state for an upgraded connection and
/// returns the handles plus the future that drives it. The connection lives
/// exactly as long as that future is polled.
pub(crate) fn establish(
    conn: Conn,
    leftover: Vec<u8>,
    params: ConnParams,
) -> (
    WsSender,
    Incoming,
    impl Future<Output = ()> + Send + 'static,
) {
    let shared = Arc::new(Shared {
        id: next_conn_id(),
        ctl: Mutex::new(Control::default()),
        writer_wake: Notify::new(),
        reader_wake: Notify::new(),
        closing: AtomicBool::new(false),
        close_written: AtomicBool::new(false),
        dead: AtomicBool::new(false),
        closed: AtomicBool::new(false),
        send_timeout: params.send_timeout,
        observer: params.observer.clone(),
    });
    let (out_tx, out_rx) = mpsc::channel(params.send_queue.max(1));
    let (in_tx, in_rx) = mpsc::channel(params.recv_queue.max(1));
    let sender = WsSender {
        tx: out_tx,
        shared: shared.clone(),
    };
    let incoming = Incoming {
        rx: in_rx,
        shared: shared.clone(),
    };
    let driver = drive(conn, leftover, params, shared, out_rx, in_tx);
    (sender, incoming, driver)
}

/// Marks the connection closed when the driver ends, however it ends
/// (including being dropped or aborted).
struct EndGuard(Arc<Shared>);

impl Drop for EndGuard {
    fn drop(&mut self) {
        let shared = &self.0;
        shared.set_outcome(close_code::ABNORMAL, "connection aborted");
        shared.closing.store(true, Ordering::SeqCst);
        shared.closed.store(true, Ordering::SeqCst);
        shared.emit(WsEvent::Closed {
            conn_id: shared.id,
            code: shared.outcome_code(),
        });
    }
}

async fn drive(
    conn: Conn,
    leftover: Vec<u8>,
    params: ConnParams,
    shared: Arc<Shared>,
    out_rx: mpsc::Receiver<Message>,
    in_tx: mpsc::Sender<Message>,
) {
    shared.emit(WsEvent::Opened { conn_id: shared.id });
    let (read_half, write_half) = tokio::io::split(conn);
    let now = Instant::now();
    let reader = ReadSide {
        frames: FrameReader::new(read_half, leftover, params.server, params.max_frame_size),
        shared: shared.clone(),
        in_tx: Some(in_tx),
        max_message_size: params.max_message_size,
        idle_timeout: params.idle_timeout,
        ping_interval: params.ping_interval,
        pong_timeout: params.pong_timeout,
        close_timeout: params.close_timeout,
        frag: None,
        last_rx: now,
        ping_due: params.ping_interval.map(|every| after(now, every)),
        awaiting_pong: None,
        ping_seq: 0,
    }
    .run();
    let writer = write_loop(
        write_half,
        out_rx,
        shared.clone(),
        params.server,
        params.write_timeout,
    );
    tokio::pin!(reader, writer);
    // Declared last so it is dropped first: the close result is recorded
    // before the incoming queue's sender (inside `reader`) goes away.
    let _guard = EndGuard(shared);

    // The reader decides when the connection is over; the writer may finish
    // earlier (after writing the close frame).
    let mut writer_done = false;
    loop {
        tokio::select! {
            _ = &mut reader => break,
            _ = &mut writer, if !writer_done => writer_done = true,
        }
    }
}

// ---- writer --------------------------------------------------------------

async fn write_loop(
    mut w: WriteHalf<Conn>,
    mut rx: mpsc::Receiver<Message>,
    shared: Arc<Shared>,
    server: bool,
    write_timeout: Option<Duration>,
) {
    // Clients mask every frame with a key from a per-connection generator.
    let mut mask = if server { None } else { Some(MaskGen::new()) };
    // Cleared once every sender is gone; only control frames remain then.
    let mut queue_open = true;
    loop {
        // Control frames first: they are small, bounded in number and must
        // not wait behind queued application data.
        let (pong, ping, close) = {
            let mut ctl = shared.ctl();
            (ctl.pong.take(), ctl.ping.take(), ctl.close.take())
        };
        if let Some(payload) = pong {
            if !write_frame(
                &mut w,
                &shared,
                &mut mask,
                Opcode::Pong,
                &payload,
                write_timeout,
            )
            .await
            {
                return;
            }
        }
        if let Some(payload) = ping {
            if !write_frame(
                &mut w,
                &shared,
                &mut mask,
                Opcode::Ping,
                &payload,
                write_timeout,
            )
            .await
            {
                return;
            }
            shared.emit(WsEvent::PingSent { conn_id: shared.id });
        }
        if let Some(close) = close {
            if close.flush {
                while let Ok(msg) = rx.try_recv() {
                    if !write_message(&mut w, &shared, &mut mask, msg, write_timeout).await {
                        return;
                    }
                }
            }
            let payload = frame::close_payload(close.code, &close.reason);
            if !write_frame(
                &mut w,
                &shared,
                &mut mask,
                Opcode::Close,
                &payload,
                write_timeout,
            )
            .await
            {
                return;
            }
            // Nothing follows a close frame: half-close the transport (this
            // also sends the TLS `close_notify`).
            let _ = bounded(write_timeout, w.shutdown()).await;
            shared.close_written.store(true, Ordering::SeqCst);
            shared.reader_wake.notify_one();
            return;
        }
        tokio::select! {
            biased;
            _ = shared.writer_wake.notified() => {}
            msg = rx.recv(), if queue_open => match msg {
                Some(msg) => {
                    if !write_message(&mut w, &shared, &mut mask, msg, write_timeout).await {
                        return;
                    }
                }
                // Every sender is gone. Whoever dropped them (a handler
                // that returned or panicked) decides how the connection is
                // closed; keep serving control frames until then.
                None => queue_open = false,
            },
        }
    }
}

async fn write_message(
    w: &mut WriteHalf<Conn>,
    shared: &Shared,
    mask: &mut Option<MaskGen>,
    msg: Message,
    limit: Option<Duration>,
) -> bool {
    let (opcode, payload) = match &msg {
        Message::Text(text) => (Opcode::Text, text.as_bytes()),
        Message::Binary(bytes) => (Opcode::Binary, bytes.as_slice()),
        // Senders turn a close message into a close request; one can never
        // be queued.
        Message::Close { .. } => return true,
    };
    if !write_frame(w, shared, mask, opcode, payload, limit).await {
        return false;
    }
    shared.emit(WsEvent::MessageSent {
        conn_id: shared.id,
        bytes: payload.len(),
    });
    true
}

/// Writes one frame, bounded by the write timeout. On failure the connection
/// is marked dead, the reader is woken, and `false` is returned.
async fn write_frame(
    w: &mut WriteHalf<Conn>,
    shared: &Shared,
    mask: &mut Option<MaskGen>,
    opcode: Opcode,
    payload: &[u8],
    limit: Option<Duration>,
) -> bool {
    let key = mask.as_mut().map(MaskGen::next_key);
    let bytes = frame::encode(true, opcode, payload, key);
    let result = bounded(limit, async {
        w.write_all(&bytes).await?;
        w.flush().await
    })
    .await;
    let why = match result {
        Ok(Ok(())) => return true,
        Ok(Err(_)) => "write failed",
        Err(()) => {
            shared.emit(WsEvent::TimedOut {
                conn_id: shared.id,
                kind: TimeoutKind::Write,
            });
            "write timed out"
        }
    };
    shared.set_outcome(close_code::ABNORMAL, why);
    shared.dead.store(true, Ordering::SeqCst);
    shared.reader_wake.notify_one();
    false
}

/// Awaits `fut`, giving up with `Err(())` after `limit` when one is set.
async fn bounded<T>(limit: Option<Duration>, fut: impl Future<Output = T>) -> Result<T, ()> {
    match limit {
        Some(limit) => tokio::time::timeout(limit, fut).await.map_err(|_| ()),
        None => Ok(fut.await),
    }
}

// ---- reader --------------------------------------------------------------

/// A fragmented message being reassembled.
struct Fragments {
    text: bool,
    buf: Vec<u8>,
    /// For text: how many bytes of `buf` are known to be valid UTF-8 (the
    /// rest is at most one incomplete sequence).
    validated: usize,
}

/// What the reader does after handling an event.
enum Flow {
    /// Keep reading.
    Continue,
    /// The peer's close frame arrived; our answer is queued.
    PeerClosed,
    /// A close was requested on this side.
    LocalClose,
    /// The connection is unusable; stop immediately.
    Dead,
    /// The peer broke the protocol: fail with this close code.
    Fail(u16, &'static str),
}

/// How the reader waits for the peer once a close is under way.
#[derive(Clone, Copy)]
enum Wait {
    /// Keep parsing frames, looking for the peer's close frame.
    Frames,
    /// The stream can no longer be trusted (protocol error): discard bytes
    /// until the peer closes.
    Discard,
}

struct ReadSide {
    frames: FrameReader<ReadHalf<Conn>>,
    shared: Arc<Shared>,
    /// Dropped as soon as no further message will be delivered, so the
    /// application sees the end of the stream promptly.
    in_tx: Option<mpsc::Sender<Message>>,
    max_message_size: usize,
    idle_timeout: Option<Duration>,
    ping_interval: Option<Duration>,
    pong_timeout: Duration,
    close_timeout: Duration,
    frag: Option<Fragments>,
    /// When the last frame arrived.
    last_rx: Instant,
    /// When the next keepalive ping is due.
    ping_due: Option<Instant>,
    /// The payload of the keepalive ping awaiting its pong, and the deadline.
    awaiting_pong: Option<(Vec<u8>, Instant)>,
    ping_seq: u64,
}

impl ReadSide {
    fn id(&self) -> u64 {
        self.shared.id
    }

    async fn run(mut self) {
        loop {
            let deadline = self.next_deadline();
            let flow = tokio::select! {
                biased;
                _ = self.shared.reader_wake.notified() => self.on_wake(),
                frame = self.frames.read_frame() => match frame {
                    Ok(frame) => {
                        self.last_rx = Instant::now();
                        self.on_frame(frame).await
                    }
                    Err(FrameError::Protocol(why)) => {
                        Flow::Fail(close_code::PROTOCOL_ERROR, why)
                    }
                    Err(FrameError::TooBig) => {
                        Flow::Fail(close_code::MESSAGE_TOO_BIG, "frame exceeds the size limit")
                    }
                    Err(FrameError::Eof) => {
                        self.shared.set_outcome(
                            close_code::ABNORMAL,
                            "connection closed without a close frame",
                        );
                        Flow::Dead
                    }
                    Err(FrameError::Io(error)) => {
                        self.shared.set_outcome(
                            close_code::ABNORMAL,
                            &format!("transport error: {error}"),
                        );
                        Flow::Dead
                    }
                },
                _ = sleep_until_opt(deadline) => self.on_timer(),
            };
            match flow {
                Flow::Continue => {}
                Flow::Dead => return,
                Flow::PeerClosed => return self.finish(Wait::Frames, true).await,
                Flow::LocalClose => return self.finish(Wait::Frames, false).await,
                Flow::Fail(code, reason) => {
                    self.shared.emit(WsEvent::ProtocolError {
                        conn_id: self.id(),
                        code,
                        reason,
                    });
                    // Recorded even if a close was already under way.
                    self.shared.set_outcome(code, reason);
                    self.shared.request_close(Some(code), reason, false, true);
                    return self.finish(Wait::Discard, false).await;
                }
            }
        }
    }

    /// Re-reads the shared flags after a wake-up.
    fn on_wake(&self) -> Flow {
        if self.shared.dead.load(Ordering::SeqCst) {
            Flow::Dead
        } else if self.shared.is_closing() {
            Flow::LocalClose
        } else {
            Flow::Continue
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        let mut consider = |at: Instant| {
            next = Some(match next {
                Some(current) if current <= at => current,
                _ => at,
            });
        };
        if let Some(idle) = self.idle_timeout {
            consider(after(self.last_rx, idle));
        }
        if let Some((_, deadline)) = &self.awaiting_pong {
            consider(*deadline);
        }
        if let Some(due) = self.ping_due {
            consider(due);
        }
        next
    }

    fn on_timer(&mut self) -> Flow {
        let now = Instant::now();
        if let Some(idle) = self.idle_timeout {
            if now >= after(self.last_rx, idle) {
                self.shared.emit(WsEvent::TimedOut {
                    conn_id: self.id(),
                    kind: TimeoutKind::Idle,
                });
                self.shared.request_close(
                    Some(close_code::GOING_AWAY),
                    "idle timeout",
                    false,
                    true,
                );
                return Flow::LocalClose;
            }
        }
        if let Some((_, deadline)) = &self.awaiting_pong {
            if now >= *deadline {
                self.shared.emit(WsEvent::TimedOut {
                    conn_id: self.id(),
                    kind: TimeoutKind::Pong,
                });
                self.shared.request_close(
                    Some(close_code::INTERNAL_ERROR),
                    "keepalive timeout",
                    false,
                    true,
                );
                return Flow::LocalClose;
            }
        }
        if let (Some(due), Some(every)) = (self.ping_due, self.ping_interval) {
            if now >= due {
                // One ping outstanding at a time: if the previous one is
                // still unanswered its deadline is already ticking.
                if self.awaiting_pong.is_none() {
                    self.ping_seq = self.ping_seq.wrapping_add(1);
                    let payload = self.ping_seq.to_be_bytes().to_vec();
                    self.shared.queue_ping(payload.clone());
                    self.awaiting_pong = Some((payload, after(now, self.pong_timeout)));
                }
                self.ping_due = Some(after(now, every));
            }
        }
        Flow::Continue
    }

    async fn on_frame(&mut self, frame: Frame) -> Flow {
        match frame.opcode {
            Opcode::Ping => {
                self.shared
                    .emit(WsEvent::PingReceived { conn_id: self.id() });
                // The pong carries the ping's payload unchanged (§5.5.3).
                self.shared.queue_pong(frame.payload);
                Flow::Continue
            }
            Opcode::Pong => {
                // Only a pong matching the outstanding keepalive ping counts;
                // unsolicited pongs are ignored (§5.5.3).
                let matches =
                    matches!(&self.awaiting_pong, Some((sent, _)) if *sent == frame.payload);
                if matches {
                    self.awaiting_pong = None;
                    self.shared
                        .emit(WsEvent::PongReceived { conn_id: self.id() });
                }
                Flow::Continue
            }
            Opcode::Close => match frame::parse_close(&frame.payload) {
                Ok(parsed) => {
                    let code = parsed.as_ref().map(|(code, _)| *code);
                    let (shown, reason) = parsed.unwrap_or((close_code::NO_STATUS, String::new()));
                    self.shared.set_outcome(shown, &reason);
                    // Echo the status code back (§5.5.1); queued data is
                    // dropped, the peer no longer wants it.
                    self.shared.request_close(code, "", false, false);
                    Flow::PeerClosed
                }
                Err((code, why)) => Flow::Fail(code, why),
            },
            Opcode::Text | Opcode::Binary => {
                if self.frag.is_some() {
                    return Flow::Fail(
                        close_code::PROTOCOL_ERROR,
                        "new data frame inside a fragmented message",
                    );
                }
                if frame.payload.len() > self.max_message_size {
                    return Flow::Fail(
                        close_code::MESSAGE_TOO_BIG,
                        "message exceeds the size limit",
                    );
                }
                let text = frame.opcode == Opcode::Text;
                if frame.fin {
                    return self.complete(text, frame.payload).await;
                }
                let mut frag = Fragments {
                    text,
                    buf: frame.payload,
                    validated: 0,
                };
                if text && !advance_utf8(&mut frag) {
                    return Flow::Fail(
                        close_code::INVALID_PAYLOAD,
                        "text message is not valid UTF-8",
                    );
                }
                self.frag = Some(frag);
                Flow::Continue
            }
            Opcode::Continuation => {
                let Some(frag) = self.frag.as_mut() else {
                    return Flow::Fail(
                        close_code::PROTOCOL_ERROR,
                        "continuation frame without a message to continue",
                    );
                };
                if frag.buf.len().saturating_add(frame.payload.len()) > self.max_message_size {
                    self.frag = None;
                    return Flow::Fail(
                        close_code::MESSAGE_TOO_BIG,
                        "message exceeds the size limit",
                    );
                }
                frag.buf.extend_from_slice(&frame.payload);
                // Validate text as it arrives, so invalid UTF-8 is refused
                // at the offending fragment rather than at the end.
                if frag.text && !advance_utf8(frag) {
                    self.frag = None;
                    return Flow::Fail(
                        close_code::INVALID_PAYLOAD,
                        "text message is not valid UTF-8",
                    );
                }
                if !frame.fin {
                    return Flow::Continue;
                }
                let Some(frag) = self.frag.take() else {
                    return Flow::Continue;
                };
                self.complete(frag.text, frag.buf).await
            }
        }
    }

    /// Turns a complete payload into a message and hands it over.
    async fn complete(&mut self, text: bool, payload: Vec<u8>) -> Flow {
        let msg = if text {
            // Strict: invalid UTF-8 fails the connection, it is never
            // replaced (§8.1).
            match String::from_utf8(payload) {
                Ok(text) => Message::Text(text),
                Err(_) => {
                    return Flow::Fail(
                        close_code::INVALID_PAYLOAD,
                        "text message is not valid UTF-8",
                    )
                }
            }
        } else {
            Message::Binary(payload)
        };
        self.deliver(msg).await
    }

    /// Queues a message for the application, waiting while the incoming
    /// queue is full (which stops reading and so pushes back on the peer).
    async fn deliver(&mut self, msg: Message) -> Flow {
        self.shared.emit(WsEvent::MessageReceived {
            conn_id: self.id(),
            bytes: msg.len(),
        });
        let Some(tx) = self.in_tx.clone() else {
            return Flow::Continue;
        };
        let started = Instant::now();
        let permit = loop {
            tokio::select! {
                biased;
                _ = self.shared.reader_wake.notified() => match self.on_wake() {
                    Flow::Continue => {}
                    other => return other,
                },
                permit = tx.reserve() => match permit {
                    Ok(permit) => break permit,
                    Err(_) => {
                        // The application dropped its receiving half (its
                        // handler returned or panicked). The owner of the
                        // connection closes it; until then incoming
                        // messages have nowhere to go.
                        self.in_tx = None;
                        return Flow::Continue;
                    }
                },
            }
        };
        permit.send(msg);
        // Time spent waiting for the application is not the peer's silence:
        // restart the idle clock and extend a pending pong deadline.
        let now = Instant::now();
        let waited = now - started;
        self.last_rx = now;
        if let Some((_, deadline)) = self.awaiting_pong.as_mut() {
            *deadline = after(*deadline, waited);
        }
        Flow::Continue
    }

    /// Completes the close handshake: waits (bounded by the close timeout)
    /// for our close frame to be written and for the peer to finish.
    async fn finish(mut self, wait: Wait, mut peer_done: bool) {
        self.frag = None;
        let shared = self.shared.clone();
        let deadline = after(Instant::now(), self.close_timeout);
        loop {
            if shared.has_outcome() {
                // The result is known: let the application see the end of
                // the stream without waiting for the handshake to finish.
                self.in_tx = None;
            }
            if shared.dead.load(Ordering::SeqCst) {
                break;
            }
            if peer_done && shared.close_written.load(Ordering::SeqCst) {
                break;
            }
            tokio::select! {
                biased;
                _ = shared.reader_wake.notified() => {}
                _ = tokio::time::sleep_until(deadline) => break,
                done = self.next_while_closing(wait), if !peer_done => peer_done = done,
            }
        }
        shared.set_outcome(
            close_code::ABNORMAL,
            if peer_done {
                "connection closed without a close frame"
            } else {
                "close handshake timed out"
            },
        );
    }

    /// Reads once while closing. Returns `true` when the peer is done (its
    /// close frame arrived, or the stream ended).
    async fn next_while_closing(&mut self, wait: Wait) -> bool {
        match wait {
            Wait::Discard => !matches!(self.frames.discard().await, Ok(n) if n > 0),
            Wait::Frames => match self.frames.read_frame().await {
                Ok(frame) if frame.opcode == Opcode::Close => {
                    if let Ok(parsed) = frame::parse_close(&frame.payload) {
                        let (code, reason) =
                            parsed.unwrap_or((close_code::NO_STATUS, String::new()));
                        self.shared.set_outcome(code, &reason);
                    }
                    true
                }
                // Data that crossed our close frame on the wire is dropped.
                Ok(_) => false,
                Err(_) => true,
            },
        }
    }
}

/// Extends the validated prefix of a text message under reassembly. Returns
/// `false` if the bytes can no longer become valid UTF-8.
fn advance_utf8(frag: &mut Fragments) -> bool {
    match std::str::from_utf8(&frag.buf[frag.validated..]) {
        Ok(_) => {
            frag.validated = frag.buf.len();
            true
        }
        // An incomplete sequence at the very end may be completed by the
        // next fragment.
        Err(e) if e.error_len().is_none() => {
            frag.validated += e.valid_up_to();
            true
        }
        Err(_) => false,
    }
}

/// `base + delay`, saturating far in the future instead of panicking when a
/// timeout is configured as an effectively infinite duration.
pub(crate) fn after(base: Instant, delay: Duration) -> Instant {
    const FAR: Duration = Duration::from_secs(60 * 60 * 24 * 365 * 30);
    base.checked_add(delay)
        .or_else(|| base.checked_add(FAR))
        .unwrap_or(base)
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

    fn params(server: bool) -> ConnParams {
        ConnParams {
            server,
            max_frame_size: 1 << 16,
            max_message_size: 1 << 16,
            idle_timeout: None,
            write_timeout: Some(Duration::from_secs(2)),
            ping_interval: None,
            pong_timeout: Duration::from_secs(1),
            close_timeout: Duration::from_millis(200),
            send_timeout: Duration::from_millis(100),
            send_queue: 2,
            recv_queue: 2,
            observer: None,
        }
    }

    fn start(p: ConnParams) -> (WsSender, Incoming, DuplexStream) {
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let (sender, incoming, driver) = establish(Box::new(ours), Vec::new(), p);
        tokio::spawn(driver);
        (sender, incoming, theirs)
    }

    #[test]
    fn infinite_timeouts_do_not_overflow_deadlines() {
        let now = Instant::now();
        assert!(after(now, Duration::MAX) > now);
        assert_eq!(
            after(now, Duration::from_secs(1)),
            now + Duration::from_secs(1)
        );
    }

    #[test]
    fn pending_pongs_are_coalesced() {
        let shared = Shared {
            id: 1,
            ctl: Mutex::new(Control::default()),
            writer_wake: Notify::new(),
            reader_wake: Notify::new(),
            closing: AtomicBool::new(false),
            close_written: AtomicBool::new(false),
            dead: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            send_timeout: Duration::from_secs(1),
            observer: None,
        };
        assert!(!shared.queue_pong(b"one".to_vec()));
        // A second ping before the first pong was written replaces it: a
        // flood never holds more than one pending pong.
        assert!(shared.queue_pong(b"two".to_vec()));
        assert!(shared.queue_pong(b"three".to_vec()));
        assert_eq!(shared.ctl().pong.take().unwrap(), b"three");
        assert!(shared.ctl().pong.is_none());
    }

    #[test]
    fn the_first_close_request_wins() {
        // The driver is never polled here: only the request bookkeeping is
        // under test.
        let (ours, _theirs) = tokio::io::duplex(64);
        let (sender, _incoming, _driver) = establish(Box::new(ours), Vec::new(), params(true));
        assert!(sender.close(1005, "").is_err()); // reserved code
        assert!(sender.close(4000, "bye").is_ok());
        assert!(sender.is_closed());
        assert!(!sender
            .shared()
            .request_close(Some(1001), "later", false, true));
        let ctl = sender.shared().ctl();
        assert_eq!(ctl.close.as_ref().unwrap().code, Some(4000));
        assert!(ctl.outcome.is_none());
    }

    #[tokio::test]
    async fn server_side_reads_a_masked_message_and_echoes_the_close_code() {
        let (sender, mut incoming, mut peer) = start(params(true));
        peer.write_all(&frame::encode(
            true,
            Opcode::Text,
            b"hi",
            Some([1, 2, 3, 4]),
        ))
        .await
        .unwrap();
        assert_eq!(incoming.recv().await, Some(Message::text("hi")));
        sender.send(Message::text("yo")).await.unwrap();
        let mut head = [0u8; 4];
        peer.read_exact(&mut head).await.unwrap();
        assert_eq!(&head, &[0x81, 2, b'y', b'o']); // unmasked server frame

        let close = frame::close_payload(Some(4001), "done");
        peer.write_all(&frame::encode(
            true,
            Opcode::Close,
            &close,
            Some([9, 9, 9, 9]),
        ))
        .await
        .unwrap();
        assert_eq!(incoming.recv().await, Some(Message::close(4001, "done")));
        assert_eq!(incoming.recv().await, None);
        let mut echo = [0u8; 4];
        peer.read_exact(&mut echo).await.unwrap();
        assert_eq!(&echo, &[0x88, 2, 0x0f, 0xa1]); // close 4001, no reason
        assert!(matches!(
            sender.send(Message::text("late")).await,
            Err(WsError::Closed)
        ));
    }

    #[tokio::test]
    async fn client_side_masks_frames_with_changing_keys() {
        let (sender, _incoming, mut peer) = start(params(false));
        sender.send(Message::text("aaaa")).await.unwrap();
        sender.send(Message::text("aaaa")).await.unwrap();
        let mut a = [0u8; 10];
        let mut b = [0u8; 10];
        peer.read_exact(&mut a).await.unwrap();
        peer.read_exact(&mut b).await.unwrap();
        assert_eq!(a[1], 0x84); // masked, 4 bytes
        assert_ne!(a[2..6], b[2..6]); // a fresh key per frame
        let mut payload = a[6..].to_vec();
        frame::apply_mask(&mut payload, [a[2], a[3], a[4], a[5]]);
        assert_eq!(payload, b"aaaa");
    }

    #[tokio::test]
    async fn fragmented_text_is_validated_across_fragments() {
        let (_sender, mut incoming, mut peer) = start(params(true));
        let euro = "€".as_bytes(); // E2 82 AC, split across two frames
        peer.write_all(&frame::encode(
            false,
            Opcode::Text,
            &euro[..1],
            Some([0; 4]),
        ))
        .await
        .unwrap();
        peer.write_all(&frame::encode(
            true,
            Opcode::Continuation,
            &euro[1..],
            Some([0; 4]),
        ))
        .await
        .unwrap();
        assert_eq!(incoming.recv().await, Some(Message::text("€")));
    }
}
