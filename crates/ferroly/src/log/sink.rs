//! Output sinks: where formatted records go.

use std::cell::Cell;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Mutex;

use super::{Format, Level};

/// What a non-blocking sink does when its buffer is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overflow {
    /// Drop the record and count it (see [`Logger::dropped`](super::Logger::dropped)).
    /// The logging call never waits. This is the default.
    #[default]
    Drop,
    /// Wait for space. No record is lost, at the price of the logging call
    /// blocking while the writer is slower than the producers — the right
    /// choice for audit logs.
    Block,
}

/// A command sent to a background writer thread.
enum Cmd {
    /// Write a formatted record.
    Write(Vec<u8>),
    /// Flush the writer, then acknowledge on the given channel.
    Flush(SyncSender<()>),
}

enum Output {
    Sync(Mutex<Box<dyn Write + Send>>),
    Async {
        tx: SyncSender<Cmd>,
        dropped: AtomicU64,
    },
}

thread_local! {
    /// Set while this thread is inside a synchronous sink's writer, so a
    /// writer (or anything it calls) that logs cannot re-enter the same lock.
    static WRITING: Cell<bool> = const { Cell::new(false) };
}

/// True while the current thread is writing to a synchronous sink.
pub(super) fn writing() -> bool {
    WRITING.with(Cell::get)
}

struct WritingGuard;

impl WritingGuard {
    fn enter() -> Self {
        WRITING.with(|w| w.set(true));
        WritingGuard
    }
}

impl Drop for WritingGuard {
    fn drop(&mut self) {
        WRITING.with(|w| w.set(false));
    }
}

/// One output of a [`Logger`](super::Logger): a writer plus its own optional
/// format and minimum level. A logger fans each record out to every sink whose
/// level admits it.
///
/// ```
/// use ferroly::log::{Format, Level, Logger, Sink};
///
/// // Human-readable everything on stderr, JSON warnings and errors elsewhere.
/// let audit: Vec<u8> = Vec::new();
/// let log = Logger::new()
///     .add_sink(Sink::writer(audit).format(Format::Json).level(Level::Warn));
/// log.info("only on stderr", &[]);
/// ```
pub struct Sink {
    out: Output,
    pub(super) format: Option<Format>,
    pub(super) min: Level,
    flush_at: Level,
    overflow: Overflow,
}

impl std::fmt::Debug for Sink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sink")
            .field("non_blocking", &matches!(self.out, Output::Async { .. }))
            .field("format", &self.format)
            .field("level", &self.min)
            .field("overflow", &self.overflow)
            .finish()
    }
}

impl Sink {
    fn with_output(out: Output) -> Self {
        Sink {
            out,
            format: None,
            min: Level::Trace,
            flush_at: Level::Warn,
            overflow: Overflow::Drop,
        }
    }

