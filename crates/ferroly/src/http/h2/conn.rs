//! The HTTP/2 connection engine shared by the server and the client.
//!
//! One connection runs as two tasks:
//!
//! - a **reader** that reads the preface and frames off the socket, applies
//!   the per-frame rules, and assembles HEADERS + CONTINUATION into complete
//!   header blocks (bounded in size, frame count and time). It feeds a small
//!   bounded queue, so a peer that sends faster than the connection can
//!   process is pushed back by TCP;
//! - the **actor** ([`Connection::run`]), which owns every piece of state —
//!   streams, flow-control windows, HPACK tables, the output buffer — and
//!   reacts to inbound frames, to commands from the tasks that produce and
//!   consume stream bodies ([`Cmd`]), and to a housekeeping tick.
//!
//! Output is encoded into one buffer that the actor writes as the socket
//! accepts it. Past a first mark the actor stops scheduling DATA; past a
//! higher one it also stops reading frames, so a peer that does not read
//! cannot make it grow: everything the connection queues is bounded by flow
//! control, the stream limit or those pauses. Between the two marks frames
//! are still read, so a peer that reads a large response slowly still has its
//! PINGs, resets and window updates processed.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::AbortHandle;
use tokio::time::Instant;

use super::error::{h2_error, Reason};
use super::frame::{self, setting, Frame, FrameError, Head, Priority};
use super::hpack::{self, DecodeLimits};
use super::server::ServerCtx;
use super::{Event, Http2Config};
use crate::http::{HeaderMap, Http2Cause, HttpError, Request, StatusCode};

/// Output buffered beyond this pauses DATA scheduling.
const WRITE_HIGH_WATER: usize = 64 * 1024;
/// Output buffered beyond this pauses reading frames too. What a frame read
/// can add to the output directly (an acknowledgement, a reset, a window
/// update) is small and covered by the flood budgets; responses go through
/// the scheduler, which stops at [`WRITE_HIGH_WATER`].
const READ_HIGH_WATER: usize = 256 * 1024;
/// How much a response blocked on flow control must send before the time it
/// spent blocked is forgotten (the stream stall timeout counts blocked time
/// across short bursts of progress, so a 1-byte window trickle cannot hold a
/// stream for ever).
const STALL_PROGRESS: usize = 16 * 1024;
/// Frames read ahead of the actor.
const INBOUND_QUEUE: usize = 8;
/// Most CONTINUATION frames in one header block (CVE-2024-27316).
const MAX_CONTINUATIONS: usize = 64;
/// Most interim (1xx) responses accepted on one client stream.
const MAX_INTERIM: usize = 16;
/// After a fatal error, how long and how much input is drained so the peer
/// can read the GOAWAY before the socket closes.
const LINGER: Duration = Duration::from_secs(1);
const LINGER_BYTES: u64 = 1 << 20;
/// Bound on the final flush when no write timeout is configured.
const CLOSE_FLUSH: Duration = Duration::from_secs(5);
/// The largest stream identifier.
const MAX_STREAM_ID: u32 = (1 << 31) - 1;
/// How many closed streams are remembered, to tell late frames on a stream
/// this side reset (ignored) from frames on a stream the peer had already
/// ended or reset (errors), and lower ids that were never used.
const RECENT_CLOSED: usize = 256;
/// PING payloads: a keep-alive probe, and the first phase of a graceful
/// GOAWAY.
const PING_KEEPALIVE: [u8; 8] = *b"ferro-ka";
const PING_GOAWAY: [u8; 8] = *b"ferro-ga";
/// Longest wait between the two GOAWAY frames of a graceful shutdown when
/// the peer does not answer the PING.
const GOAWAY_GRACE: Duration = Duration::from_secs(1);

/// What the engine needs to know about its role and limits.
#[derive(Debug, Clone)]
pub(crate) struct Options {
    pub(crate) server: bool,
    /// Local settings and flood budgets (already normalised).
    pub(crate) h2: Http2Config,
    /// Cap on a decoded header list.
    pub(crate) max_list_size: usize,
    /// Cap on the number of fields in one header list.
    pub(crate) max_fields: usize,
    /// Close the connection after this long with no stream.
    pub(crate) idle_timeout: Option<Duration>,
    /// Close the connection when buffered output makes no progress for this
    /// long.
    pub(crate) write_timeout: Option<Duration>,
    /// Server: deadline for the preface + SETTINGS, and for each header
    /// block once its first frame has arrived.
    pub(crate) head_timeout: Option<Duration>,
    /// Server: deadline for a buffered request body.
    pub(crate) body_timeout: Option<Duration>,
    /// Server: reset a stream whose response cannot be sent (the peer gives
    /// no flow-control credit) for this long.
    pub(crate) stall_timeout: Option<Duration>,
    /// Server: after this many streams, finish with GOAWAY.
    pub(crate) max_requests: Option<u64>,
    /// Server: buffered request-body bytes held across all streams before
    /// connection-level credit is withheld.
    pub(crate) body_budget: usize,
}

/// State visible outside the actor (the client's connection cache).
#[derive(Debug)]
pub(crate) struct Shared {
    /// The connection may still open new streams.
    pub(crate) usable: AtomicBool,
    /// Streams open or waiting for a slot (published by the actor).
    pub(crate) active: AtomicUsize,
    /// The peer's stream limit (published by the actor).
    pub(crate) capacity: AtomicUsize,
}

impl Shared {
    pub(crate) fn new() -> Self {
        Shared {
            usable: AtomicBool::new(true),
            active: AtomicUsize::new(0),
            capacity: AtomicUsize::new(usize::MAX),
        }
    }

    /// Whether every stream slot the peer allows is taken.
    pub(crate) fn saturated(&self) -> bool {
        self.active.load(Ordering::Relaxed) >= self.capacity.load(Ordering::Relaxed)
    }
}

/// Why a client stream could not be opened.
#[derive(Debug)]
pub(crate) enum OpenError {
    /// The connection is going away or closed; nothing was sent, so the
    /// request can be retried on another connection.
    Refused,
}

/// A request to open a client stream.
pub(crate) struct Open {
    pub(crate) fields: Vec<(String, String)>,
    pub(crate) end_stream: bool,
    pub(crate) head_request: bool,
    pub(crate) events: mpsc::UnboundedSender<Event>,
    pub(crate) reply: oneshot::Sender<Result<u32, OpenError>>,
}

/// A command from a task that produces or consumes a stream.
pub(crate) enum Cmd {
    /// Client: open a stream and send its request head.
    Open(Open),
    /// Server: send the response head.
    Headers {
        id: u32,
        fields: Vec<(String, String)>,
        end_stream: bool,
    },
    /// Send body data. `ack` is answered once the data has been handed to the
    /// output buffer (or the stream is gone), which is what paces producers.
    Data {
        id: u32,
        data: Vec<u8>,
        end_stream: bool,
        ack: Option<oneshot::Sender<Result<(), HttpError>>>,
    },
    /// Send trailers, ending the stream.
    Trailers {
        id: u32,
        fields: Vec<(String, String)>,
    },
    /// Reset the stream.
    Reset { id: u32, reason: Reason },
    /// The consumer took `n` body bytes: credit the flow-control windows.
    Consumed { id: u32, n: usize },
    /// Server: the handler dropped the request body; discard the rest.
    Discard { id: u32 },
    /// Server: the stream's handler task has finished.
    TaskDone { id: u32 },
}

/// What the reader hands the actor.
enum Inbound {
    Frame(Frame),
    /// A complete header block.
    Headers {
        stream: u32,
        block: Vec<u8>,
        end_stream: bool,
        priority: Option<Priority>,
    },
    Error(FrameError),
    Timeout,
    Closed,
    Io(std::io::Error),
}

/// A connection-fatal condition.
struct Fatal {
    /// Send GOAWAY with this code and debug text first.
    goaway: Option<(Reason, &'static str)>,
    /// What to report; the open streams get [`Connection::stream_failure`]
    /// of it.
    error: Option<HttpError>,
}

impl Fatal {
    /// The peer broke a connection-level rule (or exceeded a budget, for
    /// `ENHANCE_YOUR_CALM`): GOAWAY with `reason`.
    fn protocol(reason: Reason, why: &'static str) -> Self {
        let cause = if reason == Reason::ENHANCE_YOUR_CALM {
            Http2Cause::LocalLimit
        } else {
            Http2Cause::Protocol(reason)
        };
        Fatal {
            goaway: Some((reason, why)),
            error: Some(h2_error(cause, why)),
        }
    }

    /// A connection-level timer fired.
    fn timeout(goaway: Option<Reason>, why: &'static str) -> Self {
        Fatal {
            goaway: goaway.map(|r| (r, why)),
            error: Some(h2_error(Http2Cause::Timeout, why)),
        }
    }

    fn quiet(error: Option<HttpError>) -> Self {
        Fatal {
            goaway: None,
            error,
        }
    }
}

/// A malformed message on a stream: reset with `PROTOCOL_ERROR`.
fn malformed(why: &'static str) -> Option<HttpError> {
    Some(h2_error(Http2Cause::Protocol(Reason::PROTOCOL_ERROR), why))
}

/// Copies an engine error (they are all cheap to copy) for the next consumer.
fn dup(e: &HttpError) -> HttpError {
    match e {
        HttpError::Http2(e) => HttpError::Http2(e.clone()),
        HttpError::Timeout => HttpError::Timeout,
        HttpError::Protocol(m) => HttpError::Protocol(m.clone()),
        HttpError::Io(io) => HttpError::Io(std::io::Error::new(io.kind(), io.to_string())),
        other => h2_error(Http2Cause::ConnectionClosed, other.to_string()),
    }
}

/// How a stream ended, remembered for a while after it is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Closed {
    /// This side reset it (or refused it): late frames are expected.
    ByUs,
    /// The peer reset it: further frames are a stream error.
    PeerReset,
    /// Both sides ended it normally: further frames are a connection error.
    Ended,
}

/// A bounded memory of recently closed streams.
#[derive(Default)]
struct RecentlyClosed {
    map: HashMap<u32, Closed>,
    order: VecDeque<u32>,
    /// The highest id forgotten so far: below it, "unknown" may mean
    /// "closed long ago".
    forgotten_max: u32,
}

impl RecentlyClosed {
    fn record(&mut self, id: u32, how: Closed) {
        if self.map.insert(id, how).is_none() {
            self.order.push_back(id);
            if self.order.len() > RECENT_CLOSED {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                    self.forgotten_max = self.forgotten_max.max(old);
                }
            }
        }
    }

    fn get(&self, id: u32) -> Option<Closed> {
        self.map.get(&id).copied()
    }
}

/// Where a stream's inbound body goes.
enum Sink {
    /// Thrown away (but credited).
    Discard,
    /// Server, buffered mode: collected, then the request is dispatched.
    Buffer {
        req: Option<Box<Request>>,
        body: Vec<u8>,
        max: usize,
        head_request: bool,
        /// What the body may take of the connection's body budget: its
        /// content-length, or `max` when it has none.
        reserve: usize,
        /// The reservation is held: the stream gets stream-level credit.
        /// Streams that do not fit the budget wait (in arrival order) with
        /// only the window the peer started with.
        admitted: bool,
    },
    /// Delivered as events.
    Channel(mpsc::UnboundedSender<Event>),
}

/// What a stream's sink held when it was given up.
#[derive(Default)]
struct Released {
    /// Bytes of a buffered body.
    buffered: usize,
    /// The budget reservation of an admitted buffered body.
    reserved: usize,
    /// Bytes staged for a channel and never delivered (still owed to the
    /// flow-control windows).
    staged: usize,
}

