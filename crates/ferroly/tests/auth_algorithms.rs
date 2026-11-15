#![cfg(all(feature = "auth", feature = "http"))]
// Without a provider only the EdDSA and replay tests run; the RSA/EC fixtures sit idle.
#![cfg_attr(not(feature = "tls-ring"), allow(dead_code))]
//! The asymmetric JWT algorithms added after RS256 / ES256 — RS384, RS512,
//! PS256, PS384, PS512, ES384 (through the rustls provider) and EdDSA
//! (through `ferroly::sig`) — and `jti` replay detection.
//!
//! RSA and P-384 fixtures were generated and signed with the LibreSSL 3.3
//! command-line tool (PSS with a salt as long as the hash, as RFC 7518 §3.5
//! requires); the private keys were discarded. The EdDSA fixture is the
//! RFC 8037 appendix A key (RFC 8032 TEST 1), signed by an independent
//! big-integer RFC 8032 implementation. All tokens carry
//! `{"iss":"https://issuer.example","aud":"orders-api","sub":"user-1",
//! "jti":"tok-1","exp":4102444800}`.

use std::sync::Arc;

use ferroly::auth::{
    decode_hs256_with, encode_hmac, Algorithm, Jwk, Jwks, JwtError, KeyType, MemoryReplayCache,
    ReplayCache, Validation,
};
use ferroly::codec::Value;
use ferroly::hash::base64;

/// An RSA-2048 modulus (`e` = 65537), `kid` "rsa-2".
const RSA_N: &str = "scf8-LzGSlodf-A0gbfThaLWGWhWeNEV68YVbyzTm3R568aoWHkNrr6LHYRFYKyI\
     8kePOEv1hwh7B3vDCY66ZCveEl5qNYeWoigNNDhNgNWiqcP1HnWw7r54aaLBzZIt\
     oL78BvxqpidRDCa6sV1yYXL5VT2iKrO3cN2OwQF1TgdYYJsLHi45oC3stBfZxua6\
     3NYJ0lIQKwq5mrBZ8_-SFrUICbgAmLJH0yICTTJ2iFWVXFNfvGMlnyNgeatCNKzG\
     Yv60wZQCU_Gcmo29yTZYEYcKXn-LQSBFz0IDiZekgIANWuVQogKLpjyxyKU13Fci\
     SkUggqnX9euT9SWCVPd6NQ";
const RS384_TOKEN: &str = "eyJhbGciOiJSUzM4NCIsInR5cCI6IkpXVCIsImtpZCI6InJzYS0yIn0.eyJpc3Mi\
     OiJodHRwczovL2lzc3Vlci5leGFtcGxlIiwiYXVkIjoib3JkZXJzLWFwaSIsInN1\
     YiI6InVzZXItMSIsImp0aSI6InRvay0xIiwiZXhwIjo0MTAyNDQ0ODAwfQ.eVnCd\
     3Czjw1JIyn6a5wMv-pYX9E8Q4puJIpuiNuu54hPXKy1IhdJpPqTpuGfzPciq5in1\
     h3v4yeGS4VaPZd7LuUPbyoHTvoaRqGLuYfbIQ7m257HqhzuEHOG38i8Z8R7BKZ-5\
     e3F5EI8TI004PCx7MKLbgeAS0bEyrQoU8qzvthmXnEKlnxT-vsjkPrNjhYtHuDkK\
     zk2zMwAUYUM6ZRmNxJE05UA4oPyZjVhJ_6Ni9odpYxrvSgSouKLMLSooKtBvoL-n\
     7qo6bRHUBt2nQM6I8_KR5Sln6_WI-4ZJt7k6YPIFAcrcI_mWw1WJym52Z_Xvfwjo\
     DExwrgniXNxDblOKw";
