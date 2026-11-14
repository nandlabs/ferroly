//! RFC 6455 frame encoding and strict, cancel-safe frame decoding.

use tokio::io::{AsyncRead, AsyncReadExt};

use super::message::close_code;

/// Upper bound on a single buffer reservation, so a frame that *claims* a huge
/// length cannot force a huge allocation before its bytes actually arrive.
const READ_CHUNK: usize = 64 * 1024;
/// Smallest reservation made before a socket read.
const MIN_READ: usize = 4 * 1024;
/// Largest payload a control frame may carry (RFC 6455 §5.5).
pub(crate) const MAX_CONTROL_PAYLOAD: usize = 125;

/// A WebSocket frame opcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Opcode {
    Continuation,
    Text,
    Binary,
    Close,
    Ping,
    Pong,
}

impl Opcode {
    fn from_bits(b: u8) -> Option<Opcode> {
        Some(match b {
            0x0 => Opcode::Continuation,
            0x1 => Opcode::Text,
            0x2 => Opcode::Binary,
            0x8 => Opcode::Close,
            0x9 => Opcode::Ping,
            0xA => Opcode::Pong,
            _ => return None,
        })
    }

    fn bits(self) -> u8 {
        match self {
            Opcode::Continuation => 0x0,
            Opcode::Text => 0x1,
            Opcode::Binary => 0x2,
            Opcode::Close => 0x8,
            Opcode::Ping => 0x9,
            Opcode::Pong => 0xA,
        }
    }

    pub(crate) fn is_control(self) -> bool {
        matches!(self, Opcode::Close | Opcode::Ping | Opcode::Pong)
    }
}

/// A decoded (and unmasked) WebSocket frame.
#[derive(Debug)]
pub(crate) struct Frame {
    pub fin: bool,
    pub opcode: Opcode,
    pub payload: Vec<u8>,
}

/// Why a frame could not be read.
#[derive(Debug)]
pub(crate) enum FrameError {
    /// The peer closed the connection cleanly between frames.
    Eof,
    /// The transport failed (including EOF in the middle of a frame).
    Io(std::io::Error),
    /// The frame violates RFC 6455; answer with close code `1002`.
    Protocol(&'static str),
    /// The frame's payload exceeds the configured cap; answer with `1009`.
    TooBig,
}

/// Validates the two fixed header bytes and returns
/// `(fin, opcode, masked, length-marker)`.
///
/// `expect_masked` is `true` on a server (clients MUST mask, §5.1) and
/// `false` on a client (servers MUST NOT mask). Rejected here, before any
/// payload byte is buffered: reserved bits (no extension is ever negotiated),
/// unknown opcodes, the wrong masking direction, and control frames that are
/// fragmented or longer than 125 bytes.
pub(crate) fn parse_header(
    b0: u8,
    b1: u8,
    expect_masked: bool,
) -> Result<(bool, Opcode, bool, u8), &'static str> {
    if b0 & 0x70 != 0 {
        return Err("reserved bits set without a negotiated extension");
    }
    let fin = b0 & 0x80 != 0;
    let opcode = Opcode::from_bits(b0 & 0x0f).ok_or("unknown opcode")?;
    let masked = b1 & 0x80 != 0;
    if masked != expect_masked {
        return Err(if expect_masked {
            "client frame is not masked"
        } else {
            "server frame is masked"
        });
    }
    let len7 = b1 & 0x7f;
    if opcode.is_control() {
        if !fin {
            return Err("fragmented control frame");
        }
        if len7 as usize > MAX_CONTROL_PAYLOAD {
            return Err("control frame payload longer than 125 bytes");
        }
    }
    Ok((fin, opcode, masked, len7))
}

