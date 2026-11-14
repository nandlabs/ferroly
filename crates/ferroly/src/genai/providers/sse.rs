//! Framing and pumping of provider streams (Server-Sent Events and
//! newline-delimited JSON), fed by `ferroly::http::Response::chunk()`.
//!
//! Network chunks are buffered as **bytes** and only complete lines are
//! decoded, so a multi-byte UTF-8 sequence, a line or an event that is split
//! across chunks is reassembled before anything looks at it.

use std::time::Duration;

use tokio::sync::mpsc::Sender;

use super::common::{map_http_error, CallGuard};
use ferroly::genai::{CompletionChunk, GenAiError, TimeoutPhase};
use ferroly::http::Response;

/// Upper bound on one line (and on one SSE event's data) — guards against a
/// broken or hostile server that never ends a line.
pub(crate) const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// How long the pump keeps draining a body after the provider's terminal
/// marker, so that the connection can go back to the pool.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Accumulates response bytes and yields complete lines across chunk
/// boundaries. Lines end at `\n`, `\r\n` or a lone `\r`.
pub(crate) struct LineBuffer {
    buf: Vec<u8>,
    /// The previous push ended in `\r`: a `\n` starting the next one belongs
    /// to the same line ending.
    pending_cr: bool,
    /// The current line overflowed and is being skipped to its end.
    skipping: bool,
    max: usize,
}

impl LineBuffer {
    pub(crate) fn new() -> Self {
        Self::with_max(MAX_LINE_BYTES)
    }

    pub(crate) fn with_max(max: usize) -> Self {
        Self {
            buf: Vec::new(),
            pending_cr: false,
            skipping: false,
            max: max.max(1),
        }
    }

    /// Appends a network chunk and moves every line it completes into `out`.
    /// Returns `false` when a line exceeded the size limit: that line is
    /// discarded (its tail is skipped when it finally ends) and the caller
    /// should fail the stream.
    pub(crate) fn push(&mut self, bytes: &[u8], out: &mut Vec<String>) -> bool {
        let mut bytes = bytes;
        if self.pending_cr && !bytes.is_empty() {
            self.pending_cr = false;
            if bytes[0] == b'\n' {
                bytes = &bytes[1..];
            }
        }
        self.buf.extend_from_slice(bytes);
        let mut ok = true;
        let mut start = 0usize;
        while let Some(offset) = self.buf[start..]
            .iter()
            .position(|&b| b == b'\n' || b == b'\r')
        {
            let end = start + offset;
            let mut next = end + 1;
            if self.buf[end] == b'\r' {
                match self.buf.get(next) {
                    Some(b'\n') => next += 1,
                    Some(_) => {}
                    None => self.pending_cr = true,
                }
            }
            if self.skipping {
                self.skipping = false;
            } else if end - start > self.max {
                ok = false;
            } else {
                out.push(String::from_utf8_lossy(&self.buf[start..end]).into_owned());
            }
            start = next;
        }
        self.buf.drain(..start);
        if self.buf.len() > self.max {
            // An unterminated line past the limit: forget it and skip the rest
            // of it when its end arrives.
            self.buf.clear();
            self.skipping = true;
            ok = false;
        }
        ok
    }

    /// At end of stream: the unterminated trailing line, if any.
    pub(crate) fn finish(&mut self) -> Option<String> {
        if self.skipping || self.buf.is_empty() {
            self.buf.clear();
            return None;
        }
        let line = String::from_utf8_lossy(&self.buf).into_owned();
        self.buf.clear();
        Some(line)
    }
}

/// One dispatched Server-Sent Event.
#[cfg(any(feature = "openai", feature = "claude"))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub(crate) event: Option<String>,
    pub(crate) data: String,
}

/// Assembles SSE fields into events: `data:` lines of one event are joined
/// with `\n`, `event:` names it, `:` comment lines and unknown fields are
/// ignored, and a blank line dispatches.
#[cfg(any(feature = "openai", feature = "claude"))]
#[derive(Default)]
pub(crate) struct SseAssembler {
    event: Option<String>,
    data: String,
    has_data: bool,
    first_line: bool,
    overflow: bool,
}

#[cfg(any(feature = "openai", feature = "claude"))]
impl SseAssembler {
    pub(crate) fn new() -> Self {
        SseAssembler {
            first_line: true,
            ..SseAssembler::default()
        }
    }