/// A queued piece of outbound stream data.
enum Out {
    Headers {
        fields: Vec<(String, String)>,
        end_stream: bool,
    },
    Data {
        data: Vec<u8>,
        offset: usize,
        end_stream: bool,
        ack: Option<oneshot::Sender<Result<(), HttpError>>>,
    },
    Trailers(Vec<(String, String)>),
}

impl Out {
    fn fail(self, error: impl Fn() -> HttpError) {
        if let Out::Data { ack: Some(ack), .. } = self {
            let _ = ack.send(Err(error()));
        }
    }
}

struct Stream {
    recv_open: bool,
    send_open: bool,
    /// Reset (either way); kept only until its handler task stops.
    reset: bool,
    /// The peer reset it (rather than this side).
    peer_reset: bool,
    /// The peer ended its side with END_STREAM.
    peer_ended: bool,
    /// When the stream was opened (the rapid-reset rule).
    opened: Instant,
    recv_window: i64,
    /// Consumed bytes not yet returned with WINDOW_UPDATE.
    credit: usize,
    send_window: i64,
    sink: Sink,
    content_length: Option<u64>,
    received: u64,
    /// Client: the final response head arrived.
    got_head: bool,
    /// Client: the request was HEAD (the response has no body).
    head_request: bool,
    interim: usize,
    queue: VecDeque<Out>,
    in_ready: bool,
    /// Something that ends the send side is queued (or was sent).
    end_queued: bool,
    /// Bytes received for the channel while it already held unread data:
    /// delivered as one chunk once the reader has taken the previous one,
    /// so tiny DATA frames cannot queue one event each.
    staged: Vec<u8>,
    /// Bytes delivered to the channel and not yet consumed.
    unread: usize,
    /// Output blocked on flow control since then.
    stalled_since: Option<Instant>,
    /// Time blocked on flow control before `stalled_since`, not yet
    /// forgotten (see [`STALL_PROGRESS`]).
    stall_total: Duration,
    /// Bytes sent since `stall_total` was last cleared.
    stall_progress: usize,
    task_running: bool,
    abort: Option<AbortHandle>,
    body_deadline: Option<Instant>,
}

impl Stream {
    fn new(send_window: u32, recv_window: u32, sink: Sink) -> Self {
        Stream {
            recv_open: true,
            send_open: true,
            reset: false,
            peer_reset: false,
            peer_ended: false,
            opened: Instant::now(),
            recv_window: i64::from(recv_window),
            credit: 0,
            send_window: i64::from(send_window),
            sink,
            content_length: None,
            received: 0,
            got_head: false,
            head_request: false,
            interim: 0,
            queue: VecDeque::new(),
            in_ready: false,
            end_queued: false,
            staged: Vec::new(),
            unread: 0,
            stalled_since: None,
            stall_total: Duration::ZERO,
            stall_progress: 0,
            task_running: false,
            abort: None,
            body_deadline: None,
        }
    }

    /// Sends `event` to the channel, after any staged data. Returns the
    /// staged bytes that could not be delivered (the reader is gone), which
    /// the caller credits back.
    fn deliver(&mut self, event: Event) -> usize {
        let Sink::Channel(tx) = &self.sink else {
            return 0;
        };
        if !self.staged.is_empty() {
            let data = std::mem::take(&mut self.staged);
            let len = data.len();
            if tx.send(Event::Data(data)).is_err() {
                return len;
            }
            self.unread += len;
        }
        let _ = tx.send(event);
        0
    }

    /// Replaces the sink with `Discard`, returning what it held.
    fn take_sink(&mut self) -> Released {
        let staged = std::mem::take(&mut self.staged).len();
        match std::mem::replace(&mut self.sink, Sink::Discard) {
            Sink::Buffer {
                body,
                reserve,
                admitted,
                ..
            } => Released {
                buffered: body.len(),
                reserved: if admitted { reserve } else { 0 },
                staged,
            },
            _ => Released {
                staged,
                ..Released::default()
            },
        }
    }

    /// Time the response has spent blocked on flow control, not yet
    /// forgotten.
    fn stalled_for(&self, now: Instant) -> Duration {
        self.stall_total
            + self
                .stalled_since
                .map_or(Duration::ZERO, |since| now.saturating_duration_since(since))
    }

    /// How the stream ended, for [`RecentlyClosed`].
    fn closed_as(&self) -> Closed {
        if self.peer_reset {
            Closed::PeerReset
        } else if self.reset || !self.peer_ended {
            Closed::ByUs
        } else {
            Closed::Ended
        }
    }
}

/// Rolling counters for the flood defences.
#[derive(Clone, Copy)]
struct Flood {
    window_start: Instant,
    resets: u32,
    control: u32,
    /// Server: client resets of streams whose response was not complete,
    /// whatever their age.
    cancels: u32,
    /// Server: streams that ended without a reset.
    completed: u32,
}

impl Flood {
    fn new(now: Instant) -> Self {
        Flood {
            window_start: now,
            resets: 0,
            control: 0,
            cancels: 0,
            completed: 0,
        }
    }
}

/// The connection actor.
pub(crate) struct Connection<W> {
    opts: Arc<Options>,
    writer: W,
    wbuf: Vec<u8>,
    wpos: usize,
    needs_flush: bool,
    /// When a write last made progress, or output became pending after
    /// none was (the write timeout runs from the later of the two).
    write_clock: Instant,
    /// The last write left output pending: the socket was full.
    backlogged: bool,
    /// When a write last made progress while the socket was full — the
    /// peer is reading (keep-alive).
    last_drain: Instant,
    decoder: hpack::Decoder,
    encoder: hpack::Encoder,
    peer_max_frame: usize,
    peer_initial_window: u32,
    peer_max_streams: u32,
    conn_send_window: i64,
    conn_recv_window: i64,
    conn_credit: usize,
    streams: HashMap<u32, Stream>,
    ready: VecDeque<u32>,
    /// Server: the highest stream id the client opened.
    last_peer_stream: u32,
    /// Client: the next stream id to use.
    next_stream_id: u32,
    goaway_sent: bool,
    /// Server: the first phase of a graceful GOAWAY was sent at this time;
    /// the final one follows the PING acknowledgement.
    goaway_pending: Option<Instant>,
    goaway_received: Option<u32>,
    /// The code of the peer's (last) GOAWAY.
    goaway_code: Reason,
    pending_opens: VecDeque<Open>,
    shared: Arc<Shared>,
    server: Option<ServerCtx>,
    served: u64,
    flood: Flood,
    idle_since: Option<Instant>,
    recent: RecentlyClosed,
    /// Server: request-body bytes held in `Sink::Buffer`s.
    buffered: usize,
    /// Server: the budget reservations of admitted buffered bodies.
    reserved: usize,
    /// Server: buffered bodies waiting for admission, oldest first.
    waiting: VecDeque<u32>,
    /// Server: connection credit held back because `buffered` exceeds the
    /// budget (always `buffered - budget`, or 0).
    withheld: usize,
    /// When a frame last arrived (keep-alive).
    last_read: Instant,
    /// A keep-alive PING is outstanding since then (it was queued then).
    ping_sent: Option<Instant>,
    /// Our SETTINGS are not acknowledged yet; sent then.
    settings_sent: Option<Instant>,
}

/// Starts the engine on `io`: spawns the reader and returns the actor, ready
/// to [`run`](Connection::run). `preface_sent_by_caller` is unused on the
/// server; a client always writes the preface first.
pub(crate) fn start<IO>(
    io: IO,
    opts: Arc<Options>,
    shared: Arc<Shared>,
    server: Option<ServerCtx>,
) -> (Connection<tokio::io::WriteHalf<IO>>, Reader)
where
    IO: AsyncRead + AsyncWrite + Send + 'static,
{
    let (read_half, write_half) = tokio::io::split(io);
    let (tx, rx) = mpsc::channel(INBOUND_QUEUE);
    let cfg = ReaderCfg {
        server: opts.server,
        max_frame: opts.h2.max_frame_size,
        max_block: (opts.h2.max_header_list_size as usize).saturating_mul(2),
        head_timeout: opts.head_timeout,
    };
    let task = tokio::spawn(read_loop(read_half, cfg, tx));
    let reader = Reader {
        rx,
        task: task.abort_handle(),
    };
    let now = Instant::now();
    let conn = Connection {
        writer: write_half,
        wbuf: Vec::with_capacity(16 * 1024),
        wpos: 0,
        needs_flush: false,
        write_clock: now,
        backlogged: false,
        last_drain: now,
        decoder: hpack::Decoder::new(opts.h2.header_table_size as usize),
        encoder: hpack::Encoder::new(),
        peer_max_frame: frame::DEFAULT_MAX_FRAME_SIZE as usize,
        peer_initial_window: frame::DEFAULT_WINDOW,
        peer_max_streams: u32::MAX,
        conn_send_window: i64::from(frame::DEFAULT_WINDOW),
        conn_recv_window: i64::from(frame::DEFAULT_WINDOW),
        conn_credit: 0,
        streams: HashMap::new(),
        ready: VecDeque::new(),
        last_peer_stream: 0,
        next_stream_id: 1,
        goaway_sent: false,
        goaway_pending: None,
        goaway_received: None,
        goaway_code: Reason::NO_ERROR,
        pending_opens: VecDeque::new(),
        shared,
        server,
        served: 0,
        flood: Flood::new(now),
        idle_since: Some(now),
        recent: RecentlyClosed::default(),
        buffered: 0,
        reserved: 0,
        waiting: VecDeque::new(),
        withheld: 0,
        last_read: now,
        ping_sent: None,
        settings_sent: None,
        opts,
    };
    (conn, reader)
}

/// The actor's handle on the reader task.
pub(crate) struct Reader {
    rx: mpsc::Receiver<Inbound>,
    task: AbortHandle,
}

impl Drop for Reader {
    fn drop(&mut self) {
        // The connection is gone (finished, or its task was dropped, e.g.
        // when the server gives up draining): stop reading the socket.
        self.task.abort();
    }
}

impl<W> Drop for Connection<W> {
    fn drop(&mut self) {
        // Handler tasks do not outlive their connection.
        for st in self.streams.values_mut() {
            if let Some(abort) = st.abort.take() {
                abort.abort();
            }
        }
    }
}

impl<W: AsyncWrite + Unpin> Connection<W> {
    fn report(&self, error: &HttpError) {
        if let Some(server) = &self.server {
            server.report(error);
        }
    }

    fn pending_output(&self) -> usize {
        self.wbuf.len() - self.wpos
    }

    /// Queues the preface (client), our SETTINGS and the connection window
    /// increase.
    fn handshake(&mut self) {
        let h2 = &self.opts.h2;
        if !self.opts.server {
            self.wbuf.extend_from_slice(frame::PREFACE);
        }
        let mut params = vec![
            (setting::MAX_CONCURRENT_STREAMS, h2.max_concurrent_streams),
            (setting::INITIAL_WINDOW_SIZE, h2.initial_stream_window_size),
            (setting::MAX_FRAME_SIZE, h2.max_frame_size),
            (
                setting::MAX_HEADER_LIST_SIZE,
                self.opts.max_list_size as u32,
            ),
        ];
        if h2.header_table_size != 4096 {
            params.push((setting::HEADER_TABLE_SIZE, h2.header_table_size));
        }
        if !self.opts.server {
            params.push((setting::ENABLE_PUSH, 0));
        }
        frame::encode_settings(&mut self.wbuf, &params);
        self.settings_sent = Some(Instant::now());
        let extra = h2.initial_connection_window_size - frame::DEFAULT_WINDOW;
        if extra > 0 {
            frame::encode_window_update(&mut self.wbuf, 0, extra);
            self.conn_recv_window += i64::from(extra);
        }
    }