/// Encodes one frame. `mask` must be `Some` for client-to-server frames and
/// `None` for server-to-client frames.
pub(crate) fn encode(fin: bool, opcode: Opcode, payload: &[u8], mask: Option<[u8; 4]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(if fin { 0x80 } else { 0 } | opcode.bits());

    let mask_bit = if mask.is_some() { 0x80 } else { 0 };
    let len = payload.len();
    if len < 126 {
        out.push(mask_bit | len as u8);
    } else if len <= u16::MAX as usize {
        out.push(mask_bit | 126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(mask_bit | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }

    match mask {
        Some(key) => {
            out.extend_from_slice(&key);
            let start = out.len();
            out.extend_from_slice(payload);
            apply_mask(&mut out[start..], key);
        }
        None => out.extend_from_slice(payload),
    }
    out
}

/// Applies a masking key in place (masking and unmasking are the same XOR).
pub(crate) fn apply_mask(payload: &mut [u8], key: [u8; 4]) {
    for (i, b) in payload.iter_mut().enumerate() {
        *b ^= key[i & 3];
    }
}

/// Builds a close-frame payload. `None` produces an empty payload (no status
/// code). The reason is truncated to fit the 125-byte control-frame limit at
/// a character boundary.
pub(crate) fn close_payload(code: Option<u16>, reason: &str) -> Vec<u8> {
    let Some(code) = code else {
        return Vec::new();
    };
    let mut end = reason.len().min(MAX_CONTROL_PAYLOAD - 2);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = Vec::with_capacity(2 + end);
    out.extend_from_slice(&code.to_be_bytes());
    out.extend_from_slice(&reason.as_bytes()[..end]);
    out
}

/// Validates a received close-frame payload (RFC 6455 §5.5.1, §7.4).
///
/// `Ok(None)` is an empty payload (no status). `Err((code, why))` is the
/// close code to fail the connection with: `1002` for a 1-byte payload or a
/// status code that must not appear on the wire, `1007` for a reason that is
/// not UTF-8.
pub(crate) fn parse_close(payload: &[u8]) -> Result<Option<(u16, String)>, (u16, &'static str)> {
    match payload.len() {
        0 => Ok(None),
        1 => Err((
            close_code::PROTOCOL_ERROR,
            "close frame with a 1-byte payload",
        )),
        _ => {
            let code = u16::from_be_bytes([payload[0], payload[1]]);
            if !close_code::is_sendable(code) {
                return Err((close_code::PROTOCOL_ERROR, "invalid close status code"));
            }
            match std::str::from_utf8(&payload[2..]) {
                Ok(reason) => Ok(Some((code, reason.to_string()))),
                Err(_) => Err((
                    close_code::INVALID_PAYLOAD,
                    "close reason is not valid UTF-8",
                )),
            }
        }
    }
}

/// A frame decoder over the read half of a connection, seeded with any bytes
/// left over from the handshake.
///
/// [`read_frame`](Self::read_frame) is cancel-safe: all progress lives in the
/// internal buffer, so the future can be dropped (for example by a
/// `tokio::select!`) and called again without losing or duplicating bytes.
pub(crate) struct FrameReader<R> {
    inner: R,
    buf: Vec<u8>,
    pos: usize,
    /// How many more bytes the frame being parsed needs.
    want: usize,
    expect_masked: bool,
    max_frame_size: usize,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub(crate) fn new(
        inner: R,
        leftover: Vec<u8>,
        expect_masked: bool,
        max_frame_size: usize,
    ) -> Self {
        Self {
            inner,
            buf: leftover,
            pos: 0,
            want: 0,
            expect_masked,
            max_frame_size,
        }
    }

    /// Reads the next frame. Cancel-safe.
    pub(crate) async fn read_frame(&mut self) -> Result<Frame, FrameError> {
        loop {
            if let Some(frame) = self.try_parse()? {
                return Ok(frame);
            }
            self.fill().await?;
        }
    }

    /// Parses one frame out of the buffer if it is complete. The header is
    /// validated as soon as its bytes are present, before the payload has
    /// arrived, so an oversized or malformed frame is refused without
    /// buffering it.
    fn try_parse(&mut self) -> Result<Option<Frame>, FrameError> {
        let avail = &self.buf[self.pos..];
        if avail.len() < 2 {
            self.want = 2 - avail.len();
            return Ok(None);
        }
        let (fin, opcode, masked, len7) =
            parse_header(avail[0], avail[1], self.expect_masked).map_err(FrameError::Protocol)?;
        let (len, header_len): (u64, usize) = match len7 {
            126 => {
                if avail.len() < 4 {
                    self.want = 4 - avail.len();
                    return Ok(None);
                }
                (u64::from(u16::from_be_bytes([avail[2], avail[3]])), 4)
            }
            127 => {
                if avail.len() < 10 {
                    self.want = 10 - avail.len();
                    return Ok(None);
                }
                let mut raw = [0u8; 8];
                raw.copy_from_slice(&avail[2..10]);
                let raw = u64::from_be_bytes(raw);
                // RFC 6455 §5.2: the most significant bit MUST be 0.
                if raw > i64::MAX as u64 {
                    return Err(FrameError::Protocol(
                        "64-bit frame length has its top bit set",
                    ));
                }
                (raw, 10)
            }
            n => (u64::from(n), 2),
        };
        // Control frames are already limited to 125 bytes by `parse_header`.
        if !opcode.is_control() && len > self.max_frame_size as u64 {
            return Err(FrameError::TooBig);
        }
        let len = len as usize; // fits: it is <= max_frame_size (or <= 125)
        let key_len = if masked { 4 } else { 0 };
        let total = header_len
            .checked_add(key_len)
            .and_then(|n| n.checked_add(len))
            .ok_or(FrameError::TooBig)?;
        if avail.len() < total {
            self.want = total - avail.len();
            return Ok(None);
        }
        let body = self.pos + header_len + key_len;
        let mut payload = self.buf[body..body + len].to_vec();
        if masked {
            let k = &self.buf[body - 4..body];
            apply_mask(&mut payload, [k[0], k[1], k[2], k[3]]);
        }
        self.pos += total;
        if self.pos == self.buf.len() {
            self.pos = 0;
            if self.buf.capacity() > 2 * READ_CHUNK {
                // Do not keep a large frame's buffer for the connection's
                // whole lifetime.
                self.buf = Vec::new();
            } else {
                self.buf.clear();
            }
        }
        Ok(Some(Frame {
            fin,
            opcode,
            payload,
        }))
    }

    /// Reads more bytes into the buffer. Memory grows with bytes actually
    /// received, never with the length a frame merely claims.
    async fn fill(&mut self) -> Result<(), FrameError> {
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.reserve(self.want.clamp(MIN_READ, READ_CHUNK));
        let n = self
            .inner
            .read_buf(&mut self.buf)
            .await
            .map_err(FrameError::Io)?;
        if n == 0 {
            return Err(if self.buf.is_empty() {
                FrameError::Eof
            } else {
                FrameError::Io(std::io::ErrorKind::UnexpectedEof.into())
            });
        }
        Ok(())
    }

    /// Reads and throws away whatever the peer sends next, without parsing
    /// it. Returns the number of bytes dropped (`0` at EOF). Cancel-safe.
    pub(crate) async fn discard(&mut self) -> std::io::Result<usize> {
        self.buf.clear();
        self.pos = 0;
        self.buf.reserve(MIN_READ);
        let n = self.inner.read_buf(&mut self.buf).await?;
        self.buf.clear();
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn round_trips_unmasked_text() {
        let bytes = encode(true, Opcode::Text, b"hi", None);
        assert_eq!(bytes[0], 0x81); // FIN + text
        assert_eq!(bytes[1], 2); // len, no mask
        assert_eq!(&bytes[2..], b"hi");
    }

    #[test]
    fn masks_client_frames() {
        let bytes = encode(true, Opcode::Binary, b"\x01\x02\x03", Some([9, 8, 7, 6]));
        assert_eq!(bytes[0], 0x82);
        assert_eq!(bytes[1], 0x83); // mask bit + len 3
        let key = [bytes[2], bytes[3], bytes[4], bytes[5]];
        assert_eq!(key, [9, 8, 7, 6]);
        let mut payload = bytes[6..].to_vec();
        assert_ne!(payload, vec![1, 2, 3]);
        apply_mask(&mut payload, key);
        assert_eq!(payload, vec![1, 2, 3]);
    }

    #[test]
    fn parse_header_never_panics_and_enforces_the_rules() {
        // Exhaustively feed every possible first two frame bytes.
        for b0 in 0u16..=255 {
            for b1 in 0u16..=255 {
                for expect_masked in [false, true] {
                    let (b0, b1) = (b0 as u8, b1 as u8);
                    let parsed = parse_header(b0, b1, expect_masked);
                    let rsv = b0 & 0x70 != 0;
                    let known = matches!(b0 & 0x0f, 0 | 1 | 2 | 8 | 9 | 10);
                    let control = b0 & 0x08 != 0;
                    let masked = b1 & 0x80 != 0;
                    let bad_control = control && (b0 & 0x80 == 0 || (b1 & 0x7f) > 125);
                    let ok = !rsv && known && masked == expect_masked && !bad_control;
                    assert_eq!(parsed.is_ok(), ok, "b0={b0:#x} b1={b1:#x}");
                }
            }
        }
    }

    #[test]
    fn close_payload_validation() {
        assert_eq!(parse_close(&[]).unwrap(), None);
        assert_eq!(parse_close(&[0x03]).unwrap_err().0, 1002);
        assert_eq!(
            parse_close(&[0x03, 0xE8, b'o', b'k']).unwrap(),
            Some((1000, "ok".to_string()))
        );
        for bad in [0u16, 999, 1004, 1005, 1006, 1015, 1016, 2999, 5000, 65535] {
            assert_eq!(parse_close(&bad.to_be_bytes()).unwrap_err().0, 1002);
        }
        for good in [1000u16, 1001, 1002, 1003, 1007, 1011, 1014, 3000, 4999] {
            assert!(parse_close(&good.to_be_bytes()).is_ok());
        }
        assert_eq!(parse_close(&[0x03, 0xE8, 0xff]).unwrap_err().0, 1007);
    }

    #[test]
    fn close_payload_truncates_at_a_char_boundary() {
        assert!(close_payload(None, "ignored").is_empty());
        let reason = "é".repeat(100); // 200 bytes
        let p = close_payload(Some(1000), &reason);
        assert!(p.len() <= MAX_CONTROL_PAYLOAD);
        assert!(std::str::from_utf8(&p[2..]).is_ok());
    }

    fn reader(bytes: &[u8], expect_masked: bool, cap: usize) -> FrameReader<&[u8]> {
        FrameReader::new(bytes, Vec::new(), expect_masked, cap)
    }

    /// A crafted 64-bit length with the MSB set (RFC 6455 §5.2 violation, an
    /// overflow case) must be rejected, not fed to an allocation.
    #[tokio::test]
    async fn rejects_64bit_length_with_msb_set() {
        let framed = [0x82u8, 0x7F, 0x80, 0, 0, 0, 0, 0, 0, 0];
        let mut r = reader(&framed, false, usize::MAX);
        assert!(matches!(r.read_frame().await, Err(FrameError::Protocol(_))));
    }

    #[tokio::test]
    async fn oversized_frame_is_refused_from_its_header_alone() {
        // Claims 1 MiB but sends no payload: refused without waiting for it.
        let framed = [0x82u8, 0x7F, 0, 0, 0, 0, 0, 0x10, 0, 0];
        let mut r = reader(&framed, false, 1024);
        assert!(matches!(r.read_frame().await, Err(FrameError::TooBig)));
    }

    #[tokio::test]
    async fn eof_between_frames_is_clean_and_mid_frame_is_an_error() {
        let mut r = reader(&[], false, 1024);
        assert!(matches!(r.read_frame().await, Err(FrameError::Eof)));
        let mut r = reader(&[0x81, 0x05, b'h'], false, 1024);
        assert!(matches!(r.read_frame().await, Err(FrameError::Io(_))));
    }

    #[tokio::test]
    async fn a_cancelled_read_keeps_the_bytes_already_buffered() {
        let (mut tx, rx) = tokio::io::duplex(64);
        let mut r = FrameReader::new(rx, Vec::new(), true, 1024);
        let wire = encode(true, Opcode::Text, b"hello world", Some([1, 2, 3, 4]));
        let (first, second) = wire.split_at(5);
        tx.write_all(first).await.unwrap();
        // The frame is incomplete: the read must time out, and dropping the
        // future must not lose the bytes already buffered.
        let pending =
            tokio::time::timeout(std::time::Duration::from_millis(20), r.read_frame()).await;
        assert!(pending.is_err());
        // The rest arrives one byte at a time.
        for byte in second {
            tx.write_all(&[*byte]).await.unwrap();
        }
        let frame = r.read_frame().await.unwrap();
        assert!(frame.fin);
        assert_eq!(frame.opcode, Opcode::Text);
        assert_eq!(frame.payload, b"hello world");
    }

    /// Frame decoding must never panic on arbitrary bytes: feed random,
    /// structurally-biased byte streams and require an `Ok`/`Err` — not a panic.
    #[tokio::test]
    async fn read_frame_never_panics_on_arbitrary_bytes() {
        // Deterministic SplitMix64 so any failure reproduces.
        let mut state = 0xB16B_00B5u64;
        let mut next = || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };

        for round in 0..2000 {
            let len = (next() % 24) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            let mut r = reader(&bytes, round % 2 == 0, 1 << 16);
            // Ok or Err are both acceptable — only a panic/hang would fail.
            while r.read_frame().await.is_ok() {}
        }
    }
}
