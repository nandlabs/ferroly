#![cfg(all(feature = "auth", feature = "http"))]
//! Asymmetric JWT verification (RS256 / ES256) from JWK and JWKS keys.
//!
//! Proven against the RFC 7515 appendix A.2 (RS256) and A.3 (ES256) examples,
//! plus a second key pair of each type generated and signed with the OpenSSL
//! command-line tool, so the JOSE -> DER conversions are checked against two
//! independent sources. The private keys of the OpenSSL fixtures were
//! discarded; only public values appear here.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::auth::{
    decode_header, decode_hs256, decode_hs256_with, encode_hmac, encode_hs256, Algorithm, Jwk,
    Jwks, JwtError, KeyType, RemoteJwks, Scopes, Validation,
};
use ferroly::codec::Value;
use ferroly::hash::base64;
use ferroly::http::{serve, BoxFuture, Client, HttpHandler, HttpResponse, Request, StatusCode};
use tokio::net::TcpListener;

// ---- fixtures ----------------------------------------------------------------

/// RFC 7515 appendix A.2: RS256 token and the RSA public key (`n`, `e`).
const RFC7515_A2_TOKEN: &str = "eyJhbGciOiJSUzI1NiJ9.eyJpc3MiOiJqb2UiLA0KICJleHAiOjEzMDA4MTkzODA\
     sDQogImh0dHA6Ly9leGFtcGxlLmNvbS9pc19yb290Ijp0cnVlfQ.cC4hiUPoj9Ee\
     tdgtv3hF80EGrhuB__dzERat0XF9g2VtQgr9PJbu3XOiZj5RZmh7AAuHIm4Bh-0Q\
     c_lF5YKt_O8W2Fp5jujGbds9uJdbF9CUAr7t1dnZcAcQjbKBYNX4BAynRFdiuB--\
     f_nZLgrnbyTyWzO75vRK5h6xBArLIARNPvkSjtQBMHlb1L07Qe7K0GarZRmB_eSN\
     9383LcOLn6_dO--xi12jzDwusC-eOkHWEsqtFZESc6BfI7noOPqvhJ1phCnvWh6I\
     eYI2w9QOYEUipUTI8np6LbgGY9Fs98rqVt5AXLIhWkWywlVmtVrBp0igcN_IoypG\
     lUPQGe77Rw";
const RFC7515_A2_N: &str = "ofgWCuLjybRlzo0tZWJjNiuSfb4p4fAkd_wWJcyQoTbji9k0l8W26mPddxHmfHQp\
     -Vaw-4qPCJrcS2mJPMEzP1Pt0Bm4d4QlL-yRT-SFd2lZS-pCgNMsD1W_YpRPEwOW\
     vG6b32690r2jZ47soMZo9wGzjb_7OMg0LOL-bSf63kpaSHSXndS5z5rexMdbBYUs\
     LA9e-KXBdQOS-UTo7WTBEMa2R2CapHg665xsmtdVMTBQY4uDZlxvb3qCo5ZwKh9k\
     G4LT6_I5IhlJH7aGhyxXFvUK-DWNmoudF8NAco9_h9iaGNj8q2ethFkMLs91kzk2\
     PAcDTW9gb54h4FRWyuXpoQ";
const RFC7515_A2_E: &str = "AQAB";

/// RFC 7515 appendix A.3: ES256 token and the P-256 public key (`x`, `y`).
const RFC7515_A3_TOKEN: &str = "eyJhbGciOiJFUzI1NiJ9.eyJpc3MiOiJqb2UiLA0KICJleHAiOjEzMDA4MTkzODA\
     sDQogImh0dHA6Ly9leGFtcGxlLmNvbS9pc19yb290Ijp0cnVlfQ.DtEhU3ljbEg8\
     L38VWAfUAqOyKAM6-Xx-F4GawxaepmXFCgfTjDxw5djxLa8ISlSApmWQxfKTUJqP\
     P3-Kg6NU1Q";
const RFC7515_A3_X: &str = "f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU";
const RFC7515_A3_Y: &str = "x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0";

// A second RSA-2048 key and an RS256 token, generated and signed with the
// OpenSSL command-line tool (header kid "rsa-1", exp in the year 2100).
const OPENSSL_RSA_N: &str = "uOZtUwSf139eYpboPCSTbEcmsJhZkvFJJ3Zr2Ipn-1b71vaGZFfsld0ytcC0UHGG\
     dRsvSGw0_NFGAN3gR8w7CGA1MAMYvtpuZs12jRUzhlmD2-ud4ZcDkji9e6FkZ2e5\
     8FsjH5Bi6E_nQiPoMdg1LpjbPqcAaMpm92DDhMs1Qz53uoXsk7-rJCCWqW-T82lw\
     f4c3F59Di8uso3i8uPEdxcFhMiXmKWO-nd2d5gnv4qT9HkfwnJiCRyPP7frSdWMu\
     VT9EuOxU0EWhG3e3SHxLWQ9ivLFN_mO-H5VLEyachrcNIRi5rTwCFnsYSi-xMc5L\
     6sTAVzxMvQQb0ylk4wSEnQ";
const OPENSSL_RSA_TOKEN: &str = "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCIsImtpZCI6InJzYS0xIn0.eyJpc3Mi\
     OiJodHRwczovL2lzc3Vlci5leGFtcGxlIiwiYXVkIjpbIm9yZGVycy1hcGkiLCJi\
     aWxsaW5nLWFwaSJdLCJzdWIiOiJ1c2VyLTEiLCJzY29wZSI6Im9yZGVyczpyZWFk\
     IG9yZGVyczp3cml0ZSIsImlhdCI6MTcwMDAwMDAwMCwiZXhwIjo0MTAyNDQ0ODAw\
     fQ.UoNJweNTPUYgO_s21uutCaPFrnkHC8ykGIgZ04bOR2-PAtDBZuDVJ9HoC0w1e\
     0HhvDlSsItePNe--p3BkKzLSr5YilqAkm-tJFOkcmduKLEQsrtPkWHQ_iskg3k2C\
     B8wZ57DbmmSGQLXxFUFyKhJTlXH3odT2FIjSzjZ5zj64FYtyYTAjmeaayHxB3tL8\
     yL_Q_nd_pt-IAPZbEPXyXm2X0r0oYzxA579kg5jKL81Vg5WjcJ9nmlRd-wgF2kTj\
     IxoRdRupJNweJOvaZSTHu6zge7ySGD85AyJIt1IAKl5onQlSSo_Nc0Ipe7jEQmPj\
     wkMgj37H450eRJK_tTyy01L3A";
