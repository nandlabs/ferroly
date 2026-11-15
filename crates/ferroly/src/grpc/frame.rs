//! Length-prefixed message framing: a 1-byte compressed flag, a 4-byte
//! big-endian length, then the message bytes.

use super::status::Status;
use crate::proto::{Limits, Message, ProtoError};

/// The default largest message accepted or sent: 4 MiB (what the reference
/// implementations use).
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;

/// The 5-byte prefix in front of every message.
pub const PREFIX_LEN: usize = 5;

/// Encodes `message` as one length-prefixed frame (uncompressed). Fails with
/// `RESOURCE_EXHAUSTED` when the encoding is larger than `max_size`.
///
/// ```
/// use ferroly::grpc::encode_frame;
///
/// #[derive(ferroly::proto::Message, Default)]
/// struct Ping {
///     #[proto(tag = 1)]
///     n: u32,
/// }
///
/// let frame = encode_frame(&Ping { n: 150 }, 1024).unwrap();
/// assert_eq!(frame, [0, 0, 0, 0, 3, 0x08, 0x96, 0x01]);
/// ```
pub fn encode_frame<M: Message>(message: &M, max_size: usize) -> Result<Vec<u8>, Status> {
    let len = message.encoded_len();
    if len > max_size || u32::try_from(len).is_err() {
        return Err(Status::resource_exhausted(format!(
            "message of {len} bytes is larger than the limit of {max_size} bytes"
        )));
    }
    let mut out = Vec::with_capacity(PREFIX_LEN + len);
    out.push(0);
    out.extend_from_slice(&(len as u32).to_be_bytes());
    message.encode_raw(&mut out);
    Ok(out)
}

/// How many bytes of decoded, in-memory message one byte of a received
/// message may cost, by default: the decoded-memory budget of a message is
/// this multiple of the receive limit (64 MiB for the default 4 MiB). Real
/// messages stay well below it (scalar- and string-heavy ones cost 3–8× their
/// wire size); a stream of empty sub-messages of a wide struct, two bytes each
/// on the wire, is what it stops.
pub(crate) const DECODED_BYTES_PER_BYTE: usize = 16;

/// The decode budget for one received message: at most `max_size` bytes of
/// input, and `max_decoded` (or [`DECODED_BYTES_PER_BYTE`] × `max_size`, at
/// least 1 MiB) bytes of decoded memory.
pub(crate) fn decode_limits(max_size: usize, max_decoded: Option<usize>) -> Limits {
    let decoded = max_decoded.unwrap_or_else(|| {
        max_size
            .saturating_mul(DECODED_BYTES_PER_BYTE)
            .max(1024 * 1024)
    });
    Limits::new()
        .with_max_input_bytes(max_size.max(1))
        .with_max_decoded_bytes(decoded)
}

/// Decodes `bytes` (one message, without the prefix) as `M`, failing with
/// `RESOURCE_EXHAUSTED` when it exceeds the budget and `INTERNAL` on
/// malformed input.
pub(crate) fn decode_message<M: Message>(bytes: &[u8], limits: Limits) -> Result<M, Status> {
    M::decode_with(bytes, limits).map_err(|e| match e {
        ProtoError::LimitExceeded(_) => {
            Status::resource_exhausted(format!("failed to decode the message: {e}"))
        }
        e => Status::internal(format!("failed to decode the message: {e}")),
    })
}

/// Splits a stream of bytes, arriving in chunks of any size, into messages.
///
/// The declared length is checked against the limit as soon as the prefix
/// is complete — before any of the message is buffered — and the buffer only
/// ever holds what has actually arrived, so a hostile length costs nothing.
///
/// ```
/// use ferroly::grpc::FrameDecoder;
///
/// let mut decoder = FrameDecoder::new(1024);
/// // Two frames ("hi" and an empty one), split at awkward places.
/// for chunk in [&[0u8, 0, 0][..], &[0, 2, b'h'], &[b'i', 0, 0, 0, 0, 0]] {
///     decoder.push(chunk);
/// }
/// assert_eq!(decoder.next_frame().unwrap(), Some(b"hi".to_vec()));
/// assert_eq!(decoder.next_frame().unwrap(), Some(Vec::new()));
/// assert_eq!(decoder.next_frame().unwrap(), None);
/// assert!(decoder.finish().is_ok());
///
/// // A length over the limit fails before the body arrives.
/// let mut decoder = FrameDecoder::new(1024);
/// decoder.push(&[0, 0, 0, 0x10, 0]);
/// assert!(decoder.next_frame().is_err());
/// ```
#[derive(Debug)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    start: usize,
    max_size: usize,
    failed: Option<Status>,
    encoding: Encoding,
}

