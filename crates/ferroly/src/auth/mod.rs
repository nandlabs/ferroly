//! Authentication primitives. Currently HS256 JWTs (mint + verify), built on a
//! from-scratch SHA-256/HMAC — dep-free, per the near-zero-dependency policy.
//!
//! RS256 / ES256 (asymmetric) are intentionally **not** here: they need RSA /
//! ECDSA, which live behind the TLS boundary (`rustls`); they are planned for a
//! later addition. Use HS256 (a shared secret) for in-process signing.
//!
//! ```
//! use ferroly::auth::{encode_hs256, decode_hs256};
//! use ferroly::codec::Value;
//!
//! let claims = Value::Object(vec![("sub".into(), "u123".into())]);
//! let token = encode_hs256(&claims, b"secret");
//! let back = decode_hs256(&token, b"secret").unwrap();
//! assert_eq!(back.get("sub").and_then(Value::as_str), Some("u123"));
//! ```

#![deny(missing_docs)]

mod sha256;

use ferroly::codec::{json, Value};
use ferroly_derive::FerrolyError;

use sha256::hmac_sha256;

/// Errors from JWT verification.
#[derive(Debug, FerrolyError, PartialEq, Eq)]
#[non_exhaustive]
pub enum JwtError {
    /// The token is not a well-formed `header.payload.signature`.
    #[error("malformed JWT")]
    Malformed,
    /// The `alg` header is not `HS256`.
    #[error("unsupported JWT algorithm (only HS256)")]
    UnsupportedAlg,
    /// The signature did not verify under the secret.
    #[error("invalid JWT signature")]
    InvalidSignature,
    /// The token's `exp` claim is in the past.
    #[error("JWT expired")]
    Expired,
    /// The token's `nbf` (not-before) claim is in the future.
    #[error("JWT not yet valid")]
    NotYetValid,
}

/// Mints an HS256 JWT for `claims` signed with `secret`.
pub fn encode_hs256(claims: &Value, secret: &[u8]) -> String {
    let header = b64url_encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = b64url_encode(json::to_string(claims).as_bytes());
    let signing_input = format!("{header}.{payload}");
    let sig = hmac_sha256(secret, signing_input.as_bytes());
    format!("{signing_input}.{}", b64url_encode(&sig))
}

/// A numeric JWT claim (a `NumericDate`, which RFC 7519 allows to be
/// fractional). Returns `None` if the claim is absent, or `Some(Err(()))` if it
/// is present but not a number (a malformed token — never silently ignored).
fn numeric_claim(claims: &Value, key: &str) -> Option<Result<f64, ()>> {
    claims.get(key).map(|v| match v {
        Value::Int(i) => Ok(*i as f64),
        Value::UInt(u) => Ok(*u as f64),
        Value::Float(f) => Ok(*f),
        _ => Err(()),
    })
}

/// Verifies an HS256 JWT under `secret` and returns its claims.
///
/// Checks the `alg` header, the HMAC signature (in constant time), and — if
/// present — the `exp` (expiry) and `nbf` (not-before) claims against the
/// current time. `exp`/`nbf` may be integer or fractional; a present-but-
/// non-numeric `exp`/`nbf` is rejected as [`JwtError::Malformed`] rather than
/// silently ignored.
pub fn decode_hs256(token: &str, secret: &[u8]) -> Result<Value, JwtError> {
    let mut parts = token.split('.');
    let (h, p, s) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s), None) => (h, p, s),
        _ => return Err(JwtError::Malformed),
    };

    let header: Value = json::from_slice(&b64url_decode(h).ok_or(JwtError::Malformed)?)
        .map_err(|_| JwtError::Malformed)?;
    if header.get("alg").and_then(Value::as_str) != Some("HS256") {
        return Err(JwtError::UnsupportedAlg);
    }

    let signing_input = format!("{h}.{p}");
    let expected = hmac_sha256(secret, signing_input.as_bytes());
    let actual = b64url_decode(s).ok_or(JwtError::Malformed)?;
    if !constant_time_eq(&expected, &actual) {
        return Err(JwtError::InvalidSignature);
    }

    let claims: Value = json::from_slice(&b64url_decode(p).ok_or(JwtError::Malformed)?)
        .map_err(|_| JwtError::Malformed)?;

    let now = now_unix() as f64;
    match numeric_claim(&claims, "exp") {
        Some(Ok(exp)) if now >= exp => return Err(JwtError::Expired),
        Some(Err(())) => return Err(JwtError::Malformed),
        _ => {}
    }
    match numeric_claim(&claims, "nbf") {
        Some(Ok(nbf)) if now < nbf => return Err(JwtError::NotYetValid),
        Some(Err(())) => return Err(JwtError::Malformed),
        _ => {}
    }
    Ok(claims)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Length-checked, timing-safe byte comparison.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// URL-safe base64 without padding.