    /// Runs the connection to completion.
    pub(crate) async fn run(
        mut self,
        mut reader: Reader,
        mut cmds: mpsc::UnboundedReceiver<Cmd>,
        mut drain: Option<watch::Receiver<bool>>,
    ) -> Result<(), HttpError> {
        self.handshake();
        let mut tick = tokio::time::interval(self.tick_period());
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut cmds_open = true;
        let mut reading = true;
        let mut had_output = false;
        let outcome: Result<(), Fatal> = loop {
            if let Some(done) = self.finished(cmds_open) {
                break done;
            }
            self.publish();
            let has_output = self.pending_output() > 0;
            if has_output && !had_output {
                // The write timeout measures a stall of output that is
                // waiting, not the quiet time before it was queued.
                self.write_clock = Instant::now();
            }
            had_output = has_output;
            let io_pending = has_output || self.needs_flush;
            let can_read = reading && self.pending_output() < READ_HIGH_WATER;
            let watch_drain = drain.is_some() && !self.goaway_sent && self.goaway_pending.is_none();
            let step: Result<(), Fatal> = tokio::select! {
                r = pump(&mut self.writer, &self.wbuf[self.wpos..]), if io_pending => {
                    self.on_written(r)
                }
                item = reader.rx.recv(), if can_read => match item {
                    Some(item) => self.on_inbound(item),
                    None => {
                        reading = false;
                        Err(Fatal::quiet(None))
                    }
                },
                cmd = cmds.recv(), if cmds_open => {
                    match cmd {
                        Some(cmd) => self.on_cmd(cmd),
                        None => cmds_open = false,
                    }
                    Ok(())
                }
                _ = wait_drain(&mut drain), if watch_drain => {
                    self.go_away();
                    Ok(())
                }
                _ = tick.tick() => self.on_tick(),
            };
            match step {
                Ok(()) => self.schedule(),
                Err(fatal) => break Err(fatal),
            }
        };
        self.close(outcome, &mut reader).await
    }

    fn tick_period(&self) -> Duration {
        let o = &self.opts;
        [
            o.idle_timeout,
            o.write_timeout,
            o.body_timeout,
            o.stall_timeout,
            o.h2.keepalive_interval,
            Some(o.h2.keepalive_timeout),
        ]
        .into_iter()
        .flatten()
        .map(|d| d / 4)
        .min()
        .unwrap_or(Duration::from_secs(1))
        .clamp(Duration::from_millis(10), Duration::from_secs(1))
    }

    /// Publishes the stream counts the client's pool balances on.
    fn publish(&self) {
        if !self.opts.server {
            let active = self.streams.len() + self.pending_opens.len();
            self.shared.active.store(active, Ordering::Relaxed);
            self.shared
                .capacity
                .store(self.peer_max_streams as usize, Ordering::Relaxed);
        }
    }

    /// Whether the connection has nothing left to do.
    fn finished(&mut self, cmds_open: bool) -> Option<Result<(), Fatal>> {
        if !self.streams.is_empty() || !self.pending_opens.is_empty() {
            return None;
        }
        if self.goaway_pending.is_some() {
            // Wait for the final GOAWAY: the client may still open streams.
            return None;
        }
        let ending =
            self.goaway_sent || self.goaway_received.is_some() || (!self.opts.server && !cmds_open);
        if ending {
            if !self.goaway_sent {
                self.final_goaway();
            }
            return Some(Ok(()));
        }
        None
    }

    fn on_written(&mut self, r: std::io::Result<usize>) -> Result<(), Fatal> {
        match r {
            Ok(0) if self.pending_output() > 0 => Err(Fatal::quiet(Some(HttpError::Io(
                std::io::ErrorKind::WriteZero.into(),
            )))),
            Ok(n) => {
                let now = Instant::now();
                self.write_clock = now;
                if self.pending_output() == 0 {
                    // That was a flush.
                    self.needs_flush = false;
                    return Ok(());
                }
                if self.backlogged && n > 0 {
                    // The socket was full and took more: the peer read.
                    self.last_drain = now;
                }
                self.wpos += n;
                self.backlogged = self.wpos < self.wbuf.len();
                if self.wpos == self.wbuf.len() {
                    self.wbuf.clear();
                    self.wpos = 0;
                    self.needs_flush = true;
                } else if self.wpos > WRITE_HIGH_WATER {
                    self.wbuf.drain(..self.wpos);
                    self.wpos = 0;
                }
                Ok(())
            }
            Err(e) => Err(Fatal::quiet(Some(HttpError::Io(e)))),
        }
    }

    /// Starts a graceful shutdown. A server sends the two-phase GOAWAY of
    /// RFC 9113 §6.8: GOAWAY with the largest stream id and a PING now, the
    /// final GOAWAY once the PING is answered (or after [`GOAWAY_GRACE`]),
    /// so streams the client opened in the meantime are still served. A
    /// client (which accepts no streams) sends the final GOAWAY at once.
    fn go_away(&mut self) {
        if self.goaway_sent || self.goaway_pending.is_some() {
            return;
        }
        self.shared.usable.store(false, Ordering::Release);
        if self.opts.server {
            frame::encode_goaway(&mut self.wbuf, MAX_STREAM_ID, Reason::NO_ERROR, b"");
            frame::encode_ping(&mut self.wbuf, false, PING_GOAWAY);
            self.goaway_pending = Some(Instant::now());
        } else {
            self.final_goaway();
        }
    }

    /// Sends GOAWAY(NO_ERROR) with the real last stream id and stops
    /// accepting new streams.
    fn final_goaway(&mut self) {
        if self.goaway_sent {
            return;
        }
        self.goaway_pending = None;
        self.goaway_sent = true;
        self.shared.usable.store(false, Ordering::Release);
        frame::encode_goaway(&mut self.wbuf, self.last_peer_stream, Reason::NO_ERROR, b"");
        // Client: requests waiting for a stream slot go elsewhere.
        while let Some(open) = self.pending_opens.pop_front() {
            let _ = open.reply.send(Err(OpenError::Refused));
        }
    }

    // ----- flood accounting -------------------------------------------------

    fn roll_flood_window(&mut self) {
        let now = Instant::now();
        if now.saturating_duration_since(self.flood.window_start) >= self.opts.h2.flood_window {
            self.flood = Flood::new(now);
        }
    }

    fn count_control(&mut self) -> Result<(), Fatal> {
        self.roll_flood_window();
        self.flood.control += 1;
        if self.flood.control > self.opts.h2.max_control_frames_per_window {
            return Err(Fatal::protocol(
                Reason::ENHANCE_YOUR_CALM,
                "too many control frames",
            ));
        }
        Ok(())
    }

    /// Counts a stream reset toward the rapid-reset / MadeYouReset budget.
    fn count_reset(&mut self) -> Result<(), Fatal> {
        self.roll_flood_window();
        self.flood.resets += 1;
        if self.flood.resets > self.opts.h2.max_resets_per_window {
            return Err(Fatal::protocol(
                Reason::ENHANCE_YOUR_CALM,
                "too many stream resets",
            ));
        }
        Ok(())
    }

    /// Server: counts the client's reset of a stream whose response was not
    /// complete, whatever its age, against
    /// [`max_cancels_per_window`](Http2Config::max_cancels_per_window) (or
    /// twice the streams completed in the window, when that is more). The
    /// age rule alone is sidestepped by holding each stream just past
    /// `rapid_reset_threshold`, which turns the stream limit into a rate of
    /// `max_concurrent_streams / threshold` handler starts per second.
    fn count_cancel(&mut self) -> Result<(), Fatal> {
        self.roll_flood_window();
        self.flood.cancels += 1;
        let allowed = self
            .opts
            .h2
            .max_cancels_per_window
            .max(self.flood.completed.saturating_mul(2));
        if self.flood.cancels > allowed {
            return Err(Fatal::protocol(
                Reason::ENHANCE_YOUR_CALM,
                "too many cancelled streams",
            ));
        }
        Ok(())
    }

    /// Whether the peer's RST_STREAM of a live stream is the rapid-reset
    /// pattern: on a server, a stream reset within `rapid_reset_threshold`
    /// of its opening, before its response was complete. A client never
    /// counts the server's resets of its own streams (it decides how many it
    /// opens), and nobody counts resets of completed streams or of streams
    /// that lived long enough to be ordinary cancellations.
    fn is_rapid_reset(&self, st: &Stream) -> bool {
        let threshold = self.opts.h2.rapid_reset_threshold;
        self.opts.server
            && !threshold.is_zero()
            && !st.end_queued
            && Instant::now().saturating_duration_since(st.opened) < threshold
    }

    // ----- stream id bookkeeping --------------------------------------------

    /// Whether `id` names a stream that was never opened.
    fn is_idle(&self, id: u32) -> bool {
        let peer_initiated = (id % 2 == 1) == self.opts.server;
        if peer_initiated {
            id > self.last_peer_stream
        } else {
            id >= self.next_stream_id
        }
    }

    // ----- flow control -----------------------------------------------------

    /// Returns `n` consumed bytes to the connection window and, when given and
    /// still receiving, to the stream's window.
    fn release(&mut self, id: Option<u32>, n: usize) {
        self.credit_conn(n);
        if let Some(id) = id {
            self.credit_stream(id, n);
        }
    }

    /// Returns `n` bytes to the connection window.
    fn credit_conn(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        self.conn_credit += n;
        // Half the window, or half of what is left of it once withheld
        // credit is taken off, so the peer is not left waiting on credit
        // this side holds only to batch updates.
        let window = self.opts.h2.initial_connection_window_size as usize;
        let conn_threshold = (window / 2)
            .min(window.saturating_sub(self.withheld) / 2)
            .max(1);
        if self.conn_credit >= conn_threshold {
            let inc = self.conn_credit.min(frame::MAX_WINDOW as usize) as u32;
            frame::encode_window_update(&mut self.wbuf, 0, inc);
            self.conn_recv_window += i64::from(inc);
            self.conn_credit -= inc as usize;
        }
    }

    /// Returns `n` bytes to a stream's window while it is still receiving.
    fn credit_stream(&mut self, id: u32, n: usize) {
        if n == 0 {
            return;
        }
        let threshold = (self.opts.h2.initial_stream_window_size / 2) as usize;
        if let Some(st) = self.streams.get_mut(&id) {
            if !st.recv_open || st.reset {
                return;
            }
            st.credit += n;
            if matches!(
                st.sink,
                Sink::Buffer {
                    admitted: false,
                    ..
                }
            ) {
                // Owed, and sent on admission.
                return;
            }
            if st.credit >= threshold {
                let inc = st.credit.min(frame::MAX_WINDOW as usize) as u32;
                frame::encode_window_update(&mut self.wbuf, id, inc);
                st.recv_window += i64::from(inc);
                st.credit -= inc as usize;
            }
        }
    }

    /// `n` more bytes are held in a buffered body. Returns how many of them
    /// may be credited to the connection now; the rest is withheld while the
    /// connection is over its budget.
    fn buffer_bytes(&mut self, n: usize) -> usize {
        self.buffered += n;
        let withheld = self.buffered.saturating_sub(self.opts.body_budget);
        let extra = withheld - self.withheld;
        self.withheld = withheld;
        n - extra
    }

