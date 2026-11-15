//! The plain-text span exporter: the span counterpart of `log`'s plain format.

use std::fmt::Write as _;
use std::io::Write;
use std::sync::Mutex;
use std::time::Duration;

use ferroly::codec::Value;

use super::{span, Exporter, FinishedSpan, Level, Status};
pub use crate::textfmt::Precision;
use crate::textfmt::{resolve, rfc3339_unix_nanos, write_plain_fields, write_token, Field};

/// An [`Exporter`] that writes each finished span as human-readable text, in
/// the same `key=value` style as `log`'s plain format, so an application in
/// "human" output mode gets one readable stream for both.
///
/// One line per span, written when it ends, then one indented line per event:
///
/// ```text
/// 2026-11-15T09:12:03.121Z INFO  span request 48.2ms trace=4bf92f35 route=/v1/chat/completions tokens=24
///   event "first token" +12.3ms n=1
/// ```
///
/// * **Span line:** `<start> <LEVEL> span <name> <duration> trace=<8 hex>
///   key=value …`. The start is RFC 3339 (UTC, milliseconds by default; see
///   [`time_precision`](PlainExporter::time_precision) and
///   [`utc_offset`](PlainExporter::utc_offset)), the level is padded to five
///   columns like `log`'s, and the duration has an adaptive unit — `ns`
///   below a microsecond, then `µs`, `ms` or `s` with three significant
///   digits (`750ns`, `12.3µs`, `48.2ms`, `2.50s`; whole seconds from
///   1000 s).
/// * **Event lines:** `  event <name> +<offset from the span's start>
///   key=value …`, with the offset in the same units.
/// * **Ids:** the first 8 hex digits of the trace id by default;
///   [`with_ids`](PlainExporter::with_ids) prints the full trace id and the
///   span id (`trace=<32 hex> span=<16 hex>`), and
///   [`with_parent`](PlainExporter::with_parent) adds `parent=<id>` for a
///   span that has one (8 hex digits, or all 16 with `with_ids`).
/// * **Status:** a span whose status is [`Status::Error`](super::Status::Error)
///   gets `status=error error=<message>` after the ids (`error=` is left out
///   when the message is empty); a successful or unset status prints nothing.
/// * **Names and fields** are written exactly as `log`'s plain format writes
///   them (it is the same code): a string bare unless it is empty or holds
///   whitespace, a control character, `=`, `"` or `\`, in which case it is
///   quoted and escaped; numbers, booleans and null as they are; arrays and
///   objects as compact JSON. A repeated key prints once, with its last
///   value, and a field named like one of the line's own keys (`trace`, and
///   `span` / `parent` / `status` / `error` when printed) moves to the end as
///   `fields.<key>`. With the `log` feature, `redact` masks sensitive fields
///   with a `log::Redactor`; nothing is masked otherwise.
///
/// A standalone event — the zero-duration span named `event` that
/// [`obs::event`](super::event) emits when no span is current — is written as
/// a single line, `<time> <LEVEL> event <message> trace=… key=value …`, and is
/// never dropped by [`min_duration`](PlainExporter::min_duration).
/// `obs::event` marks the span it emits as it hands it to the exporters, so
/// any other span, even one named `event`, is written as a span.
///
/// The writer is behind a mutex and each span (its line and its event lines)
/// is written with one `write_all` under it, so spans finishing on many
/// threads never interleave within a line or split a span from its events.
/// Like [`JsonExporter`](super::JsonExporter), nothing is buffered here:
/// wrap the writer in a `BufWriter` to batch writes, and call
/// [`obs::flush`](super::flush) / [`obs::shutdown`](super::shutdown) to flush
/// it.
///
/// ```
/// use std::time::Duration;
/// use ferroly::obs::{self, PlainExporter};
///
/// obs::set_exporter(Box::new(
///     PlainExporter::new(std::io::stderr()).min_duration(Duration::from_micros(100)),
/// ));
/// # obs::clear_exporters();
/// ```
pub struct PlainExporter {
    writer: Mutex<Box<dyn Write + Send>>,
    with_ids: bool,
    with_parent: bool,
    min_duration_nanos: u128,
    precision: Precision,
    offset_secs: i32,
    #[cfg(feature = "log")]
    redactor: Option<ferroly::log::Redactor>,
}