    /// Feeds one line (without its terminator). Returns the event a blank
    /// line completes. `Err(())` means the event's data outgrew the limit.
    pub(crate) fn line(&mut self, line: &str) -> Result<Option<SseEvent>, ()> {
        let line = if std::mem::take(&mut self.first_line) {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return Ok(None); // comment / keep-alive
        }
        let (name, value) = match line.split_once(':') {
            Some((n, v)) => (n, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match name {
            "data" => {
                if self.data.len() + value.len() > MAX_LINE_BYTES {
                    self.overflow = true;
                    self.data.clear();
                } else if !self.overflow {
                    if self.has_data {
                        self.data.push('\n');
                    }
                    self.data.push_str(value);
                }
                self.has_data = true;
            }
            "event" => self.event = Some(value.to_string()),
            _ => {} // `id`, `retry` and unknown fields carry nothing we use
        }
        Ok(None)
    }

    /// Dispatches whatever is pending — a blank line, or the end of the
    /// stream (a final event that lacks its blank line is still delivered).
    pub(crate) fn dispatch(&mut self) -> Result<Option<SseEvent>, ()> {
        let event = self.event.take();
        let data = std::mem::take(&mut self.data);
        let had_data = std::mem::take(&mut self.has_data);
        if std::mem::take(&mut self.overflow) {
            return Err(());
        }
        Ok(if had_data {
            Some(SseEvent { event, data })
        } else {
            None
        })
    }
}

/// How a provider frames its stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Framing {
    /// `text/event-stream`.
    #[cfg(any(feature = "openai", feature = "claude"))]
    Sse,
    /// One JSON document per line.
    #[cfg(feature = "ollama")]
    NdJson,
}

/// A provider's stateful stream decoder.
pub(crate) trait StreamParser: Send + 'static {
    /// Handles one unit of the stream — the data of an SSE event (with its
    /// `event:` name) or one NDJSON line — appending the chunks it yields.
    /// An `Err` ends the stream.
    fn on_data(
        &mut self,
        event: Option<&str>,
        data: &str,
        out: &mut Vec<Result<CompletionChunk, GenAiError>>,
    );

    /// Whether the response is complete enough that the body may end here
    /// (a finish reason was seen). A stream that closes before that is
    /// reported as a failure instead of looking like a short success.
    fn is_complete(&self) -> bool;

    /// Whether the provider sent its terminal marker (`[DONE]`,
    /// `message_stop`, `done: true`): nothing more will follow.
    fn is_terminal(&self) -> bool;
}

/// Turns raw stream bytes into parser calls.
struct Decoder {
    lines: LineBuffer,
    #[cfg(any(feature = "openai", feature = "claude"))]
    sse: SseAssembler,
    framing: Framing,
    scratch: Vec<String>,
}

impl Decoder {
    fn new(framing: Framing) -> Self {
        Decoder {
            lines: LineBuffer::new(),
            #[cfg(any(feature = "openai", feature = "claude"))]
            sse: SseAssembler::new(),
            framing,
            scratch: Vec::new(),
        }
    }

    fn feed<P: StreamParser>(
        &mut self,
        bytes: &[u8],
        parser: &mut P,
        out: &mut Vec<Result<CompletionChunk, GenAiError>>,
    ) {
        let mut lines = std::mem::take(&mut self.scratch);
        lines.clear();
        let ok = self.lines.push(bytes, &mut lines);
        for line in &lines {
            self.line(line, parser, out);
        }
        self.scratch = lines;
        if !ok {
            out.push(Err(too_long()));
        }
    }

    fn line<P: StreamParser>(
        &mut self,
        line: &str,
        parser: &mut P,
        out: &mut Vec<Result<CompletionChunk, GenAiError>>,
    ) {
        match self.framing {
            #[cfg(any(feature = "openai", feature = "claude"))]
            Framing::Sse => match self.sse.line(line) {
                Ok(Some(ev)) => parser.on_data(ev.event.as_deref(), &ev.data, out),
                Ok(None) => {}
                Err(()) => out.push(Err(too_long())),
            },
            #[cfg(feature = "ollama")]
            Framing::NdJson => {
                if !line.trim().is_empty() {
                    parser.on_data(None, line, out);
                }
            }
        }
    }

    /// The body ended: flush an unterminated last line / event.
    fn finish<P: StreamParser>(
        &mut self,
        parser: &mut P,
        out: &mut Vec<Result<CompletionChunk, GenAiError>>,
    ) {
        if let Some(rest) = self.lines.finish() {
            self.line(&rest, parser, out);
        }
        match self.framing {
            #[cfg(any(feature = "openai", feature = "claude"))]
            Framing::Sse => match self.sse.dispatch() {
                Ok(Some(ev)) => parser.on_data(ev.event.as_deref(), &ev.data, out),
                Ok(None) => {}
                Err(()) => out.push(Err(too_long())),
            },
            #[cfg(feature = "ollama")]
            Framing::NdJson => {}
        }
    }
}

fn too_long() -> GenAiError {
    GenAiError::ResponseParse(format!(
        "a line of the provider's stream exceeded {MAX_LINE_BYTES} bytes"
    ))
}

/// Reads the next body chunk, enforcing the idle timeout.
async fn read_chunk(
    resp: &mut Response,
    idle: Option<Duration>,
) -> Result<Option<Vec<u8>>, GenAiError> {
    let read = resp.chunk();
    let result = match idle {
        Some(limit) => match tokio::time::timeout(limit, read).await {
            Ok(r) => r,
            Err(_) => {
                return Err(GenAiError::Timeout {
                    phase: TimeoutPhase::StreamIdle,
                })
            }
        },
        None => read.await,
    };
    result.map_err(|e| map_http_error(e, TimeoutPhase::StreamIdle))
}

