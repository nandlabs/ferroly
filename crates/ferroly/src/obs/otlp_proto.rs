//! OTLP protobuf encoding of finished spans: an
//! `opentelemetry.proto.collector.trace.v1.ExportTraceServiceRequest`, written
//! by hand with the [`ferroly::proto::wire`] helpers. Used for OTLP/HTTP with
//! `application/x-protobuf` and for OTLP/gRPC.
//!
//! Field numbers (opentelemetry-proto `trace.proto`, `common.proto`,
//! `resource.proto`):
//!
//! | Message | Fields |
//! |---|---|
//! | `ExportTraceServiceRequest` | `resource_spans` 1 |
//! | `ResourceSpans` | `resource` 1, `scope_spans` 2 |
//! | `Resource` | `attributes` 1 |
//! | `ScopeSpans` | `scope` 1, `spans` 2 |
//! | `InstrumentationScope` | `name` 1, `version` 2 |
//! | `Span` | `trace_id` 1, `span_id` 2, `trace_state` 3, `parent_span_id` 4, `name` 5, `kind` 6, `start_time_unix_nano` 7 (fixed64), `end_time_unix_nano` 8 (fixed64), `attributes` 9, `events` 11, `links` 13, `status` 15, `flags` 16 (fixed32) |
//! | `Span.Event` | `time_unix_nano` 1 (fixed64), `name` 2, `attributes` 3 |
//! | `Span.Link` | `trace_id` 1, `span_id` 2, `trace_state` 3, `attributes` 4 |
//! | `Status` | `message` 2, `code` 3 |
//! | `KeyValue` | `key` 1, `value` 2 |
//! | `AnyValue` | `string_value` 1, `bool_value` 2, `int_value` 3, `double_value` 4, `array_value` 5, `kvlist_value` 6, `bytes_value` 7 |
//! | `ArrayValue` / `KeyValueList` | `values` 1 |

use ferroly::codec::Value;
use ferroly::proto::wire::{encode_key, encode_varint, WireType};

use super::otlp::{kind_code, span_flags};
use super::{FinishedSpan, Resource, Status};

/// Appends a length-delimited field holding `bytes`.
fn put_bytes(buf: &mut Vec<u8>, tag: u32, bytes: &[u8]) {
    encode_key(tag, WireType::Len, buf);
    encode_varint(bytes.len() as u64, buf);
    buf.extend_from_slice(bytes);
}

fn put_str(buf: &mut Vec<u8>, tag: u32, text: &str) {
    put_bytes(buf, tag, text.as_bytes());
}

fn put_varint(buf: &mut Vec<u8>, tag: u32, value: u64) {
    encode_key(tag, WireType::Varint, buf);
    encode_varint(value, buf);
}

fn put_fixed64(buf: &mut Vec<u8>, tag: u32, value: u64) {
    encode_key(tag, WireType::I64, buf);
    buf.extend_from_slice(&value.to_le_bytes());
}

fn put_fixed32(buf: &mut Vec<u8>, tag: u32, value: u32) {
    encode_key(tag, WireType::I32, buf);
    buf.extend_from_slice(&value.to_le_bytes());
}

/// Appends an embedded message written by `write`.
fn put_message(buf: &mut Vec<u8>, tag: u32, write: impl FnOnce(&mut Vec<u8>)) {
    let mut inner = Vec::new();
    write(&mut inner);
    put_bytes(buf, tag, &inner);
}

/// Decodes a lowercase or uppercase hex id into its bytes; an id that is not
/// hex (never produced by `obs`) becomes empty.
fn hex_bytes(hex: &str) -> Vec<u8> {
    fn nibble(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let bytes = hex.as_bytes();
    if bytes.len() % 2 != 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        match (nibble(pair[0]), nibble(pair[1])) {
            (Some(hi), Some(lo)) => out.push(hi << 4 | lo),
            _ => return Vec::new(),
        }
    }
    out
}