const RS512_TOKEN: &str = "eyJhbGciOiJSUzUxMiIsInR5cCI6IkpXVCIsImtpZCI6InJzYS0yIn0.eyJpc3Mi\
     OiJodHRwczovL2lzc3Vlci5leGFtcGxlIiwiYXVkIjoib3JkZXJzLWFwaSIsInN1\
     YiI6InVzZXItMSIsImp0aSI6InRvay0xIiwiZXhwIjo0MTAyNDQ0ODAwfQ.HjuSk\
     XwkUZlpmMfUM9tOMOYAYEZ2HSUD6Hin-2FOkrliOBDX6ys3TN37ukVbAOwtKCR-R\
     ZFzDDe60riXI0_nAqAVor2aNgbZ8FbcO84LhpfevjRWTou6ddhDqrRc19wl3XQso\
     N-J9eIGYlIl3WtmexqnlARTPZtgsNjn24IOQ9E7V-isi0X4j22PIKrUYWQwZreh0\
     1NlykjPY3htBcmH3kWeC5hx3mUlwftMmZRNjjEWwMDNr9IdDr7U-ow6Tt5A8faD-\
     QFzPPTr7fwu0eangsvhjJGAPzA2RPbm4AuOeYsF6X8S7d4o-mkwKZ72mERO7zfyE\
     6t6_v1ArNAfMqCO2w";
const PS256_TOKEN: &str = "eyJhbGciOiJQUzI1NiIsInR5cCI6IkpXVCIsImtpZCI6InJzYS0yIn0.eyJpc3Mi\
     OiJodHRwczovL2lzc3Vlci5leGFtcGxlIiwiYXVkIjoib3JkZXJzLWFwaSIsInN1\
     YiI6InVzZXItMSIsImp0aSI6InRvay0xIiwiZXhwIjo0MTAyNDQ0ODAwfQ.T_0Rw\
     8_dTyMlbIydX3KvilR1pHV4AOzlef7Jmjs3--jnYY-nR7iWZTaE2aQYVQ1Vw7l5H\
     -jSAjmIs14GngeXOh7dciurW5cCIuw5vy_HKuF0DbhjY5xGjUQnH0dG6n35wD2dW\
     CMrXfYZlgJNAOoBI7XryF2KXNycXo3jYAv0QyhqH5rDBsatq-sgDHEztMwxmTNeZ\
     nryYQydc7BEUFjF_jNqGqRdKYdfvz-zwESCgBYpTHXuKK7AcEx583RcRry9siWBO\
     8v3rAz3muH9vsM43Y10rn4ek-FaHVwPJ6c9IpSk6NHFkLpp56s7hLDnudLwjXc7L\
     SjJ2cDasXJ6e370xQ";
const PS384_TOKEN: &str = "eyJhbGciOiJQUzM4NCIsInR5cCI6IkpXVCIsImtpZCI6InJzYS0yIn0.eyJpc3Mi\
     OiJodHRwczovL2lzc3Vlci5leGFtcGxlIiwiYXVkIjoib3JkZXJzLWFwaSIsInN1\
     YiI6InVzZXItMSIsImp0aSI6InRvay0xIiwiZXhwIjo0MTAyNDQ0ODAwfQ.LtOgd\
     5JtFz_cMzQqridybyDS2lEc_g7e3P1iGju8wsRG6UoQ94vtiZogATQQtF7ugInhk\
     t2Sp6lJg_1mij4zHmrFtnLrLs53cyBWHK0XDXrzuRrs_yXmEgUw4qzcGTUukkgwK\
     CaIOjBLxrQvzjRaDbO-YPbFvVOaaNGH2sVbPPGcNxugBxiJZdX3z6XulutTnHOFf\
     rVpB7bGxMGdYHKjoGq4aVgPjHM8Cm-A698IT97q01hTix7TE72B3wPkK1xwYpoIa\
     f2KBVcS2cWLlOnlZlPoHiBs-aSUO_6n67YjfgFgB86pSfmUN1gCOm4Iq76RKgrW_\
     uGPB1EQmKjKgTQy8Q";