/// Reads a streaming HTTP response chunk by chunk, decodes it with `parser`
/// and forwards the resulting events over `tx`.
///
/// Runs inside a spawned task and ends — closing the connection — when the
/// body ends, an error occurs, the stream goes silent for longer than `idle`,
/// or **the receiver is dropped** (so an abandoned stream never leaks the task
/// or the socket). `call` reports the stream's outcome to the observer.
pub(crate) async fn pump<P: StreamParser>(
    mut resp: Response,
    tx: Sender<Result<CompletionChunk, GenAiError>>,
    mut parser: P,
    framing: Framing,
    idle: Option<Duration>,
    mut call: CallGuard,
) {
    let mut decoder = Decoder::new(framing);
    let mut out: Vec<Result<CompletionChunk, GenAiError>> = Vec::new();
    loop {
        let next = tokio::select! {
            biased;
            _ = tx.closed() => return, // receiver gone: `call` drops as cancelled
            r = read_chunk(&mut resp, idle) => r,
        };
        let ended = match next {
            Ok(Some(bytes)) => {
                decoder.feed(&bytes, &mut parser, &mut out);
                false
            }
            Ok(None) => {
                decoder.finish(&mut parser, &mut out);
                if !parser.is_complete() && !out.iter().any(Result::is_err) {
                    out.push(Err(GenAiError::Transport(
                        "the stream ended before the provider signalled completion".into(),
                    )));
                }
                true
            }
            Err(e) => {
                out.push(Err(e));
                true
            }
        };
        for item in out.drain(..) {
            match item {
                Ok(chunk) => {
                    call.observe_chunk(&chunk);
                    if tx.send(Ok(chunk)).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    call.end_err(&e);
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            }
        }
        if ended {
            call.end_stream_ok();
            return;
        }
        if parser.is_terminal() {
            // The provider is done. Hand the result over now, then drain the
            // little that may remain so the connection can be reused.
            call.end_stream_ok();
            drop(tx);
            let _ = tokio::time::timeout(DRAIN_GRACE, async {
                while let Ok(Some(_)) = resp.chunk().await {}
            })
            .await;
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_of(chunks: &[&[u8]]) -> Vec<String> {
        let mut lb = LineBuffer::new();
        let mut out = Vec::new();
        for c in chunks {
            assert!(lb.push(c, &mut out));
        }
        if let Some(rest) = lb.finish() {
            out.push(rest);
        }
        out
    }

    #[test]
    fn multi_byte_utf8_split_at_every_point() {
        let text = "h\u{e9}llo \u{4e16}\u{754c} \u{1f600}\nnext\n";
        let bytes = text.as_bytes();
        for split in 0..=bytes.len() {
            let got = lines_of(&[&bytes[..split], &bytes[split..]]);
            assert_eq!(
                got,
                vec!["h\u{e9}llo \u{4e16}\u{754c} \u{1f600}", "next"],
                "split at {split}"
            );
        }
    }

    #[test]
    fn line_endings_and_split_lines() {
        assert_eq!(lines_of(&[b"a\r\nb\rc\nd"]), vec!["a", "b", "c", "d"]);
        // CRLF split between two chunks is one line ending, not two.
        assert_eq!(lines_of(&[b"a\r", b"\nb\n"]), vec!["a", "b"]);
        assert_eq!(
            lines_of(&[b"par", b"tial li", b"ne\n"]),
            vec!["partial line"]
        );
        // A blank line is preserved (it dispatches an SSE event).
        assert_eq!(lines_of(&[b"a\n\nb\n"]), vec!["a", "", "b"]);
    }

    #[test]
    fn over_long_line_is_dropped_and_reported() {
        let mut lb = LineBuffer::with_max(8);
        let mut out = Vec::new();
        assert!(lb.push(b"ok\n", &mut out));
        assert!(!lb.push(b"0123456789abcdef", &mut out), "overflow reported");
        // The tail of the long line is skipped; the next line is intact.
        assert!(lb.push(b"tail\nfine\n", &mut out));
        assert_eq!(out, vec!["ok", "fine"]);
        // A terminated line that is too long is dropped too.
        assert!(!lb.push(b"0123456789\nshort\n", &mut out));
        assert_eq!(out, vec!["ok", "fine", "short"]);
    }

    #[cfg(any(feature = "openai", feature = "claude"))]
    #[test]
    fn sse_fields_comments_and_multi_line_data() {
        let mut sse = SseAssembler::new();
        let mut events = Vec::new();
        for line in [
            "\u{feff}: keep-alive",
            "event: greeting",
            "data: hello",
            "data:world",
            "id: 7",
            "retry: 10",
            "",
            "",
            ": another comment",
            "data",
            "",
            "data: {\"a\": \"x: y\"}",
        ] {
            if let Some(ev) = sse.line(line).unwrap() {
                events.push(ev);
            }
        }
        if let Some(ev) = sse.dispatch().unwrap() {
            events.push(ev);
        }
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: Some("greeting".into()),
                    data: "hello\nworld".into()
                },
                SseEvent {
                    event: None,
                    data: String::new()
                },
                SseEvent {
                    event: None,
                    data: "{\"a\": \"x: y\"}".into()
                },
            ]
        );
    }
}