/// A message encoding (`grpc-encoding`) this side can decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encoding {
    /// No compression.
    Identity,
    /// `gzip` (RFC 1952).
    Gzip,
    /// `deflate` (a zlib stream, RFC 1950).
    Deflate,
}

/// What this side advertises in `grpc-accept-encoding`: it decodes these,
/// and always sends `identity`.
pub(crate) const ACCEPT_ENCODING: &str = "identity,gzip,deflate";

impl Encoding {
    /// The encoding a `grpc-encoding` value names, `None` when unsupported.
    pub(crate) fn parse(value: &str) -> Option<Encoding> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("identity") {
            Some(Encoding::Identity)
        } else if value.eq_ignore_ascii_case("gzip") {
            Some(Encoding::Gzip)
        } else if value.eq_ignore_ascii_case("deflate") {
            Some(Encoding::Deflate)
        } else {
            None
        }
    }
}

/// Inflates one compressed message, failing with `RESOURCE_EXHAUSTED` as
/// soon as the output passes `max_size` (so a compression bomb costs at most
/// the limit) and `INTERNAL` when the data is not a complete stream.
fn decompress(encoding: Encoding, data: &[u8], max_size: usize) -> Result<Vec<u8>, Status> {
    use crate::http::inflate::{Decoder, Format};
    let format = match encoding {
        Encoding::Identity => return Ok(data.to_vec()),
        Encoding::Gzip => Format::Gzip,
        Encoding::Deflate => Format::Deflate,
    };
    let bad = |e: &dyn std::fmt::Display| {
        Status::internal(format!("failed to decompress a message: {e}"))
    };
    let mut decoder = Decoder::new(format);
    decoder.feed(data);
    let mut out = Vec::new();
    loop {
        let room = max_size.saturating_sub(out.len()).saturating_add(1);
        let chunk = decoder.decode(room.min(64 * 1024)).map_err(|e| bad(&e))?;
        if chunk.is_empty() {
            break;
        }
        out.extend_from_slice(&chunk);
        if out.len() > max_size {
            return Err(Status::resource_exhausted(format!(
                "decompressed message is larger than the limit of {max_size} bytes"
            )));
        }
    }
    if !decoder.is_complete() || data.is_empty() {
        return Err(bad(&"the compressed data is truncated"));
    }
    Ok(out)
}

impl FrameDecoder {
    /// A decoder accepting messages of at most `max_size` bytes.
    pub fn new(max_size: usize) -> Self {
        FrameDecoder {
            buf: Vec::new(),
            start: 0,
            max_size,
            failed: None,
            encoding: Encoding::Identity,
        }
    }

    /// Decodes messages whose compressed flag is set with `encoding` (the
    /// stream's `grpc-encoding`); with `Identity` a set flag is an error.
    pub(crate) fn with_encoding(mut self, encoding: Encoding) -> Self {
        self.encoding = encoding;
        self
    }

