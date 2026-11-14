//! Authentication primitives: JSON Web Tokens and scope checks.
//!
//! - **HMAC tokens** (`HS256`, `HS384`, `HS512`): mint with [`encode_hmac`],
//!   verify with [`decode_hs256_with`] / [`decode_hmac_with`] and a
//!   [`Validation`] policy (expiry required by default, leeway, `aud`, `iss`,
//!   `sub`, required claims, maximum age, `typ`).
//! - **Asymmetric tokens** (`RS256`, `ES256`), verify-only, from JWK / JWKS
//!   public keys — `Jwk`, `Jwks`, `RemoteJwks`. Available when the `http`
//!   feature is also enabled (the signature verification is delegated to the
//!   TLS stack's crypto provider).
//! - **Authorization helpers**: [`Scopes`] reads `scope` / `scp` / `roles`
//!   claims and checks them.
//! - [`Secret`], re-exported from [`ferroly::hash`](crate::hash), keeps key
//!   material out of `Debug` output.
//!
//! ```
//! use ferroly::auth::{decode_hs256_with, encode_hmac, Algorithm, Validation};
//! use ferroly::codec::Value;
//!
//! let secret = b"an-example-secret-of-32-bytes!!!";
//! let claims = Value::Object(vec![
//!     ("sub".into(), "u123".into()),
//!     ("exp".into(), Value::Int(4_102_444_800)), // 2100-01-01
//! ]);
//! let token = encode_hmac(Algorithm::Hs256, &claims, secret).unwrap();
//! let back = decode_hs256_with(&token, secret, &Validation::new()).unwrap();
//! assert_eq!(back.get("sub").and_then(Value::as_str), Some("u123"));
//! ```
//!
//! The original [`encode_hs256`] / [`decode_hs256`] pair is kept for
//! compatibility; it does not require `exp` and validates no audience or
//! issuer. New code should use the `_with` functions.
//!
//! The HMAC and SHA-2 primitives are ferroly's own
//! ([`ferroly::hash`](crate::hash)): hand-rolled, vector-tested, not
//! FIPS-validated.

#![deny(missing_docs)]

#[cfg(feature = "http")]
mod jwk;
mod jwt;
mod scope;

pub use ferroly::hash::Secret;
#[cfg(feature = "http")]
pub use jwk::{Jwk, Jwks, KeyType, RemoteJwks};
pub use jwt::{
    decode_header, decode_hmac_with, decode_hs256, decode_hs256_with, encode_hmac, encode_hs256,
    encode_with_header, Algorithm, Header, JwtError, Validation, MAX_TOKEN_LEN, MIN_SECRET_LEN,
};
pub use scope::{ScopeError, Scopes};

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::codec::Value;

    fn now_unix() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    // The behaviour of the original HS256 pair, which `turbo::jwt_auth`
    // depends on, must not regress.

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
        let claims = Value::Object(vec![("exp".into(), Value::Float(1.5))]);
        let token = encode_hs256(&claims, b"s");
        assert_eq!(decode_hs256(&token, b"s"), Err(JwtError::Expired));

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
        let future = now_unix() + 3600;
        let claims = Value::Object(vec![("nbf".into(), Value::Int(future))]);
        let token = encode_hs256(&claims, b"s");
        assert_eq!(decode_hs256(&token, b"s"), Err(JwtError::NotYetValid));

        let claims = Value::Object(vec![("nbf".into(), Value::Int(1))]);
        let token = encode_hs256(&claims, b"s");
        assert!(decode_hs256(&token, b"s").is_ok());
    }

    #[test]
    fn legacy_decode_still_accepts_tokens_without_exp_and_short_secrets() {
        let claims = Value::Object(vec![("sub".into(), "u1".into())]);
        let token = encode_hs256(&claims, b"secret");
        assert!(decode_hs256(&token, b"secret").is_ok());
        // ...which the validating API refuses on both counts.
        assert_eq!(
            decode_hs256_with(&token, b"secret", &Validation::new()),
            Err(JwtError::WeakSecret)
        );
    }
}
