//! Base64 (RFC 4648) encoding and decoding — the single shared implementation.
//!
//! Two alphabets ([`Alphabet::Standard`] `+/` and [`Alphabet::UrlSafe`] `-_`),
//! each with and without `=` padding:
//!
//! | | encode | strict decode |
//! |---|---|---|
//! | standard, padded | [`encode`] | [`decode`] |
//! | standard, unpadded | [`encode_no_pad`] | [`decode_no_pad`] |
//! | url-safe, padded | [`encode_url`] | [`decode_url`] |
//! | url-safe, unpadded | [`encode_url_no_pad`] | [`decode_url_no_pad`] |
//!
//! The decoders above are **strict**: exactly one input decodes to a given
//! byte string. They reject characters outside the chosen alphabet (including
//! whitespace), `=` anywhere but as the correct final padding, a missing or
//! unexpected padding, an impossible length, and non-zero trailing bits in the
//! final quantum (the "non-canonical" encodings RFC 4648 §3.5 warns about).
//! Use them for anything security-relevant — tokens, signatures, keys — so two
//! different strings can never stand for the same bytes.
//!
//! [`decode_lenient`] is the explicitly named forgiving decoder for
//! human-edited input such as PEM bodies: it accepts either alphabet, optional
//! padding, ASCII whitespace and non-zero trailing bits.
//!
//! ```
//! # #[cfg(feature = "hash")] {
//! use ferroly::hash::base64;
//!
//! assert_eq!(base64::encode(b"hi!"), "aGkh");
//! assert_eq!(base64::encode(b"hi"), "aGk=");
//! assert_eq!(base64::encode_url_no_pad(&[0xfb, 0xff]), "-_8");
//! assert_eq!(base64::decode("aGk=").unwrap(), b"hi");
//!
//! // Strict: the same bytes with stray trailing bits are rejected.
//! assert!(base64::decode("aGl=").is_err());
//! // Lenient: accepted.
//! assert_eq!(base64::decode_lenient("aGl=\n").unwrap(), b"hi");
//! # }
//! ```
//!
//! Decoding is table-driven and is **not** constant-time with respect to the
//! input characters.

use std::fmt;

/// Which 64-character alphabet to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alphabet {
    /// The standard alphabet (`A-Z a-z 0-9 + /`), RFC 4648 §4.
    Standard,
    /// The URL- and filename-safe alphabet (`A-Z a-z 0-9 - _`), RFC 4648 §5.
    UrlSafe,
}

const STANDARD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL_SAFE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Marks a byte that is not part of an alphabet in a decode table.
const INVALID: u8 = 0xff;

const fn decode_table(alphabet: &[u8; 64], also: Option<&[u8; 64]>) -> [u8; 256] {
    let mut table = [INVALID; 256];
    let mut i = 0;
    while i < 64 {
        table[alphabet[i] as usize] = i as u8;
        if let Some(other) = also {
            table[other[i] as usize] = i as u8;
        }
        i += 1;
    }
    table
}

const STANDARD_DECODE: [u8; 256] = decode_table(STANDARD, None);
const URL_SAFE_DECODE: [u8; 256] = decode_table(URL_SAFE, None);
const EITHER_DECODE: [u8; 256] = decode_table(STANDARD, Some(URL_SAFE));

/// Why a base64 input was rejected.
///
/// The error never carries any of the input, so it is safe to log even when
/// the input was a credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeError {
    /// A byte outside the alphabet (this includes whitespace and a `=` that is
    /// not valid padding) at the given byte offset.
    InvalidCharacter {
        /// Byte offset of the offending character.
        index: usize,
    },
    /// The input length cannot be a base64 encoding.
    InvalidLength,
    /// Padding is missing, unexpected, misplaced or of the wrong length.
    InvalidPadding,
    /// The final quantum has non-zero unused bits (a non-canonical encoding).
    TrailingBits,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::InvalidCharacter { index } => {
                write!(f, "invalid base64 character at offset {index}")
            }
            DecodeError::InvalidLength => f.write_str("invalid base64 length"),
            DecodeError::InvalidPadding => f.write_str("invalid base64 padding"),
            DecodeError::TrailingBits => {
                f.write_str("non-canonical base64 (non-zero trailing bits)")
            }
        }
    }
}

impl std::error::Error for DecodeError {}

/// The length of the base64 encoding of `input_len` bytes.
pub const fn encoded_len(input_len: usize, pad: bool) -> usize {
    let full = input_len / 3 * 4;
    match input_len % 3 {
        0 => full,
        rem => {
            if pad {
                full + 4
            } else {
                full + rem + 1
            }
        }
    }
}