    /// A buffered body (`r`) was dispatched or dropped: give back its
    /// reservation, admit the bodies waiting for one, and return the
    /// connection credit that no longer needs to be withheld.
    fn unbuffer(&mut self, r: &Released) {
        self.reserved = self.reserved.saturating_sub(r.reserved);
        if r.buffered > 0 {
            self.buffered = self.buffered.saturating_sub(r.buffered);
            let withheld = self.buffered.saturating_sub(self.opts.body_budget);
            if self.withheld > withheld {
                let back = self.withheld - withheld;
                self.withheld = withheld;
                self.credit_conn(back);
            }
        }
        self.admit_waiting();
    }

    /// Whether a buffered body reserving `reserve` fits the budget now.
    fn fits(&self, reserve: usize) -> bool {
        reserve == 0 || self.reserved == 0 || self.reserved + reserve <= self.opts.body_budget
    }

    /// Admits waiting buffered bodies, oldest first, while they fit the
    /// budget, and sends each the stream credit it is owed. The oldest
    /// waiting body always fits once nothing else is admitted (the budget is
    /// at least `max_body_bytes`), so uploads always make progress.
    fn admit_waiting(&mut self) {
        while let Some(&id) = self.waiting.front() {
            let reserve = match self.streams.get(&id).map(|st| &st.sink) {
                Some(Sink::Buffer {
                    reserve,
                    admitted: false,
                    ..
                }) => *reserve,
                _ => {
                    self.waiting.pop_front();
                    continue;
                }
            };
            if !self.fits(reserve) {
                break;
            }
            self.waiting.pop_front();
            self.reserved += reserve;
            let Some(st) = self.streams.get_mut(&id) else {
                continue;
            };
            if let Sink::Buffer { admitted, .. } = &mut st.sink {
                *admitted = true;
            }
            if st.recv_open && !st.reset && st.credit > 0 {
                let inc = st.credit.min(frame::MAX_WINDOW as usize) as u32;
                frame::encode_window_update(&mut self.wbuf, id, inc);
                st.recv_window += i64::from(inc);
                st.credit -= inc as usize;
            }
        }
    }

    /// The peer cannot send any DATA because this side withholds credit for
    /// buffered bodies: make sure the uploads can still finish. First return
    /// any credit only held back to batch updates; if there is none, refuse
    /// (`REFUSED_STREAM`, so the client may retry it) the youngest waiting
    /// body that holds data, which frees its bytes. Only a peer that fills
    /// the connection window with bodies beyond the budget gets here: the
    /// admitted bodies fit the budget, and a waiting one can send at most
    /// the stream window it started with.
    fn unstick(&mut self) {
        while self.withheld > 0 && self.conn_recv_window <= 0 {
            if self.conn_credit > 0 {
                let inc = self.conn_credit.min(frame::MAX_WINDOW as usize) as u32;
                frame::encode_window_update(&mut self.wbuf, 0, inc);
                self.conn_recv_window += i64::from(inc);
                self.conn_credit -= inc as usize;
                continue;
            }
            let shed = self.waiting.iter().rev().copied().find(|id| {
                self.streams.get(id).is_some_and(
                    |st| matches!(&st.sink, Sink::Buffer { body, .. } if !body.is_empty()),
                )
            });
            let Some(id) = shed else {
                return;
            };
            let error = h2_error(
                Http2Cause::LocalReset(Reason::REFUSED_STREAM),
                "refused: the connection's buffered request bodies are over budget",
            );
            self.reset_stream(id, Reason::REFUSED_STREAM, Some(error));
        }
    }

    /// Gives up stream `id`'s sink: the buffered body leaves the budget, and
    /// staged channel bytes nobody will read are credited back.
    fn drop_sink(&mut self, id: u32) {
        let Some(st) = self.streams.get_mut(&id) else {
            return;
        };
        let r = st.take_sink();
        self.waiting.retain(|&w| w != id);
        self.release(Some(id), r.staged);
        self.unbuffer(&r);
    }

    /// Delivers `event` on stream `id` (after any staged data), crediting
    /// what could not be delivered.
    fn deliver(&mut self, id: u32, event: Event) {
        let Some(st) = self.streams.get_mut(&id) else {
            return;
        };
        let lost = st.deliver(event);
        self.release(Some(id), lost);
    }

    /// The reader took `n` bytes of stream `id`: credit them, and hand over
    /// the data staged meanwhile as one chunk.
    fn consumed(&mut self, id: u32, n: usize) {
        self.release(Some(id), n);
        let Some(st) = self.streams.get_mut(&id) else {
            return;
        };
        st.unread = st.unread.saturating_sub(n);
        if st.unread > 0 || st.staged.is_empty() {
            return;
        }
        let data = std::mem::take(&mut st.staged);
        let len = data.len();
        let delivered = match &st.sink {
            Sink::Channel(tx) => tx.send(Event::Data(data)).is_ok(),
            _ => false,
        };
        if delivered {
            st.unread = len;
        } else {
            st.sink = Sink::Discard;
            self.release(Some(id), len);
        }
    }

    // ----- inbound ----------------------------------------------------------

    fn on_inbound(&mut self, item: Inbound) -> Result<(), Fatal> {
        // Anything from the peer shows it is alive.
        self.last_read = Instant::now();
        self.ping_sent = None;
        match item {
            Inbound::Frame(f) => self.on_frame(f),
            Inbound::Headers {
                stream,
                block,
                end_stream,
                priority,
            } => self.on_headers(stream, &block, end_stream, priority),
            Inbound::Error(FrameError::Connection(reason, why)) => {
                Err(Fatal::protocol(reason, why))
            }
            Inbound::Error(FrameError::Stream(id, reason, _)) => {
                self.count_reset()?;
                self.reset_stream(id, reason, None);
                Ok(())
            }
            Inbound::Timeout => Err(Fatal::quiet(Some(HttpError::Timeout))),
            Inbound::Closed => {
                let error = if self.streams.is_empty() {
                    None
                } else {
                    Some(self.stream_failure(None))
                };
                Err(Fatal::quiet(error))
            }
            Inbound::Io(e) => Err(Fatal::quiet(Some(HttpError::Io(e)))),
        }
    }

    /// What the open streams are told when the connection ends with
    /// `error` (or without one).
    fn stream_failure(&self, error: Option<&HttpError>) -> HttpError {
        if let Some(HttpError::Http2(e)) = error {
            return HttpError::Http2(e.clone());
        }
        if let Some(last) = self.goaway_received {
            return h2_error(
                Http2Cause::GoAway {
                    code: self.goaway_code,
                    last_stream_id: last,
                },
                "connection closed after the peer's GOAWAY",
            );
        }
        match error {
            Some(HttpError::Io(e)) => {
                h2_error(Http2Cause::Io(e.kind()), format!("connection failed: {e}"))
            }
            Some(HttpError::Timeout) => h2_error(Http2Cause::Timeout, "connection timed out"),
            Some(other) => h2_error(Http2Cause::ConnectionClosed, other.to_string()),
            None => h2_error(Http2Cause::ConnectionClosed, "connection closed"),
        }
    }

    fn on_frame(&mut self, f: Frame) -> Result<(), Fatal> {
        match f {
            Frame::Data {
                stream,
                data,
                end_stream,
                flow_len,
            } => self.on_data(stream, data, end_stream, flow_len),
            Frame::Headers { .. } | Frame::Continuation { .. } => {
                // The reader assembles these into `Inbound::Headers`.
                Err(Fatal::protocol(Reason::PROTOCOL_ERROR, "unexpected frame"))
            }
            Frame::Priority { stream, priority } => {
                self.count_control()?;
                if priority.dependency == stream {
                    self.count_reset()?;
                    self.reset_stream(stream, Reason::PROTOCOL_ERROR, None);
                }
                Ok(())
            }
            Frame::RstStream { stream, code } => {
                if self.is_idle(stream) {
                    return Err(Fatal::protocol(
                        Reason::PROTOCOL_ERROR,
                        "RST_STREAM on an idle stream",
                    ));
                }
                let (rapid, cancel) = match self.streams.get(&stream) {
                    Some(st) if !st.reset => {
                        (self.is_rapid_reset(st), self.opts.server && !st.end_queued)
                    }
                    // Both sides reset it at once: legal, and no work.
                    Some(_) => return Ok(()),
                    // Closed a moment ago (the peer had not seen the end of
                    // the response yet): legal, and no work.
                    None if self.recent.get(stream).is_some() => return Ok(()),
                    // Closed long ago: noise.
                    None => return self.count_control(),
                };
                if rapid {
                    self.count_reset()?;
                }
                if cancel {
                    self.count_cancel()?;
                }
                self.peer_reset(stream, code);
                Ok(())
            }
            Frame::Settings { ack, params } => {
                self.count_control()?;
                if ack {
                    self.settings_sent = None;
                    return Ok(());
                }
                for (id, value) in params {
                    self.apply_setting(id, value)?;
                }
                frame::encode_settings_ack(&mut self.wbuf);
                // Client: a raised stream limit frees slots.
                self.open_pending();
                Ok(())
            }
            Frame::PushPromise { .. } => Err(Fatal::protocol(
                Reason::PROTOCOL_ERROR,
                "PUSH_PROMISE is not allowed",
            )),
            Frame::Ping { ack, data } => {
                self.count_control()?;
                if !ack {
                    frame::encode_ping(&mut self.wbuf, true, data);
                } else if data == PING_GOAWAY && self.goaway_pending.is_some() {
                    // A round trip since the first GOAWAY: whatever the
                    // client sent before seeing it has arrived.
                    self.final_goaway();
                }
                Ok(())
            }
            Frame::GoAway {
                last_stream, code, ..
            } => {
                self.shared.usable.store(false, Ordering::Release);
                let last = self
                    .goaway_received
                    .map_or(last_stream, |l| l.min(last_stream));
                self.goaway_received = Some(last);
                self.goaway_code = code;
                if !self.opts.server {
                    // Our streams above `last` were not processed: refused.
                    let refused: Vec<u32> = self
                        .streams
                        .keys()
                        .copied()
                        .filter(|&id| id > last)
                        .collect();
                    for id in refused {
                        self.deliver(id, Event::Refused);
                        self.drop_sink(id);
                        self.drop_stream(id);
                    }
                    while let Some(open) = self.pending_opens.pop_front() {
                        let _ = open.reply.send(Err(OpenError::Refused));
                    }
                }
                Ok(())
            }
            Frame::WindowUpdate { stream, increment } => {
                if stream == 0 {
                    self.conn_send_window += i64::from(increment);
                    if self.conn_send_window > i64::from(frame::MAX_WINDOW) {
                        return Err(Fatal::protocol(
                            Reason::FLOW_CONTROL_ERROR,
                            "connection window overflow",
                        ));
                    }
                    return Ok(());
                }
                if self.is_idle(stream) {
                    return Err(Fatal::protocol(
                        Reason::PROTOCOL_ERROR,
                        "WINDOW_UPDATE on an idle stream",
                    ));
                }
                let Some(st) = self.streams.get_mut(&stream) else {
                    // Late updates for a stream closed a moment ago are
                    // legal, routine (a client returns window for body
                    // bytes it reads after the response has ended) and do
                    // nothing, like updates on stream 0. Only updates for
                    // streams closed so long ago that they are forgotten
                    // count as control frames.
                    if self.recent.get(stream).is_some() {
                        return Ok(());
                    }
                    return self.count_control();
                };
                if st.reset {
                    return Ok(());
                }
                st.send_window += i64::from(increment);
                if st.send_window > i64::from(frame::MAX_WINDOW) {
                    self.count_reset()?;
                    self.reset_stream(stream, Reason::FLOW_CONTROL_ERROR, None);
                    return Ok(());
                }
                if !st.queue.is_empty() && !st.in_ready {
                    st.in_ready = true;
                    self.ready.push_back(stream);
                }
                Ok(())
            }
            Frame::Unknown { .. } => self.count_control(),
        }
    }