const PS512_TOKEN: &str = "eyJhbGciOiJQUzUxMiIsInR5cCI6IkpXVCIsImtpZCI6InJzYS0yIn0.eyJpc3Mi\
     OiJodHRwczovL2lzc3Vlci5leGFtcGxlIiwiYXVkIjoib3JkZXJzLWFwaSIsInN1\
     YiI6InVzZXItMSIsImp0aSI6InRvay0xIiwiZXhwIjo0MTAyNDQ0ODAwfQ.FJGCk\
     l77yepgOD_BdZZpLN98Wk0Sq0e-YyDdmoKLJ1wHy4pDN6Yt0AZgcpj-DV9x4Kt9f\
     v9_qUgfLnCw7aZCvuSBrMBQ3U4AB8QjYQ1ysHaj3C3Vmct3A4r41DfLiBHUYBmkq\
     eRfsR3TfQm2zsfEg4Xooztlruvmx9ckzR6zEXffjLBneeTVofwg_P8EvIXW91cLJ\
     KmovddEk0p5LFvKQK6tOmhvFSuCW3QHnFgHfEhONzTNOtPUJPv0N0MJ3Rxg76dYa\
     FHp2YUuFfhy2DLhSgs-qn95tLhbd7iHRJyoMKHXqSyfMCjHIr7cXX3OSuDKARpIQ\
     f-qcrE4bAxl-UWVSA";
const ES384_TOKEN: &str = "eyJhbGciOiJFUzM4NCIsInR5cCI6IkpXVCIsImtpZCI6ImVjMzg0In0.eyJpc3Mi\
     OiJodHRwczovL2lzc3Vlci5leGFtcGxlIiwiYXVkIjoib3JkZXJzLWFwaSIsInN1\
     YiI6InVzZXItMSIsImp0aSI6InRvay0xIiwiZXhwIjo0MTAyNDQ0ODAwfQ.9AMAm\
     sIwe3zagmCqQb7uhFhWiYu-opBZruZIdE71vGnVVetw0lDpgw-iCQyyaMf58W1qk\
     kXjDCA-QrZ9kojZnFzp_p_ItCTufef-oPIkzRHUeRSCm99KxbWZs93teMN8";
/// The P-384 public key of the ES384 token, `kid` "ec384".
const P384_X: &str = "rdtcbhMk31uMlrIopLl0rQerfssNG4v_3lk0YJ9heoH3SIyqVgNFtLeVNE88AkBW";
const P384_Y: &str = "xMNo65S1fpiIDiPhFKrQj3dq2N9DfmqGgxK9_rElu5PxqZcB7LDWYyl9o4jSmJ5z";
/// RFC 8037 A.2: the Ed25519 public key `x` (RFC 8032 TEST 1).
const ED_X: &str = "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo";
/// An EdDSA token under that key, `kid` "ed-1".
const EDDSA_TOKEN: &str = "eyJhbGciOiJFZERTQSIsInR5cCI6IkpXVCIsImtpZCI6ImVkLTEifQ.eyJpc3MiO\
     iJodHRwczovL2lzc3Vlci5leGFtcGxlIiwiYXVkIjoib3JkZXJzLWFwaSIsInN1Y\
     iI6InVzZXItMSIsImp0aSI6InRvay0xIiwiZXhwIjo0MTAyNDQ0ODAwfQ.53mhed\
     vv6pyU8WzGUaYDyaDI2uKEw3DIWnQBaUopcnqdCeTQX9hhkcNI1iLoMvy4KQLR_A\
     W80a4L90kAe92XAg";
/// RFC 8037 A.4: a JWS over "Example of Ed25519 signing" (not a JWT: the
/// payload is not a claims object).
const RFC8037_A4: &str =
    "eyJhbGciOiJFZERTQSJ9.RXhhbXBsZSBvZiBFZDI1NTE5IHNpZ25pbmc.hgyY0il_MGCjP0Jz\
     lnLWG1PPOt7-09PGcvMg3AIbQR6dWbhijcNR4ki4iylGjg5BhVsPt9g7sVvpAr_MuM0KAg";

fn rsa_jwk() -> Jwk {
    Jwk::from_json(&format!(
        r#"{{"kty":"RSA","kid":"rsa-2","n":"{RSA_N}","e":"AQAB"}}"#
    ))
    .unwrap()
}

fn p384_jwk() -> Jwk {
    Jwk::from_json(&format!(
        r#"{{"kty":"EC","crv":"P-384","kid":"ec384","x":"{P384_X}","y":"{P384_Y}"}}"#
    ))
    .unwrap()
}

