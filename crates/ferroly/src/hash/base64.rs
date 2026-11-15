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
//!
//! This module is a re-export of the crate's single internal implementation,
//! which the `http` and `obs` modules also use without this feature.

pub use crate::base64::{
    decode, decode_lenient, decode_no_pad, decode_url, decode_url_no_pad, decode_with, encode,
    encode_no_pad, encode_url, encode_url_no_pad, encode_with, encoded_len, Alphabet, DecodeError,
};