    fn apply_setting(&mut self, id: u16, value: u32) -> Result<(), Fatal> {
        match id {
            setting::HEADER_TABLE_SIZE => self.encoder.table_size_changed(),
            setting::ENABLE_PUSH => {
                if !self.opts.server && value != 0 {
                    return Err(Fatal::protocol(
                        Reason::PROTOCOL_ERROR,
                        "a server enabled push",
                    ));
                }
            }
            setting::MAX_CONCURRENT_STREAMS => self.peer_max_streams = value,
            setting::INITIAL_WINDOW_SIZE => {
                let delta = i64::from(value) - i64::from(self.peer_initial_window);
                self.peer_initial_window = value;
                for (&id, st) in self.streams.iter_mut() {
                    st.send_window += delta;
                    if st.send_window > i64::from(frame::MAX_WINDOW) {
                        return Err(Fatal::protocol(
                            Reason::FLOW_CONTROL_ERROR,
                            "stream window overflow",
                        ));
                    }
                    if delta > 0 && !st.queue.is_empty() && !st.in_ready {
                        st.in_ready = true;
                        self.ready.push_back(id);
                    }
                }
            }
            setting::MAX_FRAME_SIZE => self.peer_max_frame = value as usize,
            _ => {}
        }
        Ok(())
    }

    fn on_data(
        &mut self,
        id: u32,
        data: Vec<u8>,
        end_stream: bool,
        flow_len: u32,
    ) -> Result<(), Fatal> {
        self.conn_recv_window -= i64::from(flow_len);
        if self.conn_recv_window < 0 {
            return Err(Fatal::protocol(
                Reason::FLOW_CONTROL_ERROR,
                "connection window exceeded",
            ));
        }
        let flow = flow_len as usize;
        if self.is_idle(id) {
            return Err(Fatal::protocol(
                Reason::PROTOCOL_ERROR,
                "DATA on an idle stream",
            ));
        }
        let state = self
            .streams
            .get(&id)
            .map(|s| (s.reset, s.peer_reset, s.recv_open));
        match state {
            None => {
                // Closed: nobody reads it, but the window is owed.
                self.release(None, flow);
                return self.on_closed_frame(id, flow == 0, "DATA");
            }
            Some((true, peer_reset, _)) => {
                self.release(None, flow);
                if peer_reset {
                    // RFC 9113 §5.1: frames after the peer's RST_STREAM.
                    self.count_reset()?;
                    frame::encode_rst(&mut self.wbuf, id, Reason::STREAM_CLOSED);
                    return Ok(());
                }
                // Data still in flight when this side reset the stream is
                // normal and bounded by flow control; only empty frames are
                // noise.
                if flow == 0 {
                    return self.count_control();
                }
                return Ok(());
            }
            Some((false, _, false)) => {
                self.release(None, flow);
                self.count_reset()?;
                self.reset_stream(id, Reason::STREAM_CLOSED, None);
                return Ok(());
            }
            Some((false, _, true)) => {}
        }
        if data.is_empty() && !end_stream {
            self.count_control()?;
        }
        if !self.opts.server && self.streams.get(&id).is_some_and(|s| !s.got_head) {
            self.release(None, flow);
            self.count_reset()?;
            self.reset_stream(
                id,
                Reason::PROTOCOL_ERROR,
                malformed("response body before its head"),
            );
            return Ok(());
        }
        let Some(st) = self.streams.get_mut(&id) else {
            return Ok(());
        };
        st.recv_window -= i64::from(flow_len);
        if st.recv_window < 0 {
            self.release(None, flow);
            self.count_reset()?;
            self.reset_stream(id, Reason::FLOW_CONTROL_ERROR, None);
            return Ok(());
        }
        st.received += data.len() as u64;
        if st.content_length.is_some_and(|cl| st.received > cl) {
            self.release(None, flow);
            self.count_reset()?;
            self.reset_stream(
                id,
                Reason::PROTOCOL_ERROR,
                malformed("body longer than its content-length"),
            );
            return Ok(());
        }
        let padding = flow - data.len();
        let len = data.len();
        // Where the bytes go decides when they are credited back: discarded
        // and buffered bytes now (buffered ones only within the connection's
        // budget, for the connection window), channel bytes once read.
        let mut consumed_now = padding;
        let mut buffered_now = 0;
        let mut too_large = false;
        match &mut st.sink {
            Sink::Discard => consumed_now += len,
            Sink::Buffer { body, max, .. } => {
                if body.len() + len > *max {
                    too_large = true;
                    consumed_now += len;
                } else {
                    body.extend_from_slice(&data);
                    buffered_now = len;
                }
            }
            Sink::Channel(tx) => {
                if len == 0 {
                } else if st.unread > 0 {
                    // The reader has not taken the last chunk yet: add to
                    // the next one instead of queueing an event per frame.
                    st.staged.extend_from_slice(&data);
                } else if tx.send(Event::Data(data)).is_ok() {
                    st.unread = len;
                } else {
                    st.sink = Sink::Discard;
                    consumed_now += len;
                }
            }
        }
        if buffered_now > 0 {
            let conn_now = self.buffer_bytes(buffered_now);
            self.credit_conn(consumed_now + conn_now);
            self.credit_stream(id, consumed_now + buffered_now);
        } else {
            self.release(Some(id), consumed_now);
        }
        if too_large {
            let max = match self.server.as_ref() {
                Some(s) => s.config.max_body_bytes,
                None => 0,
            };
            self.report(&HttpError::BodyTooLarge(max as u64));
            self.respond_simple(id, StatusCode::PAYLOAD_TOO_LARGE, "payload too large");
            return Ok(());
        }
        if end_stream {
            self.on_recv_end(id)?;
        }
        if buffered_now > 0 {
            self.unstick();
        }
        Ok(())
    }

    /// A DATA or HEADERS frame (`what`) for a stream that is no longer open
    /// (RFC 9113 §5.1): ignored when this side reset the stream (frames in
    /// flight are expected), a stream error after the peer's own
    /// RST_STREAM, a connection error after both sides ended it. An id this
    /// side never saw (a lower one the peer skipped) is a connection error
    /// for HEADERS on a server (§5.1.1).
    fn on_closed_frame(&mut self, id: u32, noise: bool, what: &str) -> Result<(), Fatal> {
        match self.recent.get(id) {
            Some(Closed::Ended) => Err(Fatal::protocol(
                Reason::STREAM_CLOSED,
                "frame on a stream the peer had ended",
            )),
            Some(Closed::PeerReset) => {
                self.count_reset()?;
                frame::encode_rst(&mut self.wbuf, id, Reason::STREAM_CLOSED);
                Ok(())
            }
            Some(Closed::ByUs) => {
                if noise {
                    self.count_control()
                } else {
                    Ok(())
                }
            }
            None => {
                let never_used = self.opts.server
                    && what == "HEADERS"
                    && id > self.recent.forgotten_max
                    && id % 2 == 1;
                if never_used {
                    return Err(Fatal::protocol(
                        Reason::PROTOCOL_ERROR,
                        "HEADERS on a stream id lower than one already used",
                    ));
                }
                if noise {
                    self.count_control()
                } else {
                    Ok(())
                }
            }
        }
    }

    fn on_headers(
        &mut self,
        id: u32,
        block: &[u8],
        end_stream: bool,
        priority: Option<Priority>,
    ) -> Result<(), Fatal> {
        // Always decode first: the HPACK state is shared by every stream.
        let limits = DecodeLimits {
            max_list_size: self.opts.max_list_size,
            max_fields: self.opts.max_fields,
        };
        let decoded = self
            .decoder
            .decode(block, &limits)
            .map_err(|e| Fatal::protocol(Reason::COMPRESSION_ERROR, e.as_str()))?;
        if priority.is_some_and(|p| p.dependency == id) {
            self.count_reset()?;
            self.reset_stream(id, Reason::PROTOCOL_ERROR, None);
            return Ok(());
        }
        if let Some(st) = self.streams.get(&id) {
            if st.reset {
                if st.peer_reset {
                    self.count_reset()?;
                    frame::encode_rst(&mut self.wbuf, id, Reason::STREAM_CLOSED);
                    return Ok(());
                }
                return self.count_control();
            }
            if !st.recv_open {
                self.count_reset()?;
                self.reset_stream(id, Reason::STREAM_CLOSED, None);
                return Ok(());
            }
            if !self.opts.server && !st.got_head {
                return self.on_response_head(id, decoded, end_stream);
            }
            return self.on_trailers(id, decoded, end_stream);
        }
        if !self.opts.server {
            if id % 2 == 1 && id < self.next_stream_id {
                // A response to a stream we already reset or finished: the
                // server sent it before it saw our RST_STREAM.
                return self.on_closed_frame(id, true, "HEADERS");
            }
            return Err(Fatal::protocol(
                Reason::PROTOCOL_ERROR,
                "HEADERS on a stream the client did not open",
            ));
        }
        if id % 2 == 1 && id <= self.last_peer_stream {
            return self.on_closed_frame(id, true, "HEADERS");
        }
        self.on_request(id, decoded, end_stream)
    }

    fn on_trailers(
        &mut self,
        id: u32,
        decoded: hpack::Decoded,
        end_stream: bool,
    ) -> Result<(), Fatal> {
        if !end_stream {
            self.count_reset()?;
            self.reset_stream(
                id,
                Reason::PROTOCOL_ERROR,
                malformed("trailers without END_STREAM"),
            );
            return Ok(());
        }
        if decoded.too_large {
            self.count_reset()?;
            self.reset_stream(
                id,
                Reason::PROTOCOL_ERROR,
                malformed("trailer section too large"),
            );
            return Ok(());
        }
        let trailers = match fields_to_headers(&decoded.fields, false) {
            Ok(t) => t,
            Err(why) => {
                self.count_reset()?;
                self.reset_stream(id, Reason::PROTOCOL_ERROR, malformed(why));
                return Ok(());
            }
        };
        self.deliver(id, Event::Trailers(trailers));
        self.on_recv_end(id)
    }

    fn on_response_head(
        &mut self,
        id: u32,
        decoded: hpack::Decoded,
        end_stream: bool,
    ) -> Result<(), Fatal> {
        if decoded.too_large {
            self.count_reset()?;
            self.reset_stream(
                id,
                Reason::PROTOCOL_ERROR,
                malformed("response head too large"),
            );
            return Ok(());
        }
        let (status, headers) = match parse_response_head(&decoded.fields) {
            Ok(head) => head,
            Err(why) => {
                self.count_reset()?;
                self.reset_stream(id, Reason::PROTOCOL_ERROR, malformed(why));
                return Ok(());
            }
        };
        let Some(st) = self.streams.get_mut(&id) else {
            return Ok(());
        };
        if (100..200).contains(&status) {
            st.interim += 1;
            if end_stream || status == 101 || st.interim > MAX_INTERIM {
                self.count_reset()?;
                self.reset_stream(
                    id,
                    Reason::PROTOCOL_ERROR,
                    malformed("invalid interim response"),
                );
            }
            return Ok(());
        }
        let cl = match headers.content_length_checked() {
            Ok(cl) => cl,
            Err(_) => {
                self.count_reset()?;
                self.reset_stream(
                    id,
                    Reason::PROTOCOL_ERROR,
                    malformed("invalid content-length"),
                );
                return Ok(());
            }
        };
        let bodyless = st.head_request || status == 204 || status == 304;
        st.content_length = if bodyless { Some(0) } else { cl };
        st.got_head = true;
        self.deliver(id, Event::Head { status, headers });
        if end_stream {
            self.on_recv_end(id)?;
        }
        Ok(())
    }

