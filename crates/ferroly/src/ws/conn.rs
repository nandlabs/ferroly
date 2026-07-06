//! Connection read/write loops shared by the WebSocket client and server.

use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use ferroly::http::Conn;

use super::frame::{self, apply_mask, Frame, Opcode};
use super::Message;

/// Upper bound on a single socket read / initial buffer reservation, so a frame
/// that *claims* a huge length cannot force a huge allocation before its bytes
/// actually arrive.
const READ_CHUNK: usize = 64 * 1024;

/// A message queued to be written to the peer.
pub(crate) enum Outgoing {
    Msg(Message),
    Pong(Vec<u8>),
    Close,
}

/// A buffered reader over a connection's read half, seeded with any bytes left
/// over from the handshake.
pub(crate) struct Reader {
    inner: ReadHalf<Conn>,
    buf: Vec<u8>,
    pos: usize,
    /// Reject any single frame whose payload exceeds this, if set.
    max_frame_size: Option<usize>,
}

impl Reader {
    pub(crate) fn new(
        inner: ReadHalf<Conn>,
        leftover: Vec<u8>,
        max_frame_size: Option<usize>,
    ) -> Self {
        Self {
            inner,
            buf: leftover,
            pos: 0,
            max_frame_size,
        }
    }

    async fn read_exact(&mut self, n: usize) -> std::io::Result<Vec<u8>> {
        // Reserve only a bounded amount up front: a frame may *claim* a huge
        // length, but memory must grow with bytes actually received, never with
        // the attacker-controlled claimed size.
        let mut out = Vec::with_capacity(n.min(READ_CHUNK));
        while out.len() < n {
            if self.pos < self.buf.len() {
                let take = (n - out.len()).min(self.buf.len() - self.pos);
                out.extend_from_slice(&self.buf[self.pos..self.pos + take]);
                self.pos += take;
            } else {
                let mut tmp = vec![0u8; (n - out.len()).min(READ_CHUNK)];
                let read = self.inner.read(&mut tmp).await?;
                if read == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "eof",
                    ));
                }
                out.extend_from_slice(&tmp[..read]);
            }
        }
        Ok(out)
    }

    async fn read_frame(&mut self) -> std::io::Result<Frame> {
        let hdr = self.read_exact(2).await?;
        let (fin, opcode, masked, len7) = Frame::parse_header(hdr[0], hdr[1])
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad opcode"))?;
        let len = match len7 {
            126 => {
                let b = self.read_exact(2).await?;
                u16::from_be_bytes([b[0], b[1]]) as usize
            }
            127 => {
                let b = self.read_exact(8).await?;
                let raw = u64::from_be_bytes(b.try_into().unwrap());
                // RFC 6455 §5.2: the most-significant bit of a 64-bit payload
                // length MUST be 0. Rejecting it also guards against usize
                // overflow (32-bit) and absurd allocation requests.
                if raw > i64::MAX as u64 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "frame length exceeds maximum",
                    ));
                }
                usize::try_from(raw).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "frame length exceeds usize",
                    )
                })?
            }
            n => n as usize,
        };
        if let Some(max) = self.max_frame_size {
            if len > max {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "frame exceeds max_frame_size",
                ));
            }
        }
        let mask_key = if masked {
            let k = self.read_exact(4).await?;
            Some([k[0], k[1], k[2], k[3]])
        } else {
            None
        };
        let mut payload = self.read_exact(len).await?;
        if let Some(key) = mask_key {
            apply_mask(&mut payload, key);
        }
        Ok(Frame {
            fin,
            opcode,
            payload,
        })
    }
}

