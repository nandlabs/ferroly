//! Hexadecimal (base16) encoding and decoding.
//!
//! ```
//! use ferroly::hash::hex;
//!
//! assert_eq!(hex::encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
//! assert_eq!(hex::encode_upper(&[0xde, 0xad]), "DEAD");
//! assert_eq!(hex::decode("DEADbeef").unwrap(), [0xde, 0xad, 0xbe, 0xef]);
//! assert!(hex::decode("abc").is_err()); // odd length
//! ```

use std::fmt;

/// Why a hex input was rejected. Never carries any of the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeError {
    /// The input has an odd number of characters.
    OddLength,
    /// A byte that is not `0-9`, `a-f` or `A-F`, at the given byte offset.
    InvalidCharacter {
        /// Byte offset of the offending character.
        index: usize,
    },
    /// The decoded length does not match the fixed-size destination.
    InvalidLength,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::OddLength => f.write_str("hex input has an odd length"),
            DecodeError::InvalidCharacter { index } => {
                write!(f, "invalid hex character at offset {index}")
            }
            DecodeError::InvalidLength => f.write_str("hex input has the wrong length"),
        }
    }
}

impl std::error::Error for DecodeError {}

const LOWER: &[u8; 16] = b"0123456789abcdef";
const UPPER: &[u8; 16] = b"0123456789ABCDEF";

fn encode_using(bytes: &[u8], digits: &[u8; 16]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(digits[(b >> 4) as usize] as char);
        out.push(digits[(b & 0x0f) as usize] as char);
    }
    out
}

/// Encodes `bytes` as lowercase hex (two characters per byte).
pub fn encode(bytes: &[u8]) -> String {
    encode_using(bytes, LOWER)
}

/// Encodes `bytes` as uppercase hex.
pub fn encode_upper(bytes: &[u8]) -> String {
    encode_using(bytes, UPPER)
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Decodes a hex string (either case, no prefix, no separators).
pub fn decode(input: impl AsRef<[u8]>) -> Result<Vec<u8>, DecodeError> {
    let input = input.as_ref();
    let mut out = vec![0u8; input.len() / 2];
    decode_to_slice(input, &mut out)?;
    Ok(out)
}

/// Decodes a hex string into `out`, which must be exactly half as long as the
/// input.
pub fn decode_to_slice(input: impl AsRef<[u8]>, out: &mut [u8]) -> Result<(), DecodeError> {
    let input = input.as_ref();
    if input.len() % 2 != 0 {
        return Err(DecodeError::OddLength);
    }
    if input.len() / 2 != out.len() {
        return Err(DecodeError::InvalidLength);
    }
    for (i, (pair, slot)) in input.chunks_exact(2).zip(out.iter_mut()).enumerate() {
        let hi = nibble(pair[0]).ok_or(DecodeError::InvalidCharacter { index: i * 2 })?;
        let lo = nibble(pair[1]).ok_or(DecodeError::InvalidCharacter { index: i * 2 + 1 })?;
        *slot = (hi << 4) | lo;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_both_cases() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(&[0x00, 0x0f, 0xf0, 0xff]), "000ff0ff");
        assert_eq!(encode_upper(&[0x00, 0x0f, 0xf0, 0xff]), "000FF0FF");
    }

    #[test]
    fn round_trips_every_byte_value() {
        let all: Vec<u8> = (0u8..=255).collect();
        assert_eq!(decode(encode(&all)).unwrap(), all);
        assert_eq!(decode(encode_upper(&all)).unwrap(), all);
    }

    #[test]
    fn rejects_malformed_input() {
        assert_eq!(decode("abc"), Err(DecodeError::OddLength));
        assert_eq!(
            decode("0g"),
            Err(DecodeError::InvalidCharacter { index: 1 })
        );
        assert_eq!(
            decode("x0"),
            Err(DecodeError::InvalidCharacter { index: 0 })
        );
        assert_eq!(
            decode("0x1f"),
            Err(DecodeError::InvalidCharacter { index: 1 })
        );
        assert!(decode("de ad").is_err());
        assert!(decode("+1").is_err());
    }

    #[test]
    fn decode_to_slice_checks_the_destination_length() {
        let mut out = [0u8; 2];
        decode_to_slice("beef", &mut out).unwrap();
        assert_eq!(out, [0xbe, 0xef]);
        assert_eq!(
            decode_to_slice("beef00", &mut out),
            Err(DecodeError::InvalidLength)
        );
    }
}