/// Nanoseconds as the `fixed64` OTLP uses (saturating; `u64` reaches 2554).
fn nanos(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Encodes `spans` as an `ExportTraceServiceRequest`.
pub(super) fn export_request(resource: &Resource, spans: &[FinishedSpan]) -> Vec<u8> {
    let mut out = Vec::new();
    put_message(&mut out, 1, |rs| {
        put_message(rs, 1, |r| {
            for (key, value) in resource.attributes() {
                put_message(r, 1, |kv| key_value(kv, key, value));
            }
        });
        put_message(rs, 2, |ss| {
            put_message(ss, 1, |scope| {
                put_str(scope, 1, "ferroly");
                put_str(scope, 2, env!("CARGO_PKG_VERSION"));
            });
            for span in spans {
                put_message(ss, 2, |s| encode_span(s, span));
            }
        });
    });
    out
}

fn encode_span(buf: &mut Vec<u8>, span: &FinishedSpan) {
    put_bytes(buf, 1, &hex_bytes(&span.trace_id));
    put_bytes(buf, 2, &hex_bytes(&span.span_id));
    if let Some(state) = &span.trace_state {
        put_str(buf, 3, state);
    }
    if let Some(parent) = &span.parent_id {
        put_bytes(buf, 4, &hex_bytes(parent));
    }
    put_str(buf, 5, &span.name);
    put_varint(buf, 6, kind_code(span.kind) as u64);
    let start = nanos(span.start_unix_nanos);
    let end = nanos(span.start_unix_nanos.saturating_add(span.duration_nanos));
    put_fixed64(buf, 7, start);
    put_fixed64(buf, 8, end);
    for (key, value) in &span.fields {
        put_message(buf, 9, |kv| key_value(kv, key, value));
    }
    for event in &span.events {
        put_message(buf, 11, |e| {
            put_fixed64(e, 1, nanos(event.unix_nanos));
            put_str(e, 2, &event.message);
            for (key, value) in &event.fields {
                put_message(e, 3, |kv| key_value(kv, key, value));
            }
        });
    }
    for link in &span.links {
        put_message(buf, 13, |l| {
            put_bytes(l, 1, &hex_bytes(&link.trace_id));
            put_bytes(l, 2, &hex_bytes(&link.span_id));
            if let Some(state) = &link.trace_state {
                put_str(l, 3, state);
            }
            for (key, value) in &link.fields {
                put_message(l, 4, |kv| key_value(kv, key, value));
            }
        });
    }
    // STATUS_CODE_UNSET (0) is the default and is left out.
    match &span.status {
        Status::Unset => {}
        Status::Ok => put_message(buf, 15, |s| put_varint(s, 3, 1)),
        Status::Error(message) => put_message(buf, 15, |s| {
            put_str(s, 2, message);
            put_varint(s, 3, 2);
        }),
    }
    put_fixed32(buf, 16, span_flags(span.parent_remote) as u32);
}

fn key_value(buf: &mut Vec<u8>, key: &str, value: &Value) {
    put_str(buf, 1, key);
    put_message(buf, 2, |v| any_value(v, value));
}

/// Writes the fields of an `AnyValue`. `Null` is an empty `AnyValue`; an
/// unsigned value above `i64::MAX` (which `int_value` cannot hold) is sent as
/// its decimal string.
fn any_value(buf: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => {}
        Value::Str(s) => put_str(buf, 1, s),
        Value::Bool(b) => put_varint(buf, 2, u64::from(*b)),
        Value::Int(i) => put_varint(buf, 3, *i as u64),
        Value::UInt(u) => match i64::try_from(*u) {
            Ok(i) => put_varint(buf, 3, i as u64),
            Err(_) => put_str(buf, 1, &u.to_string()),
        },
        Value::Float(f) => put_fixed64(buf, 4, f.to_bits()),
        Value::Array(items) => put_message(buf, 5, |array| {
            for item in items {
                put_message(array, 1, |v| any_value(v, item));
            }
        }),
        Value::Object(entries) => put_message(buf, 6, |list| {
            for (key, value) in entries {
                put_message(list, 1, |kv| key_value(kv, key, value));
            }
        }),
        Value::Bytes(bytes) => put_bytes(buf, 7, bytes),
    }
}

/// A message carried as already-encoded bytes: the OTLP/gRPC request (the
/// bytes of [`export_request`]) and the response, whose content
/// (`ExportTraceServiceResponse.partial_success`) the exporter does not use.
#[cfg(feature = "grpc")]
#[derive(Debug, Default)]
pub(super) struct RawMessage(pub(super) Vec<u8>);