    fn on_request(
        &mut self,
        id: u32,
        decoded: hpack::Decoded,
        end_stream: bool,
    ) -> Result<(), Fatal> {
        if id % 2 == 0 {
            return Err(Fatal::protocol(
                Reason::PROTOCOL_ERROR,
                "client opened an even-numbered stream",
            ));
        }
        if id <= self.last_peer_stream {
            return self.on_closed_frame(id, true, "HEADERS");
        }
        self.last_peer_stream = id;
        if self.goaway_sent {
            frame::encode_rst(&mut self.wbuf, id, Reason::REFUSED_STREAM);
            self.recent.record(id, Closed::ByUs);
            return self.count_control();
        }
        if self.streams.len() >= self.opts.h2.max_concurrent_streams as usize {
            frame::encode_rst(&mut self.wbuf, id, Reason::REFUSED_STREAM);
            self.recent.record(id, Closed::ByUs);
            if self.settings_sent.is_some() {
                // The client may not have seen the limit yet: until it
                // acknowledges our SETTINGS, opening more is legal.
                return Ok(());
            }
            // Not a reset the client provoked by breaking a rule (a reset
            // stream keeps its slot until its handler stops, so even a
            // careful client can be refused): a control frame.
            return self.count_control();
        }
        let window = self.opts.h2.initial_stream_window_size;
        let mut st = Stream::new(self.peer_initial_window, window, Sink::Discard);
        if end_stream {
            st.recv_open = false;
            st.peer_ended = true;
        }
        self.streams.insert(id, st);
        self.idle_since = None;
        if decoded.too_large {
            self.report(&HttpError::Protocol("request header list too large".into()));
            self.respond_simple(
                id,
                StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
                "request header fields too large",
            );
            return Ok(());
        }
        let Some(ctx) = self.server.as_ref() else {
            return Ok(());
        };
        let built = super::server::build_request(&decoded.fields, ctx);
        let (mut req, head_request) = match built {
            Ok(r) => r,
            Err(super::server::Reject::Malformed(why)) => {
                self.report(&HttpError::Protocol(format!(
                    "malformed HTTP/2 request: {why}"
                )));
                self.count_reset()?;
                self.reset_stream(id, Reason::PROTOCOL_ERROR, None);
                return Ok(());
            }
            Err(super::server::Reject::Respond(status, message)) => {
                self.report(&match status.as_u16() {
                    413 => HttpError::BodyTooLarge(ctx.config.max_body_bytes as u64),
                    _ => HttpError::Protocol(message.to_string()),
                });
                self.respond_simple(id, status, message);
                return Ok(());
            }
        };
        let content_length = req.headers.content_length();
        if end_stream && content_length.is_some_and(|cl| cl != 0) {
            self.count_reset()?;
            self.reset_stream(id, Reason::PROTOCOL_ERROR, None);
            return Ok(());
        }
        self.served += 1;
        let streaming = ctx.wants_body_stream(&req);
        let cmd = ctx.cmd_tx.clone();
        let body_deadline = self.opts.body_timeout.map(|d| Instant::now() + d);
        let max = ctx.config.max_body_bytes;
        if !streaming && content_length.is_some_and(|cl| cl > max as u64) {
            // `max_body_bytes` caps buffered bodies only; a streaming
            // handler reads at its own pace.
            self.report(&HttpError::BodyTooLarge(max as u64));
            self.respond_simple(id, StatusCode::PAYLOAD_TOO_LARGE, "payload too large");
        } else if streaming {
            let (tx, rx) = mpsc::unbounded_channel();
            let recv = super::RecvStream::new(id, rx, cmd, super::OnDrop::Discard);
            req.set_body_stream(crate::http::BodyStream::new(crate::http::body::Inner::H2(
                recv,
            )));
            if end_stream {
                let _ = tx.send(Event::End);
            }
            let abort = ctx.spawn(id, req, head_request);
            if let Some(st) = self.streams.get_mut(&id) {
                st.content_length = content_length;
                st.sink = Sink::Channel(tx);
                st.task_running = true;
                st.abort = Some(abort);
            }
        } else {
            // Bodies are admitted to the connection's budget by their
            // declared size, in arrival order; one that does not fit yet
            // waits with only its initial stream window.
            let reserve = match content_length {
                _ if end_stream => 0,
                Some(cl) => cl as usize,
                None => max,
            };
            let admitted = self.waiting.is_empty() && self.fits(reserve);
            if admitted {
                self.reserved += reserve;
            } else {
                self.waiting.push_back(id);
            }
            if let Some(st) = self.streams.get_mut(&id) {
                st.content_length = content_length;
                st.sink = Sink::Buffer {
                    req: Some(Box::new(req)),
                    body: Vec::new(),
                    max,
                    head_request,
                    reserve,
                    admitted,
                };
                st.body_deadline = body_deadline;
            }
            if end_stream {
                self.dispatch(id);
            }
        }
        if self.opts.max_requests.is_some_and(|max| self.served >= max) {
            self.go_away();
        }
        Ok(())
    }

    /// Server, buffered mode: the body is complete, run the handler.
    fn dispatch(&mut self, id: u32) {
        let Some(ctx) = self.server.as_ref() else {
            return;
        };
        let Some(st) = self.streams.get_mut(&id) else {
            return;
        };
        let Sink::Buffer {
            req,
            body,
            head_request,
            ..
        } = &mut st.sink
        else {
            return;
        };
        let Some(mut req) = req.take() else {
            return;
        };
        let head_request = *head_request;
        req.body = std::mem::take(body);
        let mut released = st.take_sink();
        released.buffered = req.body.len();
        st.body_deadline = None;
        st.task_running = true;
        st.abort = Some(ctx.spawn(id, *req, head_request));
        // The body now belongs to the handler, whose number is bounded by
        // the stream limit.
        self.waiting.retain(|&w| w != id);
        self.unbuffer(&released);
    }

    /// The peer ended its side of the stream.
    fn on_recv_end(&mut self, id: u32) -> Result<(), Fatal> {
        let Some(st) = self.streams.get_mut(&id) else {
            return Ok(());
        };
        st.recv_open = false;
        st.peer_ended = true;
        if st.content_length.is_some_and(|cl| cl != st.received) {
            // A reset this side is made to send: it counts like the others
            // ("MadeYouReset", CVE-2025-8671).
            self.count_reset()?;
            self.reset_stream(
                id,
                Reason::PROTOCOL_ERROR,
                malformed("body shorter than its content-length"),
            );
            return Ok(());
        }
        match &st.sink {
            Sink::Channel(_) => self.deliver(id, Event::End),
            Sink::Buffer { .. } => self.dispatch(id),
            Sink::Discard => {}
        }
        self.maybe_remove(id);
        Ok(())
    }

    // ----- commands ---------------------------------------------------------

    fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Open(open) => self.open_stream(open),
            Cmd::Headers {
                id,
                fields,
                end_stream,
            } => self.enqueue(id, Out::Headers { fields, end_stream }, end_stream),
            Cmd::Data {
                id,
                data,
                end_stream,
                ack,
            } => self.enqueue(
                id,
                Out::Data {
                    data,
                    offset: 0,
                    end_stream,
                    ack,
                },
                end_stream,
            ),
            Cmd::Trailers { id, fields } => self.enqueue(id, Out::Trailers(fields), true),
            Cmd::Reset { id, reason } => {
                if self.streams.get(&id).is_some_and(|s| !s.reset) {
                    let error =
                        h2_error(Http2Cause::LocalReset(reason), "stream reset by this side");
                    self.reset_stream(id, reason, Some(error));
                }
            }
            Cmd::Consumed { id, n } => self.consumed(id, n),
            Cmd::Discard { id } => self.drop_sink(id),
            Cmd::TaskDone { id } => {
                let Some(st) = self.streams.get_mut(&id) else {
                    return;
                };
                st.task_running = false;
                st.abort = None;
                if st.send_open && !st.end_queued && !st.reset {
                    // The handler stopped without completing its response.
                    let error = h2_error(
                        Http2Cause::LocalReset(Reason::INTERNAL_ERROR),
                        "the handler stopped before completing its response",
                    );
                    self.reset_stream(id, Reason::INTERNAL_ERROR, Some(error));
                }
                self.maybe_remove(id);
            }
        }
    }

    fn open_stream(&mut self, open: Open) {
        if self.goaway_sent || self.goaway_received.is_some() || self.next_stream_id > MAX_STREAM_ID
        {
            self.shared.usable.store(false, Ordering::Release);
            let _ = open.reply.send(Err(OpenError::Refused));
            return;
        }
        if self.streams.len() >= self.peer_max_streams as usize {
            // Callers that gave up (their timeout fired) leave nothing behind.
            self.pending_opens.retain(|o| !o.reply.is_closed());
            self.pending_opens.push_back(open);
            return;
        }
        let id = self.next_stream_id;
        self.next_stream_id = self.next_stream_id.saturating_add(2);
        if self.next_stream_id > MAX_STREAM_ID {
            self.shared.usable.store(false, Ordering::Release);
        }
        if open.reply.send(Ok(id)).is_err() {
            // The caller gave up; the id is burnt but nothing was sent.
            return;
        }
        let mut st = Stream::new(
            self.peer_initial_window,
            self.opts.h2.initial_stream_window_size,
            Sink::Channel(open.events),
        );
        st.head_request = open.head_request;
        st.end_queued = open.end_stream;
        st.queue.push_back(Out::Headers {
            fields: open.fields,
            end_stream: open.end_stream,
        });
        st.in_ready = true;
        self.streams.insert(id, st);
        self.ready.push_back(id);
        self.idle_since = None;
    }

    fn enqueue(&mut self, id: u32, out: Out, ends: bool) {
        let gone = super::error::closed;
        let Some(st) = self.streams.get_mut(&id) else {
            out.fail(gone);
            return;
        };
        if st.reset || !st.send_open || st.end_queued {
            out.fail(gone);
            return;
        }
        if ends {
            st.end_queued = true;
        }
        st.queue.push_back(out);
        if !st.in_ready {
            st.in_ready = true;
            self.ready.push_back(id);
        }
    }

    // ----- outbound ---------------------------------------------------------

    /// Moves queued stream output into the output buffer as flow control and
    /// the buffer allow, round-robin across streams.
    fn schedule(&mut self) {
        let now = Instant::now();
        let mut finished = Vec::new();
        // Streams whose next item is DATA blocked on the connection window:
        // set aside (first in line once credit arrives) so the HEADERS and
        // trailers of other streams, which flow control does not cover,
        // still go out.
        let mut blocked: Vec<u32> = Vec::new();
        while self.pending_output() < WRITE_HIGH_WATER {
            let Some(id) = self.ready.pop_front() else {
                break;
            };
            let Some(st) = self.streams.get_mut(&id) else {
                continue;
            };
            st.in_ready = false;
            let Some(item) = st.queue.pop_front() else {
                continue;
            };
            match item {
                Out::Headers { fields, end_stream } => {
                    let mut block = Vec::new();
                    self.encoder.encode(
                        fields.iter().map(|(k, v)| (k.as_str(), v.as_str())),
                        &mut block,
                    );
                    frame::encode_headers(
                        &mut self.wbuf,
                        id,
                        &block,
                        end_stream,
                        self.peer_max_frame,
                    );
                    if end_stream {
                        st.send_open = false;
                    }
                }
                Out::Trailers(fields) => {
                    let mut block = Vec::new();
                    self.encoder.encode(
                        fields.iter().map(|(k, v)| (k.as_str(), v.as_str())),
                        &mut block,
                    );
                    frame::encode_headers(&mut self.wbuf, id, &block, true, self.peer_max_frame);
                    st.send_open = false;
                }
                Out::Data {
                    data,
                    offset,
                    end_stream,
                    ack,
                } => {
                    let remaining = data.len() - offset;
                    if remaining == 0 {
                        if end_stream {
                            frame::encode_data(&mut self.wbuf, id, &[], true);
                            st.send_open = false;
                        }
                        if let Some(ack) = ack {
                            let _ = ack.send(Ok(()));
                        }
                    } else {
                        let window = st.send_window.min(self.conn_send_window);
                        if window <= 0 {
                            // Wait for WINDOW_UPDATE.
                            st.queue.push_front(Out::Data {
                                data,
                                offset,
                                end_stream,
                                ack,
                            });
                            if st.stalled_since.is_none() {
                                st.stalled_since = Some(now);
                            }
                            if self.conn_send_window <= 0 {
                                // Blocked on the connection window: keep its
                                // place, and look for non-DATA work.
                                st.in_ready = true;
                                blocked.push(id);
                            }
                            continue;
                        }
                        let n = remaining.min(window as usize).min(self.peer_max_frame);
                        // Blocked time adds up until the response has made
                        // real progress again.
                        if let Some(since) = st.stalled_since.take() {
                            st.stall_total += now.saturating_duration_since(since);
                        }
                        st.stall_progress += n;
                        if st.stall_progress >= STALL_PROGRESS {
                            st.stall_total = Duration::ZERO;
                            st.stall_progress = 0;
                        }
                        let last = n == remaining;
                        frame::encode_data(
                            &mut self.wbuf,
                            id,
                            &data[offset..offset + n],
                            last && end_stream,
                        );
                        st.send_window -= n as i64;
                        self.conn_send_window -= n as i64;
                        if last {
                            if end_stream {
                                st.send_open = false;
                            }
                            if let Some(ack) = ack {
                                let _ = ack.send(Ok(()));
                            }
                        } else {
                            st.queue.push_front(Out::Data {
                                data,
                                offset: offset + n,
                                end_stream,
                                ack,
                            });
                        }
                    }
                }
            }
            if !st.queue.is_empty() {
                st.in_ready = true;
                self.ready.push_back(id);
            }
            if !st.send_open {
                finished.push(id);
            }
        }
        for id in blocked.into_iter().rev() {
            self.ready.push_front(id);
        }
        for id in finished {
            self.maybe_remove(id);
        }
    }

    /// Server: answers a stream with a small text response from the
    /// connection itself (431, 413, 408, 400), ignoring the rest of its body.
    fn respond_simple(&mut self, id: u32, status: StatusCode, text: &str) {
        let Some(st) = self.streams.get_mut(&id) else {
            return;
        };
        if st.reset || st.end_queued {
            return;
        }
        if let Some(abort) = st.abort.take() {
            abort.abort();
        }
        st.body_deadline = None;
        self.drop_sink(id);
        let fields = vec![
            (":status".to_string(), status.as_u16().to_string()),
            (
                "content-type".to_string(),
                "text/plain; charset=utf-8".to_string(),
            ),
            ("content-length".to_string(), text.len().to_string()),
        ];
        self.enqueue(
            id,
            Out::Headers {
                fields,
                end_stream: false,
            },
            false,
        );
        self.enqueue(
            id,
            Out::Data {
                data: text.as_bytes().to_vec(),
                offset: 0,
                end_stream: true,
                ack: None,
            },
            true,
        );
    }

    /// Removes a stream that has nothing left to do. A server stream whose
    /// response is complete while the request is still arriving is closed
    /// with RST_STREAM(NO_ERROR) (RFC 9113 §8.1).
    fn maybe_remove(&mut self, id: u32) {
        let Some(st) = self.streams.get_mut(&id) else {
            return;
        };
        if st.task_running {
            return;
        }
        if !st.reset && st.send_open {
            return;
        }
        if !st.reset && st.recv_open {
            if !self.opts.server {
                return;
            }
            frame::encode_rst(&mut self.wbuf, id, Reason::NO_ERROR);
            st.recv_open = false;
            self.deliver(
                id,
                Event::Error(h2_error(
                    Http2Cause::LocalReset(Reason::NO_ERROR),
                    "the response is complete; the rest of the request is not needed",
                )),
            );
        }
        self.drop_stream(id);
    }

    fn drop_stream(&mut self, id: u32) {
        if let Some(mut st) = self.streams.remove(&id) {
            self.recent.record(id, st.closed_as());
            if self.opts.server && !st.reset {
                self.roll_flood_window();
                self.flood.completed = self.flood.completed.saturating_add(1);
            }
            for out in st.queue.drain(..) {
                out.fail(super::error::closed);
            }
            if let Some(abort) = st.abort.take() {
                abort.abort();
            }
            let released = st.take_sink();
            self.waiting.retain(|&w| w != id);
            // The stream is gone: what it was owed goes to the connection.
            self.release(None, released.staged);
            self.unbuffer(&released);
        }
        if self.streams.is_empty() {
            self.idle_since = Some(Instant::now());
        }
        self.open_pending();
    }

    /// Client: opens queued streams while the peer's limit allows.
    fn open_pending(&mut self) {
        while self.streams.len() < self.peer_max_streams as usize {
            let Some(open) = self.pending_opens.pop_front() else {
                break;
            };
            self.open_stream(open);
        }
    }

    /// Resets a stream from this side, telling its consumer `error`.
    fn reset_stream(&mut self, id: u32, reason: Reason, error: Option<HttpError>) {
        if self.is_idle(id) && !self.streams.contains_key(&id) {
            // A stream error on a stream that never existed (e.g. a bad
            // PRIORITY frame): just answer with RST_STREAM.
            frame::encode_rst(&mut self.wbuf, id, reason);
            return;
        }
        frame::encode_rst(&mut self.wbuf, id, reason);
        if !self.streams.contains_key(&id) {
            return;
        }
        // Without an explicit error, the reset is a reaction to the peer
        // breaking a rule on the stream.
        let error = error
            .unwrap_or_else(|| h2_error(Http2Cause::Protocol(reason), "stream reset by this side"));
        self.mark_reset(id, error);
    }

    /// The peer reset a stream.
    fn peer_reset(&mut self, id: u32, code: Reason) {
        let refused = code == Reason::REFUSED_STREAM && !self.opts.server;
        if let Some(st) = self.streams.get_mut(&id) {
            st.peer_reset = true;
            if refused && !st.got_head {
                self.deliver(id, Event::Refused);
                self.drop_sink(id);
            }
        }
        let error = h2_error(Http2Cause::Reset(code), "stream reset by the peer");
        self.mark_reset(id, error);
    }

    fn mark_reset(&mut self, id: u32, error: HttpError) {
        let Some(st) = self.streams.get_mut(&id) else {
            return;
        };
        st.reset = true;
        st.recv_open = false;
        st.send_open = false;
        for out in st.queue.drain(..) {
            out.fail(|| dup(&error));
        }
        self.deliver(id, Event::Error(error));
        self.drop_sink(id);
        let Some(st) = self.streams.get_mut(&id) else {
            return;
        };
        if let Some(abort) = st.abort.take() {
            // The stream keeps its slot until the task has really stopped
            // (`TaskDone`), so reset streams still count against the limit.
            abort.abort();
        }
        self.maybe_remove(id);
    }

    // ----- housekeeping -----------------------------------------------------

    fn on_tick(&mut self) -> Result<(), Fatal> {
        let now = Instant::now();
        if let (Some(limit), true) = (self.opts.write_timeout, self.pending_output() > 0) {
            if now.saturating_duration_since(self.write_clock) >= limit {
                return Err(Fatal::quiet(Some(HttpError::Timeout)));
            }
        }
        if let (Some(limit), Some(since)) = (self.opts.idle_timeout, self.idle_since) {
            if self.streams.is_empty() && now.saturating_duration_since(since) >= limit {
                self.go_away();
            }
        }
        if let Some(since) = self.goaway_pending {
            if now.saturating_duration_since(since) >= GOAWAY_GRACE {
                self.final_goaway();
            }
        }
        let ack_limit = self.opts.h2.keepalive_timeout;
        if let Some(since) = self.settings_sent {
            if now.saturating_duration_since(since) >= ack_limit {
                return Err(Fatal::timeout(
                    Some(Reason::SETTINGS_TIMEOUT),
                    "SETTINGS not acknowledged in time",
                ));
            }
        }
        // The peer shows it is alive by sending anything, or by reading:
        // output that drains from a full socket (a PING queued behind a
        // large response to a slow reader is not overdue while that
        // response keeps draining).
        let alive = self.last_read.max(self.last_drain);
        if let Some(since) = self.ping_sent {
            if now.saturating_duration_since(since.max(alive)) >= ack_limit {
                return Err(Fatal::timeout(None, "keep-alive PING not answered in time"));
            }
        } else if let Some(interval) = self.opts.h2.keepalive_interval {
            let busy = !self.streams.is_empty() || !self.pending_opens.is_empty();
            if busy && now.saturating_duration_since(alive) >= interval {
                frame::encode_ping(&mut self.wbuf, false, PING_KEEPALIVE);
                self.ping_sent = Some(now);
            }
        }
        let mut stalled = Vec::new();
        let mut late = Vec::new();
        for (&id, st) in &self.streams {
            if let (Some(limit), Some(_)) = (self.opts.stall_timeout, st.stalled_since) {
                if st.stalled_for(now) >= limit && !st.reset {
                    stalled.push(id);
                }
            }
            if st.body_deadline.is_some_and(|d| now >= d) && st.recv_open && !st.reset {
                late.push(id);
            }
        }
        for id in stalled {
            self.report(&HttpError::Timeout);
            let error = h2_error(
                Http2Cause::Timeout,
                "response output stalled on flow control",
            );
            self.reset_stream(id, Reason::CANCEL, Some(error));
        }
        for id in late {
            self.report(&HttpError::Timeout);
            self.respond_simple(id, StatusCode::REQUEST_TIMEOUT, "request timeout");
        }
        Ok(())
    }

    /// Ends the connection: GOAWAY on a protocol error, fail what is left,
    /// flush, close the write side, and linger briefly on input.
    async fn close(
        mut self,
        outcome: Result<(), Fatal>,
        reader: &mut Reader,
    ) -> Result<(), HttpError> {
        let (goaway, error) = match outcome {
            Ok(()) => (None, None),
            Err(fatal) => (fatal.goaway, fatal.error),
        };
        if let Some((reason, why)) = goaway {
            frame::encode_goaway(
                &mut self.wbuf,
                self.last_peer_stream,
                reason,
                why.as_bytes(),
            );
        }
        self.shared.usable.store(false, Ordering::Release);
        // Every open stream learns why, with the cause intact (an I/O error
        // stays an I/O error, a local flood stays `LocalLimit`).
        let failure = self.stream_failure(error.as_ref());
        let for_streams = || dup(&failure);
        let ids: Vec<u32> = self.streams.keys().copied().collect();
        for id in ids {
            if let Some(st) = self.streams.get_mut(&id) {
                st.deliver(Event::Error(for_streams()));
                for out in st.queue.drain(..) {
                    out.fail(for_streams);
                }
                if let Some(abort) = st.abort.take() {
                    abort.abort();
                }
            }
        }
        self.streams.clear();
        self.publish();
        while let Some(open) = self.pending_opens.pop_front() {
            let _ = open.reply.send(Err(OpenError::Refused));
        }
        let limit = self.opts.write_timeout.unwrap_or(CLOSE_FLUSH);
        let wbuf = std::mem::take(&mut self.wbuf);
        let pending = &wbuf[self.wpos.min(wbuf.len())..];
        let writer = &mut self.writer;
        let _ = tokio::time::timeout(limit, async {
            writer.write_all(pending).await?;
            writer.flush().await?;
            writer.shutdown().await
        })
        .await;
        if goaway.is_some() || self.goaway_received.is_some() {
            // Let the peer read the GOAWAY (ours, or what we sent after
            // its own): closing a socket with unread input — a PING the
            // peer sent after its GOAWAY, say — would reset the connection
            // first.
            let _ =
                tokio::time::timeout(LINGER, async { while reader.rx.recv().await.is_some() {} })
                    .await;
        }
        reader.task.abort();
        match error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// Writes the next slice of output, or flushes when there is none.
async fn pump<W: AsyncWrite + Unpin>(w: &mut W, buf: &[u8]) -> std::io::Result<usize> {
    if buf.is_empty() {
        w.flush().await?;
        Ok(0)
    } else {
        w.write(buf).await
    }
}

/// Resolves once the server starts draining (or is gone).
async fn wait_drain(drain: &mut Option<watch::Receiver<bool>>) {
    let Some(rx) = drain.as_mut() else {
        return std::future::pending().await;
    };
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

// ----- header validation -----------------------------------------------------

/// Header fields that are meaningless (and forbidden) in HTTP/2
/// (RFC 9113 §8.2.2).
pub(crate) fn is_connection_specific(name: &str) -> bool {
    matches!(
        name,
        "connection" | "keep-alive" | "proxy-connection" | "transfer-encoding" | "upgrade"
    )
}

/// Validates one field name: a lower-case token.
pub(crate) fn valid_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name
            .iter()
            .all(|&b| crate::http::core::is_tchar(b) && !b.is_ascii_uppercase())
}

/// Validates one field value: no leading or trailing whitespace
/// (RFC 9113 §8.2.1) and, as over HTTP/1.1, no control character other than
/// HTAB (so NUL, CR, LF and DEL are refused too).
pub(crate) fn valid_value(value: &[u8]) -> bool {
    let edge = |b: &u8| *b == b' ' || *b == b'\t';
    value
        .iter()
        .all(|&b| crate::http::core::is_field_value_byte(b))
        && !value.first().is_some_and(edge)
        && !value.last().is_some_and(edge)
}

/// A field value as this side sends it: without the surrounding whitespace
/// HTTP/2 forbids (RFC 9113 §8.2.1). HTTP/1.1 ignores it, so a handler that
/// works there works here.
pub(crate) fn trim_value(value: &str) -> String {
    value.trim_matches([' ', '\t']).to_string()
}

/// Converts regular (non-pseudo) fields into a header map; `allow_pseudo`
/// skips nothing — a pseudo-header here is an error.
pub(crate) fn fields_to_headers(
    fields: &[hpack::Field],
    allow_pseudo: bool,
) -> Result<HeaderMap, &'static str> {
    let mut headers = HeaderMap::new();
    for (name, value) in fields {
        if name.first() == Some(&b':') {
            if allow_pseudo {
                continue;
            }
            return Err("pseudo-header in a trailer section");
        }
        if !valid_name(name) {
            return Err("invalid header name");
        }
        if !valid_value(value) {
            return Err("invalid header value");
        }
        let name = String::from_utf8_lossy(name).into_owned();
        if is_connection_specific(&name) {
            return Err("connection-specific header field");
        }
        headers.push_validated(name, String::from_utf8_lossy(value).into_owned());
    }
    Ok(headers)
}

