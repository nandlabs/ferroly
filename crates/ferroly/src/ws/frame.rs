//! RFC 6455 frame encoding/decoding.

use super::rand;

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
}

/// A decoded WebSocket frame.
pub(crate) struct Frame {
    pub fin: bool,
    pub opcode: Opcode,
    pub payload: Vec<u8>,
}

impl Frame {
    /// Parses the opcode byte and mask/length metadata from a two-byte header,
    /// returning `(fin, opcode, masked, length-marker)`.
    pub(crate) fn parse_header(b0: u8, b1: u8) -> Option<(bool, Opcode, bool, u8)> {
        let fin = b0 & 0x80 != 0;
        let opcode = Opcode::from_bits(b0 & 0x0f)?;
        let masked = b1 & 0x80 != 0;
        let len7 = b1 & 0x7f;
        Some((fin, opcode, masked, len7))
    }
}

/// Encodes a frame. `mask` must be true for client→server frames.
pub(crate) fn encode(fin: bool, opcode: Opcode, payload: &[u8], mask: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(if fin { 0x80 } else { 0 } | opcode.bits());

    let mask_bit = if mask { 0x80 } else { 0 };
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

    if mask {
        let mut key = [0u8; 4];
        rand::fill(&mut key);
        out.extend_from_slice(&key);
        for (i, b) in payload.iter().enumerate() {
            out.push(b ^ key[i % 4]);
        }
    } else {
        out.extend_from_slice(payload);
    }
    out
}

/// Applies a masking key in place (used to unmask received frames).
pub(crate) fn apply_mask(payload: &mut [u8], key: [u8; 4]) {
    for (i, b) in payload.iter_mut().enumerate() {
        *b ^= key[i % 4];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_unmasked_text() {
        let bytes = encode(true, Opcode::Text, b"hi", false);
        assert_eq!(bytes[0], 0x81); // FIN + text
        assert_eq!(bytes[1], 2); // len, no mask
        assert_eq!(&bytes[2..], b"hi");
    }

    #[test]
    fn masks_client_frames() {
        let bytes = encode(true, Opcode::Binary, b"\x01\x02\x03", true);
        assert_eq!(bytes[0], 0x82);
        assert_eq!(bytes[1], 0x83); // mask bit + len 3
        let key = [bytes[2], bytes[3], bytes[4], bytes[5]];
        let mut payload = bytes[6..].to_vec();
        apply_mask(&mut payload, key);
        assert_eq!(payload, vec![1, 2, 3]);
    }

    #[test]
    fn parse_header_never_panics_over_all_inputs() {
        // Exhaustively feed every possible first two frame bytes: header parsing
        // must return `Some`/`None`, never panic.
        for b0 in 0u16..=255 {
            for b1 in 0u16..=255 {
                let _ = Frame::parse_header(b0 as u8, b1 as u8);
            }
        }
    }
}