impl PlainExporter {
    /// Builds an exporter that writes to `writer` (for example
    /// `std::io::stderr()` or an open file), with the defaults: 8-digit trace
    /// ids, no parent ids, no minimum duration, UTC millisecond timestamps.
    pub fn new(writer: impl Write + Send + 'static) -> PlainExporter {
        PlainExporter {
            writer: Mutex::new(Box::new(writer)),
            with_ids: false,
            with_parent: false,
            min_duration_nanos: 0,
            precision: Precision::Millis,
            offset_secs: 0,
            #[cfg(feature = "log")]
            redactor: None,
        }
    }

    /// Prints the full trace id and the span id (`trace=<32 hex>
    /// span=<16 hex>`) instead of the 8-digit trace prefix (default off).
    pub fn with_ids(mut self, on: bool) -> PlainExporter {
        self.with_ids = on;
        self
    }

    /// Prints `parent=<id>` for spans that have a parent (default off).
    pub fn with_parent(mut self, on: bool) -> PlainExporter {
        self.with_parent = on;
        self
    }

    /// Skips spans shorter than `min`, so tight inner spans do not flood an
    /// interactive terminal (default zero: every span is written).
    /// Standalone events are always written.
    pub fn min_duration(mut self, min: Duration) -> PlainExporter {
        self.min_duration_nanos = min.as_nanos();
        self
    }

    /// Sets the timestamp precision (default [`Precision::Millis`]), as
    /// `log::Logger::time_precision` does for log records.
    pub fn time_precision(mut self, precision: Precision) -> PlainExporter {
        self.precision = precision;
        self
    }

    /// Writes timestamps at a fixed offset east of UTC, in seconds (default
    /// 0, rendered `Z`), as `log::Logger::utc_offset` does. Clamped to
    /// RFC 3339's `±23:59` and truncated to whole minutes.
    pub fn utc_offset(mut self, seconds_east: i32) -> PlainExporter {
        self.offset_secs = seconds_east;
        self
    }

    /// Masks sensitive span and event fields with `redactor`, exactly as a
    /// `log::Logger` configured with it would (default: no redaction).
    /// Requires the `log` feature.
    #[cfg(feature = "log")]
    pub fn redact(mut self, redactor: ferroly::log::Redactor) -> PlainExporter {
        self.redactor = Some(redactor);
        self
    }

    /// The text written for `span` (newline-terminated lines), or `None`
    /// when it is filtered out by [`min_duration`](PlainExporter::min_duration).
    fn render(&self, span: &FinishedSpan) -> Option<String> {
        // Only the synthetic span `obs::event` emits, never one inferred
        // from its shape: a real span may look just like it.
        let standalone = span::exporting_standalone_event() && span.events.len() == 1;
        if !standalone && span.duration_nanos < self.min_duration_nanos {
            return None;
        }
        let mut out = String::with_capacity(128);
        out.push_str(&rfc3339_unix_nanos(
            span.start_unix_nanos,
            self.precision,
            self.offset_secs,
        ));
        let _ = write!(out, " {:<5} ", upper(span.level));

        let fields: &[(String, Value)] = if standalone {
            let event = &span.events[0];
            out.push_str("event ");
            write_token(&mut out, &event.message);
            &event.fields
        } else {
            out.push_str("span ");
            write_token(&mut out, &span.name);
            out.push(' ');
            write_duration(&mut out, span.duration_nanos);
            &span.fields
        };

        let show_parent = self.with_parent && span.parent_id.is_some();
        if !span.trace_id.is_empty() {
            out.push_str(" trace=");
            if self.with_ids {
                write_token(&mut out, &span.trace_id);
            } else {
                write_token(&mut out, prefix(&span.trace_id));
            }
        }
        if self.with_ids && !span.span_id.is_empty() {
            out.push_str(" span=");
            write_token(&mut out, &span.span_id);
        }
        if let Some(parent) = span.parent_id.as_deref().filter(|_| show_parent) {
            out.push_str(" parent=");
            write_token(
                &mut out,
                if self.with_ids {
                    parent
                } else {
                    prefix(parent)
                },
            );
        }

        let error = match &span.status {
            Status::Error(message) if !standalone => Some(message.as_str()),
            _ => None,
        };
        if let Some(message) = error {
            out.push_str(" status=error");
            if !message.is_empty() {
                out.push_str(" error=");
                write_token(&mut out, message);
            }
        }

        let with_ids = self.with_ids;
        let reserved = |key: &str| match key {
            "trace" => !span.trace_id.is_empty(),
            "span" => with_ids && !span.span_id.is_empty(),
            "parent" => show_parent,
            "status" => error.is_some(),
            "error" => error.is_some_and(|m| !m.is_empty()),
            _ => false,
        };
        self.write_fields(&mut out, &reserved, fields);
        out.push('\n');

        if !standalone {
            for event in &span.events {
                out.push_str("  event ");
                write_token(&mut out, &event.message);
                out.push_str(" +");
                write_duration(
                    &mut out,
                    event.unix_nanos.saturating_sub(span.start_unix_nanos),
                );
                self.write_fields(&mut out, &|_| false, &event.fields);
                out.push('\n');
            }
        }
        Some(out)
    }