const OPENSSL_RSA_PUBLIC_PEM: &str = "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAuOZtUwSf139eYpboPCST\nbEcmsJhZkvFJJ3Zr2Ipn+1b71vaGZFfsld0ytcC0UHGGdRsvSGw0/NFGAN3gR8w7\nCGA1MAMYvtpuZs12jRUzhlmD2+ud4ZcDkji9e6FkZ2e58FsjH5Bi6E/nQiPoMdg1\nLpjbPqcAaMpm92DDhMs1Qz53uoXsk7+rJCCWqW+T82lwf4c3F59Di8uso3i8uPEd\nxcFhMiXmKWO+nd2d5gnv4qT9HkfwnJiCRyPP7frSdWMuVT9EuOxU0EWhG3e3SHxL\nWQ9ivLFN/mO+H5VLEyachrcNIRi5rTwCFnsYSi+xMc5L6sTAVzxMvQQb0ylk4wSE\nnQIDAQAB\n-----END PUBLIC KEY-----\n";
// A 1024-bit RSA modulus (too small to be accepted).
const OPENSSL_RSA_1024_N: &str = "7vfqvh4IAJK9nBQCRttiMGUA5c8ydwIwgc68XRR7fLK0sMoKA9vMcf0CzswVKPYW\
     -GEnXYgPnvbyKBFDK82V3NWvjJZJ7LnTEgoCqx7rO4zg87XmtwHPL1NGdK6OFGSm\
     G2hH9O1RwdBqlKwVQgJEDzq6X41ryplQ1p8djlBBJJc";

// A second P-256 key and ES256 tokens signed with OpenSSL (kid "ec-1").
// The variants exercise the r||s -> DER conversion: both values below
// 2^255, both with the top bit set (DER needs a zero pad), and r / s with a
// leading zero byte (DER must drop it).
const OPENSSL_EC_X: &str = "_iLIZys4clYQvd78666xpI4CHTgGL8ZgdepgxQ8hYmI";
const OPENSSL_EC_Y: &str = "X8Z9qeAdIla4aqhl8d3jlLja9jEz8PUgbHX9XA4wP74";
const OPENSSL_EC_TOKEN_PLAIN: &str =
    "eyJhbGciOiJFUzI1NiIsInR5cCI6IkpXVCIsImtpZCI6ImVjLTEifQ.eyJpc3MiO\
     iJodHRwczovL2lzc3Vlci5leGFtcGxlIiwiYXVkIjpbIm9yZGVycy1hcGkiLCJia\
     WxsaW5nLWFwaSJdLCJzdWIiOiJ1c2VyLTEiLCJzY29wZSI6Im9yZGVyczpyZWFkI\
     G9yZGVyczp3cml0ZSIsImlhdCI6MTcwMDAwMDAwMCwiZXhwIjo0MTAyNDQ0ODAwf\
     Q.HR-ngoQuF4Uj5HkRO-xllbZLJz_vndbZsB1FqZ7gJgJf-QlPLrfve8ppWGxP0P\
     O6tqlXVCVhAEK4ozZPlREucQ";
const OPENSSL_EC_TOKEN_HIGH_BIT: &str =
    "eyJhbGciOiJFUzI1NiIsInR5cCI6IkpXVCIsImtpZCI6ImVjLTEifQ.eyJpc3MiO\
     iJodHRwczovL2lzc3Vlci5leGFtcGxlIiwiYXVkIjpbIm9yZGVycy1hcGkiLCJia\
     WxsaW5nLWFwaSJdLCJzdWIiOiJ1c2VyLTEiLCJzY29wZSI6Im9yZGVyczpyZWFkI\
     G9yZGVyczp3cml0ZSIsImlhdCI6MTcwMDAwMDAwMCwiZXhwIjo0MTAyNDQ0ODAwf\
     Q.jZkSxDc7ZYMVKE_7CDPreB79zqc2h85V36WEG9L8ge6G2wP-D7jB8pwhr3C1nY\
     fZJswOfNArIbYIJxL51HUR5w";
const OPENSSL_EC_TOKEN_SHORT_R: &str =
    "eyJhbGciOiJFUzI1NiIsInR5cCI6IkpXVCIsImtpZCI6ImVjLTEifQ.eyJpc3MiO\
     iJodHRwczovL2lzc3Vlci5leGFtcGxlIiwiYXVkIjpbIm9yZGVycy1hcGkiLCJia\
     WxsaW5nLWFwaSJdLCJzdWIiOiJ1c2VyLTEiLCJzY29wZSI6Im9yZGVyczpyZWFkI\
     G9yZGVyczp3cml0ZSIsImlhdCI6MTcwMDAwMDAwMCwiZXhwIjo0MTAyNDQ0ODAwf\
     Q.ABySguacx_CuIIsuLFEVlO-3Kz_g0CvUDJT51qOqs6wi4f6p3vIVYGLlWgXjdS\
     X4S3DibmKR6lw5f4lILaWysA";
const OPENSSL_EC_TOKEN_SHORT_S: &str =
    "eyJhbGciOiJFUzI1NiIsInR5cCI6IkpXVCIsImtpZCI6ImVjLTEifQ.eyJpc3MiO\
     iJodHRwczovL2lzc3Vlci5leGFtcGxlIiwiYXVkIjpbIm9yZGVycy1hcGkiLCJia\
     WxsaW5nLWFwaSJdLCJzdWIiOiJ1c2VyLTEiLCJzY29wZSI6Im9yZGVyczpyZWFkI\
     G9yZGVyczp3cml0ZSIsImlhdCI6MTcwMDAwMDAwMCwiZXhwIjo0MTAyNDQ0ODAwf\
     Q.on-FoLYGRJVRTtJie1IB4eCsqhMghjrB5dgPq3P1HCgAFCu6SsZbwLiln_5aKR\
     Uppp6gKhtj03uPNq1hkP6ubw";

/// The `exp` of the RFC 7515 examples is 1300819380 (March 2011).
const RFC_EXP: u64 = 1_300_819_380;

fn rfc_time() -> Validation {
    Validation::new().leeway(0).current_time(RFC_EXP - 60)
}