    /// Appends received bytes. Call [`next_frame`](Self::next_frame) until
    /// it returns `None` after each push, so at most one partial message is
    /// ever buffered.
    pub fn push(&mut self, chunk: &[u8]) {
        if self.failed.is_some() || chunk.is_empty() {
            return;
        }
        if self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        } else if self.start > 0 && self.start >= self.buf.len() / 2 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        self.buf.extend_from_slice(chunk);
    }

    /// The next complete message, `None` when more bytes are needed.
    ///
    /// Fails with `RESOURCE_EXHAUSTED` when the declared length exceeds the
    /// limit, and with `INTERNAL` when the compressed flag is set (a decoder
    /// made with [`new`](Self::new) handles only the `identity` encoding) or
    /// is neither 0 nor 1. After an error every call returns the same error.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, Status> {
        if let Some(status) = &self.failed {
            return Err(status.clone());
        }
        let pending = &self.buf[self.start..];
        if pending.len() < PREFIX_LEN {
            return Ok(None);
        }
        let flag = pending[0];
        let len = u32::from_be_bytes([pending[1], pending[2], pending[3], pending[4]]) as usize;
        let error = match flag {
            0 if len > self.max_size => Some(Status::resource_exhausted(format!(
                "received message of {len} bytes is larger than the limit of {} bytes",
                self.max_size
            ))),
            0 => None,
            1 if self.encoding == Encoding::Identity => Some(Status::internal(
                "received a compressed message, but the stream's grpc-encoding \
                 is identity (or absent)",
            )),
            1 if len > self.max_size => Some(Status::resource_exhausted(format!(
                "received compressed message of {len} bytes is larger than the limit of {} bytes",
                self.max_size
            ))),
            1 => None,
            other => Some(Status::internal(format!(
                "invalid compressed flag {other} in a message prefix"
            ))),
        };
        if let Some(status) = error {
            self.failed = Some(status.clone());
            self.buf = Vec::new();
            self.start = 0;
            return Err(status);
        }
        if pending.len() - PREFIX_LEN < len {
            return Ok(None);
        }
        let raw = &pending[PREFIX_LEN..PREFIX_LEN + len];
        let body = if flag == 1 {
            match decompress(self.encoding, raw, self.max_size) {
                Ok(body) => body,
                Err(status) => {
                    self.failed = Some(status.clone());
                    self.buf = Vec::new();
                    self.start = 0;
                    return Err(status);
                }
            }
        } else {
            raw.to_vec()
        };
        self.start += PREFIX_LEN + len;
        Ok(Some(body))
    }

    /// Whether no partial message is buffered.
    pub fn is_empty(&self) -> bool {
        self.start == self.buf.len()
    }

    /// Checks the end of the stream: fails with `INTERNAL` when it stopped
    /// inside a message (or the decoder had already failed).
    pub fn finish(&self) -> Result<(), Status> {
        if let Some(status) = &self.failed {
            return Err(status.clone());
        }
        if self.is_empty() {
            Ok(())
        } else {
            Err(Status::internal(format!(
                "the stream ended inside a message ({} bytes of it received)",
                self.buf.len() - self.start
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grpc::Code;

    fn frame(body: &[u8]) -> Vec<u8> {
        let mut out = vec![0];
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn every_split_point_decodes_the_same() {
        let mut stream = Vec::new();
        let bodies: [&[u8]; 4] = [b"first", b"", b"third message", &[0u8; 300]];
        for b in bodies {
            stream.extend(frame(b));
        }
        for size in 1..stream.len() {
            let mut d = FrameDecoder::new(1024);
            let mut got = Vec::new();
            for chunk in stream.chunks(size) {
                d.push(chunk);
                while let Some(m) = d.next_frame().unwrap() {
                    got.push(m);
                }
            }
            assert!(d.finish().is_ok());
            assert_eq!(got, bodies.iter().map(|b| b.to_vec()).collect::<Vec<_>>());
        }
    }

    #[test]
    fn limits_flags_and_truncation() {
        let mut d = FrameDecoder::new(4);
        d.push(&frame(b"1234"));
        assert_eq!(d.next_frame().unwrap(), Some(b"1234".to_vec()));
        d.push(&frame(b"12345")[..5]);
        let e = d.next_frame().unwrap_err();
        assert_eq!(e.code(), Code::ResourceExhausted);
        assert_eq!(d.next_frame().unwrap_err(), e);
        assert!(d.finish().is_err());

        // u32::MAX length with no body: rejected without allocating.
        let mut d = FrameDecoder::new(DEFAULT_MAX_MESSAGE_SIZE);
        d.push(&[0, 0xff, 0xff, 0xff, 0xff]);
        assert_eq!(d.next_frame().unwrap_err().code(), Code::ResourceExhausted);

        let mut d = FrameDecoder::new(10);
        d.push(&[1, 0, 0, 0, 1, 9]);
        assert_eq!(d.next_frame().unwrap_err().code(), Code::Internal);
        let mut d = FrameDecoder::new(10);
        d.push(&[7, 0, 0, 0, 0]);
        assert_eq!(d.next_frame().unwrap_err().code(), Code::Internal);

        let mut d = FrameDecoder::new(10);
        d.push(&[0, 0, 0]);
        assert_eq!(d.next_frame().unwrap(), None);
        assert_eq!(d.finish().unwrap_err().code(), Code::Internal);
        let mut d = FrameDecoder::new(10);
        d.push(&[0, 0, 0, 0, 3, 1]);
        assert_eq!(d.next_frame().unwrap(), None);
        assert_eq!(d.finish().unwrap_err().code(), Code::Internal);
    }

    #[test]
    fn encode_respects_the_limit() {
        #[derive(crate::proto::Message, Default)]
        struct Blob {
            #[proto(tag = 1)]
            data: Vec<u8>,
        }
        let small = Blob { data: vec![7; 3] };
        assert_eq!(
            encode_frame(&small, 5).unwrap(),
            [0, 0, 0, 0, 5, 10, 3, 7, 7, 7]
        );
        let e = encode_frame(&small, 4).err().unwrap();
        assert_eq!(e.code(), Code::ResourceExhausted);
        let back: Blob = decode_message(&[10, 3, 7, 7, 7], decode_limits(5, None)).unwrap();
        assert_eq!(back.data, [7, 7, 7]);
        assert_eq!(
            decode_message::<Blob>(&[10, 9], decode_limits(5, None))
                .err()
                .unwrap()
                .code(),
            Code::Internal
        );
    }

    /// `data` as a zlib stream of one stored block.
    fn zlib_stored(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x78, 0x01, 1];
        let len = data.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(data);
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in data {
            a = (a + u32::from(byte)) % 65_521;
            b = (b + a) % 65_521;
        }
        out.extend_from_slice(&((b << 16) | a).to_be_bytes());
        out
    }

    fn compressed(body: &[u8]) -> Vec<u8> {
        let mut out = vec![1];
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn compressed_messages_are_inflated_within_the_limit() {
        // `gzip.compress(b"hello gzip world", mtime=0)`.
        let gzip: Vec<u8> = (0..72)
            .step_by(2)
            .map(|i| {
                let hex =
                    "1f8b08000000000002ffcb48cdc9c95748afca2c5028cf2fca4901006b7de8b710000000";
                u8::from_str_radix(&hex[i..i + 2], 16).unwrap()
            })
            .collect();
        let mut d = FrameDecoder::new(64).with_encoding(Encoding::Gzip);
        d.push(&compressed(&gzip));
        d.push(&frame(b"plain"));
        assert_eq!(d.next_frame().unwrap(), Some(b"hello gzip world".to_vec()));
        // An uncompressed frame on a compressed stream is fine too.
        assert_eq!(d.next_frame().unwrap(), Some(b"plain".to_vec()));

        let mut d = FrameDecoder::new(64).with_encoding(Encoding::Deflate);
        d.push(&compressed(&zlib_stored(b"zlib body")));
        assert_eq!(d.next_frame().unwrap(), Some(b"zlib body".to_vec()));

        // Over the limit once inflated: RESOURCE_EXHAUSTED.
        let mut d = FrameDecoder::new(8).with_encoding(Encoding::Deflate);
        d.push(&compressed(&zlib_stored(b"twelve bytes")));
        assert_eq!(d.next_frame().unwrap_err().code(), Code::ResourceExhausted);

        // Truncated or corrupt: INTERNAL.
        let whole = zlib_stored(b"zlib body");
        let mut d = FrameDecoder::new(64).with_encoding(Encoding::Deflate);
        d.push(&compressed(&whole[..whole.len() - 2]));
        assert_eq!(d.next_frame().unwrap_err().code(), Code::Internal);
        let mut bad = whole.clone();
        let n = bad.len();
        bad[n - 1] ^= 1;
        let mut d = FrameDecoder::new(64).with_encoding(Encoding::Deflate);
        d.push(&compressed(&bad));
        assert_eq!(d.next_frame().unwrap_err().code(), Code::Internal);
        let mut d = FrameDecoder::new(64).with_encoding(Encoding::Gzip);
        d.push(&compressed(b""));
        assert_eq!(d.next_frame().unwrap_err().code(), Code::Internal);

        assert_eq!(Encoding::parse(" GZIP "), Some(Encoding::Gzip));
        assert_eq!(Encoding::parse("identity"), Some(Encoding::Identity));
        assert_eq!(Encoding::parse("deflate"), Some(Encoding::Deflate));
        assert_eq!(Encoding::parse("snappy"), None);
    }

    #[test]
    fn decoded_memory_is_budgeted() {
        #[derive(crate::proto::Message, Default)]
        struct Wide {
            #[proto(tag = 1)]
            a: String,
            #[proto(tag = 2)]
            b: String,
            #[proto(tag = 3)]
            c: String,
            #[proto(tag = 4)]
            d: Vec<String>,
        }
        #[derive(crate::proto::Message, Default)]
        struct Many {
            #[proto(tag = 1)]
            items: Vec<Wide>,
        }
        // 8 KiB of empty sub-messages (about 400 KiB decoded) under a 64 KiB
        // message limit: the default budget (at least 1 MiB) takes it, a
        // tight one does not.
        let bytes: Vec<u8> = (0..4 * 1024).flat_map(|_| [0x0a, 0x00]).collect();
        assert!(decode_message::<Many>(&bytes, decode_limits(64 * 1024, None)).is_ok());
        let e = decode_message::<Many>(&bytes, decode_limits(64 * 1024, Some(64 * 1024)))
            .err()
            .unwrap();
        assert_eq!(e.code(), Code::ResourceExhausted);
        assert_eq!(
            decode_limits(4 * 1024 * 1024, None).max_decoded_bytes,
            64 * 1024 * 1024
        );
        assert_eq!(decode_limits(10, None).max_decoded_bytes, 1024 * 1024);
    }
}