    fn write_fields(
        &self,
        out: &mut String,
        reserved: &dyn Fn(&str) -> bool,
        fields: &[(String, Value)],
    ) {
        let sources = fields.iter().map(|(k, v)| (k.as_str(), v));
        let (top, nested): (Vec<Field<'_>>, Vec<Field<'_>>) = {
            #[cfg(feature = "log")]
            {
                match &self.redactor {
                    Some(redactor) => resolve(reserved, &|k, v| redactor.apply(k, v), sources),
                    None => resolve(reserved, &|_, v| std::borrow::Cow::Borrowed(v), sources),
                }
            }
            #[cfg(not(feature = "log"))]
            {
                resolve(reserved, &|_, v| std::borrow::Cow::Borrowed(v), sources)
            }
        };
        write_plain_fields(out, &top, &nested);
    }
}

impl Exporter for PlainExporter {
    fn export(&self, span: &FinishedSpan) {
        let Some(text) = self.render(span) else {
            return;
        };
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writer.write_all(text.as_bytes());
    }

    fn flush(&self) {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writer.flush();
    }
}

impl std::fmt::Debug for PlainExporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlainExporter")
            .field("with_ids", &self.with_ids)
            .field("with_parent", &self.with_parent)
            .field("min_duration_nanos", &self.min_duration_nanos)
            .field("precision", &self.precision)
            .field("offset_secs", &self.offset_secs)
            .finish_non_exhaustive()
    }
}

/// The uppercase level name `log`'s plain format uses.
fn upper(level: Level) -> &'static str {
    match level {
        Level::Trace => "TRACE",
        Level::Debug => "DEBUG",
        Level::Info => "INFO",
        Level::Warn => "WARN",
        Level::Error => "ERROR",
    }
}