/// Reads frames, reassembles fragmented messages, answers pings, and forwards
/// application messages to `in_tx`. Ends on close or transport error.
pub(crate) async fn read_loop(
    mut reader: Reader,
    in_tx: UnboundedSender<Message>,
    out_tx: UnboundedSender<Outgoing>,
    max_message_size: Option<usize>,
) {
    let mut frag: Option<(Opcode, Vec<u8>)> = None;
    loop {
        let frame = match reader.read_frame().await {
            Ok(f) => f,
            Err(_) => break,
        };
        match frame.opcode {
            Opcode::Ping => {
                let _ = out_tx.send(Outgoing::Pong(frame.payload));
            }
            Opcode::Pong => {}
            Opcode::Close => {
                let _ = out_tx.send(Outgoing::Close);
                break;
            }
            Opcode::Text | Opcode::Binary => {
                if frame.fin {
                    if exceeds(max_message_size, frame.payload.len())
                        || !emit(&in_tx, frame.opcode, frame.payload)
                    {
                        break;
                    }
                } else {
                    frag = Some((frame.opcode, frame.payload));
                }
            }
            Opcode::Continuation => {
                if let Some((_, buf)) = frag.as_mut() {
                    buf.extend_from_slice(&frame.payload);
                    if exceeds(max_message_size, buf.len()) {
                        break;
                    }
                    if frame.fin {
                        let (op, buf) = frag.take().unwrap();
                        if !emit(&in_tx, op, buf) {
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// `true` if a size cap is set and `len` exceeds it.
fn exceeds(max: Option<usize>, len: usize) -> bool {
    matches!(max, Some(m) if len > m)
}

/// Forwards a reassembled message; returns `false` if the receiver is gone.
fn emit(in_tx: &UnboundedSender<Message>, opcode: Opcode, payload: Vec<u8>) -> bool {
    let msg = match opcode {
        Opcode::Text => Message::Text(String::from_utf8_lossy(&payload).into_owned()),
        _ => Message::Binary(payload),
    };
    in_tx.send(msg).is_ok()
}

/// Drains outgoing messages, encoding frames (masking client→server frames).
pub(crate) async fn write_loop(
    mut writer: WriteHalf<Conn>,
    mut out_rx: UnboundedReceiver<Outgoing>,
    mask: bool,
) {
    while let Some(msg) = out_rx.recv().await {
        let bytes = match msg {
            Outgoing::Msg(Message::Text(t)) => {
                frame::encode(true, Opcode::Text, t.as_bytes(), mask)
            }
            Outgoing::Msg(Message::Binary(b)) => frame::encode(true, Opcode::Binary, &b, mask),
            Outgoing::Pong(p) => frame::encode(true, Opcode::Pong, &p, mask),
            Outgoing::Close => {
                let c = frame::encode(true, Opcode::Close, &[], mask);
                let _ = writer.write_all(&c).await;
                let _ = writer.flush().await;
                break;
            }
        };
        if writer.write_all(&bytes).await.is_err() || writer.flush().await.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// A crafted 64-bit length with the MSB set (RFC 6455 §5.2 violation, an
    /// overflow case) must be rejected, not fed to an allocation.
    #[tokio::test]
    async fn rejects_64bit_length_with_msb_set() {
        let (mut writer, server_side) = tokio::io::duplex(64);
        // FIN + text (0x81), extended-127 length (0x7F, unmasked), then an
        // 8-byte length of 0x8000_0000_0000_0000 — MSB set.
        let framed = [0x81u8, 0x7F, 0x80, 0, 0, 0, 0, 0, 0, 0];
        writer.write_all(&framed).await.unwrap();

        let conn: ferroly::http::Conn = Box::new(server_side);
        let (read_half, _write_half) = tokio::io::split(conn);
        let mut reader = Reader::new(read_half, Vec::new(), None);
        match reader.read_frame().await {
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidData),
            Ok(_) => panic!("oversize frame length was not rejected"),
        }
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

        for _ in 0..2000 {
            let len = (next() % 24) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            let (mut writer, server_side) = tokio::io::duplex(64);
            writer.write_all(&bytes).await.unwrap();
            drop(writer); // close the write half so reads hit EOF, never hang

            let conn: ferroly::http::Conn = Box::new(server_side);
            let (read_half, _write_half) = tokio::io::split(conn);
            // A frame-size cap so an absurd claimed length is rejected promptly.
            let mut reader = Reader::new(read_half, Vec::new(), Some(1 << 16));
            // Ok or Err are both acceptable — only a panic/hang would fail here.
            let _ = reader.read_frame().await;
        }
    }
}
