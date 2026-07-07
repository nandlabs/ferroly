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

    /// Serializes the event to its wire frame, terminated by the blank line
    /// that dispatches it. Field order is comment, event, id, retry, then
    /// `data:` lines — the ordering browsers expect.
    pub fn to_frame(&self) -> String {
        let mut out = String::new();
        if let Some(c) = &self.comment {
            for line in c.split('\n') {
                out.push_str(": ");
                out.push_str(line);
                out.push('\n');
            }
        }
        if let Some(e) = &self.event {
            out.push_str("event: ");
            out.push_str(e);
            out.push('\n');
        }
        if let Some(id) = &self.id {
            out.push_str("id: ");
            out.push_str(id);
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
            for line in self.data.split('\n') {
                out.push_str("data: ");
                out.push_str(line);
                out.push('\n');
            }
        }
        out.push('\n');
        out
    }
}

/// An incremental decoder for a `text/event-stream` body.
///
/// Feed it raw bytes as they arrive with [`push`](SseDecoder::push); it buffers
/// across chunk boundaries and returns each [`Event`] once its dispatching blank
/// line is seen.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
    cur: Event,
    dirty: bool,
}

impl SseDecoder {
    /// A fresh decoder.
    pub fn new() -> Self {
        SseDecoder::default()
    }

    /// Feeds more bytes and returns any events completed by them.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.buf.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=nl).collect();
            line.pop(); // drop '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            if line.is_empty() {
                if self.dirty {
                    // Strip the single trailing newline accumulated after the
                    // last `data:` line.
                    if self.cur.data.ends_with('\n') {
                        self.cur.data.pop();
                    }
                    events.push(std::mem::take(&mut self.cur));
                    self.dirty = false;
                }
                continue;
            }
            self.field(&line);
        }
        events
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
                self.cur.data.push_str(value);
                self.cur.data.push('\n');
            }
            "event" => self.cur.event = Some(value.to_string()),
            "id" => self.cur.id = Some(value.to_string()),
            "retry" => {
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
}
