//! Server-Sent Events (RFC / WHATWG `text/event-stream`): a structured [`Event`]
//! with correct multi-field framing, and a streaming [`SseDecoder`] for the
//! client side.
//!
//! Build events on the server and stream them with
//! [`HttpResponse::sse`](crate::http::HttpResponse::sse); decode an incoming
//! `text/event-stream` body incrementally with [`SseDecoder`], feeding it the
//! chunks from [`Response::chunk`](crate::http::Response::chunk).
//!
//! ```
//! use ferroly::http::sse::{Event, SseDecoder};
//!
//! let frame = Event::new("hello\nworld").event("greeting").id("1").to_frame();
//! assert_eq!(frame, "event: greeting\nid: 1\ndata: hello\ndata: world\n\n");
//!
//! let mut dec = SseDecoder::new();
//! let events = dec.push(frame.as_bytes());
//! assert_eq!(events[0].data, "hello\nworld");
//! assert_eq!(events[0].event.as_deref(), Some("greeting"));
//! ```

#![deny(missing_docs)]

/// A single Server-Sent Event.
///
/// Construct with [`Event::new`] and the chained setters, or read the fields
/// directly after decoding. `data` may contain newlines; each line is framed as
/// its own `data:` field on the wire.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Event {
    /// The event payload (`data:` field). May span multiple lines.
    pub data: String,
    /// The event type (`event:` field), if any.
    pub event: Option<String>,
    /// The last-event-id (`id:` field), if any.
    pub id: Option<String>,
    /// A client reconnection time in milliseconds (`retry:` field), if any.
    pub retry: Option<u64>,
    /// A comment line (`: …`), ignored by clients but useful as a keep-alive.
    pub comment: Option<String>,
}

impl Event {
    /// A new event carrying `data`.
    pub fn new(data: impl Into<String>) -> Self {
        Event {
            data: data.into(),
            ..Default::default()
        }
    }

    /// A comment-only event (`: <text>`) — a keep-alive that clients ignore.
    pub fn keep_alive(text: impl Into<String>) -> Self {
        Event {
            comment: Some(text.into()),
            ..Default::default()
        }
    }

    /// Sets the event type (`event:`).
    pub fn event(mut self, name: impl Into<String>) -> Self {
        self.event = Some(name.into());
        self
    }

    /// Sets the event id (`id:`).
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Sets the reconnection time in milliseconds (`retry:`).
    pub fn retry(mut self, ms: u64) -> Self {
        self.retry = Some(ms);
        self
    }

    /// Sets a comment line (`: …`).
    pub fn comment(mut self, text: impl Into<String>) -> Self {
        self.comment = Some(text.into());
        self
    }

    /// Encodes the event to its wire frame, terminated by the blank line
    /// that dispatches it. Field order is comment, event, id, retry, then
    /// `data:` lines — the ordering browsers expect.
    ///
    /// No field value can inject another field or end the event early:
    ///
    /// - `data` and `comment` are split into one line per `\r\n`, `\r` or
    ///   `\n`, each written as its own `data:` / `:` line;
    /// - `event` and `id` are single-line fields, so every `\r` and `\n` is
    ///   removed from them (and NUL from `id`, which the format forbids).
    pub fn to_frame(&self) -> String {
        let mut out = String::new();
        if let Some(c) = &self.comment {
            for line in split_lines(c) {
                out.push_str(": ");
                out.push_str(line);
                out.push('\n');
            }
        }
        if let Some(e) = &self.event {
            out.push_str("event: ");
            out.extend(e.chars().filter(|c| !matches!(c, '\r' | '\n')));
            out.push('\n');
        }
        if let Some(id) = &self.id {
            out.push_str("id: ");
            out.extend(id.chars().filter(|c| !matches!(c, '\r' | '\n' | '\0')));
            out.push('\n');
        }
        if let Some(r) = &self.retry {
            out.push_str("retry: ");
            out.push_str(&r.to_string());
            out.push('\n');
        }
        // A payload-less comment/keep-alive still needs the dispatching blank
        // line, but emits no `data:` field.
        if !self.data.is_empty() || (self.comment.is_none() && self.event.is_none()) {
            for line in split_lines(&self.data) {
                out.push_str("data: ");
                out.push_str(line);
                out.push('\n');
            }
        }
        out.push('\n');
        out
    }
}

/// Splits on every line ending the event-stream format recognises: `\r\n`,
/// a lone `\r`, or a lone `\n`. Always yields at least one (possibly empty)
/// line.
fn split_lines(s: &str) -> impl Iterator<Item = &str> {
    let mut rest = Some(s);
    std::iter::from_fn(move || {
        let s = rest?;
        match s.find(['\r', '\n']) {
            Some(i) => {
                let skip = if s[i..].starts_with("\r\n") { 2 } else { 1 };
                rest = Some(&s[i + skip..]);
                Some(&s[..i])
            }
            None => {
                rest = None;
                Some(s)
            }
        }
    })
}

/// Default cap on a single line and on one event's accumulated data.
const DEFAULT_MAX_EVENT_BYTES: usize = 1024 * 1024;