fn rfc_rsa_jwk() -> Jwk {
    Jwk::from_json(&format!(
        r#"{{"kty":"RSA","n":"{RFC7515_A2_N}","e":"{RFC7515_A2_E}"}}"#
    ))
    .unwrap()
}

fn rfc_ec_jwk() -> Jwk {
    Jwk::from_json(&format!(
        r#"{{"kty":"EC","crv":"P-256","x":"{RFC7515_A3_X}","y":"{RFC7515_A3_Y}"}}"#
    ))
    .unwrap()
}

fn openssl_rsa_jwk() -> Jwk {
    Jwk::from_json(&format!(
        r#"{{"kty":"RSA","kid":"rsa-1","use":"sig","alg":"RS256","n":"{OPENSSL_RSA_N}","e":"AQAB"}}"#
    ))
    .unwrap()
}

fn openssl_ec_jwk() -> Jwk {
    Jwk::from_json(&format!(
        r#"{{"kty":"EC","crv":"P-256","kid":"ec-1","use":"sig","alg":"ES256","key_ops":["verify"],"x":"{OPENSSL_EC_X}","y":"{OPENSSL_EC_Y}"}}"#
    ))
    .unwrap()
}

fn b64(bytes: &[u8]) -> String {
    base64::encode_url_no_pad(bytes)
}

fn unb64(text: &str) -> Vec<u8> {
    base64::decode_url_no_pad(text).unwrap()
}

/// Splits a compact token into (header, payload, signature) segments.
fn segments(token: &str) -> (&str, &str, &str) {
    let mut it = token.split('.');
    (it.next().unwrap(), it.next().unwrap(), it.next().unwrap())
}

/// Replaces the payload segment, keeping header and signature.
fn with_payload(token: &str, payload_json: &str) -> String {
    let (h, _, s) = segments(token);
    format!("{h}.{}.{s}", b64(payload_json.as_bytes()))
}

/// Flips one bit of the decoded signature at `byte`.
fn with_flipped_signature_bit(token: &str, byte: usize) -> String {
    let (h, p, s) = segments(token);
    let mut sig = unb64(s);
    sig[byte] ^= 0x01;
    format!("{h}.{p}.{}", b64(&sig))
}

// ---- RFC 7515 appendix A.2: RS256 ------------------------------------------