fn b64url_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64URL[((n >> 18) & 63) as usize] as char);
        out.push(B64URL[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64URL[((n >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(B64URL[(n & 63) as usize] as char);
        }
    }
    out
}

/// Decodes URL-safe base64 (with or without padding). `None` on invalid input.
fn b64url_decode(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = input.bytes().filter(|&b| b != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for &c in chunk {
            n = (n << 6) | val(c)?;
        }
        // Left-align when the final chunk is short.
        n <<= 6 * (4 - chunk.len());
        match chunk.len() {
            4 => {
                out.push((n >> 16) as u8);
                out.push((n >> 8) as u8);
                out.push(n as u8);
            }
            3 => {
                out.push((n >> 16) as u8);
                out.push((n >> 8) as u8);
            }
            2 => out.push((n >> 16) as u8),
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_roundtrip() {
        for s in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"] {
            assert_eq!(b64url_decode(&b64url_encode(s)).unwrap(), s);
        }
    }

    #[test]
    fn jwt_roundtrip_and_verify() {
        let claims = Value::Object(vec![
            ("sub".into(), "alice".into()),
            ("role".into(), "admin".into()),
        ]);
        let token = encode_hs256(&claims, b"topsecret");
        let back = decode_hs256(&token, b"topsecret").unwrap();
        assert_eq!(back.get("sub").and_then(Value::as_str), Some("alice"));

        // wrong secret -> invalid signature
        assert_eq!(
            decode_hs256(&token, b"wrong"),
            Err(JwtError::InvalidSignature)
        );
        // tampered payload -> invalid signature
        let mut parts: Vec<&str> = token.split('.').collect();
        parts[1] = "eyJzdWIiOiJtYWxsb3J5In0";
        let tampered = parts.join(".");
        assert_eq!(
            decode_hs256(&tampered, b"topsecret"),
            Err(JwtError::InvalidSignature)
        );
        // malformed
        assert_eq!(decode_hs256("not.a.jwt.x", b"s"), Err(JwtError::Malformed));
    }

    #[test]
    fn expired_token_rejected() {
        let claims = Value::Object(vec![("exp".into(), Value::Int(1))]); // 1970
        let token = encode_hs256(&claims, b"s");
        assert_eq!(decode_hs256(&token, b"s"), Err(JwtError::Expired));
    }

    #[test]
    fn fractional_exp_is_honored_not_skipped() {
        // A fractional `exp` in the past must still expire the token (previously
        // `as_i64` returned None for a float and expiry was silently skipped).
        let claims = Value::Object(vec![("exp".into(), Value::Float(1.5))]);
        let token = encode_hs256(&claims, b"s");
        assert_eq!(decode_hs256(&token, b"s"), Err(JwtError::Expired));

        // A fractional `exp` far in the future is accepted.
        let future = (now_unix() + 3600) as f64 + 0.5;
        let claims = Value::Object(vec![("exp".into(), Value::Float(future))]);
        let token = encode_hs256(&claims, b"s");
        assert!(decode_hs256(&token, b"s").is_ok());
    }

    #[test]
    fn non_numeric_exp_is_rejected_not_ignored() {
        let claims = Value::Object(vec![("exp".into(), Value::Str("soon".into()))]);
        let token = encode_hs256(&claims, b"s");
        assert_eq!(decode_hs256(&token, b"s"), Err(JwtError::Malformed));
    }

    #[test]
    fn nbf_not_before_is_enforced() {
        // nbf in the future -> not yet valid.
        let future = now_unix() + 3600;
        let claims = Value::Object(vec![("nbf".into(), Value::Int(future))]);
        let token = encode_hs256(&claims, b"s");
        assert_eq!(decode_hs256(&token, b"s"), Err(JwtError::NotYetValid));

        // nbf in the past -> valid.
        let claims = Value::Object(vec![("nbf".into(), Value::Int(1))]);
        let token = encode_hs256(&claims, b"s");
        assert!(decode_hs256(&token, b"s").is_ok());
    }
}