/// An incremental decoder for a `text/event-stream` body.
///
/// Feed it raw bytes as they arrive with [`push`](SseDecoder::push); it buffers
/// across chunk boundaries and returns each [`Event`] once its dispatching blank
/// line is seen.
///
/// Lines end at `\r\n`, a lone `\r` or a lone `\n`, as the format specifies. A
/// leading UTF-8 byte-order mark is ignored. To bound memory against a
/// hostile stream, a line longer than the size limit (1 MiB by default, see
/// [`with_max_event_bytes`](SseDecoder::with_max_event_bytes)) is discarded,
/// and so is an event whose data grows past it.
#[derive(Debug)]
pub struct SseDecoder {
    buf: Vec<u8>,
    cur: Event,
    dirty: bool,
    /// The previous push ended in `\r`: a `\n` at the start of the next one
    /// belongs to the same line ending.
    pending_cr: bool,
    /// Nothing has been consumed yet (a byte-order mark may still follow).
    at_start: bool,
    /// The current line overflowed and is being skipped to its end.
    skipping_line: bool,
    /// The current event overflowed and is dropped at its blank line.
    dropping_event: bool,
    max_bytes: usize,
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self::with_max_event_bytes(DEFAULT_MAX_EVENT_BYTES)
    }
}

impl SseDecoder {
    /// A fresh decoder.
    pub fn new() -> Self {
        SseDecoder::default()
    }

    /// A decoder that discards any line, and any event whose data, exceeds
    /// `max` bytes.
    pub fn with_max_event_bytes(max: usize) -> Self {
        SseDecoder {
            buf: Vec::new(),
            cur: Event::default(),
            dirty: false,
            pending_cr: false,
            at_start: true,
            skipping_line: false,
            dropping_event: false,
            max_bytes: max.max(1),
        }
    }