#[test]
fn rfc7515_a2_rs256_token_verifies() {
    let claims = rfc_rsa_jwk().verify(RFC7515_A2_TOKEN, &rfc_time()).unwrap();
    assert_eq!(claims.get("iss").and_then(Value::as_str), Some("joe"));
    assert_eq!(claims.get("exp").and_then(Value::as_u64), Some(RFC_EXP));
    assert_eq!(
        claims
            .get("http://example.com/is_root")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(rfc_rsa_jwk().key_type(), KeyType::Rsa);
    assert_eq!(rfc_rsa_jwk().algorithm(), Algorithm::Rs256);
}

#[test]
fn rfc7515_a2_rs256_detached_signature_verifies() {
    let (h, p, s) = segments(RFC7515_A2_TOKEN);
    let message = format!("{h}.{p}");
    let signature = unb64(s);
    let key = rfc_rsa_jwk();
    assert_eq!(key.verify_signature(message.as_bytes(), &signature), Ok(()));
    assert_eq!(
        key.verify_signature(b"another message", &signature),
        Err(JwtError::InvalidSignature)
    );
}

#[test]
fn rs256_rejects_tampered_payload() {
    let forged = with_payload(
        RFC7515_A2_TOKEN,
        r#"{"iss":"mallory","exp":1300819380,"http://example.com/is_root":true}"#,
    );
    assert_eq!(
        rfc_rsa_jwk().verify(&forged, &rfc_time()),
        Err(JwtError::InvalidSignature)
    );
}

#[test]
fn rs256_rejects_tampered_signature() {
    for byte in [0usize, 1, 100, 255] {
        let forged = with_flipped_signature_bit(RFC7515_A2_TOKEN, byte);
        assert_eq!(
            rfc_rsa_jwk().verify(&forged, &rfc_time()),
            Err(JwtError::InvalidSignature),
            "byte {byte}"
        );
    }
    // Truncated, extended and empty signatures.
    let (h, p, s) = segments(RFC7515_A2_TOKEN);
    let sig = unb64(s);
    for bad in [&sig[..255], &[&sig[..], &[0u8]].concat()[..], &[][..]] {
        let token = format!("{h}.{p}.{}", b64(bad));
        assert_eq!(
            rfc_rsa_jwk().verify(&token, &rfc_time()),
            Err(JwtError::InvalidSignature)
        );
    }
}

#[test]
fn rs256_rejects_the_wrong_key() {
    // A different, valid RSA-2048 key.
    assert_eq!(
        openssl_rsa_jwk().verify(RFC7515_A2_TOKEN, &rfc_time()),
        Err(JwtError::InvalidSignature)
    );
    // The right modulus with a different exponent is a different key too.
    let n = unb64(RFC7515_A2_N);
    let other_e = Jwk::from_rsa_components(&n, &[0x03]).unwrap();
    assert_eq!(
        other_e.verify(RFC7515_A2_TOKEN, &rfc_time()),
        Err(JwtError::InvalidSignature)
    );
}

#[test]
fn rs256_openssl_generated_key_and_signature_verify() {
    let claims = openssl_rsa_jwk()
        .verify(OPENSSL_RSA_TOKEN, &Validation::new())
        .unwrap();
    assert_eq!(claims.get("sub").and_then(Value::as_str), Some("user-1"));
    // ...and not under the RFC key.
    assert_eq!(
        rfc_rsa_jwk().verify(OPENSSL_RSA_TOKEN, &Validation::new()),
        Err(JwtError::InvalidSignature)
    );
}

// ---- RFC 7515 appendix A.3: ES256 ------------------------------------------

#[test]
fn rfc7515_a3_es256_token_verifies() {
    let claims = rfc_ec_jwk().verify(RFC7515_A3_TOKEN, &rfc_time()).unwrap();
    assert_eq!(claims.get("iss").and_then(Value::as_str), Some("joe"));
    assert_eq!(rfc_ec_jwk().key_type(), KeyType::EcP256);
    assert_eq!(rfc_ec_jwk().algorithm(), Algorithm::Es256);
}

#[test]
fn rfc7515_a3_es256_detached_signature_verifies() {
    let (h, p, s) = segments(RFC7515_A3_TOKEN);
    let message = format!("{h}.{p}");
    let signature = unb64(s);
    assert_eq!(signature.len(), 64);
    // R and S exactly as printed in the RFC.
    assert_eq!(signature[..4], [14, 209, 33, 83]);
    assert_eq!(signature[32..36], [197, 10, 7, 211]);
    let key = rfc_ec_jwk();
    assert_eq!(key.verify_signature(message.as_bytes(), &signature), Ok(()));
    assert_eq!(
        key.verify_signature(b"another message", &signature),
        Err(JwtError::InvalidSignature)
    );
}

#[test]
fn es256_rejects_tampered_payload() {
    let forged = with_payload(
        RFC7515_A3_TOKEN,
        r#"{"iss":"mallory","exp":1300819380,"http://example.com/is_root":true}"#,
    );
    assert_eq!(
        rfc_ec_jwk().verify(&forged, &rfc_time()),
        Err(JwtError::InvalidSignature)
    );
}

#[test]
fn es256_rejects_tampered_signature() {
    for byte in [0usize, 31, 32, 63] {
        let forged = with_flipped_signature_bit(RFC7515_A3_TOKEN, byte);
        assert_eq!(
            rfc_ec_jwk().verify(&forged, &rfc_time()),
            Err(JwtError::InvalidSignature),
            "byte {byte}"
        );
    }
    let (h, p, s) = segments(RFC7515_A3_TOKEN);
    let sig = unb64(s);
    // Wrong lengths (including a DER-encoded signature, which JWS forbids),
    // r = 0, s = 0, and r/s swapped.
    let mut zero_r = sig.clone();
    zero_r[..32].fill(0);
    let mut zero_s = sig.clone();
    zero_s[32..].fill(0);
    let swapped = [&sig[32..], &sig[..32]].concat();
    let der_like = [&[0x30u8, 0x44, 0x02, 0x20][..], &sig[..]].concat();
    for bad in [
        &sig[..63],
        &[&sig[..], &[0u8]].concat()[..],
        &[][..],
        &zero_r[..],
        &zero_s[..],
        &swapped[..],
        &der_like[..],
        &[0xffu8; 64][..],
    ] {
        let token = format!("{h}.{p}.{}", b64(bad));
        assert_eq!(
            rfc_ec_jwk().verify(&token, &rfc_time()),
            Err(JwtError::InvalidSignature),
            "signature of {} bytes",
            bad.len()
        );
    }
}

#[test]
fn es256_rejects_the_wrong_key() {
    assert_eq!(
        openssl_ec_jwk().verify(RFC7515_A3_TOKEN, &rfc_time()),
        Err(JwtError::InvalidSignature)
    );
    // A point that is not on the curve can never verify anything.
    let mut y = unb64(RFC7515_A3_Y);
    y[31] ^= 1;
    let off_curve = Jwk::from_ec_p256_coordinates(&unb64(RFC7515_A3_X), &y).unwrap();
    assert_eq!(
        off_curve.verify(RFC7515_A3_TOKEN, &rfc_time()),
        Err(JwtError::InvalidSignature)
    );
}

#[test]
fn es256_openssl_signatures_cover_every_der_integer_shape() {
    let key = openssl_ec_jwk();
    let high_bit = unb64(segments(OPENSSL_EC_TOKEN_HIGH_BIT).2);
    assert!(high_bit[0] >= 0x80 && high_bit[32] >= 0x80);
    let short_r = unb64(segments(OPENSSL_EC_TOKEN_SHORT_R).2);
    assert_eq!(short_r[0], 0);
    let short_s = unb64(segments(OPENSSL_EC_TOKEN_SHORT_S).2);
    assert_eq!(short_s[32], 0);
    for token in [
        OPENSSL_EC_TOKEN_PLAIN,
        OPENSSL_EC_TOKEN_HIGH_BIT,
        OPENSSL_EC_TOKEN_SHORT_R,
        OPENSSL_EC_TOKEN_SHORT_S,
    ] {
        let claims = key.verify(token, &Validation::new()).unwrap();
        assert_eq!(claims.get("sub").and_then(Value::as_str), Some("user-1"));
        // Not under the RFC key, and not after a bit flip.
        assert_eq!(
            rfc_ec_jwk().verify(token, &Validation::new()),
            Err(JwtError::InvalidSignature)
        );
        assert_eq!(
            key.verify(&with_flipped_signature_bit(token, 40), &Validation::new()),
            Err(JwtError::InvalidSignature)
        );
    }
}

// ---- algorithm confusion -----------------------------------------------------

#[test]
fn header_alg_must_match_the_key_type() {
    // An RS256 token offered to an EC key and vice versa.
    assert_eq!(
        rfc_ec_jwk().verify(RFC7515_A2_TOKEN, &rfc_time()),
        Err(JwtError::UnsupportedAlg)
    );
    assert_eq!(
        rfc_rsa_jwk().verify(RFC7515_A3_TOKEN, &rfc_time()),
        Err(JwtError::UnsupportedAlg)
    );
    // Rewriting only the header's alg (keeping the genuine signature) fails
    // on the algorithm check, before any signature work.
    let (_, p, s) = segments(RFC7515_A2_TOKEN);
    let relabelled = format!("{}.{p}.{s}", b64(br#"{"alg":"ES256"}"#));
    assert_eq!(
        rfc_rsa_jwk().verify(&relabelled, &rfc_time()),
        Err(JwtError::UnsupportedAlg)
    );
    // An allow-list that excludes the key's algorithm.
    assert_eq!(
        rfc_rsa_jwk().verify(
            RFC7515_A2_TOKEN,
            &rfc_time().algorithms(&[Algorithm::Es256])
        ),
        Err(JwtError::UnsupportedAlg)
    );
}

#[test]
fn a_public_key_is_never_usable_as_an_hmac_secret() {
    let claims = Value::Object(vec![
        ("iss".into(), "mallory".into()),
        ("exp".into(), Value::Int(4_102_444_800)),
    ]);
    let jwk_json = format!(r#"{{"kty":"RSA","n":"{OPENSSL_RSA_N}","e":"AQAB"}}"#);
    let jwks_json = format!(r#"{{"keys":[{jwk_json}]}}"#);
    let jwks = Jwks::from_json(&jwks_json).unwrap();
    let key = Jwk::from_json(&jwk_json).unwrap();

    // Everything an attacker knows about the public key, used as an HMAC key
    // to forge an HS256 token (`encode_hs256` performs no checks, like an
    // attacker's own tooling).
    let modulus = unb64(OPENSSL_RSA_N);
    let public_material: [&[u8]; 5] = [
        OPENSSL_RSA_PUBLIC_PEM.as_bytes(),
        jwk_json.as_bytes(),
        jwks_json.as_bytes(),
        &modulus,
        OPENSSL_RSA_N.as_bytes(),
    ];
    for material in public_material {
        let forged = encode_hs256(&claims, material);
        assert_eq!(decode_header(&forged).unwrap().alg, Algorithm::Hs256);
        // The asymmetric entry points refuse every HMAC algorithm.
        assert_eq!(
            key.verify(&forged, &Validation::new()),
            Err(JwtError::UnsupportedAlg)
        );
        assert_eq!(
            jwks.verify(&forged, &Validation::new()),
            Err(JwtError::UnsupportedAlg)
        );
        // Even explicitly allowing HS256 cannot make a JWK verify an HMAC.
        assert_eq!(
            key.verify(&forged, &Validation::new().algorithms(&[Algorithm::Hs256])),
            Err(JwtError::UnsupportedAlg)
        );
        assert_eq!(
            jwks.verify(&forged, &Validation::new().algorithms(&[Algorithm::Hs256])),
            Err(JwtError::UnsupportedAlg)
        );
    }

    // And the HMAC entry points refuse key material that is recognisably a
    // public key, certificate or JWK document.
    for material in [
        OPENSSL_RSA_PUBLIC_PEM.as_bytes(),
        jwk_json.as_bytes(),
        jwks_json.as_bytes(),
        b"-----BEGIN CERTIFICATE-----\nMIIB...\n-----END CERTIFICATE-----\n".as_slice(),
        b"-----BEGIN RSA PUBLIC KEY-----\nMIIB...\n-----END RSA PUBLIC KEY-----\n".as_slice(),
        b"ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQ user@host".as_slice(),
    ] {
        let forged = encode_hs256(&claims, material);
        assert_eq!(decode_hs256(&forged, material), Err(JwtError::InvalidKey));
        assert_eq!(
            decode_hs256_with(&forged, material, &Validation::new()),
            Err(JwtError::InvalidKey)
        );
        assert_eq!(
            encode_hmac(Algorithm::Hs256, &claims, material),
            Err(JwtError::InvalidKey)
        );
    }

    // The debug form of a key shows its id and type, not its bytes.
    let debug = format!("{:?}", openssl_rsa_jwk());
    assert!(debug.contains("rsa-1") && debug.contains("Rsa"), "{debug}");
    assert!(!debug.contains(&OPENSSL_RSA_N[..16]), "{debug}");
}

#[test]
fn unsecured_and_unknown_algorithms_are_rejected() {
    let (_, p, _) = segments(RFC7515_A2_TOKEN);
    for alg in [
        "none", "None", "NONE", "RS384", "PS256", "ES384", "EdDSA", "rs256", "",
    ] {
        let header = b64(format!(r#"{{"alg":"{alg}"}}"#).as_bytes());
        for signature in ["", segments(RFC7515_A2_TOKEN).2] {
            let token = format!("{header}.{p}.{signature}");
            assert_eq!(
                rfc_rsa_jwk().verify(&token, &rfc_time()),
                Err(JwtError::UnsupportedAlg),
                "alg {alg:?}"
            );
            assert_eq!(
                Jwks::from_keys(vec![rfc_rsa_jwk()]).verify(&token, &rfc_time()),
                Err(JwtError::UnsupportedAlg),
                "alg {alg:?}"
            );
        }
    }
    // A missing alg and a non-string alg.
    for header in [r#"{"typ":"JWT"}"#, r#"{"alg":256}"#, r#"{"alg":null}"#] {
        let token = format!("{}.{p}.", b64(header.as_bytes()));
        assert_eq!(
            rfc_rsa_jwk().verify(&token, &rfc_time()),
            Err(JwtError::UnsupportedAlg)
        );
    }
}

// ---- claims validation applies to asymmetric tokens too ----------------------

#[test]
fn claims_are_validated_after_the_signature() {
    let key = rfc_rsa_jwk();
    // Expired relative to the real clock, and exactly at `exp`.
    assert_eq!(
        key.verify(RFC7515_A2_TOKEN, &Validation::new()),
        Err(JwtError::Expired)
    );
    assert_eq!(
        key.verify(
            RFC7515_A2_TOKEN,
            &Validation::new().leeway(0).current_time(RFC_EXP)
        ),
        Err(JwtError::Expired)
    );
    assert!(key
        .verify(
            RFC7515_A2_TOKEN,
            &Validation::new().leeway(0).current_time(RFC_EXP - 1)
        )
        .is_ok());
    assert!(key
        .verify(RFC7515_A2_TOKEN, &rfc_time().issuer("joe"))
        .is_ok());
    assert_eq!(
        key.verify(RFC7515_A2_TOKEN, &rfc_time().issuer("jane")),
        Err(JwtError::InvalidIssuer)
    );
    assert_eq!(
        key.verify(RFC7515_A2_TOKEN, &rfc_time().audience("api")),
        Err(JwtError::MissingClaim("aud".into()))
    );

    let ec = openssl_ec_jwk();
    let ok = Validation::new()
        .issuer("https://issuer.example")
        .audience("billing-api")
        .subject("user-1")
        .typ("JWT")
        .require_claim("scope");
    let claims = ec.verify(OPENSSL_EC_TOKEN_PLAIN, &ok).unwrap();
    assert!(Scopes::from_claims(&claims)
        .require_all(&["orders:read", "orders:write"])
        .is_ok());
    assert_eq!(
        ec.verify(
            OPENSSL_EC_TOKEN_PLAIN,
            &Validation::new().audience("someone-else")
        ),
        Err(JwtError::InvalidAudience)
    );
    assert_eq!(
        ec.verify(OPENSSL_EC_TOKEN_PLAIN, &Validation::new().typ("at+jwt")),
        Err(JwtError::InvalidType)
    );
}

// ---- JWK parsing -------------------------------------------------------------

#[test]
fn jwk_parsing_rejects_unusable_keys() {
    let rsa = |extra: &str| format!(r#"{{"kty":"RSA","n":"{RFC7515_A2_N}","e":"AQAB"{extra}}}"#);
    let ec = |extra: &str| {
        format!(r#"{{"kty":"EC","crv":"P-256","x":"{RFC7515_A3_X}","y":"{RFC7515_A3_Y}"{extra}}}"#)
    };
    assert!(Jwk::from_json(&rsa("")).is_ok());
    assert!(Jwk::from_json(&ec("")).is_ok());
    assert!(Jwk::from_json(&rsa(r#","use":"sig","alg":"RS256","kid":"k""#)).is_ok());
    assert!(Jwk::from_json(&ec(r#","key_ops":["verify"],"alg":"ES256""#)).is_ok());

    let invalid = [
        // Private keys do not belong in a verifier.
        rsa(r#","d":"AQAB""#),
        ec(r#","d":"jpsQnnGQmL-YBIffH1136cspYG6-0iY7X1fCE9-E9LI""#),
        // Encryption keys, or keys not allowed to verify.
        rsa(r#","use":"enc""#),
        ec(r#","key_ops":["sign"]"#),
        rsa(r#","key_ops":"verify""#),
        rsa(r#","kid":7"#),
        // Too small (1024 bits), no exponent, even exponent, exponent 1.
        format!(r#"{{"kty":"RSA","n":"{OPENSSL_RSA_1024_N}","e":"AQAB"}}"#),
        format!(r#"{{"kty":"RSA","n":"{RFC7515_A2_N}"}}"#),
        format!(r#"{{"kty":"RSA","n":"{RFC7515_A2_N}","e":"AQAA"}}"#),
        format!(r#"{{"kty":"RSA","n":"{RFC7515_A2_N}","e":"AQ"}}"#),
        format!(r#"{{"kty":"RSA","n":"{RFC7515_A2_N}","e":""}}"#),
        // Padded / non-canonical / non-alphabet base64url.
        format!(r#"{{"kty":"RSA","n":"{RFC7515_A2_N}","e":"AQAB=="}}"#),
        format!(r#"{{"kty":"RSA","n":"{RFC7515_A2_N}=","e":"AQAB"}}"#),
        format!(
            r#"{{"kty":"RSA","n":"{}","e":"AQAB"}}"#,
            RFC7515_A2_N.replace('-', "+")
        ),
        // Coordinates of the wrong size, or missing.
        format!(r#"{{"kty":"EC","crv":"P-256","x":"AQAB","y":"{RFC7515_A3_Y}"}}"#),
        format!(r#"{{"kty":"EC","crv":"P-256","x":"{RFC7515_A3_X}"}}"#),
        format!(r#"{{"kty":"EC","x":"{RFC7515_A3_X}","y":"{RFC7515_A3_Y}"}}"#),
        // Not a JWK at all.
        "[]".to_string(),
        "\"RSA\"".to_string(),
        "{}".to_string(),
        "{".to_string(),
        r#"{"kty":7}"#.to_string(),
    ];
    for text in &invalid {
        assert_eq!(Jwk::from_json(text), Err(JwtError::InvalidKey), "{text}");
    }

    let unsupported = [
        rsa(r#","alg":"RS384""#),
        rsa(r#","alg":"PS256""#),
        rsa(r#","alg":"RSA-OAEP""#),
        rsa(r#","alg":"HS256""#),
        ec(r#","alg":"RS256""#),
        format!(r#"{{"kty":"EC","crv":"P-384","x":"{RFC7515_A3_X}","y":"{RFC7515_A3_Y}"}}"#),
        r#"{"kty":"OKP","crv":"Ed25519","x":"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"}"#
            .to_string(),
        r#"{"kty":"oct","k":"AyM1SysPpbyDfgZld3umj1qzKObwVMkoqQ-EstJQLr_T-1qS0gZH75aKtMN3Yj0iPS4hcgUuTwjAzZr1Z9CAow"}"#
            .to_string(),
    ];
    for text in &unsupported {
        assert_eq!(
            Jwk::from_json(text),
            Err(JwtError::UnsupportedAlg),
            "{text}"
        );
    }
}

#[test]
fn rsa_components_tolerate_leading_zeros() {
    let mut n = vec![0u8, 0u8];
    n.extend_from_slice(&unb64(RFC7515_A2_N));
    let key = Jwk::from_rsa_components(&n, &[0, 0, 1, 0, 1]).unwrap();
    assert_eq!(key, rfc_rsa_jwk());
    assert!(key.verify(RFC7515_A2_TOKEN, &rfc_time()).is_ok());
    // Exponent bounds: odd, at least 3, below 2^33.
    let n = unb64(RFC7515_A2_N);
    assert!(Jwk::from_rsa_components(&n, &[1, 0xff, 0xff, 0xff, 0xff]).is_ok());
    for e in [
        &[][..],
        &[0][..],
        &[1][..],
        &[2][..],
        &[1, 0, 0][..],
        &[2, 0, 0, 0, 1][..],
        &[1, 0, 0, 0, 0, 1][..],
    ] {
        assert_eq!(
            Jwk::from_rsa_components(&n, e),
            Err(JwtError::InvalidKey),
            "{e:?}"
        );
    }
    assert_eq!(
        Jwk::from_rsa_components(&[0xff; 255], &[1, 0, 1]),
        Err(JwtError::InvalidKey)
    );
    assert_eq!(
        Jwk::from_rsa_components(&[0xff; 1025], &[1, 0, 1]),
        Err(JwtError::InvalidKey)
    );
}

// ---- key sets ----------------------------------------------------------------

fn mixed_jwks_json() -> String {
    format!(
        r#"{{"keys":[
            {{"kty":"RSA","kid":"rsa-1","use":"sig","alg":"RS256","n":"{OPENSSL_RSA_N}","e":"AQAB"}},
            {{"kty":"EC","crv":"P-256","kid":"ec-1","use":"sig","x":"{OPENSSL_EC_X}","y":"{OPENSSL_EC_Y}"}},
            {{"kty":"RSA","kid":"rfc-rsa","n":"{RFC7515_A2_N}","e":"AQAB"}},
            {{"kty":"EC","crv":"P-256","kid":"rfc-ec","x":"{RFC7515_A3_X}","y":"{RFC7515_A3_Y}"}},
            {{"kty":"RSA","kid":"enc-1","use":"enc","n":"{OPENSSL_RSA_N}","e":"AQAB"}},
            {{"kty":"RSA","kid":"small","n":"{OPENSSL_RSA_1024_N}","e":"AQAB"}},
            {{"kty":"EC","crv":"P-384","kid":"p384","x":"AQAB","y":"AQAB"}},
            {{"kty":"OKP","crv":"Ed25519","kid":"ed","x":"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"}},
            "not an object"
        ]}}"#
    )
}

#[test]
fn jwks_parses_and_skips_unsupported_entries() {
    let jwks = Jwks::from_json(&mixed_jwks_json()).unwrap();
    assert_eq!(jwks.len(), 4);
    assert_eq!(jwks.skipped(), 5);
    assert!(!jwks.is_empty());
    assert_eq!(jwks.keys().len(), 4);
    assert_eq!(jwks.find("rsa-1").unwrap().key_type(), KeyType::Rsa);
    assert_eq!(jwks.find("ec-1").unwrap().key_type(), KeyType::EcP256);
    assert!(jwks.find("enc-1").is_none());
    assert!(jwks.find("small").is_none());
    assert!(jwks.find("nope").is_none());

    assert_eq!(Jwks::from_json(r#"{"keys":[]}"#).unwrap().len(), 0);
    for bad in ["[]", "{}", r#"{"keys":{}}"#, r#"{"keys":"x"}"#, "nonsense"] {
        assert_eq!(Jwks::from_json(bad), Err(JwtError::InvalidKey), "{bad}");
    }
}

#[test]
fn jwks_selects_the_key_by_kid_and_algorithm() {
    let jwks = Jwks::from_json(&mixed_jwks_json()).unwrap();
    // Tokens with a kid.
    assert!(jwks.verify(OPENSSL_RSA_TOKEN, &Validation::new()).is_ok());
    assert!(jwks
        .verify(OPENSSL_EC_TOKEN_HIGH_BIT, &Validation::new())
        .is_ok());
    // Tokens without a kid (the RFC examples) are tried against every key of
    // the matching type.
    assert!(jwks.verify(RFC7515_A2_TOKEN, &rfc_time()).is_ok());
    assert!(jwks.verify(RFC7515_A3_TOKEN, &rfc_time()).is_ok());

    // An unknown kid.
    let (_, p, s) = segments(OPENSSL_RSA_TOKEN);
    let unknown = format!(
        "{}.{p}.{s}",
        b64(br#"{"alg":"RS256","kid":"rotated-away"}"#)
    );
    assert_eq!(
        jwks.verify(&unknown, &Validation::new()),
        Err(JwtError::UnknownKey)
    );
    // A kid that exists but names a key of the other type.
    let cross = format!("{}.{p}.{s}", b64(br#"{"alg":"RS256","kid":"ec-1"}"#));
    assert_eq!(
        jwks.verify(&cross, &Validation::new()),
        Err(JwtError::UnknownKey)
    );
    // A kid that names another RSA key: found, but the signature is not its.
    let wrong = format!("{}.{p}.{s}", b64(br#"{"alg":"RS256","kid":"rfc-rsa"}"#));
    assert_eq!(
        jwks.verify(&wrong, &Validation::new()),
        Err(JwtError::InvalidSignature)
    );
    // No key of the right type at all.
    let only_ec = Jwks::from_keys(vec![openssl_ec_jwk()]);
    assert_eq!(
        only_ec.verify(RFC7515_A2_TOKEN, &rfc_time()),
        Err(JwtError::UnknownKey)
    );
    // Without a kid and with no key that verifies.
    let only_other_rsa = Jwks::from_keys(vec![openssl_rsa_jwk()]);
    assert_eq!(
        only_other_rsa.verify(RFC7515_A2_TOKEN, &rfc_time()),
        Err(JwtError::InvalidSignature)
    );
    assert_eq!(
        Jwks::default().verify(RFC7515_A2_TOKEN, &rfc_time()),
        Err(JwtError::UnknownKey)
    );
}

// ---- remote key sets ---------------------------------------------------------

/// A JWKS endpoint whose body and status can be swapped, counting requests.
struct JwksEndpoint {
    body: Mutex<String>,
    status: AtomicUsize,
    hits: AtomicUsize,
}

impl HttpHandler for JwksEndpoint {
    fn handle(&self, _req: Request) -> BoxFuture<'_, HttpResponse> {
        Box::pin(async move {
            self.hits.fetch_add(1, Ordering::SeqCst);
            let status = StatusCode(self.status.load(Ordering::SeqCst) as u16);
            HttpResponse::new(status)
                .header("content-type", "application/json")
                .body(self.body.lock().unwrap().clone())
        })
    }
}

async fn spawn_endpoint(body: String) -> (Arc<JwksEndpoint>, String) {
    let endpoint = Arc::new(JwksEndpoint {
        body: Mutex::new(body),
        status: AtomicUsize::new(200),
        hits: AtomicUsize::new(0),
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/jwks.json", listener.local_addr().unwrap());
    let handler: Arc<dyn HttpHandler> = endpoint.clone();
    tokio::spawn(async move {
        let _ = serve(listener, handler, std::future::pending::<()>()).await;
    });
    (endpoint, url)
}

fn rsa_only_jwks() -> String {
    format!(r#"{{"keys":[{{"kty":"RSA","kid":"rsa-1","n":"{OPENSSL_RSA_N}","e":"AQAB"}}]}}"#)
}

fn rsa_and_ec_jwks() -> String {
    format!(
        r#"{{"keys":[
            {{"kty":"RSA","kid":"rsa-1","n":"{OPENSSL_RSA_N}","e":"AQAB"}},
            {{"kty":"EC","crv":"P-256","kid":"ec-1","x":"{OPENSSL_EC_X}","y":"{OPENSSL_EC_Y}"}}
        ]}}"#
    )
}

#[tokio::test]
async fn remote_jwks_fetches_once_and_caches() {
    let (endpoint, url) = spawn_endpoint(rsa_only_jwks()).await;
    let remote = RemoteJwks::new(url);
    for _ in 0..5 {
        let claims = remote
            .verify(OPENSSL_RSA_TOKEN, &Validation::new())
            .await
            .unwrap();
        assert_eq!(claims.get("sub").and_then(Value::as_str), Some("user-1"));
    }
    assert_eq!(remote.get().await.unwrap().len(), 1);
    assert_eq!(endpoint.hits.load(Ordering::SeqCst), 1);
    // A bad signature is not a reason to re-fetch.
    let forged = with_flipped_signature_bit(OPENSSL_RSA_TOKEN, 7);
    assert_eq!(
        remote.verify(&forged, &Validation::new()).await,
        Err(JwtError::InvalidSignature)
    );
    assert_eq!(endpoint.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn remote_jwks_concurrent_first_use_shares_one_fetch() {
    let (endpoint, url) = spawn_endpoint(rsa_only_jwks()).await;
    let remote = Arc::new(RemoteJwks::new(url));
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let remote = remote.clone();
        tasks.push(tokio::spawn(async move {
            remote.verify(OPENSSL_RSA_TOKEN, &Validation::new()).await
        }));
    }
    for task in tasks {
        assert!(task.await.unwrap().is_ok());
    }
    assert_eq!(endpoint.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn remote_jwks_refetches_on_unknown_kid_after_rotation() {
    let (endpoint, url) = spawn_endpoint(rsa_only_jwks()).await;
    let remote = RemoteJwks::new(url).min_refresh_interval(Duration::ZERO);
    assert!(remote
        .verify(OPENSSL_RSA_TOKEN, &Validation::new())
        .await
        .is_ok());
    assert_eq!(endpoint.hits.load(Ordering::SeqCst), 1);

    // The issuer starts signing with a new key the cache has not seen.
    *endpoint.body.lock().unwrap() = rsa_and_ec_jwks();
    assert!(remote
        .verify(OPENSSL_EC_TOKEN_PLAIN, &Validation::new())
        .await
        .is_ok());
    assert_eq!(endpoint.hits.load(Ordering::SeqCst), 2);
    // Now cached again.
    assert!(remote
        .verify(OPENSSL_EC_TOKEN_SHORT_R, &Validation::new())
        .await
        .is_ok());
    assert_eq!(endpoint.hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn remote_jwks_rate_limits_refetches_for_unknown_kids() {
    let (endpoint, url) = spawn_endpoint(rsa_only_jwks()).await;
    // The default 30 s minimum spacing applies.
    let remote = RemoteJwks::new(url);
    assert!(remote.get().await.is_ok());
    for _ in 0..20 {
        assert_eq!(
            remote
                .verify(OPENSSL_EC_TOKEN_PLAIN, &Validation::new())
                .await,
            Err(JwtError::UnknownKey)
        );
    }
    // Twenty unknown-kid tokens caused no extra request to the issuer.
    assert_eq!(endpoint.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn remote_jwks_refreshes_after_the_interval() {
    let (endpoint, url) = spawn_endpoint(rsa_only_jwks()).await;
    let remote = RemoteJwks::new(url)
        .refresh_interval(Duration::from_millis(40))
        .min_refresh_interval(Duration::ZERO);
    assert_eq!(remote.get().await.unwrap().len(), 1);
    assert_eq!(endpoint.hits.load(Ordering::SeqCst), 1);

    // Once the set is older than the refresh interval, the next use re-fetches.
    *endpoint.body.lock().unwrap() = rsa_and_ec_jwks();
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(remote.get().await.unwrap().len(), 2);
    assert_eq!(endpoint.hits.load(Ordering::SeqCst), 2);

    // An explicit refresh always fetches.
    assert_eq!(remote.refresh().await.unwrap().len(), 2);
    assert_eq!(endpoint.hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn remote_jwks_keeps_serving_the_cached_set_when_refresh_fails() {
    let (endpoint, url) = spawn_endpoint(rsa_only_jwks()).await;
    let remote = RemoteJwks::new(url)
        .refresh_interval(Duration::from_millis(20))
        .min_refresh_interval(Duration::ZERO);
    assert!(remote
        .verify(OPENSSL_RSA_TOKEN, &Validation::new())
        .await
        .is_ok());

    for (status, body) in [
        (503usize, "upstream down".to_string()),
        (200, "<html>maintenance</html>".to_string()),
        (200, r#"{"keys":[]}"#.to_string()),
        (200, r#"{"keys":[{"kty":"oct","k":"AQAB"}]}"#.to_string()),
    ] {
        endpoint.status.store(status, Ordering::SeqCst);
        *endpoint.body.lock().unwrap() = body;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let before = endpoint.hits.load(Ordering::SeqCst);
        // The refresh is attempted, fails, and the previous keys still work.
        assert!(remote
            .verify(OPENSSL_RSA_TOKEN, &Validation::new())
            .await
            .is_ok());
        assert!(endpoint.hits.load(Ordering::SeqCst) > before);
        assert!(matches!(remote.refresh().await, Err(JwtError::KeyFetch(_))));
    }
}

#[tokio::test]
async fn remote_jwks_first_fetch_failure_is_an_error() {
    let (endpoint, url) = spawn_endpoint(rsa_only_jwks()).await;
    endpoint.status.store(500, Ordering::SeqCst);
    let remote = RemoteJwks::new(url).min_refresh_interval(Duration::ZERO);
    match remote.verify(OPENSSL_RSA_TOKEN, &Validation::new()).await {
        Err(JwtError::KeyFetch(reason)) => assert!(reason.contains("500"), "{reason}"),
        other => panic!("expected a fetch error, got {other:?}"),
    }
    // Recovers once the endpoint does.
    endpoint.status.store(200, Ordering::SeqCst);
    assert!(remote
        .verify(OPENSSL_RSA_TOKEN, &Validation::new())
        .await
        .is_ok());

    // An oversized body is refused.
    let (_endpoint, url) = spawn_endpoint(rsa_and_ec_jwks()).await;
    let tiny = RemoteJwks::new(url).max_response_bytes(64);
    assert!(matches!(tiny.get().await, Err(JwtError::KeyFetch(_))));

    // Nothing listening / not a URL.
    let dead = RemoteJwks::new("http://127.0.0.1:1/jwks.json")
        .with_client(Client::new().with_timeout(Some(Duration::from_secs(5))));
    assert!(matches!(dead.get().await, Err(JwtError::KeyFetch(_))));
    assert!(matches!(
        RemoteJwks::new("not a url").get().await,
        Err(JwtError::KeyFetch(_))
    ));
}