/// Parses a response head: exactly one `:status` first, then regular fields.
fn parse_response_head(fields: &[hpack::Field]) -> Result<(u16, HeaderMap), &'static str> {
    let mut status = None;
    let mut regular = false;
    for (name, value) in fields {
        if name.first() == Some(&b':') {
            if regular || name.as_slice() != b":status" || status.is_some() {
                return Err("invalid pseudo-header in a response");
            }
            if value.len() != 3 || !value.iter().all(u8::is_ascii_digit) {
                return Err("invalid :status");
            }
            let code = std::str::from_utf8(value)
                .ok()
                .and_then(|s| s.parse::<u16>().ok())
                .ok_or("invalid :status")?;
            if code < 100 {
                return Err("invalid :status");
            }
            status = Some(code);
        } else {
            regular = true;
        }
    }
    let status = status.ok_or("missing :status")?;
    let headers = fields_to_headers(fields, true)?;
    Ok((status, headers))
}

// ----- the reader task -------------------------------------------------------

struct ReaderCfg {
    server: bool,
    max_frame: u32,
    max_block: usize,
    head_timeout: Option<Duration>,
}

/// An error from reading one frame.
enum ReadError {
    Closed,
    Timeout,
    Io(std::io::Error),
    Frame(FrameError),
}

impl From<ReadError> for Inbound {
    fn from(e: ReadError) -> Self {
        match e {
            ReadError::Closed => Inbound::Closed,
            ReadError::Timeout => Inbound::Timeout,
            ReadError::Io(e) => Inbound::Io(e),
            ReadError::Frame(e) => Inbound::Error(e),
        }
    }
}

async fn read_loop<R: AsyncRead + Unpin>(mut r: R, cfg: ReaderCfg, tx: mpsc::Sender<Inbound>) {
    let end = read_frames(&mut r, &cfg, &tx).await;
    let fatal = matches!(end, Some(Inbound::Error(FrameError::Connection(..))));
    if let Some(end) = end {
        if tx.send(end).await.is_err() {
            return;
        }
    }
    if fatal {
        // Keep reading (and dropping) a little so the peer can still read
        // the GOAWAY the actor sends; the actor stops us when it is done.
        let mut sink = tokio::io::sink();
        let _ = tokio::io::copy(&mut (&mut r).take(LINGER_BYTES), &mut sink).await;
    }
}

/// Reads frames until the connection ends; returns the final item.
async fn read_frames<R: AsyncRead + Unpin>(
    r: &mut R,
    cfg: &ReaderCfg,
    tx: &mpsc::Sender<Inbound>,
) -> Option<Inbound> {
    let first_deadline = cfg.head_timeout.map(|d| Instant::now() + d);
    if cfg.server {
        let mut preface = [0u8; frame::PREFACE.len()];
        match within(first_deadline, r.read_exact(&mut preface)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Some(Inbound::Closed)
            }
            Ok(Err(e)) => return Some(Inbound::Io(e)),
            Err(()) => return Some(Inbound::Timeout),
        }
        if &preface != frame::PREFACE {
            return Some(Inbound::Error(FrameError::Connection(
                Reason::PROTOCOL_ERROR,
                "invalid connection preface",
            )));
        }
    }
    let mut first = true;
    loop {
        // The peer's first frame (its SETTINGS) must arrive in time too.
        let deadline = if first && cfg.server {
            first_deadline
        } else {
            None
        };
        let (head, payload) = match read_frame(r, cfg.max_frame, deadline).await {
            Ok(f) => f,
            Err(e) => return Some(e.into()),
        };
        if first {
            first = false;
            if head.kind != frame::kind::SETTINGS || head.flags & frame::flag::ACK != 0 {
                return Some(Inbound::Error(FrameError::Connection(
                    Reason::PROTOCOL_ERROR,
                    "the first frame must be SETTINGS",
                )));
            }
        }
        let decoded = match frame::decode(head, payload) {
            Ok(f) => f,
            Err(e @ FrameError::Stream(..)) => {
                if tx.send(Inbound::Error(e)).await.is_err() {
                    return None;
                }
                continue;
            }
            Err(e) => return Some(Inbound::Error(e)),
        };
        let item = match decoded {
            Frame::Headers {
                stream,
                block,
                end_stream,
                end_headers,
                priority,
            } => match read_block(r, cfg, stream, block, end_headers).await {
                Ok(block) => Inbound::Headers {
                    stream,
                    block,
                    end_stream,
                    priority,
                },
                Err(e) => return Some(e.into()),
            },
            Frame::Continuation { .. } => {
                return Some(Inbound::Error(FrameError::Connection(
                    Reason::PROTOCOL_ERROR,
                    "CONTINUATION without HEADERS",
                )))
            }
            other => Inbound::Frame(other),
        };
        if tx.send(item).await.is_err() {
            return None;
        }
    }
}

/// Collects the CONTINUATION frames of a header block, bounded in total
/// size, frame count and time.
async fn read_block<R: AsyncRead + Unpin>(
    r: &mut R,
    cfg: &ReaderCfg,
    stream: u32,
    mut block: Vec<u8>,
    mut end_headers: bool,
) -> Result<Vec<u8>, ReadError> {
    let too_large = || {
        ReadError::Frame(FrameError::Connection(
            Reason::ENHANCE_YOUR_CALM,
            "header block too large",
        ))
    };
    if block.len() > cfg.max_block {
        return Err(too_large());
    }
    let deadline = cfg.head_timeout.map(|d| Instant::now() + d);
    let mut continuations = 0usize;
    while !end_headers {
        let (head, payload) = read_frame(r, cfg.max_frame, deadline).await?;
        if head.kind != frame::kind::CONTINUATION || head.stream != stream {
            return Err(ReadError::Frame(FrameError::Connection(
                Reason::PROTOCOL_ERROR,
                "header block interrupted",
            )));
        }
        continuations += 1;
        if continuations > MAX_CONTINUATIONS {
            return Err(ReadError::Frame(FrameError::Connection(
                Reason::ENHANCE_YOUR_CALM,
                "too many CONTINUATION frames",
            )));
        }
        if block.len() + payload.len() > cfg.max_block {
            return Err(too_large());
        }
        end_headers = head.flags & frame::flag::END_HEADERS != 0;
        block.extend_from_slice(&payload);
    }
    Ok(block)
}

/// Reads one frame header and payload. A clean EOF at a frame boundary is
/// `Closed`; the payload buffer grows only with bytes that actually arrive.
async fn read_frame<R: AsyncRead + Unpin>(
    r: &mut R,
    max_frame: u32,
    deadline: Option<Instant>,
) -> Result<(Head, Vec<u8>), ReadError> {
    let read = async {
        let mut hb = [0u8; frame::HEADER_LEN];
        let n = r.read(&mut hb[..1]).await.map_err(ReadError::Io)?;
        if n == 0 {
            return Err(ReadError::Closed);
        }
        r.read_exact(&mut hb[1..]).await.map_err(ReadError::Io)?;
        let head = Head::parse(&hb);
        if head.len > max_frame {
            return Err(ReadError::Frame(FrameError::Connection(
                Reason::FRAME_SIZE_ERROR,
                "frame larger than SETTINGS_MAX_FRAME_SIZE",
            )));
        }
        let len = head.len as usize;
        let mut payload = Vec::with_capacity(len.min(16 * 1024));
        (&mut *r)
            .take(len as u64)
            .read_to_end(&mut payload)
            .await
            .map_err(ReadError::Io)?;
        if payload.len() != len {
            return Err(ReadError::Io(std::io::ErrorKind::UnexpectedEof.into()));
        }
        Ok((head, payload))
    };
    match within(deadline, read).await {
        Ok(r) => r,
        Err(()) => Err(ReadError::Timeout),
    }
}

async fn within<T>(
    deadline: Option<Instant>,
    fut: impl std::future::Future<Output = T>,
) -> Result<T, ()> {
    match deadline {
        Some(at) => tokio::time::timeout_at(at, fut).await.map_err(|_| ()),
        None => Ok(fut.await),
    }
}