/// Encodes `input` with the chosen alphabet, with or without `=` padding.
pub fn encode_with(input: &[u8], alphabet: Alphabet, pad: bool) -> String {
    let chars = match alphabet {
        Alphabet::Standard => STANDARD,
        Alphabet::UrlSafe => URL_SAFE,
    };
    let mut out = String::with_capacity(encoded_len(input.len(), pad));
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(chars[((n >> 18) & 63) as usize] as char);
        out.push(chars[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(chars[((n >> 6) & 63) as usize] as char);
        } else if pad {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(chars[(n & 63) as usize] as char);
        } else if pad {
            out.push('=');
        }
    }
    out
}

/// Standard alphabet, padded (`TWE=`). The most common form.
pub fn encode(input: &[u8]) -> String {
    encode_with(input, Alphabet::Standard, true)
}

/// Standard alphabet, no padding (`TWE`).
pub fn encode_no_pad(input: &[u8]) -> String {
    encode_with(input, Alphabet::Standard, false)
}

/// URL-safe alphabet, padded.
pub fn encode_url(input: &[u8]) -> String {
    encode_with(input, Alphabet::UrlSafe, true)
}

/// URL-safe alphabet, no padding — the form used by JWT/JOSE (`base64url`).
pub fn encode_url_no_pad(input: &[u8]) -> String {
    encode_with(input, Alphabet::UrlSafe, false)
}

/// Strictly decodes `input` in the chosen alphabet.
///
/// With `pad == true` the input must be padded to a multiple of four with the
/// exact number of `=`; with `pad == false` it must contain no `=` at all. See
/// the [module docs](self) for everything that is rejected.
pub fn decode_with(
    input: impl AsRef<[u8]>,
    alphabet: Alphabet,
    pad: bool,
) -> Result<Vec<u8>, DecodeError> {
    let input = input.as_ref();
    let table = match alphabet {
        Alphabet::Standard => &STANDARD_DECODE,
        Alphabet::UrlSafe => &URL_SAFE_DECODE,
    };
    let data = if pad {
        if input.len() % 4 != 0 {
            // Either the padding is absent or the length is impossible.
            return Err(if input.contains(&b'=') || input.len() % 4 == 1 {
                DecodeError::InvalidLength
            } else {
                DecodeError::InvalidPadding
            });
        }
        let pad_len = input.iter().rev().take_while(|&&b| b == b'=').count();
        if pad_len > 2 {
            return Err(DecodeError::InvalidPadding);
        }
        &input[..input.len() - pad_len]
    } else {
        input
    };
    decode_quanta(data, table, true)
}

/// Strict decode: standard alphabet, padding required.
pub fn decode(input: impl AsRef<[u8]>) -> Result<Vec<u8>, DecodeError> {
    decode_with(input, Alphabet::Standard, true)
}

/// Strict decode: standard alphabet, padding forbidden.
pub fn decode_no_pad(input: impl AsRef<[u8]>) -> Result<Vec<u8>, DecodeError> {
    decode_with(input, Alphabet::Standard, false)
}

/// Strict decode: URL-safe alphabet, padding required.
pub fn decode_url(input: impl AsRef<[u8]>) -> Result<Vec<u8>, DecodeError> {
    decode_with(input, Alphabet::UrlSafe, true)
}

/// Strict decode: URL-safe alphabet, padding forbidden — the JWT/JOSE
/// `base64url` form. Any `=`, any character outside the alphabet and any
/// non-canonical final quantum is an error.
pub fn decode_url_no_pad(input: impl AsRef<[u8]>) -> Result<Vec<u8>, DecodeError> {
    decode_with(input, Alphabet::UrlSafe, false)
}

/// Lenient decode for human-edited input (PEM bodies, copy-pasted values).
///
/// Unlike the strict decoders this accepts **either** alphabet (even mixed),
/// optional padding, ASCII whitespace (space, tab, CR, LF) anywhere, and
/// non-zero trailing bits. Several different inputs therefore decode to the
/// same bytes: never use it to compare or validate tokens or signatures.
///
/// Still rejected: other non-alphabet bytes, data after padding, more than
/// two `=`, and an impossible length.
pub fn decode_lenient(input: impl AsRef<[u8]>) -> Result<Vec<u8>, DecodeError> {
    let input = input.as_ref();
    let mut data = Vec::with_capacity(input.len());
    let mut pad_len = 0usize;
    for (index, &b) in input.iter().enumerate() {
        match b {
            b' ' | b'\t' | b'\r' | b'\n' => {}
            b'=' => {
                pad_len += 1;
                if pad_len > 2 {
                    return Err(DecodeError::InvalidPadding);
                }
            }
            _ if pad_len > 0 => return Err(DecodeError::InvalidPadding),
            _ if EITHER_DECODE[b as usize] == INVALID => {
                return Err(DecodeError::InvalidCharacter { index })
            }
            _ => data.push(b),
        }
    }
    if data.len() % 4 == 1 {
        return Err(DecodeError::InvalidLength);
    }
    decode_quanta(&data, &EITHER_DECODE, false)
}

