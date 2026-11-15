//! HTTP/2 frame codec (RFC 9113 §4 and §6).
//!
//! [`decode`] turns one frame (header + payload) into a [`Frame`], applying
//! every per-frame rule of RFC 9113 §6: stream-id constraints, fixed payload
//! sizes, padding, setting ranges. A violation is reported as a
//! [`FrameError`] that says whether it is a connection or a stream error and
//! with which code. The `encode_*` functions append frames to an output
//! buffer.

use super::error::Reason;

/// The connection preface a client sends first (RFC 9113 §3.4).
pub(crate) const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
/// Size of the fixed frame header.
pub(crate) const HEADER_LEN: usize = 9;
/// The initial (and smallest allowed) `SETTINGS_MAX_FRAME_SIZE`.
pub(crate) const DEFAULT_MAX_FRAME_SIZE: u32 = 16_384;
/// The largest allowed `SETTINGS_MAX_FRAME_SIZE`.
pub(crate) const MAX_MAX_FRAME_SIZE: u32 = (1 << 24) - 1;
/// The initial flow-control window of a connection and of every stream.
pub(crate) const DEFAULT_WINDOW: u32 = 65_535;
/// The largest flow-control window.
pub(crate) const MAX_WINDOW: u32 = (1 << 31) - 1;
/// Most GOAWAY debug bytes kept from a received frame.
const MAX_DEBUG_DATA: usize = 256;

/// Frame type codes.
pub(crate) mod kind {
    pub(crate) const DATA: u8 = 0x0;
    pub(crate) const HEADERS: u8 = 0x1;
    pub(crate) const PRIORITY: u8 = 0x2;
    pub(crate) const RST_STREAM: u8 = 0x3;
    pub(crate) const SETTINGS: u8 = 0x4;
    pub(crate) const PUSH_PROMISE: u8 = 0x5;
    pub(crate) const PING: u8 = 0x6;
    pub(crate) const GOAWAY: u8 = 0x7;
    pub(crate) const WINDOW_UPDATE: u8 = 0x8;
    pub(crate) const CONTINUATION: u8 = 0x9;
}

/// Frame flag bits.
pub(crate) mod flag {
    pub(crate) const END_STREAM: u8 = 0x1;
    pub(crate) const ACK: u8 = 0x1;
    pub(crate) const END_HEADERS: u8 = 0x4;
    pub(crate) const PADDED: u8 = 0x8;
    pub(crate) const PRIORITY: u8 = 0x20;
}

/// Setting identifiers (RFC 9113 §6.5.2).
pub(crate) mod setting {
    pub(crate) const HEADER_TABLE_SIZE: u16 = 0x1;
    pub(crate) const ENABLE_PUSH: u16 = 0x2;
    pub(crate) const MAX_CONCURRENT_STREAMS: u16 = 0x3;
    pub(crate) const INITIAL_WINDOW_SIZE: u16 = 0x4;
    pub(crate) const MAX_FRAME_SIZE: u16 = 0x5;
    pub(crate) const MAX_HEADER_LIST_SIZE: u16 = 0x6;
}

/// The fixed 9-byte frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Head {
    pub(crate) len: u32,
    pub(crate) kind: u8,
    pub(crate) flags: u8,
    pub(crate) stream: u32,
}

impl Head {
    pub(crate) fn parse(b: &[u8; HEADER_LEN]) -> Head {
        Head {
            len: u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]),
            kind: b[3],
            flags: b[4],
            // The reserved high bit is ignored on receipt.
            stream: u32::from_be_bytes([b[5], b[6], b[7], b[8]]) & 0x7fff_ffff,
        }
    }
}

/// Stream priority information (parsed and validated, otherwise unused:
/// RFC 9113 deprecates the priority scheme).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Priority {
    pub(crate) dependency: u32,
}

/// A decoded frame.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Frame {
    Data {
        stream: u32,
        data: Vec<u8>,
        end_stream: bool,
        /// The whole payload length, padding included: what flow control
        /// counts.
        flow_len: u32,
    },
    /// A HEADERS frame; `block` is this frame's fragment of the header block.
    Headers {
        stream: u32,
        block: Vec<u8>,
        end_stream: bool,
        end_headers: bool,
        priority: Option<Priority>,
    },
    Priority {
        stream: u32,
        priority: Priority,
    },
    RstStream {
        stream: u32,
        code: Reason,
    },
    Settings {
        ack: bool,
        params: Vec<(u16, u32)>,
    },
    PushPromise {
        stream: u32,
    },
    Ping {
        ack: bool,
        data: [u8; 8],
    },
    GoAway {
        last_stream: u32,
        code: Reason,
        debug: Vec<u8>,
    },
    WindowUpdate {
        stream: u32,
        increment: u32,
    },
    Continuation {
        stream: u32,
        block: Vec<u8>,
        end_headers: bool,
    },
    /// A frame of an unknown type, which must be ignored.
    Unknown {
        kind: u8,
    },
}