/// The first 8 characters of a hex id (all of it when shorter).
fn prefix(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Writes a duration with an adaptive unit: whole nanoseconds below 1 µs,
/// then `µs`, `ms` or `s` with three significant digits (rounded half up,
/// moving to the next unit when rounding reaches 1000), and whole seconds
/// from 1000 s.
fn write_duration(out: &mut String, nanos: u128) {
    const SEC: u128 = 1_000_000_000;
    if nanos < 1_000 {
        let _ = write!(out, "{nanos}ns");
        return;
    }
    // Round to three significant digits.
    let mut digits = 0u32;
    let mut v = nanos;
    while v > 0 {
        v /= 10;
        digits += 1;
    }
    let step = 10u128.pow(digits - 3);
    // Saturating: a hand-built or relayed span can carry any `u128`.
    let rounded = nanos.saturating_add(step / 2) / step * step;
    if rounded >= 1_000 * SEC {
        let _ = write!(out, "{}s", nanos.saturating_add(SEC / 2) / SEC);
        return;
    }
    let (scale, unit) = if rounded < 1_000_000 {
        (1_000, "µs")
    } else if rounded < SEC {
        (1_000_000, "ms")
    } else {
        (SEC, "s")
    };
    let whole = rounded / scale;
    let decimals = match whole {
        0..=9 => 2,
        10..=99 => 1,
        _ => 0,
    };
    if decimals == 0 {
        let _ = write!(out, "{whole}{unit}");
    } else {
        let frac = rounded % scale / (scale / 10u128.pow(decimals));
        let _ = write!(
            out,
            "{whole}.{frac:0width$}{unit}",
            width = decimals as usize
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::{SpanEvent, SpanKind, Status};
    use super::*;
    use std::sync::Arc;

    /// A cloneable in-memory writer.
    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Buf {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    /// 2026-11-15T09:12:03.121Z in nanoseconds since the epoch.
    const START: u128 = 1_794_733_923_121_000_000;

    fn field(k: &str, v: impl Into<Value>) -> (String, Value) {
        (k.to_string(), v.into())
    }

    fn request_span() -> FinishedSpan {
        FinishedSpan {
            name: "request".into(),
            trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".into(),
            span_id: "00f067aa0ba902b7".into(),
            parent_id: Some("a1b2c3d4e5f60718".into()),
            level: Level::Info,
            start_unix_nanos: START,
            duration_nanos: 48_211_000,
            kind: SpanKind::Server,
            status: Status::Ok,
            fields: vec![field("route", "/v1/chat/completions"), field("tokens", 24)],
            events: vec![
                SpanEvent {
                    level: Level::Info,
                    message: "first token".into(),
                    fields: vec![field("n", 1)],
                    unix_nanos: START + 12_345_678,
                },
                SpanEvent {
                    level: Level::Warn,
                    message: "retry".into(),
                    fields: vec![field("attempt", 2), field("reason", "rate limited")],
                    unix_nanos: START + 48_000_000,
                },
            ],
            ..FinishedSpan::default()
        }
    }

    fn export_to_string(exporter: PlainExporter, spans: &[FinishedSpan]) -> String {
        let buf = Buf::default();
        let exporter = PlainExporter {
            writer: Mutex::new(Box::new(buf.clone())),
            ..exporter
        };
        for span in spans {
            exporter.export(span);
        }
        exporter.flush();
        buf.text()
    }

    fn plain() -> PlainExporter {
        PlainExporter::new(std::io::sink())
    }

    #[test]
    fn golden_span_with_fields_events_and_parent() {
        let out = export_to_string(plain(), &[request_span()]);
        assert_eq!(
            out,
            "2026-11-15T09:12:03.121Z INFO  span request 48.2ms trace=4bf92f35 \
             route=/v1/chat/completions tokens=24\n\
             \x20 event \"first token\" +12.3ms n=1\n\
             \x20 event retry +48.0ms attempt=2 reason=\"rate limited\"\n"
        );

        let out = export_to_string(plain().with_parent(true), &[request_span()]);
        assert_eq!(
            out.lines().next().unwrap(),
            "2026-11-15T09:12:03.121Z INFO  span request 48.2ms trace=4bf92f35 \
             parent=a1b2c3d4 route=/v1/chat/completions tokens=24"
        );

        let out = export_to_string(plain().with_ids(true).with_parent(true), &[request_span()]);
        assert_eq!(
            out.lines().next().unwrap(),
            "2026-11-15T09:12:03.121Z INFO  span request 48.2ms \
             trace=4bf92f3577b34da6a3ce929d0e0e4736 span=00f067aa0ba902b7 \
             parent=a1b2c3d4e5f60718 route=/v1/chat/completions tokens=24"
        );

        // A root span prints no parent even when asked to.
        let root = FinishedSpan {
            parent_id: None,
            events: vec![],
            level: Level::Error,
            ..request_span()
        };
        let out = export_to_string(plain().with_parent(true), &[root]);
        assert_eq!(
            out,
            "2026-11-15T09:12:03.121Z ERROR span request 48.2ms trace=4bf92f35 \
             route=/v1/chat/completions tokens=24\n"
        );
    }

    #[test]
    fn golden_duration_units() {
        let cases: &[(u128, &str)] = &[
            (0, "0ns"),
            (750, "750ns"),
            (999, "999ns"),
            (1_000, "1.00µs"),
            (1_234, "1.23µs"),
            (12_345, "12.3µs"),
            (123_456, "123µs"),
            (999_499, "999µs"),
            (999_500, "1.00ms"),
            (48_211_000, "48.2ms"),
            (480_000_000, "480ms"),
            (999_999_999, "1.00s"),
            (2_500_000_000, "2.50s"),
            (61_000_000_000, "61.0s"),
            (999_000_000_000, "999s"),
            (999_600_000_000, "1000s"),
            (3_600_000_000_000, "3600s"),
            (u128::from(u64::MAX), "18446744074s"),
        ];
        for &(nanos, want) in cases {
            let mut out = String::new();
            write_duration(&mut out, nanos);
            assert_eq!(out, want, "{nanos} ns");
        }
        // Each unit, end to end on a span line.
        for (nanos, unit) in [(750, "750ns"), (12_345, "12.3µs"), (48_211_000, "48.2ms")] {
            let span = FinishedSpan {
                duration_nanos: nanos,
                events: vec![],
                fields: vec![],
                ..request_span()
            };
            let out = export_to_string(plain(), &[span]);
            assert_eq!(
                out,
                format!("2026-11-15T09:12:03.121Z INFO  span request {unit} trace=4bf92f35\n")
            );
        }
        let slow = FinishedSpan {
            duration_nanos: 2_500_000_000,
            events: vec![],
            fields: vec![],
            ..request_span()
        };
        assert!(export_to_string(plain(), &[slow]).contains(" span request 2.50s trace="));
    }

    #[test]
    fn timestamp_options() {
        let span = FinishedSpan {
            start_unix_nanos: START + 456_789,
            events: vec![],
            fields: vec![],
            ..request_span()
        };
        let out = export_to_string(
            plain()
                .time_precision(Precision::Nanos)
                .utc_offset(5 * 3600 + 30 * 60),
            std::slice::from_ref(&span),
        );
        assert!(
            out.starts_with("2026-11-15T14:42:03.121456789+05:30 INFO  span"),
            "{out}"
        );
        let out = export_to_string(plain().time_precision(Precision::Seconds), &[span]);
        assert!(out.starts_with("2026-11-15T09:12:03Z INFO  span"), "{out}");
    }

    #[test]
    fn min_duration_drops_short_spans() {
        let short = FinishedSpan {
            name: "short".into(),
            duration_nanos: 99_999,
            ..request_span()
        };
        let exact = FinishedSpan {
            name: "exact".into(),
            duration_nanos: 100_000,
            ..request_span()
        };
        let long = FinishedSpan {
            name: "long".into(),
            ..request_span()
        };
        let exporter = plain().min_duration(Duration::from_micros(100));
        let out = export_to_string(exporter, &[short.clone(), exact, long]);
        assert!(!out.contains("short"), "{out}");
        assert!(out.contains(" span exact 100µs "), "{out}");
        assert!(out.contains(" span long 48.2ms "), "{out}");
        // A dropped span takes its events with it.
        assert_eq!(out.matches("event retry").count(), 2);
        // Without the option, nothing is dropped.
        assert!(export_to_string(plain(), &[short]).contains(" span short 100µs "));
    }

    #[test]
    fn with_ids_prints_full_ids() {
        let out = export_to_string(plain().with_ids(true), &[request_span()]);
        let first = out.lines().next().unwrap();
        assert!(first.contains(" trace=4bf92f3577b34da6a3ce929d0e0e4736 "));
        assert!(first.contains(" span=00f067aa0ba902b7 "));
        assert!(!first.contains("parent="), "parent ids need with_parent");
        let out = export_to_string(plain(), &[request_span()]);
        assert!(out.contains(" trace=4bf92f35 ") && !out.contains("span=00f0"));
    }

    #[test]
    fn standalone_event_is_one_line_and_never_dropped() {
        let span = FinishedSpan {
            name: "event".into(),
            trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".into(),
            span_id: "1111111111111111".into(),
            level: Level::Warn,
            start_unix_nanos: START,
            events: vec![SpanEvent {
                level: Level::Warn,
                message: "disk almost full".into(),
                fields: vec![field("free", "1.2 GiB"), field("trace", "x")],
                unix_nanos: START,
            }],
            ..FinishedSpan::default()
        };
        let out = span::as_standalone_event(|| {
            export_to_string(
                plain().min_duration(Duration::from_secs(1)),
                std::slice::from_ref(&span),
            )
        });
        assert_eq!(
            out,
            "2026-11-15T09:12:03.121Z WARN  event \"disk almost full\" trace=4bf92f35 \
             free=\"1.2 GiB\" fields.trace=x\n"
        );
        // The same span not marked by `obs::event` is a span like any other.
        let out = export_to_string(plain(), &[span]);
        assert_eq!(
            out,
            "2026-11-15T09:12:03.121Z WARN  span event 0ns trace=4bf92f35\n\
             \x20 event \"disk almost full\" +0ns free=\"1.2 GiB\" trace=x\n"
        );
    }

    /// A real span that happens to look like a standalone event (named
    /// `event`, no fields, one event at its start, a 0 ns duration on a
    /// coarse clock) is printed as a span and obeys `min_duration`.
    #[test]
    fn a_real_span_named_event_is_not_a_standalone_event() {
        let lookalike = FinishedSpan {
            name: "event".into(),
            duration_nanos: 0,
            fields: vec![],
            events: vec![SpanEvent {
                level: Level::Info,
                message: "tick".into(),
                fields: vec![],
                unix_nanos: START,
            }],
            ..request_span()
        };
        let out = export_to_string(plain(), std::slice::from_ref(&lookalike));
        assert_eq!(
            out,
            "2026-11-15T09:12:03.121Z INFO  span event 0ns trace=4bf92f35\n\
             \x20 event tick +0ns\n"
        );
        let out = export_to_string(plain().min_duration(Duration::from_nanos(1)), &[lookalike]);
        assert_eq!(out, "");
    }

    #[test]
    fn golden_error_status() {
        let failed = FinishedSpan {
            status: Status::error("upstream timed out"),
            fields: vec![field("status", 504), field("error", "x"), field("n", 1)],
            events: vec![],
            ..request_span()
        };
        let out = export_to_string(plain().with_parent(true), &[failed]);
        assert_eq!(
            out,
            "2026-11-15T09:12:03.121Z INFO  span request 48.2ms trace=4bf92f35 \
             parent=a1b2c3d4 status=error error=\"upstream timed out\" n=1 \
             fields.status=504 fields.error=x\n"
        );
        // An empty message prints the status alone, and `error` stays a
        // plain field.
        let failed = FinishedSpan {
            status: Status::error(""),
            fields: vec![field("error", "x")],
            events: vec![],
            ..request_span()
        };
        let out = export_to_string(plain(), &[failed]);
        assert_eq!(
            out,
            "2026-11-15T09:12:03.121Z INFO  span request 48.2ms trace=4bf92f35 \
             status=error error=x\n"
        );
        // Ok and Unset print nothing, and `status` stays a plain field.
        for status in [Status::Ok, Status::Unset] {
            let span = FinishedSpan {
                status,
                fields: vec![field("status", 200)],
                events: vec![],
                ..request_span()
            };
            assert_eq!(
                export_to_string(plain(), &[span]),
                "2026-11-15T09:12:03.121Z INFO  span request 48.2ms trace=4bf92f35 \
                 status=200\n"
            );
        }
    }

    /// Every timestamp and duration is a public `u128`: extreme values from
    /// a hand-built or relayed span must not overflow.
    #[test]
    fn extreme_durations_and_timestamps_do_not_overflow() {
        let mut out = String::new();
        write_duration(&mut out, u128::MAX);
        assert_eq!(out, format!("{}s", u128::MAX / 1_000_000_000));
        let span = FinishedSpan {
            duration_nanos: u128::MAX,
            fields: vec![],
            events: vec![SpanEvent {
                level: Level::Info,
                message: "late".into(),
                fields: vec![],
                unix_nanos: u128::MAX,
            }],
            ..request_span()
        };
        let out = export_to_string(plain(), &[span]);
        assert!(
            out.contains(&format!(" span request {}s ", u128::MAX / 1_000_000_000)),
            "{out}"
        );
        assert!(out.contains("  event late +"), "{out}");
        let at_end = FinishedSpan {
            start_unix_nanos: u128::MAX,
            duration_nanos: u128::MAX - 1,
            events: vec![],
            ..request_span()
        };
        assert_eq!(export_to_string(plain(), &[at_end]).lines().count(), 1);
    }

    #[test]
    fn colliding_and_repeated_keys() {
        let span = FinishedSpan {
            fields: vec![
                field("trace", "mine"),
                field("span", "s"),
                field("parent", "p"),
                field("n", 1),
                field("n", 2),
            ],
            events: vec![],
            ..request_span()
        };
        let out = export_to_string(plain(), std::slice::from_ref(&span));
        // Only `trace` is printed by the line itself without with_ids /
        // with_parent, so only it moves.
        assert!(
            out.ends_with(" trace=4bf92f35 span=s parent=p n=2 fields.trace=mine\n"),
            "{out}"
        );
        let out = export_to_string(plain().with_ids(true).with_parent(true), &[span]);
        assert!(
            out.ends_with(" n=2 fields.trace=mine fields.span=s fields.parent=p\n"),
            "{out}"
        );
    }

    #[test]
    fn names_and_values_are_quoted_like_log() {
        let span = FinishedSpan {
            name: "GET /users/:id".into(),
            fields: vec![
                field("empty", ""),
                field("eq", "a=b"),
                field("quote", "say \"hi\""),
                field("nl", "a\nb"),
                field("ok", true),
                field("ratio", 0.5),
                ("none".into(), Value::Null),
                ("list".into(), Value::Array(vec![1.into(), "x y".into()])),
                (
                    "obj".into(),
                    Value::Object(vec![("k".into(), "v\u{2028}".into())]),
                ),
                field("my key", "v"),
            ],
            events: vec![],
            ..request_span()
        };
        let out = export_to_string(plain(), &[span]);
        assert_eq!(
            out,
            "2026-11-15T09:12:03.121Z INFO  span \"GET /users/:id\" 48.2ms trace=4bf92f35 \
             empty=\"\" eq=\"a=b\" quote=\"say \\\"hi\\\"\" nl=\"a\\nb\" ok=true ratio=0.5 \
             none=null list=[1,\"x y\"] obj={\"k\":\"v\\u2028\"} \"my key\"=v\n"
        );
    }

    /// The same values through `log`'s plain format and through this
    /// exporter print identically.
    #[cfg(feature = "log")]
    #[test]
    fn field_quoting_matches_log_plain_format() {
        use ferroly::log::{Logger, Redactor};

        let values: Vec<(&str, Value)> = vec![
            ("plain", "user:42".into()),
            ("path", "/a/b?c".into()),
            ("empty", "".into()),
            ("space", "a b".into()),
            ("eq", "a=b".into()),
            ("quote", "a\"b".into()),
            ("backslash", "a\\b".into()),
            ("controls", "a\nb\rc\td\u{1b}".into()),
            ("separators", "a\u{2028}b\u{85}c".into()),
            ("unicode", "caf\u{e9}".into()),
            ("int", (-7i64).into()),
            ("uint", Value::UInt(u64::MAX)),
            ("float", 1.25.into()),
            ("bool", false.into()),
            ("null", Value::Null),
            ("bytes", Value::Bytes(vec![1, 2, 3])),
            (
                "array",
                Value::Array(vec![1.into(), "two words".into(), Value::Null]),
            ),
            (
                "object",
                Value::Object(vec![
                    ("nested key".into(), "q\"\n\u{7f}".into()),
                    ("n".into(), 1.into()),
                ]),
            ),
            ("key with space", "v".into()),
            ("k=v", "v".into()),
            ("password", "hunter2".into()),
        ];

        for redactor in [Redactor::none(), Redactor::default()] {
            let log_buf = Buf::default();
            let logger = Logger::new()
                .to_writer(log_buf.clone())
                .redact(redactor.clone());
            // `Error` passes every global level filter other tests may set.
            logger.error("m", &values);
            let log_line = log_buf.text();

            let span = FinishedSpan {
                fields: values
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.clone()))
                    .collect(),
                events: vec![],
                ..request_span()
            };
            let span_line = export_to_string(plain().redact(redactor), &[span]);
            let span_fields = span_line
                .split_once(" trace=4bf92f35")
                .map(|(_, rest)| rest)
                .unwrap();
            // Context providers registered by other tests can only add
            // fields before the call's own, so compare the tail.
            assert!(
                log_line.ends_with(span_fields),
                "log:  {log_line}span: {span_line}"
            );
            assert!(span_fields.len() > 200, "{span_fields}");
        }
    }

    #[cfg(feature = "log")]
    #[test]
    fn redaction_applies_to_span_and_event_fields() {
        let span = FinishedSpan {
            fields: vec![field("api_key", "sk-123"), field("user", "ann")],
            events: vec![SpanEvent {
                level: Level::Info,
                message: "auth".into(),
                fields: vec![field("password", "p")],
                unix_nanos: START,
            }],
            ..request_span()
        };
        let out = export_to_string(
            plain().redact(ferroly::log::Redactor::default()),
            std::slice::from_ref(&span),
        );
        assert!(out.contains(" api_key=*** user=ann\n"), "{out}");
        assert!(out.contains("  event auth +0ns password=***\n"), "{out}");
        // No redactor configured: values are printed as they are.
        let out = export_to_string(plain(), &[span]);
        assert!(
            out.contains(" api_key=sk-123 ") && out.contains("password=p"),
            "{out}"
        );
    }

    /// A writer that accepts one byte per call and yields in between, so
    /// unsynchronised writers would interleave at the byte level.
    #[derive(Clone, Default)]
    struct Trickle(Arc<Mutex<Vec<u8>>>);

    impl Write for Trickle {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            let Some(&first) = data.first() else {
                return Ok(0);
            };
            self.0.lock().unwrap().push(first);
            std::thread::yield_now();
            Ok(1)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn concurrent_spans_never_interleave() {
        const THREADS: usize = 8;
        const SPANS: usize = 40;
        const EVENTS: usize = 3;
        let sink = Trickle::default();
        let exporter = Arc::new(PlainExporter::new(sink.clone()).with_ids(true));
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let exporter = exporter.clone();
                std::thread::spawn(move || {
                    for i in 0..SPANS {
                        let span = FinishedSpan {
                            name: format!("s-{t}-{i}"),
                            fields: vec![
                                field("thread", t as i64),
                                field("i", i as i64),
                                field("pad", "x".repeat(40)),
                            ],
                            events: (0..EVENTS)
                                .map(|e| SpanEvent {
                                    level: Level::Info,
                                    message: format!("e-{t}-{i}-{e}"),
                                    fields: vec![field("e", e as i64)],
                                    unix_nanos: START + e as u128,
                                })
                                .collect(),
                            ..request_span()
                        };
                        exporter.export(&span);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let text = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), THREADS * SPANS * (1 + EVENTS));
        let mut seen = std::collections::HashSet::new();
        for block in lines.chunks(1 + EVENTS) {
            let head = block[0];
            let name = head
                .split(" span ")
                .nth(1)
                .and_then(|rest| rest.split(' ').next())
                .unwrap_or_else(|| panic!("not a span line: {head}"));
            let (t, i) = name
                .strip_prefix("s-")
                .and_then(|rest| rest.split_once('-'))
                .unwrap_or_else(|| panic!("bad span name in {head}"));
            assert!(
                head.ends_with(&format!(" thread={t} i={i} pad={}", "x".repeat(40))),
                "{head}"
            );
            for (e, line) in block[1..].iter().enumerate() {
                assert_eq!(*line, format!("  event e-{t}-{i}-{e} +{e}ns e={e}"));
            }
            assert!(
                seen.insert((t.to_string(), i.to_string())),
                "duplicate {head}"
            );
        }
        assert_eq!(seen.len(), THREADS * SPANS);
    }
}