/// Decodes padding-free `data` through `table`. Characters are validated
/// before the length, so a stray byte is reported as such.
fn decode_quanta(
    data: &[u8],
    table: &[u8; 256],
    reject_trailing_bits: bool,
) -> Result<Vec<u8>, DecodeError> {
    let mut out = Vec::with_capacity(data.len() / 4 * 3 + 2);
    for (chunk_index, chunk) in data.chunks(4).enumerate() {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            let v = table[c as usize];
            if v == INVALID {
                return Err(DecodeError::InvalidCharacter {
                    index: chunk_index * 4 + i,
                });
            }
            n = (n << 6) | v as u32;
        }
        match chunk.len() {
            4 => {
                out.push((n >> 16) as u8);
                out.push((n >> 8) as u8);
                out.push(n as u8);
            }
            3 => {
                // 18 bits carry 2 bytes; the low 2 bits must be zero.
                if reject_trailing_bits && n & 0b11 != 0 {
                    return Err(DecodeError::TrailingBits);
                }
                out.push((n >> 10) as u8);
                out.push((n >> 2) as u8);
            }
            2 => {
                // 12 bits carry 1 byte; the low 4 bits must be zero.
                if reject_trailing_bits && n & 0b1111 != 0 {
                    return Err(DecodeError::TrailingBits);
                }
                out.push((n >> 4) as u8);
            }
            _ => return Err(DecodeError::InvalidLength),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 4648 §10 test vectors.
    const RFC4648: [(&str, &str); 7] = [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ];

    #[test]
    fn rfc4648_vectors_encode_and_decode() {
        for (plain, encoded) in RFC4648 {
            assert_eq!(encode(plain.as_bytes()), encoded);
            assert_eq!(decode(encoded).unwrap(), plain.as_bytes());
            let unpadded = encoded.trim_end_matches('=');
            assert_eq!(encode_no_pad(plain.as_bytes()), unpadded);
            assert_eq!(decode_no_pad(unpadded).unwrap(), plain.as_bytes());
            assert_eq!(encode_url(plain.as_bytes()), encoded);
            assert_eq!(decode_url(encoded).unwrap(), plain.as_bytes());
            assert_eq!(encode_url_no_pad(plain.as_bytes()), unpadded);
            assert_eq!(decode_url_no_pad(unpadded).unwrap(), plain.as_bytes());
            assert_eq!(encoded_len(plain.len(), true), encoded.len());
            assert_eq!(encoded_len(plain.len(), false), unpadded.len());
        }
    }

    #[test]
    fn alphabets_differ_only_in_the_last_two_symbols() {
        let bytes = [0xfb, 0xef, 0xbe, 0xff, 0xff];
        assert_eq!(encode(&bytes), "++++//8=");
        assert_eq!(encode_url(&bytes), "----__8=");
        assert_eq!(decode("++++//8=").unwrap(), bytes);
        assert_eq!(decode_url("----__8=").unwrap(), bytes);
        // The wrong alphabet is a strict error at the first foreign symbol.
        assert_eq!(
            decode("----__8="),
            Err(DecodeError::InvalidCharacter { index: 0 })
        );
        assert_eq!(
            decode_url("++++//8="),
            Err(DecodeError::InvalidCharacter { index: 0 })
        );
    }

    #[test]
    fn round_trips_every_length_and_byte_value() {
        let data: Vec<u8> = (0u16..=600).map(|i| (i * 7 + 3) as u8).collect();
        for n in 0..data.len() {
            let slice = &data[..n];
            for alphabet in [Alphabet::Standard, Alphabet::UrlSafe] {
                for pad in [true, false] {
                    let text = encode_with(slice, alphabet, pad);
                    assert_eq!(decode_with(&text, alphabet, pad).unwrap(), slice);
                    assert_eq!(decode_lenient(&text).unwrap(), slice);
                }
            }
        }
    }

    #[test]
    fn strict_rejects_embedded_and_misplaced_padding() {
        // `=` in the middle of otherwise valid data.
        assert!(decode("Zm=9v").is_err());
        assert_eq!(
            decode("Zm=vYmFy"),
            Err(DecodeError::InvalidCharacter { index: 2 })
        );
        assert_eq!(
            decode_url_no_pad("Zm9v="),
            Err(DecodeError::InvalidCharacter { index: 4 })
        );
        assert_eq!(
            decode_url_no_pad("Zg=="),
            Err(DecodeError::InvalidCharacter { index: 2 })
        );
        // Too much, too little, or missing padding.
        assert_eq!(decode("Zg==="), Err(DecodeError::InvalidLength));
        assert_eq!(decode("Z==="), Err(DecodeError::InvalidPadding));
        assert_eq!(decode("===="), Err(DecodeError::InvalidPadding));
        assert_eq!(decode("Zg="), Err(DecodeError::InvalidLength));
        assert_eq!(decode("Zg"), Err(DecodeError::InvalidPadding));
        assert_eq!(decode("Zm8"), Err(DecodeError::InvalidPadding));
        // A full quantum followed by an all-padding quantum.
        assert_eq!(decode("Zm9v===="), Err(DecodeError::InvalidPadding));
    }

    #[test]
    fn strict_rejects_wrong_length() {
        assert_eq!(decode_no_pad("Z"), Err(DecodeError::InvalidLength));
        assert_eq!(decode_url_no_pad("Zm9vY"), Err(DecodeError::InvalidLength));
        assert_eq!(decode("Zm9vY"), Err(DecodeError::InvalidLength));
        assert_eq!(decode("Z==="), Err(DecodeError::InvalidPadding));
    }

    #[test]
    fn strict_rejects_non_alphabet_characters() {
        for bad in ["Zm 9v", "Zm9v\n", "Zm9v\r\n", "Zm9*", "Zm9\u{e9}"] {
            assert!(decode_no_pad(bad).is_err(), "{bad:?}");
            assert!(decode_url_no_pad(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            decode_no_pad("Zm9v!A"),
            Err(DecodeError::InvalidCharacter { index: 4 })
        );
    }

    #[test]
    fn strict_rejects_non_zero_trailing_bits() {
        // "Zg" is the canonical encoding of "f"; "Zh".."Zv" share its first
        // six+two bits and differ only in the four unused ones.
        assert_eq!(decode_no_pad("Zg").unwrap(), b"f");
        for alias in ["Zh", "Zi", "Zj", "Zv"] {
            assert_eq!(decode_no_pad(alias), Err(DecodeError::TrailingBits));
            assert_eq!(decode_url_no_pad(alias), Err(DecodeError::TrailingBits));
            assert_eq!(decode(format!("{alias}==")), Err(DecodeError::TrailingBits));
        }
        // Three-character tail: two unused bits.
        assert_eq!(decode_no_pad("Zm8").unwrap(), b"fo");
        for alias in ["Zm9", "Zm-", "Zm_"] {
            assert_eq!(decode_url_no_pad(alias), Err(DecodeError::TrailingBits));
        }
        assert_eq!(decode("Zm9="), Err(DecodeError::TrailingBits));
    }

    #[test]
    fn strict_decoding_is_injective_for_short_inputs() {
        // Every accepted 2- and 3-character string must re-encode to itself.
        let alphabet = URL_SAFE;
        for &a in alphabet {
            for &b in alphabet {
                let two = [a, b];
                if let Ok(bytes) = decode_url_no_pad(two) {
                    assert_eq!(encode_url_no_pad(&bytes).as_bytes(), two);
                }
                for &c in alphabet {
                    let three = [a, b, c];
                    if let Ok(bytes) = decode_url_no_pad(three) {
                        assert_eq!(encode_url_no_pad(&bytes).as_bytes(), three);
                    }
                }
            }
        }
    }

    #[test]
    fn lenient_accepts_what_strict_rejects() {
        assert_eq!(decode_lenient("Zm9v\r\nYmFy\r\n").unwrap(), b"foobar");
        assert_eq!(decode_lenient(" Zg = = ").unwrap(), b"f");
        assert_eq!(decode_lenient("Zg").unwrap(), b"f");
        assert_eq!(decode_lenient("Zh==").unwrap(), b"f");
        assert_eq!(decode_lenient("-_+/").unwrap(), [0xfb, 0xff, 0xbf]);
    }

    #[test]
    fn lenient_still_rejects_garbage() {
        assert_eq!(
            decode_lenient("Zm9v*"),
            Err(DecodeError::InvalidCharacter { index: 4 })
        );
        assert_eq!(decode_lenient("Zg==Zg=="), Err(DecodeError::InvalidPadding));
        assert_eq!(decode_lenient("Zg==="), Err(DecodeError::InvalidPadding));
        assert_eq!(decode_lenient("Z"), Err(DecodeError::InvalidLength));
    }

    #[test]
    fn errors_never_echo_the_input() {
        let err = decode("c2VjcmV0LXRva2Vu!!==").unwrap_err();
        let text = format!("{err} {err:?}");
        assert!(!text.contains("c2Vj"), "{text}");
    }
}