fn ed_jwk() -> Jwk {
    Jwk::from_json(&format!(
        r#"{{"kty":"OKP","crv":"Ed25519","kid":"ed-1","x":"{ED_X}"}}"#
    ))
    .unwrap()
}

fn policy() -> Validation {
    Validation::new()
        .issuer("https://issuer.example")
        .audience("orders-api")
}

fn segments(token: &str) -> (&str, &str, &str) {
    let mut it = token.split('.');
    (it.next().unwrap(), it.next().unwrap(), it.next().unwrap())
}

/// Flips one bit of the decoded signature.
fn tampered_signature(token: &str, byte: usize) -> String {
    let (h, p, s) = segments(token);
    let mut sig = base64::decode_url_no_pad(s).unwrap();
    sig[byte] ^= 0x01;
    format!("{h}.{p}.{}", base64::encode_url_no_pad(&sig))
}

fn tampered_payload(token: &str) -> String {
    let (h, _, s) = segments(token);
    let payload =
        br#"{"iss":"https://issuer.example","aud":"orders-api","sub":"admin","exp":4102444800}"#;
    format!("{h}.{}.{s}", base64::encode_url_no_pad(payload))
}

#[cfg(feature = "tls-ring")]
#[test]
fn rsa_pkcs1_and_pss_tokens_verify_through_the_provider() {
    let key = rsa_jwk();
    assert_eq!(key.key_type(), KeyType::Rsa);
    assert_eq!(key.algorithm(), Algorithm::Rs256);
    for (token, alg) in [
        (RS384_TOKEN, Algorithm::Rs384),
        (RS512_TOKEN, Algorithm::Rs512),
        (PS256_TOKEN, Algorithm::Ps256),
        (PS384_TOKEN, Algorithm::Ps384),
        (PS512_TOKEN, Algorithm::Ps512),
    ] {
        assert!(key.accepts(alg));
        let claims = key.verify(token, &policy()).unwrap();
        assert_eq!(
            claims.get("sub").and_then(Value::as_str),
            Some("user-1"),
            "{alg}"
        );
        assert_eq!(
            key.verify(&tampered_payload(token), &policy()),
            Err(JwtError::InvalidSignature),
            "{alg}"
        );
        for byte in [0, 100, 255] {
            assert_eq!(
                key.verify(&tampered_signature(token, byte), &policy()),
                Err(JwtError::InvalidSignature),
                "{alg} byte {byte}"
            );
        }
        // A key pinned to another algorithm refuses the token outright.
        let pinned = rsa_jwk().with_algorithm(Algorithm::Rs256).unwrap();
        assert_eq!(
            pinned.verify(token, &policy()),
            Err(JwtError::UnsupportedAlg),
            "{alg}"
        );
        let declared = Jwk::from_json(&format!(
            r#"{{"kty":"RSA","alg":"{}","n":"{RSA_N}","e":"AQAB"}}"#,
            alg.as_str()
        ))
        .unwrap();
        assert_eq!(declared.algorithms(), &[alg]);
        assert!(declared.verify(token, &policy()).is_ok(), "{alg}");
    }
    // A PSS signature is not a PKCS#1 v1.5 signature (and vice versa): only
    // the header's algorithm is tried.
    let (_, p, s) = segments(PS256_TOKEN);
    let relabelled = format!(
        "{}.{p}.{s}",
        base64::encode_url_no_pad(br#"{"alg":"RS256","typ":"JWT","kid":"rsa-2"}"#)
    );
    assert_eq!(
        key.verify(&relabelled, &policy()),
        Err(JwtError::InvalidSignature)
    );
    // And an RSA key never verifies EC or EdDSA.
    assert!(!key.accepts(Algorithm::Es384));
    assert!(!key.accepts(Algorithm::EdDsa));
    assert_eq!(
        key.with_algorithm(Algorithm::Es256),
        Err(JwtError::UnsupportedAlg)
    );
}

#[cfg(feature = "tls-ring")]
#[test]
fn es384_tokens_verify_through_the_provider() {
    let key = p384_jwk();
    assert_eq!(key.key_type(), KeyType::EcP384);
    assert_eq!(key.algorithms(), &[Algorithm::Es384]);
    let claims = key.verify(ES384_TOKEN, &policy()).unwrap();
    assert_eq!(claims.get("jti").and_then(Value::as_str), Some("tok-1"));
    assert_eq!(
        key.verify(&tampered_payload(ES384_TOKEN), &policy()),
        Err(JwtError::InvalidSignature)
    );
    for byte in [0, 47, 48, 95] {
        assert_eq!(
            key.verify(&tampered_signature(ES384_TOKEN, byte), &policy()),
            Err(JwtError::InvalidSignature),
            "byte {byte}"
        );
    }
    // r || s must be exactly 2 x 48 bytes.
    let (h, p, s) = segments(ES384_TOKEN);
    let sig = base64::decode_url_no_pad(s).unwrap();
    for bad in [&sig[..64], &sig[..95], &[&sig[..], &[0]].concat()[..]] {
        let token = format!("{h}.{p}.{}", base64::encode_url_no_pad(bad));
        assert_eq!(
            key.verify(&token, &policy()),
            Err(JwtError::InvalidSignature)
        );
    }
    // A P-256 key is not a P-384 key.
    let p256 = Jwk::from_ec_coordinates(KeyType::EcP256, &[1; 32], &[2; 32]).unwrap();
    assert_eq!(
        p256.verify(ES384_TOKEN, &policy()),
        Err(JwtError::UnsupportedAlg)
    );
    // Coordinates of the wrong size for the curve, and a non-EC key type.
    assert_eq!(
        Jwk::from_ec_coordinates(KeyType::EcP384, &[1; 32], &[2; 32]),
        Err(JwtError::InvalidKey)
    );
    assert_eq!(
        Jwk::from_ec_coordinates(KeyType::Rsa, &[1; 32], &[2; 32]),
        Err(JwtError::UnsupportedAlg)
    );
}

#[cfg(feature = "tls-ring")]
#[test]
fn es512_needs_a_provider_with_p521() {
    // ring has no P-521: the key loads, verification is UnsupportedAlg (not
    // a panic, and not InvalidSignature).
    let key = Jwk::from_json(&format!(
        r#"{{"kty":"EC","crv":"P-521","x":"{x}","y":"{x}"}}"#,
        x = base64::encode_url_no_pad(&[1; 66])
    ))
    .unwrap();
    assert_eq!(key.key_type(), KeyType::EcP521);
    assert_eq!(key.algorithm(), Algorithm::Es512);
    assert_eq!(
        key.verify_signature(b"m", &[1; 132]),
        Err(JwtError::UnsupportedAlg)
    );
    assert_eq!(
        key.verify_signature(b"m", &[1; 64]),
        Err(JwtError::InvalidSignature)
    );
}

#[test]
fn eddsa_rfc8037_example_verifies() {
    let key = Jwk::from_json(&format!(r#"{{"kty":"OKP","crv":"Ed25519","x":"{ED_X}"}}"#)).unwrap();
    assert_eq!(key.key_type(), KeyType::Ed25519);
    assert_eq!(key.algorithm(), Algorithm::EdDsa);
    let (h, p, s) = segments(RFC8037_A4);
    let sig = base64::decode_url_no_pad(s).unwrap();
    let message = format!("{h}.{p}");
    assert_eq!(key.verify_signature(message.as_bytes(), &sig), Ok(()));
    assert_eq!(
        key.verify_signature(b"another message", &sig),
        Err(JwtError::InvalidSignature)
    );
    assert_eq!(
        key.verify_signature(message.as_bytes(), &sig[..63]),
        Err(JwtError::InvalidSignature)
    );
    // The payload is not a claims object, so as a JWT it is malformed --
    // after the signature verified.
    assert_eq!(
        key.verify(RFC8037_A4, &Validation::new()),
        Err(JwtError::Malformed)
    );
}

#[test]
fn eddsa_tokens_verify_without_a_crypto_provider() {
    // EdDSA goes through ferroly::sig, so this passes with or without
    // `tls-ring`.
    let key = ed_jwk();
    let claims = key.verify(EDDSA_TOKEN, &policy()).unwrap();
    assert_eq!(claims.get("sub").and_then(Value::as_str), Some("user-1"));
    assert_eq!(
        key.verify(&tampered_payload(EDDSA_TOKEN), &policy()),
        Err(JwtError::InvalidSignature)
    );
    for byte in [0, 31, 32, 63] {
        assert_eq!(
            key.verify(&tampered_signature(EDDSA_TOKEN, byte), &policy()),
            Err(JwtError::InvalidSignature),
            "byte {byte}"
        );
    }
    // Claims are still validated.
    assert_eq!(
        key.verify(EDDSA_TOKEN, &Validation::new().audience("billing-api")),
        Err(JwtError::InvalidAudience)
    );
    // From the typed sig key too.
    let raw = base64::decode_url_no_pad(ED_X).unwrap();
    let typed = Jwk::from(ferroly::sig::Ed25519PublicKey::from_slice(&raw).unwrap());
    assert!(typed.verify(EDDSA_TOKEN, &policy()).is_ok());
    assert!(Jwk::from_ed25519_bytes(&raw)
        .unwrap()
        .verify(EDDSA_TOKEN, &policy())
        .is_ok());
    // A non-canonical or small-order key is refused when loading.
    assert_eq!(Jwk::from_ed25519_bytes(&[0; 32]), Err(JwtError::InvalidKey));
    assert_eq!(
        Jwk::from_ed25519_bytes(&raw[..31]),
        Err(JwtError::InvalidKey)
    );
    // An Ed25519 key verifies nothing else.
    assert!(!key.accepts(Algorithm::Es256));
    let (_, p, s) = segments(EDDSA_TOKEN);
    let relabelled = format!(
        "{}.{p}.{s}",
        base64::encode_url_no_pad(br#"{"alg":"ES256"}"#)
    );
    assert_eq!(
        key.verify(&relabelled, &policy()),
        Err(JwtError::UnsupportedAlg)
    );
}

#[test]
fn a_key_set_selects_eddsa_keys_by_kid() {
    let jwks = Jwks::from_json(&format!(
        r#"{{"keys":[
            {{"kty":"OKP","crv":"Ed25519","kid":"other","x":"{other}"}},
            {{"kty":"OKP","crv":"Ed25519","kid":"ed-1","use":"sig","alg":"EdDSA","x":"{ED_X}"}},
            {{"kty":"OKP","crv":"Ed448","kid":"ed448","x":"AQAB"}}
        ]}}"#,
        other = base64::encode_url_no_pad(&[
            0x3d, 0x40, 0x17, 0xc3, 0xe8, 0x43, 0x89, 0x5a, 0x92, 0xb7, 0x0a, 0xa7, 0x4d, 0x1b,
            0x7e, 0xbc, 0x9c, 0x98, 0x2c, 0xcf, 0x2e, 0xc4, 0x96, 0x8c, 0xc0, 0xcd, 0x55, 0xf1,
            0x2a, 0xf4, 0x66, 0x0c,
        ])
    ))
    .unwrap();
    assert_eq!((jwks.len(), jwks.skipped()), (2, 1));
    assert!(jwks.verify(EDDSA_TOKEN, &policy()).is_ok());
    // Without the right kid in the set.
    let only_other = Jwks::from_keys(vec![jwks.find("other").unwrap().clone()]);
    assert_eq!(
        only_other.verify(EDDSA_TOKEN, &policy()),
        Err(JwtError::UnknownKey)
    );
}

// ---- jti replay ----------------------------------------------------------------

#[test]
fn a_replay_cache_accepts_each_jti_once() {
    let cache = Arc::new(MemoryReplayCache::new(100));
    let validation = policy().replay_cache(cache.clone());
    let key = ed_jwk();
    assert!(key.verify(EDDSA_TOKEN, &validation).is_ok());
    assert_eq!(
        key.verify(EDDSA_TOKEN, &validation),
        Err(JwtError::Replayed)
    );
    // Clones of the policy share the cache.
    assert_eq!(
        key.verify(EDDSA_TOKEN, &validation.clone()),
        Err(JwtError::Replayed)
    );
    assert_eq!(cache.len(), 1);
    // Without the cache the token is fine again (the cache is opt-in).
    assert!(key.verify(EDDSA_TOKEN, &policy()).is_ok());
}

#[test]
fn a_rejected_token_does_not_consume_its_jti() {
    let cache = Arc::new(MemoryReplayCache::new(100));
    let key = ed_jwk();
    // A tampered token and a token failing a later claim check are refused
    // before the cache is consulted...
    let strict = policy().replay_cache(cache.clone());
    assert_eq!(
        key.verify(&tampered_signature(EDDSA_TOKEN, 3), &strict),
        Err(JwtError::InvalidSignature)
    );
    let wrong_audience = Validation::new()
        .audience("billing-api")
        .replay_cache(cache.clone());
    assert_eq!(
        key.verify(EDDSA_TOKEN, &wrong_audience),
        Err(JwtError::InvalidAudience)
    );
    assert!(cache.is_empty());
    // ...so the genuine token is still accepted once.
    assert!(key.verify(EDDSA_TOKEN, &strict).is_ok());
    assert_eq!(key.verify(EDDSA_TOKEN, &strict), Err(JwtError::Replayed));
}

#[test]
fn the_replay_check_requires_a_string_jti_and_keys_by_issuer() {
    let secret = [9u8; 32];
    let cache = Arc::new(MemoryReplayCache::new(100));
    let validation = Validation::new()
        .replay_cache(cache.clone())
        .current_time(1_000);
    let token = |claims: Vec<(&str, Value)>| {
        let claims = Value::Object(
            claims
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        );
        encode_hmac(Algorithm::Hs256, &claims, &secret).unwrap()
    };
    let exp = ("exp", Value::Int(2_000));
    assert_eq!(
        decode_hs256_with(&token(vec![exp.clone()]), &secret, &validation),
        Err(JwtError::MissingClaim("jti".into()))
    );
    assert_eq!(
        decode_hs256_with(
            &token(vec![exp.clone(), ("jti", Value::Int(7))]),
            &secret,
            &validation
        ),
        Err(JwtError::Malformed)
    );
    // The same jti from two issuers are two tokens.
    let a = token(vec![exp.clone(), ("jti", "j".into()), ("iss", "a".into())]);
    let b = token(vec![exp.clone(), ("jti", "j".into()), ("iss", "b".into())]);
    assert!(decode_hs256_with(&a, &secret, &validation).is_ok());
    assert!(decode_hs256_with(&b, &secret, &validation).is_ok());
    assert_eq!(
        decode_hs256_with(&a, &secret, &validation),
        Err(JwtError::Replayed)
    );
    // Once the token has expired the entry is forgotten -- but so is the
    // token, which `exp` now rejects first.
    let later = Validation::new()
        .replay_cache(cache.clone())
        .current_time(2_000 + 61);
    assert_eq!(
        decode_hs256_with(&a, &secret, &later),
        Err(JwtError::Expired)
    );
}

/// A custom shared-store implementation sees the issuer, the jti and the
/// first second the token is no longer accepted: `ceil(exp + leeway)`, or
/// the earlier `floor(iat + max_age + leeway) + 1`.
#[test]
fn a_custom_replay_cache_receives_the_token_lifetime() {
    use std::sync::Mutex;
    /// (issuer, jti, expires_at, now) per call.
    type Call = (Option<String>, String, u64, u64);
    #[derive(Default)]
    struct Recorder(Mutex<Vec<Call>>);
    impl ReplayCache for Recorder {
        fn check_and_record(&self, iss: Option<&str>, jti: &str, exp: u64, now: u64) -> bool {
            self.0
                .lock()
                .unwrap()
                .push((iss.map(str::to_string), jti.to_string(), exp, now));
            true
        }
    }
    let recorder = Arc::new(Recorder::default());
    let secret = [9u8; 32];
    let claims = Value::Object(vec![
        ("jti".into(), "abc".into()),
        ("iat".into(), Value::Int(1_000)),
        ("exp".into(), Value::Float(5_000.5)),
    ]);
    let token = encode_hmac(Algorithm::Hs256, &claims, &secret).unwrap();
    let base = Validation::new()
        .replay_cache(recorder.clone())
        .current_time(1_100);
    decode_hs256_with(&token, &secret, &base).unwrap();
    decode_hs256_with(&token, &secret, &base.clone().max_age(600)).unwrap();
    let calls = recorder.0.lock().unwrap().clone();
    assert_eq!(
        calls,
        [
            (None, "abc".to_string(), 5_061, 1_100), // ceil(5000.5 + 60)
            // Accepted while `now - iat <= 600 + 60`, so the entry must
            // still be there at 1660 and may go at 1661.
            (None, "abc".to_string(), 1_661, 1_100),
        ]
    );
}

/// SEC-1: the maximum-age bound is inclusive, so the token is still valid
/// at exactly `iat + max_age + leeway`, and so must its replay entry be.
#[test]
fn a_replay_at_the_max_age_boundary_is_detected() {
    let secret = [7u8; 32];
    let claims = Value::Object(vec![
        ("jti".into(), "one".into()),
        ("iat".into(), Value::Int(1_000)),
        ("exp".into(), Value::Int(5_000)),
    ]);
    let token = encode_hmac(Algorithm::Hs256, &claims, &secret).unwrap();
    let cache = Arc::new(MemoryReplayCache::new(100));
    let at = |now| {
        Validation::new()
            .leeway(0)
            .max_age(100)
            .current_time(now)
            .replay_cache(cache.clone())
    };
    assert!(decode_hs256_with(&token, &secret, &at(1_050)).is_ok());
    assert_eq!(
        decode_hs256_with(&token, &secret, &at(1_099)),
        Err(JwtError::Replayed)
    );
    for _ in 0..3 {
        assert_eq!(
            decode_hs256_with(&token, &secret, &at(1_100)),
            Err(JwtError::Replayed)
        );
    }
    assert_eq!(cache.len(), 1);
    assert_eq!(
        decode_hs256_with(&token, &secret, &at(1_101)),
        Err(JwtError::TooOld)
    );

    // First used in its last valid second, it is remembered too.
    let fresh = Arc::new(MemoryReplayCache::new(100));
    let last = Validation::new()
        .leeway(0)
        .max_age(100)
        .current_time(1_100)
        .replay_cache(fresh.clone());
    assert!(decode_hs256_with(&token, &secret, &last).is_ok());
    assert_eq!(
        decode_hs256_with(&token, &secret, &last),
        Err(JwtError::Replayed)
    );
    assert_eq!(fresh.len(), 1);
}

/// SEC-8: a token with no bounded lifetime would occupy the replay cache
/// forever, so with a replay cache it must carry `exp` (or be checked by
/// `max_age`).
#[test]
fn a_replay_cache_requires_a_bounded_lifetime() {
    let secret = [7u8; 32];
    let token = |claims: Vec<(&str, Value)>| {
        let claims = Value::Object(
            claims
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        );
        encode_hmac(Algorithm::Hs256, &claims, &secret).unwrap()
    };
    let cache = Arc::new(MemoryReplayCache::new(2));
    let lenient = Validation::new()
        .require_exp(false)
        .current_time(1_000)
        .replay_cache(cache.clone());
    let forever = token(vec![("jti", "forever".into())]);
    assert_eq!(
        decode_hs256_with(&forever, &secret, &lenient),
        Err(JwtError::MissingClaim("exp".into()))
    );
    assert!(cache.is_empty());
    // A checked maximum age bounds it instead.
    let aged = token(vec![("jti", "aged".into()), ("iat", Value::Int(990))]);
    let with_age = lenient.clone().max_age(300);
    assert!(decode_hs256_with(&aged, &secret, &with_age).is_ok());
    assert_eq!(
        decode_hs256_with(&aged, &secret, &with_age),
        Err(JwtError::Replayed)
    );
    // Without a replay cache nothing changes.
    let no_cache = Validation::new().require_exp(false).current_time(1_000);
    assert!(decode_hs256_with(&forever, &secret, &no_cache).is_ok());
}