#[cfg(feature = "grpc")]
impl ferroly::proto::Message for RawMessage {
    fn encode_raw(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.0);
    }

    fn encoded_len(&self) -> usize {
        self.0.len()
    }

    fn merge_from(
        &mut self,
        r: &mut ferroly::proto::wire::Reader<'_>,
    ) -> Result<(), ferroly::proto::ProtoError> {
        // Walk (and so validate) the fields without keeping them.
        while let Some((tag, wire_type)) = r.read_key()? {
            r.skip_field(tag, wire_type)?;
        }
        Ok(())
    }
}

/// The `type_url` of a `google.rpc.RetryInfo` detail.
#[cfg(feature = "grpc")]
const RETRY_INFO_TYPE: &str = "type.googleapis.com/google.rpc.RetryInfo";

/// The delay of the `google.rpc.RetryInfo` among a gRPC status's binary
/// details (`grpc-status-details-bin`, an encoded `google.rpc.Status`), if
/// it carries one. Malformed details yield `None`.
///
/// `google.rpc.Status { details = 3: repeated Any }`,
/// `Any { type_url = 1, value = 2 }`,
/// `RetryInfo { retry_delay = 1: Duration }`,
/// `Duration { seconds = 1: int64, nanos = 2: int32 }`.
#[cfg(feature = "grpc")]
pub(super) fn retry_info_delay(status: &[u8]) -> Option<std::time::Duration> {
    use ferroly::proto::wire::Reader;

    /// Calls `len(tag, bytes)` for every length-delimited field and
    /// `varint(tag, value)` for every varint field, skipping the others.
    fn walk<'a>(
        bytes: &'a [u8],
        mut len: impl FnMut(u32, &'a [u8]),
        mut varint: impl FnMut(u32, u64),
    ) -> Option<()> {
        let mut r = Reader::new(bytes);
        while let Some((tag, wire_type)) = r.read_key().ok()? {
            match wire_type {
                WireType::Len => len(tag, r.read_bytes().ok()?),
                WireType::Varint => varint(tag, r.read_varint().ok()?),
                other => r.skip_field(tag, other).ok()?,
            }
        }
        Some(())
    }

    let mut details = Vec::new();
    walk(
        status,
        |tag, b| {
            if tag == 3 {
                details.push(b);
            }
        },
        |_, _| (),
    )?;
    for any in details {
        let (mut type_url, mut value) = (None, None);
        walk(
            any,
            |tag, b| match tag {
                1 => type_url = Some(b),
                2 => value = Some(b),
                _ => {}
            },
            |_, _| (),
        )?;
        if type_url != Some(RETRY_INFO_TYPE.as_bytes()) {
            continue;
        }
        let mut delay = None;
        walk(
            value.unwrap_or_default(),
            |tag, b| {
                if tag == 1 {
                    delay = Some(b);
                }
            },
            |_, _| (),
        )?;
        let (mut seconds, mut nanos) = (0u64, 0u64);
        walk(
            delay.unwrap_or_default(),
            |_, _| {},
            |tag, v| match tag {
                1 => seconds = v,
                2 => nanos = v,
                _ => {}
            },
        )?;
        // A negative (two's-complement) or out-of-range value is not a delay.
        if seconds > i64::MAX as u64 || nanos >= 1_000_000_000 {
            return None;
        }
        return Some(std::time::Duration::new(seconds, nanos as u32));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::obs::{Level, Link, SpanEvent, SpanKind};
    use ferroly::proto::wire::Reader;

    /// One decoded field: `(tag, wire type, bytes of a Len field, value of a
    /// varint or fixed field)`.
    type Field = (u32, WireType, Vec<u8>, u64);

    /// The fields of one encoded message, in order.
    fn fields(bytes: &[u8]) -> Vec<Field> {
        let mut r = Reader::new(bytes);
        let mut out = Vec::new();
        while let Some((tag, wt)) = r.read_key().unwrap() {
            match wt {
                WireType::Len => out.push((tag, wt, r.read_bytes().unwrap().to_vec(), 0)),
                WireType::Varint => out.push((tag, wt, Vec::new(), r.read_varint().unwrap())),
                WireType::I64 => out.push((tag, wt, Vec::new(), r.read_fixed64().unwrap())),
                WireType::I32 => out.push((tag, wt, Vec::new(), r.read_fixed32().unwrap().into())),
                other => panic!("unexpected wire type {other:?}"),
            }
        }
        out
    }

    fn one(fields: &[Field], tag: u32) -> (Vec<u8>, u64) {
        let mut found = fields.iter().filter(|f| f.0 == tag);
        let f = found.next().unwrap_or_else(|| panic!("no field {tag}"));
        assert!(found.next().is_none(), "field {tag} repeated");
        (f.2.clone(), f.3)
    }

    fn all(fields: &[Field], tag: u32) -> Vec<Vec<u8>> {
        fields
            .iter()
            .filter(|f| f.0 == tag)
            .map(|f| f.2.clone())
            .collect()
    }

    fn text(bytes: Vec<u8>) -> String {
        String::from_utf8(bytes).unwrap()
    }

    /// Decodes a `KeyValue` into `(key, AnyValue fields)`.
    fn kv(bytes: &[u8]) -> (String, Vec<Field>) {
        let f = fields(bytes);
        (text(one(&f, 1).0), fields(&one(&f, 2).0))
    }

    #[test]
    fn export_request_decodes_with_the_otlp_field_numbers() {
        let span = FinishedSpan {
            name: "GET /users".into(),
            trace_id: "0123456789abcdef0123456789abcdef".into(),
            span_id: "0011223344556677".into(),
            parent_id: Some("8899aabbccddeeff".into()),
            parent_remote: true,
            trace_state: Some("vendor=1".into()),
            kind: SpanKind::Server,
            start_unix_nanos: 1_000,
            duration_nanos: 500,
            fields: vec![
                ("http.status".into(), Value::Int(-7)),
                ("route".into(), Value::Str("/x".into())),
                ("ok".into(), Value::Bool(true)),
                ("ratio".into(), Value::Float(1.5)),
                ("big".into(), Value::UInt(u64::MAX)),
                ("raw".into(), Value::Bytes(vec![1, 2])),
                ("list".into(), Value::Array(vec![Value::Int(1)])),
                (
                    "map".into(),
                    Value::Object(vec![("k".into(), Value::Bool(false))]),
                ),
            ],
            events: vec![SpanEvent {
                level: Level::Warn,
                message: "slow".into(),
                fields: vec![("ms".into(), Value::UInt(42))],
                unix_nanos: 1_200,
            }],
            links: vec![Link {
                trace_id: "a".repeat(32),
                span_id: "b".repeat(16),
                trace_state: None,
                fields: Vec::new(),
            }],
            status: Status::error("boom"),
            ..FinishedSpan::default()
        };
        let bytes = export_request(&Resource::new("checkout"), &[span]);

        let request = fields(&bytes);
        let resource_spans = fields(&one(&request, 1).0);
        let resource = fields(&one(&resource_spans, 1).0);
        let (key, value) = kv(&one(&resource, 1).0);
        assert_eq!(key, "service.name");
        assert_eq!(text(one(&value, 1).0), "checkout");

        let scope_spans = fields(&one(&resource_spans, 2).0);
        let scope = fields(&one(&scope_spans, 1).0);
        assert_eq!(text(one(&scope, 1).0), "ferroly");
        assert_eq!(text(one(&scope, 2).0), env!("CARGO_PKG_VERSION"));

        let span = fields(&one(&scope_spans, 2).0);
        assert_eq!(
            one(&span, 1).0,
            hex_bytes("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(one(&span, 1).0.len(), 16);
        assert_eq!(
            one(&span, 2).0,
            vec![0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77]
        );
        assert_eq!(text(one(&span, 3).0), "vendor=1");
        assert_eq!(one(&span, 4).0.len(), 8);
        assert_eq!(text(one(&span, 5).0), "GET /users");
        assert_eq!(one(&span, 6).1, 2); // SPAN_KIND_SERVER
        assert_eq!(one(&span, 7).1, 1_000);
        assert_eq!(one(&span, 8).1, 1_500);
        assert_eq!(one(&span, 16).1, 0x301);
        assert!(span.iter().any(|f| f.0 == 16 && f.1 == WireType::I32));
        assert!(span.iter().any(|f| f.0 == 7 && f.1 == WireType::I64));

        let attrs: Vec<_> = all(&span, 9).iter().map(|b| kv(b)).collect();
        let attr = |name: &str| attrs.iter().find(|(k, _)| k == name).unwrap().1.clone();
        assert_eq!(one(&attr("http.status"), 3).1 as i64, -7);
        assert_eq!(text(one(&attr("route"), 1).0), "/x");
        assert_eq!(one(&attr("ok"), 2).1, 1);
        assert_eq!(f64::from_bits(one(&attr("ratio"), 4).1), 1.5);
        assert_eq!(text(one(&attr("big"), 1).0), u64::MAX.to_string());
        assert_eq!(one(&attr("raw"), 7).0, vec![1, 2]);
        let list = fields(&one(&attr("list"), 5).0);
        assert_eq!(one(&fields(&one(&list, 1).0), 3).1, 1);
        let map = fields(&one(&attr("map"), 6).0);
        assert_eq!(kv(&one(&map, 1).0).0, "k");

        let event = fields(&one(&span, 11).0);
        assert_eq!(one(&event, 1).1, 1_200);
        assert_eq!(text(one(&event, 2).0), "slow");
        assert_eq!(kv(&one(&event, 3).0).0, "ms");

        let link = fields(&one(&span, 13).0);
        assert_eq!(one(&link, 1).0, vec![0xaa; 16]);
        assert_eq!(one(&link, 2).0, vec![0xbb; 8]);

        let status = fields(&one(&span, 15).0);
        assert_eq!(text(one(&status, 2).0), "boom");
        assert_eq!(one(&status, 3).1, 2);
    }

    #[test]
    fn unset_status_and_root_spans_omit_their_fields() {
        let span = FinishedSpan {
            name: "root".into(),
            trace_id: "1".repeat(32),
            span_id: "2".repeat(16),
            ..FinishedSpan::default()
        };
        let bytes = export_request(&Resource::new("svc"), &[span]);
        let request = fields(&bytes);
        let scope_spans = fields(&one(&fields(&one(&request, 1).0), 2).0);
        let span = fields(&one(&scope_spans, 2).0);
        assert!(span.iter().all(|f| f.0 != 3 && f.0 != 4 && f.0 != 15));
        assert_eq!(one(&span, 16).1, 0x101);
        assert_eq!(hex_bytes("zz"), Vec::<u8>::new());
        assert_eq!(hex_bytes("abc"), Vec::<u8>::new());
    }

    /// A `google.rpc.Status` with the given `Any` details.
    #[cfg(feature = "grpc")]
    fn rpc_status(details: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut status = Vec::new();
        put_varint(&mut status, 1, 8);
        put_str(&mut status, 2, "slow down");
        for (type_url, value) in details {
            put_message(&mut status, 3, |any| {
                put_str(any, 1, type_url);
                put_bytes(any, 2, value);
            });
        }
        status
    }

    #[cfg(feature = "grpc")]
    fn retry_info(seconds: u64, nanos: u64) -> Vec<u8> {
        let mut info = Vec::new();
        put_message(&mut info, 1, |d| {
            put_varint(d, 1, seconds);
            put_varint(d, 2, nanos);
        });
        info
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn retry_info_is_found_among_the_status_details() {
        use std::time::Duration;
        let other = (
            "type.googleapis.com/google.rpc.ErrorInfo",
            vec![10, 1, b'x'],
        );
        let status = rpc_status(&[other.clone(), (RETRY_INFO_TYPE, retry_info(2, 500_000_000))]);
        assert_eq!(
            retry_info_delay(&status),
            Some(Duration::from_millis(2_500))
        );
        assert_eq!(retry_info_delay(&rpc_status(&[other])), None);
        assert_eq!(retry_info_delay(&rpc_status(&[])), None);
        // A negative delay, and garbage, are not delays.
        let negative = rpc_status(&[(RETRY_INFO_TYPE, retry_info(u64::MAX, 0))]);
        assert_eq!(retry_info_delay(&negative), None);
        assert_eq!(retry_info_delay(&[0xff, 0xff, 0xff]), None);
    }
}