/// A frame that breaks a rule of RFC 9113.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameError {
    /// The whole connection must be closed with GOAWAY.
    Connection(Reason, &'static str),
    /// Only this stream must be reset.
    Stream(u32, Reason, &'static str),
}

fn conn(reason: Reason, why: &'static str) -> FrameError {
    FrameError::Connection(reason, why)
}

/// Removes the padding of a PADDED frame, leaving `skip` leading bytes
/// (priority fields) in place. Fails when the padding is longer than the
/// payload.
fn strip_padding(payload: &mut Vec<u8>, flags: u8, skip: usize) -> Result<(), FrameError> {
    if flags & flag::PADDED == 0 {
        if payload.len() < skip {
            return Err(conn(Reason::FRAME_SIZE_ERROR, "frame too short"));
        }
        return Ok(());
    }
    let Some(&pad) = payload.first() else {
        return Err(conn(Reason::FRAME_SIZE_ERROR, "padded frame too short"));
    };
    let pad = usize::from(pad);
    // Pad Length + skip + padding must fit in the payload.
    if 1 + skip + pad > payload.len() {
        return Err(conn(Reason::PROTOCOL_ERROR, "padding exceeds the frame"));
    }
    payload.truncate(payload.len() - pad);
    payload.remove(0);
    Ok(())
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Decodes one frame. `payload.len()` must equal `head.len` (the caller has
/// already checked it against the local maximum frame size).
pub(crate) fn decode(head: Head, mut payload: Vec<u8>) -> Result<Frame, FrameError> {
    let stream = head.stream;
    let len = payload.len();
    match head.kind {
        kind::DATA => {
            if stream == 0 {
                return Err(conn(Reason::PROTOCOL_ERROR, "DATA on stream 0"));
            }
            strip_padding(&mut payload, head.flags, 0)?;
            Ok(Frame::Data {
                stream,
                data: payload,
                end_stream: head.flags & flag::END_STREAM != 0,
                flow_len: len as u32,
            })
        }
        kind::HEADERS => {
            if stream == 0 {
                return Err(conn(Reason::PROTOCOL_ERROR, "HEADERS on stream 0"));
            }
            let has_priority = head.flags & flag::PRIORITY != 0;
            let skip = if has_priority { 5 } else { 0 };
            strip_padding(&mut payload, head.flags, skip)?;
            let priority = if has_priority {
                let dependency = be32(&payload[..4]) & 0x7fff_ffff;
                payload.drain(..5);
                Some(Priority { dependency })
            } else {
                None
            };
            Ok(Frame::Headers {
                stream,
                block: payload,
                end_stream: head.flags & flag::END_STREAM != 0,
                end_headers: head.flags & flag::END_HEADERS != 0,
                priority,
            })
        }
        kind::PRIORITY => {
            if stream == 0 {
                return Err(conn(Reason::PROTOCOL_ERROR, "PRIORITY on stream 0"));
            }
            if len != 5 {
                return Err(FrameError::Stream(
                    stream,
                    Reason::FRAME_SIZE_ERROR,
                    "PRIORITY frame of the wrong size",
                ));
            }
            Ok(Frame::Priority {
                stream,
                priority: Priority {
                    dependency: be32(&payload) & 0x7fff_ffff,
                },
            })
        }
        kind::RST_STREAM => {
            if stream == 0 {
                return Err(conn(Reason::PROTOCOL_ERROR, "RST_STREAM on stream 0"));
            }
            if len != 4 {
                return Err(conn(
                    Reason::FRAME_SIZE_ERROR,
                    "RST_STREAM of the wrong size",
                ));
            }
            Ok(Frame::RstStream {
                stream,
                code: Reason(be32(&payload)),
            })
        }
        kind::SETTINGS => {
            if stream != 0 {
                return Err(conn(Reason::PROTOCOL_ERROR, "SETTINGS on a stream"));
            }
            let ack = head.flags & flag::ACK != 0;
            if ack {
                if len != 0 {
                    return Err(conn(
                        Reason::FRAME_SIZE_ERROR,
                        "SETTINGS ACK with a payload",
                    ));
                }
                return Ok(Frame::Settings {
                    ack,
                    params: Vec::new(),
                });
            }
            if len % 6 != 0 {
                return Err(conn(Reason::FRAME_SIZE_ERROR, "SETTINGS of the wrong size"));
            }
            let mut params = Vec::with_capacity(len / 6);
            for p in payload.chunks_exact(6) {
                let id = u16::from_be_bytes([p[0], p[1]]);
                let value = be32(&p[2..]);
                match id {
                    setting::ENABLE_PUSH if value > 1 => {
                        return Err(conn(Reason::PROTOCOL_ERROR, "invalid SETTINGS_ENABLE_PUSH"))
                    }
                    setting::INITIAL_WINDOW_SIZE if value > MAX_WINDOW => {
                        return Err(conn(
                            Reason::FLOW_CONTROL_ERROR,
                            "SETTINGS_INITIAL_WINDOW_SIZE too large",
                        ))
                    }
                    setting::MAX_FRAME_SIZE
                        if !(DEFAULT_MAX_FRAME_SIZE..=MAX_MAX_FRAME_SIZE).contains(&value) =>
                    {
                        return Err(conn(
                            Reason::PROTOCOL_ERROR,
                            "invalid SETTINGS_MAX_FRAME_SIZE",
                        ))
                    }
                    _ => {}
                }
                params.push((id, value));
            }
            Ok(Frame::Settings { ack, params })
        }
        kind::PUSH_PROMISE => {
            if stream == 0 {
                return Err(conn(Reason::PROTOCOL_ERROR, "PUSH_PROMISE on stream 0"));
            }
            Ok(Frame::PushPromise { stream })
        }
        kind::PING => {
            if stream != 0 {
                return Err(conn(Reason::PROTOCOL_ERROR, "PING on a stream"));
            }
            if len != 8 {
                return Err(conn(Reason::FRAME_SIZE_ERROR, "PING of the wrong size"));
            }
            let mut data = [0u8; 8];
            data.copy_from_slice(&payload);
            Ok(Frame::Ping {
                ack: head.flags & flag::ACK != 0,
                data,
            })
        }
        kind::GOAWAY => {
            if stream != 0 {
                return Err(conn(Reason::PROTOCOL_ERROR, "GOAWAY on a stream"));
            }
            if len < 8 {
                return Err(conn(Reason::FRAME_SIZE_ERROR, "GOAWAY too short"));
            }
            let last_stream = be32(&payload) & 0x7fff_ffff;
            let code = Reason(be32(&payload[4..]));
            let end = len.min(8 + MAX_DEBUG_DATA);
            Ok(Frame::GoAway {
                last_stream,
                code,
                debug: payload[8..end].to_vec(),
            })
        }
        kind::WINDOW_UPDATE => {
            if len != 4 {
                return Err(conn(
                    Reason::FRAME_SIZE_ERROR,
                    "WINDOW_UPDATE of the wrong size",
                ));
            }
            let increment = be32(&payload) & 0x7fff_ffff;
            if increment == 0 {
                return Err(if stream == 0 {
                    conn(Reason::PROTOCOL_ERROR, "zero WINDOW_UPDATE increment")
                } else {
                    FrameError::Stream(
                        stream,
                        Reason::PROTOCOL_ERROR,
                        "zero WINDOW_UPDATE increment",
                    )
                });
            }
            Ok(Frame::WindowUpdate { stream, increment })
        }
        kind::CONTINUATION => {
            if stream == 0 {
                return Err(conn(Reason::PROTOCOL_ERROR, "CONTINUATION on stream 0"));
            }
            Ok(Frame::Continuation {
                stream,
                block: payload,
                end_headers: head.flags & flag::END_HEADERS != 0,
            })
        }
        other => Ok(Frame::Unknown { kind: other }),
    }
}

/// Appends a frame header.
pub(crate) fn write_head(out: &mut Vec<u8>, len: usize, kind: u8, flags: u8, stream: u32) {
    debug_assert!(len <= MAX_MAX_FRAME_SIZE as usize);
    out.extend_from_slice(&[(len >> 16) as u8, (len >> 8) as u8, len as u8, kind, flags]);
    out.extend_from_slice(&(stream & 0x7fff_ffff).to_be_bytes());
}

pub(crate) fn encode_settings(out: &mut Vec<u8>, params: &[(u16, u32)]) {
    write_head(out, params.len() * 6, kind::SETTINGS, 0, 0);
    for (id, value) in params {
        out.extend_from_slice(&id.to_be_bytes());
        out.extend_from_slice(&value.to_be_bytes());
    }
}

pub(crate) fn encode_settings_ack(out: &mut Vec<u8>) {
    write_head(out, 0, kind::SETTINGS, flag::ACK, 0);
}

pub(crate) fn encode_ping(out: &mut Vec<u8>, ack: bool, data: [u8; 8]) {
    write_head(out, 8, kind::PING, if ack { flag::ACK } else { 0 }, 0);
    out.extend_from_slice(&data);
}

pub(crate) fn encode_goaway(out: &mut Vec<u8>, last_stream: u32, code: Reason, debug: &[u8]) {
    write_head(out, 8 + debug.len(), kind::GOAWAY, 0, 0);
    out.extend_from_slice(&(last_stream & 0x7fff_ffff).to_be_bytes());
    out.extend_from_slice(&code.0.to_be_bytes());
    out.extend_from_slice(debug);
}

pub(crate) fn encode_rst(out: &mut Vec<u8>, stream: u32, code: Reason) {
    write_head(out, 4, kind::RST_STREAM, 0, stream);
    out.extend_from_slice(&code.0.to_be_bytes());
}

pub(crate) fn encode_window_update(out: &mut Vec<u8>, stream: u32, increment: u32) {
    write_head(out, 4, kind::WINDOW_UPDATE, 0, stream);
    out.extend_from_slice(&(increment & 0x7fff_ffff).to_be_bytes());
}

pub(crate) fn encode_data(out: &mut Vec<u8>, stream: u32, data: &[u8], end_stream: bool) {
    let flags = if end_stream { flag::END_STREAM } else { 0 };
    write_head(out, data.len(), kind::DATA, flags, stream);
    out.extend_from_slice(data);
}

/// Appends a header block as HEADERS plus as many CONTINUATION frames as
/// `max_frame` requires.
pub(crate) fn encode_headers(
    out: &mut Vec<u8>,
    stream: u32,
    block: &[u8],
    end_stream: bool,
    max_frame: usize,
) {
    let max_frame = max_frame.max(1);
    let mut chunks = block.chunks(max_frame);
    let first = chunks.next().unwrap_or(&[]);
    let mut rest = chunks.peekable();
    let mut flags = if end_stream { flag::END_STREAM } else { 0 };
    if rest.peek().is_none() {
        flags |= flag::END_HEADERS;
    }
    write_head(out, first.len(), kind::HEADERS, flags, stream);
    out.extend_from_slice(first);
    while let Some(chunk) = rest.next() {
        let flags = if rest.peek().is_none() {
            flag::END_HEADERS
        } else {
            0
        };
        write_head(out, chunk.len(), kind::CONTINUATION, flags, stream);
        out.extend_from_slice(chunk);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(bytes: &[u8]) -> (Head, Vec<u8>) {
        let mut h = [0u8; HEADER_LEN];
        h.copy_from_slice(&bytes[..HEADER_LEN]);
        let head = Head::parse(&h);
        assert_eq!(head.len as usize, bytes.len() - HEADER_LEN);
        (head, bytes[HEADER_LEN..].to_vec())
    }

    fn round(bytes: &[u8]) -> Result<Frame, FrameError> {
        let (head, payload) = split(bytes);
        decode(head, payload)
    }

    fn raw(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        write_head(&mut out, payload.len(), kind, flags, stream);
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn encodes_and_decodes_every_frame_type() {
        let mut out = Vec::new();
        encode_data(&mut out, 1, b"hello", true);
        assert_eq!(
            round(&out).unwrap(),
            Frame::Data {
                stream: 1,
                data: b"hello".to_vec(),
                end_stream: true,
                flow_len: 5
            }
        );

        out.clear();
        encode_headers(&mut out, 3, b"abc", false, 16_384);
        assert_eq!(
            round(&out).unwrap(),
            Frame::Headers {
                stream: 3,
                block: b"abc".to_vec(),
                end_stream: false,
                end_headers: true,
                priority: None
            }
        );

        out.clear();
        encode_settings(&mut out, &[(setting::MAX_CONCURRENT_STREAMS, 7)]);
        assert_eq!(
            round(&out).unwrap(),
            Frame::Settings {
                ack: false,
                params: vec![(3, 7)]
            }
        );
        out.clear();
        encode_settings_ack(&mut out);
        assert_eq!(
            round(&out).unwrap(),
            Frame::Settings {
                ack: true,
                params: vec![]
            }
        );

        out.clear();
        encode_ping(&mut out, true, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            round(&out).unwrap(),
            Frame::Ping {
                ack: true,
                data: [1, 2, 3, 4, 5, 6, 7, 8]
            }
        );

        out.clear();
        encode_goaway(&mut out, 5, Reason::ENHANCE_YOUR_CALM, b"calm");
        assert_eq!(
            round(&out).unwrap(),
            Frame::GoAway {
                last_stream: 5,
                code: Reason::ENHANCE_YOUR_CALM,
                debug: b"calm".to_vec()
            }
        );

        out.clear();
        encode_rst(&mut out, 9, Reason::CANCEL);
        assert_eq!(
            round(&out).unwrap(),
            Frame::RstStream {
                stream: 9,
                code: Reason::CANCEL
            }
        );

        out.clear();
        encode_window_update(&mut out, 0, 1000);
        assert_eq!(
            round(&out).unwrap(),
            Frame::WindowUpdate {
                stream: 0,
                increment: 1000
            }
        );

        let prio = raw(kind::PRIORITY, 0, 5, &[0x80, 0, 0, 3, 16]);
        assert_eq!(
            round(&prio).unwrap(),
            Frame::Priority {
                stream: 5,
                priority: Priority { dependency: 3 }
            }
        );
        let push = raw(kind::PUSH_PROMISE, 0, 1, &[0, 0, 0, 2]);
        assert_eq!(round(&push).unwrap(), Frame::PushPromise { stream: 1 });
        let cont = raw(kind::CONTINUATION, flag::END_HEADERS, 1, b"xy");
        assert_eq!(
            round(&cont).unwrap(),
            Frame::Continuation {
                stream: 1,
                block: b"xy".to_vec(),
                end_headers: true
            }
        );
        assert_eq!(
            round(&raw(0x42, 0, 0, b"??")).unwrap(),
            Frame::Unknown { kind: 0x42 }
        );
    }

    #[test]
    fn padding_and_priority_are_stripped() {
        // PADDED DATA: pad length 3, "hi", 3 pad bytes.
        let data = raw(kind::DATA, flag::PADDED, 1, &[3, b'h', b'i', 0, 0, 0]);
        assert_eq!(
            round(&data).unwrap(),
            Frame::Data {
                stream: 1,
                data: b"hi".to_vec(),
                end_stream: false,
                flow_len: 6
            }
        );
        // PADDED + PRIORITY HEADERS.
        let flags = flag::PADDED | flag::PRIORITY | flag::END_HEADERS;
        let payload = [1, 0, 0, 0, 7, 255, b'z', 0];
        let headers = raw(kind::HEADERS, flags, 9, &payload);
        assert_eq!(
            round(&headers).unwrap(),
            Frame::Headers {
                stream: 9,
                block: b"z".to_vec(),
                end_stream: false,
                end_headers: true,
                priority: Some(Priority { dependency: 7 })
            }
        );
        // Padding as long as the payload is an error.
        let bad = raw(kind::DATA, flag::PADDED, 1, &[5, 1, 2]);
        assert!(matches!(
            round(&bad),
            Err(FrameError::Connection(Reason::PROTOCOL_ERROR, _))
        ));
        let empty = raw(kind::DATA, flag::PADDED, 1, &[]);
        assert!(round(&empty).is_err());
        let short = raw(kind::HEADERS, flag::PRIORITY, 1, &[0, 0]);
        assert!(round(&short).is_err());
    }

    #[test]
    fn enforces_per_frame_rules() {
        let is_conn = |r: Result<Frame, FrameError>, want: Reason| matches!(r, Err(FrameError::Connection(code, _)) if code == want);
        let p = Reason::PROTOCOL_ERROR;
        let f = Reason::FRAME_SIZE_ERROR;
        assert!(is_conn(round(&raw(kind::DATA, 0, 0, b"x")), p));
        assert!(is_conn(round(&raw(kind::HEADERS, 0, 0, b"x")), p));
        assert!(is_conn(round(&raw(kind::PRIORITY, 0, 0, &[0; 5])), p));
        assert!(matches!(
            round(&raw(kind::PRIORITY, 0, 1, &[0; 4])),
            Err(FrameError::Stream(1, Reason::FRAME_SIZE_ERROR, _))
        ));
        assert!(is_conn(round(&raw(kind::RST_STREAM, 0, 0, &[0; 4])), p));
        assert!(is_conn(round(&raw(kind::RST_STREAM, 0, 1, &[0; 3])), f));
        assert!(is_conn(round(&raw(kind::SETTINGS, 0, 1, &[])), p));
        assert!(is_conn(
            round(&raw(kind::SETTINGS, flag::ACK, 0, &[0; 6])),
            f
        ));
        assert!(is_conn(round(&raw(kind::SETTINGS, 0, 0, &[0; 5])), f));
        assert!(is_conn(
            round(&raw(kind::SETTINGS, 0, 0, &[0, 2, 0, 0, 0, 2])),
            p
        ));
        assert!(is_conn(
            round(&raw(kind::SETTINGS, 0, 0, &[0, 4, 0x80, 0, 0, 0])),
            Reason::FLOW_CONTROL_ERROR
        ));
        assert!(is_conn(
            round(&raw(kind::SETTINGS, 0, 0, &[0, 5, 0, 0, 0x10, 0])),
            p
        ));
        assert!(is_conn(
            round(&raw(kind::SETTINGS, 0, 0, &[0, 5, 1, 0, 0, 0])),
            p
        ));
        // Unknown settings are ignored.
        assert!(round(&raw(kind::SETTINGS, 0, 0, &[0, 0x99, 0, 0, 0, 1])).is_ok());
        assert!(is_conn(round(&raw(kind::PUSH_PROMISE, 0, 0, &[0; 4])), p));
        assert!(is_conn(round(&raw(kind::PING, 0, 1, &[0; 8])), p));
        assert!(is_conn(round(&raw(kind::PING, 0, 0, &[0; 7])), f));
        assert!(is_conn(round(&raw(kind::GOAWAY, 0, 1, &[0; 8])), p));
        assert!(is_conn(round(&raw(kind::GOAWAY, 0, 0, &[0; 7])), f));
        assert!(is_conn(round(&raw(kind::WINDOW_UPDATE, 0, 0, &[0; 3])), f));
        assert!(is_conn(round(&raw(kind::WINDOW_UPDATE, 0, 0, &[0; 4])), p));
        assert!(matches!(
            round(&raw(kind::WINDOW_UPDATE, 0, 3, &[0; 4])),
            Err(FrameError::Stream(3, Reason::PROTOCOL_ERROR, _))
        ));
        assert!(is_conn(round(&raw(kind::CONTINUATION, 0, 0, b"x")), p));
        // A GOAWAY's debug data is truncated, not kept whole.
        let mut big = vec![0u8; 8];
        big.extend(std::iter::repeat(b'd').take(10_000));
        match round(&raw(kind::GOAWAY, 0, 0, &big)).unwrap() {
            Frame::GoAway { debug, .. } => assert_eq!(debug.len(), MAX_DEBUG_DATA),
            other => panic!("{other:?}"),
        }
        // The reserved bit of the stream id is ignored.
        let (head, _) = split(&raw(kind::DATA, 0, 0, b""));
        assert_eq!(head.stream, 0);
        let mut b = raw(kind::DATA, 0, 1, b"");
        b[5] |= 0x80;
        assert_eq!(split(&b).0.stream, 1);
    }

    #[test]
    fn header_blocks_split_into_continuations() {
        let block: Vec<u8> = (0..100u8).collect();
        let mut out = Vec::new();
        encode_headers(&mut out, 1, &block, true, 40);
        // HEADERS(40) + CONTINUATION(40) + CONTINUATION(20).
        assert_eq!(out.len(), 3 * HEADER_LEN + 100);
        let (h1, _) = split(&out[..HEADER_LEN + 40]);
        assert_eq!((h1.kind, h1.flags), (kind::HEADERS, flag::END_STREAM));
        let (h2, _) = split(&out[HEADER_LEN + 40..2 * (HEADER_LEN + 40)]);
        assert_eq!((h2.kind, h2.flags), (kind::CONTINUATION, 0));
        let (h3, _) = split(&out[2 * (HEADER_LEN + 40)..]);
        assert_eq!((h3.kind, h3.flags), (kind::CONTINUATION, flag::END_HEADERS));
        // An empty block is one HEADERS frame.
        out.clear();
        encode_headers(&mut out, 1, &[], false, 40);
        assert_eq!(out.len(), HEADER_LEN);
    }
}