    /// A synchronous sink: each record is written to `writer` with one
    /// `write_all` call before the logging call returns.
    ///
    /// The writer is flushed after every record at
    /// [`flush_level`](Self::flush_level) (default [`Level::Warn`]) or above,
    /// and by [`Logger::flush`](super::Logger::flush). If `writer` buffers (a
    /// `BufWriter`), lower-level records stay in its buffer until then.
    pub fn writer<W: Write + Send + 'static>(writer: W) -> Self {
        Self::with_output(Output::Sync(Mutex::new(Box::new(writer))))
    }

    /// A synchronous sink on standard error.
    pub fn stderr() -> Self {
        Self::writer(std::io::stderr())
    }

    /// A synchronous sink on standard output.
    pub fn stdout() -> Self {
        Self::writer(std::io::stdout())
    }

    /// A non-blocking sink: records are queued on a bounded channel of
    /// `capacity` records (minimum 1) and written by a background thread, which
    /// flushes the writer whenever the queue runs empty. What happens when the
    /// queue is full is set by [`overflow`](Self::overflow).
    pub fn non_blocking<W: Write + Send + 'static>(writer: W, capacity: usize) -> Self {
        let (tx, rx) = sync_channel::<Cmd>(capacity.max(1));
        // If the thread cannot be spawned every send fails and is counted as a
        // drop — logging never panics.
        let _ = std::thread::Builder::new()
            .name("ferroly-log".into())
            .spawn(move || writer_loop(writer, rx));
        Self::with_output(Output::Async {
            tx,
            dropped: AtomicU64::new(0),
        })
    }

    /// Sets this sink's format. Without it the sink uses its logger's format.
    pub fn format(mut self, format: Format) -> Self {
        self.format = Some(format);
        self
    }

    /// Sets this sink's minimum level (default [`Level::Trace`], i.e. every
    /// record the logger emits).
    pub fn level(mut self, level: Level) -> Self {
        self.min = level;
        self
    }

    /// Sets the full-buffer policy of a [`non_blocking`](Self::non_blocking)
    /// sink (default [`Overflow::Drop`]). No effect on a synchronous sink.
    pub fn overflow(mut self, overflow: Overflow) -> Self {
        self.overflow = overflow;
        self
    }

    /// Sets the level at and above which a synchronous sink flushes its writer
    /// after each record (default [`Level::Warn`]). Use [`Level::Trace`] to
    /// flush after every record.
    pub fn flush_level(mut self, level: Level) -> Self {
        self.flush_at = level;
        self
    }

    /// Writes one already-formatted, newline-terminated record.
    pub(super) fn write(&self, level: Level, line: &[u8]) {
        match &self.out {
            Output::Sync(m) => {
                let _guard = WritingGuard::enter();
                // A writer that panicked once must not silence the sink.
                let mut w = m.lock().unwrap_or_else(|e| e.into_inner());
                let _ = w.write_all(line);
                if level >= self.flush_at {
                    let _ = w.flush();
                }
            }
            Output::Async { tx, dropped } => {
                let cmd = Cmd::Write(line.to_vec());
                let lost = match self.overflow {
                    Overflow::Drop => match tx.try_send(cmd) {
                        Ok(()) => false,
                        Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => true,
                    },
                    Overflow::Block => tx.send(cmd).is_err(),
                };
                if lost {
                    dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Flushes the writer. For a non-blocking sink this waits until the
    /// background thread has written everything queued before the call.
    pub(super) fn flush(&self) {
        match &self.out {
            Output::Sync(m) => {
                let _guard = WritingGuard::enter();
                let mut w = m.lock().unwrap_or_else(|e| e.into_inner());
                let _ = w.flush();
            }
            Output::Async { tx, .. } => {
                let (ack_tx, ack_rx) = sync_channel::<()>(1);
                // Blocking send: the marker queues behind all pending writes.
                if tx.send(Cmd::Flush(ack_tx)).is_ok() {
                    let _ = ack_rx.recv();
                }
            }
        }
    }

    /// Records this sink has dropped (always 0 for a synchronous sink).
    pub(super) fn dropped(&self) -> u64 {
        match &self.out {
            Output::Async { dropped, .. } => dropped.load(Ordering::Relaxed),
            Output::Sync(_) => 0,
        }
    }
}

/// The background writer: drains the queue, flushing whenever it runs empty and
/// once more when every sender is gone.
fn writer_loop<W: Write>(mut w: W, rx: Receiver<Cmd>) {
    let handle = |w: &mut W, cmd: Cmd| match cmd {
        Cmd::Write(bytes) => {
            let _ = w.write_all(&bytes);
        }
        Cmd::Flush(ack) => {
            let _ = w.flush();
            let _ = ack.send(());
        }
    };
    while let Ok(cmd) = rx.recv() {
        handle(&mut w, cmd);
        while let Ok(more) = rx.try_recv() {
            handle(&mut w, more);
        }
        let _ = w.flush();
    }
    let _ = w.flush();
}