    /// Feeds more bytes and returns any events completed by them.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut bytes = bytes;
        if self.pending_cr && !bytes.is_empty() {
            self.pending_cr = false;
            if bytes[0] == b'\n' {
                bytes = &bytes[1..];
            }
        }
        self.buf.extend_from_slice(bytes);
        let mut events = Vec::new();
        let mut start = 0usize;
        if self.at_start {
            // Wait until a byte-order mark can be told apart, then skip it.
            const BOM: &[u8] = b"\xef\xbb\xbf";
            if self.buf.len() < BOM.len() && BOM.starts_with(&self.buf) {
                return events;
            }
            if self.buf.starts_with(BOM) {
                start = BOM.len();
            }
            self.at_start = false;
        }
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
                    // The `\n` of a `\r\n` may arrive in the next push.
                    None => self.pending_cr = true,
                }
            }
            if self.skipping_line {
                // The tail of an over-long line: drop it.
                self.skipping_line = false;
            } else if end - start > self.max_bytes {
                // An over-long line poisons the event it belongs to.
                self.dirty = true;
                self.dropping_event = true;
            } else if end == start {
                self.dispatch(&mut events);
            } else {
                let line = String::from_utf8_lossy(&self.buf[start..end]).into_owned();
                self.field(&line);
            }
            start = next;
        }
        self.buf.drain(..start);
        if self.buf.len() > self.max_bytes {
            // An unterminated line past the limit: forget it and skip the
            // rest of it when its end finally arrives.
            self.buf.clear();
            self.skipping_line = true;
            self.dirty = true;
            self.dropping_event = true;
        }
        events
    }

    /// A blank line: dispatch the event built so far, if any.
    fn dispatch(&mut self, events: &mut Vec<Event>) {
        if !self.dirty {
            return;
        }
        let mut event = std::mem::take(&mut self.cur);
        self.dirty = false;
        if std::mem::take(&mut self.dropping_event) {
            return;
        }
        // Strip the single trailing newline accumulated after the last
        // `data:` line.
        if event.data.ends_with('\n') {
            event.data.pop();
        }
        events.push(event);
    }

    fn field(&mut self, line: &str) {
        self.dirty = true;
        if let Some(rest) = line.strip_prefix(':') {
            let c = self.cur.comment.get_or_insert_with(String::new);
            if !c.is_empty() {
                c.push('\n');
            }
            c.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            return;
        }
        let (name, value) = match line.split_once(':') {
            Some((n, v)) => (n, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match name {
            "data" => {
                if self.cur.data.len() + value.len() > self.max_bytes {
                    self.cur.data.clear();
                    self.dropping_event = true;
                    return;
                }
                self.cur.data.push_str(value);
                self.cur.data.push('\n');
            }
            "event" => self.cur.event = Some(value.to_string()),
            // The format says an id containing NUL is ignored.
            "id" if !value.contains('\0') => self.cur.id = Some(value.to_string()),
            // Only a run of ASCII digits is a valid retry time.
            "retry" if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => {
                if let Ok(ms) = value.parse::<u64>() {
                    self.cur.retry = Some(ms);
                }
            }
            _ => {} // unknown fields are ignored per spec
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_multiline_and_fields() {
        let e = Event::new("line1\nline2").event("msg").id("42").retry(3000);
        assert_eq!(
            e.to_frame(),
            "event: msg\nid: 42\nretry: 3000\ndata: line1\ndata: line2\n\n"
        );
    }

    #[test]
    fn keep_alive_has_no_data_field() {
        let f = Event::keep_alive("ping").to_frame();
        assert_eq!(f, ": ping\n\n");
    }

    #[test]
    fn round_trips_through_decoder() {
        let e = Event::new("hello world").event("greeting").id("7");
        let mut dec = SseDecoder::new();
        let out = dec.push(e.to_frame().as_bytes());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], e);
    }

    #[test]
    fn decodes_across_split_chunks() {
        let mut dec = SseDecoder::new();
        // Frame arrives in three awkward pieces.
        assert!(dec.push(b"data: hel").is_empty());
        assert!(dec.push(b"lo\ndata: wor").is_empty());
        let out = dec.push(b"ld\n\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data, "hello\nworld");
    }

    #[test]
    fn multiple_events_in_one_push() {
        let mut dec = SseDecoder::new();
        let out = dec.push(b"data: a\n\ndata: b\nid: 2\n\n");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].data, "a");
        assert_eq!(out[1].data, "b");
        assert_eq!(out[1].id.as_deref(), Some("2"));
    }

    #[test]
    fn frame_cannot_be_injected_through_any_field() {
        // Every line ending in data becomes its own `data:` line.
        let frame = Event::new("a\rb\r\nc\nd").to_frame();
        assert_eq!(frame, "data: a\ndata: b\ndata: c\ndata: d\n\n");

        // An attacker-controlled value trying to start a new event or field.
        let evil = "x\r\n\r\ndata: forged\r\rid: 9\nevent: admin";
        let e = Event::new("real").event(evil).id(evil).comment(evil);
        let frame = e.to_frame();
        let mut dec = SseDecoder::new();
        let out = dec.push(frame.as_bytes());
        assert_eq!(out.len(), 1, "frame={frame:?}");
        assert_eq!(out[0].data, "real");
        let flat = "xdata: forgedid: 9event: admin";
        assert_eq!(out[0].event.as_deref(), Some(flat));
        assert_eq!(out[0].id.as_deref(), Some(flat));
        // No line of the frame other than the real ones starts a field.
        assert_eq!(frame.matches("\ndata: ").count(), 1);
        assert_eq!(frame.matches("\n\n").count(), 1);

        assert_eq!(Event::new("x").id("a\0b").to_frame(), "id: ab\ndata: x\n\n");
    }

    #[test]
    fn decoder_treats_cr_and_crlf_as_line_ends() {
        let mut dec = SseDecoder::new();
        let out = dec.push(b"data: a\rdata: b\r\rdata: c\r\n\r\n");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].data, "a\nb");
        assert_eq!(out[1].data, "c");

        // A CRLF split across pushes is one line ending, not two.
        let mut dec = SseDecoder::new();
        assert!(dec.push(b"data: x\r").is_empty());
        assert!(dec.push(b"\ndata: y\r").is_empty());
        let out = dec.push(b"\n\r");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data, "x\ny");
        // ...and the `\n` completing that last CRLF does not dispatch again.
        assert!(dec.push(b"\ndata: z\n\n").len() == 1);
    }

    #[test]
    fn decoder_skips_bom_and_ignores_bad_fields() {
        let mut dec = SseDecoder::new();
        assert!(dec.push(b"\xef\xbb").is_empty());
        let out = dec.push(b"\xbfdata: a\nid: bad\0id\nretry: +5\nretry: 7\n\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data, "a");
        assert_eq!(out[0].id, None);
        assert_eq!(out[0].retry, Some(7));

        // Not a BOM: the bytes are data.
        let mut dec = SseDecoder::new();
        let out = dec.push(b"data: b\n\n");
        assert_eq!(out[0].data, "b");
    }

    #[test]
    fn decoder_bounds_lines_and_events() {
        let mut dec = SseDecoder::with_max_event_bytes(16);
        // An over-long line inside an event drops that event only.
        let mut input = b"data: ok\n\ndata: ".to_vec();
        input.extend(std::iter::repeat(b'x').take(100));
        input.extend_from_slice(b"\n\ndata: after\n\n");
        let out = dec.push(&input);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].data, "ok");
        assert_eq!(out[1].data, "after");

        // The same, delivered in small pieces, never buffers past the limit.
        let mut dec = SseDecoder::with_max_event_bytes(16);
        let mut out = Vec::new();
        for piece in input.chunks(5) {
            out.extend(dec.push(piece));
            assert!(dec.buf.len() <= 16 + 5);
        }
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].data, "after");

        // Many small data lines adding up past the limit drop the event too.
        let mut dec = SseDecoder::with_max_event_bytes(16);
        let out = dec.push(b"data: 12345678\ndata: 12345678\ndata: 1\n\ndata: z\n\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data, "z");
    }
}
