#![cfg(feature = "auth")]
//! HMAC JWTs (HS256 / HS384 / HS512): known answers, strict parsing (no token
//! malleability), secret handling and the `Validation` policy.

use ferroly::auth::{
    decode_header, decode_hmac_with, decode_hs256, decode_hs256_with, encode_hmac, encode_hs256,
    encode_with_header, Algorithm, Header, JwtError, Scopes, Secret, Validation, MAX_TOKEN_LEN,
    MIN_SECRET_LEN,
};
use ferroly::codec::Value;
use ferroly::hash::{base64, hmac_sha256};

// ---- fixtures ----------------------------------------------------------------

/// RFC 7515 appendix A.1: HS256 token and its symmetric key (`k`).
const RFC7515_A1_TOKEN: &str = "eyJ0eXAiOiJKV1QiLA0KICJhbGciOiJIUzI1NiJ9.eyJpc3MiOiJqb2UiLA0KICJ\
     leHAiOjEzMDA4MTkzODAsDQogImh0dHA6Ly9leGFtcGxlLmNvbS9pc19yb290Ijp\
     0cnVlfQ.dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const RFC7515_A1_KEY: &str = "AyM1SysPpbyDfgZld3umj1qzKObwVMkoqQ-EstJQLr_T-1qS0gZH75aKtMN3Yj0i\
     PS4hcgUuTwjAzZr1Z9CAow";

/// Tokens over `{"sub":"u123","exp":4102444800}` signed with CPython's
/// `hmac` module under `REFERENCE_SECRET` — an independent implementation.
const REFERENCE_SECRET: &[u8] = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const REFERENCE_HS256_TOKEN: &str =
    "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ1MTIzIiwiZXhwIjo\
     0MTAyNDQ0ODAwfQ.pd-kKcA-qBbr5fL3TJKkA9ezkpJYdZUhwLX9DV35cPU";
const REFERENCE_HS384_TOKEN: &str =
    "eyJhbGciOiJIUzM4NCIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ1MTIzIiwiZXhwIjo\
     0MTAyNDQ0ODAwfQ.pn4MqleVc2nGI6qkdeWI7dbfL9WFnXPo2xNjRhQHQpCEn-Ps\
     I0iOYx-u8kRwK-sq";
const REFERENCE_HS512_TOKEN: &str =
    "eyJhbGciOiJIUzUxMiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ1MTIzIiwiZXhwIjo\
     0MTAyNDQ0ODAwfQ.Y850QnjmNp5G5pVBSS3mTlTnY3HGqbIFcXmwq5DaGr7Hkc2n\
     nKkBv_gvofFVBPYyIIxVGZHVCL2AtpoaU3qQ-Q";

// ---- helpers -----------------------------------------------------------------

/// A 32-byte secret (the size RFC 7518 requires for HS256).
const SECRET: &[u8] = b"an-example-secret-of-32-bytes!!!";

/// A fixed "now" so no test depends on the wall clock.
const NOW: u64 = 1_800_000_000;

fn at_now() -> Validation {
    Validation::new().leeway(0).current_time(NOW)
}

fn object(members: Vec<(&str, Value)>) -> Value {
    Value::Object(
        members
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

/// An `exp` far beyond both [`NOW`] and the real clock (2100-01-01).
const FAR_FUTURE: u64 = 4_102_444_800;

/// Claims with `exp` = [`FAR_FUTURE`], plus `extra`.
fn claims_with(extra: Vec<(&str, Value)>) -> Value {
    let mut members = vec![("exp", Value::UInt(FAR_FUTURE))];
    members.extend(extra);
    object(members)
}

fn b64(bytes: &[u8]) -> String {
    base64::encode_url_no_pad(bytes)
}

/// Signs arbitrary header and payload bytes with HS256 — what an issuer with
/// the secret (or a bug) could produce, including shapes `encode_hmac` refuses.
fn sign_raw(header: &[u8], payload: &[u8], secret: &[u8]) -> String {
    let signing_input = format!("{}.{}", b64(header), b64(payload));
    let tag = hmac_sha256(secret, signing_input.as_bytes());
    format!("{signing_input}.{}", b64(tag.as_bytes()))
}

const HS256_HEADER: &[u8] = br#"{"alg":"HS256","typ":"JWT"}"#;

fn segments(token: &str) -> (&str, &str, &str) {
    let mut it = token.split('.');
    (it.next().unwrap(), it.next().unwrap(), it.next().unwrap())
}

/// Both HS256 entry points must reject `token` with `error`.
fn assert_both_reject(token: &str, error: JwtError) {
    assert_eq!(
        decode_hs256(token, SECRET).as_ref().err(),
        Some(&error),
        "legacy: {token}"
    );
    assert_eq!(
        decode_hs256_with(token, SECRET, &at_now()).as_ref().err(),
        Some(&error),
        "validating: {token}"
    );
}

// ---- known answers -------------------------------------------------------------

#[test]
fn rfc7515_a1_hs256_token_verifies() {
    let key = base64::decode_url_no_pad(RFC7515_A1_KEY).unwrap();
    assert_eq!(key.len(), 64);
    // The example expires in March 2011.
    let then = Validation::new().leeway(0).current_time(1_300_819_380 - 60);
    let claims = decode_hs256_with(RFC7515_A1_TOKEN, &key, &then).unwrap();
    assert_eq!(claims.get("iss").and_then(Value::as_str), Some("joe"));
    assert_eq!(
        claims
            .get("http://example.com/is_root")
            .and_then(Value::as_bool),
        Some(true)
    );
    // Against today's clock it is expired, in both entry points.
    assert_eq!(decode_hs256(RFC7515_A1_TOKEN, &key), Err(JwtError::Expired));
    assert_eq!(
        decode_hs256_with(RFC7515_A1_TOKEN, &key, &Validation::new()),
        Err(JwtError::Expired)
    );
    let header = decode_header(RFC7515_A1_TOKEN).unwrap();
    assert_eq!(header.alg, Algorithm::Hs256);
    assert_eq!(header.typ.as_deref(), Some("JWT"));
    assert_eq!(header.kid, None);
}

#[test]
fn tokens_match_an_independent_hmac_implementation() {
    let claims = object(vec![
        ("sub", "u123".into()),
        ("exp", Value::Int(4_102_444_800)),
    ]);
    for (alg, reference) in [
        (Algorithm::Hs256, REFERENCE_HS256_TOKEN),
        (Algorithm::Hs384, REFERENCE_HS384_TOKEN),
        (Algorithm::Hs512, REFERENCE_HS512_TOKEN),
    ] {
        // Minting reproduces the reference token byte for byte...
        assert_eq!(
            encode_hmac(alg, &claims, REFERENCE_SECRET).unwrap(),
            reference,
            "{alg}"
        );
        // ...and the reference token verifies.
        let back = decode_hmac_with(alg, reference, REFERENCE_SECRET, &at_now()).unwrap();
        assert_eq!(back, claims, "{alg}");
        assert_eq!(decode_header(reference).unwrap().alg, alg);
    }
    assert_eq!(
        encode_hs256(&claims, REFERENCE_SECRET),
        REFERENCE_HS256_TOKEN
    );
    assert_eq!(
        decode_hs256(REFERENCE_HS256_TOKEN, REFERENCE_SECRET).unwrap(),
        claims
    );
    // Signature sizes: 32, 48 and 64 bytes.
    for (reference, len) in [
        (REFERENCE_HS256_TOKEN, 32),
        (REFERENCE_HS384_TOKEN, 48),
        (REFERENCE_HS512_TOKEN, 64),
    ] {
        let signature = base64::decode_url_no_pad(segments(reference).2).unwrap();
        assert_eq!(signature.len(), len);
    }
}

#[test]
fn hmac_algorithms_are_not_interchangeable() {
    let all = [
        (Algorithm::Hs256, REFERENCE_HS256_TOKEN),
        (Algorithm::Hs384, REFERENCE_HS384_TOKEN),
        (Algorithm::Hs512, REFERENCE_HS512_TOKEN),
    ];
    for (alg, _) in all {
        for (token_alg, token) in all {
            let outcome = decode_hmac_with(alg, token, REFERENCE_SECRET, &at_now());
            if alg == token_alg {
                assert!(outcome.is_ok());
            } else {
                assert_eq!(
                    outcome,
                    Err(JwtError::UnsupportedAlg),
                    "{alg} vs {token_alg}"
                );
            }
        }
    }
    // decode_hs256_with is HS256 only.
    assert_eq!(
        decode_hs256_with(REFERENCE_HS512_TOKEN, REFERENCE_SECRET, &at_now()),
        Err(JwtError::UnsupportedAlg)
    );
    assert_eq!(
        decode_hs256(REFERENCE_HS384_TOKEN, REFERENCE_SECRET),
        Err(JwtError::UnsupportedAlg)
    );
    // Relabelling the header keeps the old signature, which no longer fits.
    let (_, p, s) = segments(REFERENCE_HS256_TOKEN);
    let relabelled = format!("{}.{p}.{s}", b64(br#"{"alg":"HS512","typ":"JWT"}"#));
    assert_eq!(
        decode_hmac_with(Algorithm::Hs512, &relabelled, REFERENCE_SECRET, &at_now()),
        Err(JwtError::InvalidSignature)
    );
    // An allow-list narrows further.
    assert_eq!(
        decode_hs256_with(
            REFERENCE_HS256_TOKEN,
            REFERENCE_SECRET,
            &at_now().algorithms(&[Algorithm::Hs512])
        ),
        Err(JwtError::UnsupportedAlg)
    );
    assert!(decode_hs256_with(
        REFERENCE_HS256_TOKEN,
        REFERENCE_SECRET,
        &at_now().algorithms(&[Algorithm::Hs256, Algorithm::Hs512])
    )
    .is_ok());
    // The HMAC functions never accept an asymmetric algorithm.
    for alg in [Algorithm::Rs256, Algorithm::Es256] {
        assert_eq!(
            decode_hmac_with(alg, REFERENCE_HS256_TOKEN, REFERENCE_SECRET, &at_now()),
            Err(JwtError::UnsupportedAlg)
        );
        assert_eq!(
            encode_hmac(alg, &claims_with(vec![]), REFERENCE_SECRET),
            Err(JwtError::UnsupportedAlg)
        );
        assert_eq!(alg.recommended_secret_len(), None);
        assert!(!alg.is_hmac());
    }
}

#[test]
fn unsecured_and_unknown_algorithms_are_rejected() {
    let payload = br#"{"sub":"mallory","exp":4102444800}"#;
    for alg in [
        "none", "None", "NONE", "hs256", "HS-256", "HS1", "RS256", "ES256", "",
    ] {
        let header = format!(r#"{{"alg":"{alg}","typ":"JWT"}}"#);
        // Unsigned, and signed with the right secret under the wrong label.
        let unsigned = format!("{}.{}.", b64(header.as_bytes()), b64(payload));
        let signed = sign_raw(header.as_bytes(), payload, SECRET);
        for token in [&unsigned, &signed] {
            assert_both_reject(token, JwtError::UnsupportedAlg);
        }
    }
    for header in [
        &br#"{"typ":"JWT"}"#[..],
        br#"{"alg":256}"#,
        br#"{"alg":["HS256"]}"#,
    ] {
        assert_both_reject(&sign_raw(header, payload, SECRET), JwtError::UnsupportedAlg);
    }
}

// ---- G9: malleability and structure ---------------------------------------------

#[test]
fn non_canonical_base64url_is_rejected() {
    let token = sign_raw(HS256_HEADER, br#"{"sub":"alice","exp":4102444800}"#, SECRET);
    assert!(decode_hs256(&token, SECRET).is_ok());
    assert!(decode_hs256_with(&token, SECRET, &at_now()).is_ok());
    let (h, p, s) = segments(&token);

    // The 32-byte signature encodes to 43 characters; the last one carries
    // two unused bits. Every alias of that character decodes (leniently) to
    // the very same signature bytes, so each would be a second valid spelling
    // of one token.
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let last = s.as_bytes()[42];
    let index = alphabet.iter().position(|&c| c == last).unwrap();
    assert_eq!(index % 4, 0, "canonical encodings have zero trailing bits");
    let mut aliases = 0;
    for offset in 1..4 {
        let alias = alphabet[index + offset] as char;
        let forged = format!("{h}.{p}.{}{alias}", &s[..42]);
        assert_eq!(
            base64::decode_lenient(segments(&forged).2).unwrap(),
            base64::decode_url_no_pad(s).unwrap(),
            "the alias really is the same signature"
        );
        assert_both_reject(&forged, JwtError::Malformed);
        aliases += 1;
    }
    assert_eq!(aliases, 3);

    // Padding, embedded `=`, whitespace and the standard alphabet in the
    // header or signature segment: not canonical base64url.
    let malformed = [
        format!("{h}.{p}.{s}="),
        format!("{h}.{p}.{s}=="),
        format!("{h}=.{p}.{s}"),
        format!("{h}.{p}.{}={}", &s[..20], &s[20..]),
        format!("{h}.{p}.{s}\n"),
        format!(" {h}.{p}.{s}"),
        format!("{h}.{p}.{s} "),
        format!("{h}.{p}.{}", s.replace('-', "+").replace('_', "/")),
    ];
    for forged in &malformed {
        if forged == &token {
            continue; // the signature happened to contain neither `-` nor `_`
        }
        assert_both_reject(forged, JwtError::Malformed);
    }
    // The payload segment is covered by the signature byte for byte, so any
    // respelling of it — even one that would decode to the same JSON — no
    // longer matches the signature.
    let resigned_needed = [
        format!("{h}.{p}=.{s}"),
        format!("{h}.{}={}.{s}", &p[..4], &p[4..]),
        format!("{h}.{p} .{s}"),
        format!("{h}.{p}\n.{s}"),
    ];
    for forged in &resigned_needed {
        assert_both_reject(forged, JwtError::InvalidSignature);
    }
    // ...and a payload that is signed in a non-canonical spelling is refused
    // when it is decoded.
    let padded_payload = format!("{h}.{p}=");
    let tag = hmac_sha256(SECRET, padded_payload.as_bytes());
    assert_both_reject(
        &format!("{padded_payload}.{}", b64(tag.as_bytes())),
        JwtError::Malformed,
    );
}

#[test]
fn wrong_number_of_segments_is_malformed() {
    let token = sign_raw(HS256_HEADER, br#"{"exp":4102444800}"#, SECRET);
    let (h, p, s) = segments(&token);
    for bad in [
        String::new(),
        ".".to_string(),
        "..".to_string(),
        "...".to_string(),
        h.to_string(),
        format!("{h}.{p}"),
        format!("{h}.{p}.{s}.{s}"),
        format!("{h}..{s}"),
        format!(".{p}.{s}"),
        format!("{h}.{p}.{s}."),
        "not a token".to_string(),
    ] {
        assert_both_reject(&bad, JwtError::Malformed);
    }
    // A token larger than the cap is refused before any decoding.
    let huge = format!("{h}.{}.{s}", "A".repeat(MAX_TOKEN_LEN));
    assert_both_reject(&huge, JwtError::Malformed);
}

#[test]
fn claims_must_be_a_json_object() {
    // Each of these is correctly signed, so only the shape can reject it.
    for payload in [
        &b"[1,2,3]"[..],
        b"\"just a string\"",
        b"42",
        b"null",
        b"true",
        b"[]",
        b"",
        b"not json",
        b"{\"exp\":4102444800",
        b"{\"exp\":4102444800}{}",
        b"\xff\xfe",
    ] {
        let token = sign_raw(HS256_HEADER, payload, SECRET);
        assert_both_reject(&token, JwtError::Malformed);
    }
    // Minting refuses non-objects too.
    for claims in [
        Value::Array(vec![]),
        Value::Str("x".into()),
        Value::Int(1),
        Value::Null,
    ] {
        assert_eq!(
            encode_hmac(Algorithm::Hs256, &claims, SECRET),
            Err(JwtError::Malformed)
        );
    }
}

#[test]
fn header_must_be_a_json_object() {
    let payload = br#"{"exp":4102444800}"#;
    for header in [
        &b"[]"[..],
        b"\"HS256\"",
        b"null",
        b"not json",
        b"{\"alg\":\"HS256\"",
    ] {
        assert_both_reject(&sign_raw(header, payload, SECRET), JwtError::Malformed);
    }
    for header in [
        &br#"{"alg":"HS256","typ":7}"#[..],
        br#"{"alg":"HS256","kid":["a"]}"#,
        br#"{"alg":"HS256","cty":null}"#,
    ] {
        assert_both_reject(&sign_raw(header, payload, SECRET), JwtError::Malformed);
    }
}

#[test]
fn duplicate_member_names_are_rejected() {
    let payload = br#"{"exp":4102444800}"#;
    // Parsers disagree on which duplicate wins; a token must not depend on it.
    for header in [
        &br#"{"alg":"none","alg":"HS256"}"#[..],
        br#"{"alg":"HS256","alg":"none"}"#,
        br#"{"alg":"HS256","typ":"JWT","typ":"at+jwt"}"#,
    ] {
        assert_both_reject(&sign_raw(header, payload, SECRET), JwtError::Malformed);
    }
    for payload in [
        &br#"{"exp":1,"exp":4102444800}"#[..],
        br#"{"exp":4102444800,"sub":"alice","sub":"admin"}"#,
    ] {
        assert_both_reject(
            &sign_raw(HS256_HEADER, payload, SECRET),
            JwtError::Malformed,
        );
    }
    let twice = Value::Object(vec![
        ("exp".into(), Value::Int(4_102_444_800)),
        ("exp".into(), Value::Int(1)),
    ]);
    assert_eq!(
        encode_hmac(Algorithm::Hs256, &twice, SECRET),
        Err(JwtError::Malformed)
    );
}

#[test]
fn critical_header_parameters_are_rejected() {
    let payload = br#"{"exp":4102444800}"#;
    for header in [
        &br#"{"alg":"HS256","crit":["exp"],"exp":1}"#[..],
        br#"{"alg":"HS256","crit":[]}"#,
        br#"{"alg":"HS256","crit":null}"#,
        br#"{"alg":"HS256","crit":["b64"],"b64":false}"#,
    ] {
        assert_both_reject(
            &sign_raw(header, payload, SECRET),
            JwtError::UnsupportedCritical,
        );
        assert_eq!(
            decode_header(&sign_raw(header, payload, SECRET)),
            Err(JwtError::UnsupportedCritical)
        );
    }
}

#[test]
fn signature_must_match_exactly() {
    let token = sign_raw(HS256_HEADER, br#"{"sub":"alice","exp":4102444800}"#, SECRET);
    let (h, p, s) = segments(&token);
    let tag = base64::decode_url_no_pad(s).unwrap();

    // Wrong secret; another payload; a flipped bit anywhere in the tag.
    assert_eq!(
        decode_hs256_with(&token, b"another-secret-of-32-bytes!!!!!!", &at_now()),
        Err(JwtError::InvalidSignature)
    );
    let other_payload = format!("{h}.{}.{s}", b64(br#"{"sub":"admin","exp":4102444800}"#));
    assert_both_reject(&other_payload, JwtError::InvalidSignature);
    for byte in 0..tag.len() {
        let mut flipped = tag.clone();
        flipped[byte] ^= 0x80;
        assert_both_reject(
            &format!("{h}.{p}.{}", b64(&flipped)),
            JwtError::InvalidSignature,
        );
    }
    // Truncated, extended and empty tags are not prefixes that "match".
    for bad in [
        &tag[..16],
        &tag[..31],
        &[&tag[..], &[0u8]].concat()[..],
        &[][..],
    ] {
        assert_both_reject(&format!("{h}.{p}.{}", b64(bad)), JwtError::InvalidSignature);
    }
}

// ---- secrets -------------------------------------------------------------------

#[test]
fn empty_and_short_secrets_are_rejected() {
    let claims = claims_with(vec![]);
    assert_eq!(MIN_SECRET_LEN, 16);
    assert_eq!(Algorithm::Hs256.recommended_secret_len(), Some(32));
    assert_eq!(Algorithm::Hs384.recommended_secret_len(), Some(48));
    assert_eq!(Algorithm::Hs512.recommended_secret_len(), Some(64));

    // Empty: rejected everywhere it can be reported.
    let empty_signed = encode_hs256(&claims, b"");
    assert_eq!(decode_hs256(&empty_signed, b""), Err(JwtError::WeakSecret));
    assert_eq!(
        decode_hs256_with(&empty_signed, b"", &at_now()),
        Err(JwtError::WeakSecret)
    );
    assert_eq!(
        encode_hmac(Algorithm::Hs256, &claims, b""),
        Err(JwtError::WeakSecret)
    );

    // The validating API enforces the hard minimum of 16 bytes.
    let fifteen = b"123456789012345";
    let sixteen = b"1234567890123456";
    assert_eq!(
        encode_hmac(Algorithm::Hs256, &claims, fifteen),
        Err(JwtError::WeakSecret)
    );
    let short_signed = encode_hs256(&claims, fifteen);
    assert_eq!(
        decode_hs256_with(&short_signed, fifteen, &at_now()),
        Err(JwtError::WeakSecret)
    );
    let token = encode_hmac(Algorithm::Hs256, &claims, sixteen).unwrap();
    assert!(decode_hs256_with(&token, sixteen, &at_now()).is_ok());
    for alg in [Algorithm::Hs384, Algorithm::Hs512] {
        assert_eq!(
            encode_hmac(alg, &claims, fifteen),
            Err(JwtError::WeakSecret)
        );
        let token = encode_hmac(alg, &claims, sixteen).unwrap();
        assert!(decode_hmac_with(alg, &token, sixteen, &at_now()).is_ok());
        assert_eq!(
            decode_hmac_with(alg, &token, fifteen, &at_now()),
            Err(JwtError::WeakSecret)
        );
    }

    // The original entry point only refuses the empty secret.
    assert!(decode_hs256(&short_signed, fifteen).is_ok());
    assert!(decode_hs256(&encode_hs256(&claims, b"s"), b"s").is_ok());
}

#[test]
fn a_secret_wrapper_works_as_the_key_and_is_never_printed() {
    let secret = Secret::from("super-secret-signing-key-0123456789");
    let token = encode_hmac(Algorithm::Hs256, &claims_with(vec![]), secret.expose()).unwrap();
    assert!(decode_hs256_with(&token, secret.expose(), &at_now()).is_ok());
    assert_eq!(format!("{secret:?}"), "Secret([REDACTED])");

    // No error carries the secret or the token.
    let wrong = Secret::from("another-signing-key-0123456789abcdef");
    let errors = [
        decode_hs256_with(&token, wrong.expose(), &at_now()).unwrap_err(),
        decode_hs256_with(&token, b"short", &at_now()).unwrap_err(),
        decode_hs256_with("a.b.c", secret.expose(), &at_now()).unwrap_err(),
        decode_hs256_with(&token, secret.expose(), &at_now().audience("x")).unwrap_err(),
        decode_hs256_with(&token, secret.expose(), &at_now().current_time(FAR_FUTURE)).unwrap_err(),
    ];
    for error in errors {
        let text = format!("{error} / {error:?}");
        assert!(!text.contains("super-secret"), "{text}");
        assert!(!text.contains("another-signing"), "{text}");
        assert!(!text.contains(segments(&token).2), "{text}");
    }
}

// ---- expiry, not-before, issued-at ----------------------------------------------

#[test]
fn exp_is_required_by_default() {
    let token = sign_raw(HS256_HEADER, br#"{"sub":"alice"}"#, SECRET);
    assert_eq!(
        decode_hs256_with(&token, SECRET, &at_now()),
        Err(JwtError::MissingClaim("exp".into()))
    );
    assert_eq!(
        decode_hs256_with(&token, SECRET, &Validation::default()),
        Err(JwtError::MissingClaim("exp".into()))
    );
    // Opt out explicitly...
    assert!(decode_hs256_with(&token, SECRET, &at_now().require_exp(false)).is_ok());
    // ...or use the original entry point, which never required it.
    assert!(decode_hs256(&token, SECRET).is_ok());
    // `exp: null` is not an expiry.
    let null_exp = sign_raw(HS256_HEADER, br#"{"exp":null}"#, SECRET);
    assert_eq!(
        decode_hs256_with(&null_exp, SECRET, &at_now()),
        Err(JwtError::Malformed)
    );
}

#[test]
fn exp_honours_leeway_at_the_boundary() {
    let exp = NOW;
    let token = sign_raw(
        HS256_HEADER,
        format!(r#"{{"exp":{exp}}}"#).as_bytes(),
        SECRET,
    );
    let check = |now: u64, leeway: u64| {
        decode_hs256_with(
            &token,
            SECRET,
            &Validation::new().leeway(leeway).current_time(now),
        )
    };
    // No leeway: valid strictly before exp.
    assert!(check(exp - 1, 0).is_ok());
    assert_eq!(check(exp, 0), Err(JwtError::Expired));
    // 30 s leeway: valid until exp + 30, exclusive.
    assert!(check(exp + 29, 30).is_ok());
    assert_eq!(check(exp + 30, 30), Err(JwtError::Expired));
    // The default leeway is 60 s.
    let default_at =
        |now: u64| decode_hs256_with(&token, SECRET, &Validation::new().current_time(now));
    assert!(default_at(exp + 59).is_ok());
    assert_eq!(default_at(exp + 60), Err(JwtError::Expired));
    // A present exp is enforced even when it is not required.
    assert_eq!(
        decode_hs256_with(
            &token,
            SECRET,
            &Validation::new()
                .require_exp(false)
                .leeway(0)
                .current_time(exp + 1)
        ),
        Err(JwtError::Expired)
    );
}

#[test]
fn fractional_and_malformed_numeric_dates() {
    let token_with = |payload: &str| sign_raw(HS256_HEADER, payload.as_bytes(), SECRET);
    // Fractional seconds are honoured, not truncated away.
    let fractional = token_with(&format!(r#"{{"exp":{}.5}}"#, NOW));
    assert!(decode_hs256_with(&fractional, SECRET, &at_now()).is_ok());
    assert_eq!(
        decode_hs256_with(&fractional, SECRET, &at_now().current_time(NOW + 1)),
        Err(JwtError::Expired)
    );
    // Present but not a number: malformed, never ignored.
    for payload in [
        r#"{"exp":"4102444800"}"#,
        r#"{"exp":true}"#,
        r#"{"exp":[4102444800]}"#,
        r#"{"exp":{}}"#,
        r#"{"exp":4102444800,"nbf":"0"}"#,
        r#"{"exp":4102444800,"iat":"0"}"#,
    ] {
        assert_eq!(
            decode_hs256_with(&token_with(payload), SECRET, &at_now()),
            Err(JwtError::Malformed),
            "{payload}"
        );
    }
}

#[test]
fn nbf_honours_leeway_at_the_boundary() {
    let nbf = NOW + 100;
    let token = sign_raw(
        HS256_HEADER,
        format!(r#"{{"exp":{},"nbf":{nbf}}}"#, NOW + 3600).as_bytes(),
        SECRET,
    );
    let check = |now: u64, leeway: u64| {
        decode_hs256_with(
            &token,
            SECRET,
            &Validation::new().leeway(leeway).current_time(now),
        )
    };
    assert_eq!(check(nbf - 1, 0), Err(JwtError::NotYetValid));
    assert!(check(nbf, 0).is_ok());
    assert_eq!(check(nbf - 31, 30), Err(JwtError::NotYetValid));
    assert!(check(nbf - 30, 30).is_ok());
}

#[test]
fn iat_in_the_future_is_rejected() {
    let token_iat = |iat: u64| {
        sign_raw(
            HS256_HEADER,
            format!(r#"{{"exp":{},"iat":{iat}}}"#, NOW + 3600).as_bytes(),
            SECRET,
        )
    };
    assert!(decode_hs256_with(&token_iat(NOW), SECRET, &at_now()).is_ok());
    assert!(decode_hs256_with(&token_iat(NOW - 500), SECRET, &at_now()).is_ok());
    assert_eq!(
        decode_hs256_with(&token_iat(NOW + 1), SECRET, &at_now()),
        Err(JwtError::IssuedInFuture)
    );
    // Within the leeway, and with the check switched off.
    assert!(decode_hs256_with(&token_iat(NOW + 10), SECRET, &at_now().leeway(10)).is_ok());
    assert_eq!(
        decode_hs256_with(&token_iat(NOW + 11), SECRET, &at_now().leeway(10)),
        Err(JwtError::IssuedInFuture)
    );
    assert!(
        decode_hs256_with(&token_iat(NOW + 999), SECRET, &at_now().validate_iat(false)).is_ok()
    );
    // The original entry point never looked at iat.
    let far_future_iat = sign_raw(HS256_HEADER, br#"{"iat":99999999999}"#, SECRET);
    assert!(decode_hs256(&far_future_iat, SECRET).is_ok());
}

#[test]
fn max_age_bounds_the_token_lifetime() {
    let token = sign_raw(
        HS256_HEADER,
        format!(r#"{{"exp":{},"iat":{}}}"#, NOW + 86_400, NOW - 100).as_bytes(),
        SECRET,
    );
    assert!(decode_hs256_with(&token, SECRET, &at_now().max_age(100)).is_ok());
    assert_eq!(
        decode_hs256_with(&token, SECRET, &at_now().max_age(99)),
        Err(JwtError::TooOld)
    );
    assert!(decode_hs256_with(&token, SECRET, &at_now().max_age(90).leeway(10)).is_ok());
    // A maximum age needs an iat to measure from.
    let no_iat = sign_raw(
        HS256_HEADER,
        format!(r#"{{"exp":{}}}"#, NOW + 60).as_bytes(),
        SECRET,
    );
    assert_eq!(
        decode_hs256_with(&no_iat, SECRET, &at_now().max_age(100)),
        Err(JwtError::MissingClaim("iat".into()))
    );
    assert_eq!(
        decode_hs256_with(&no_iat, SECRET, &at_now().max_age(100).validate_iat(false)),
        Err(JwtError::MissingClaim("iat".into()))
    );
}

// ---- audience, issuer, subject, required claims -----------------------------------

#[test]
fn audience_accepts_string_or_array_claims() {
    let string_aud = encode_hmac(
        Algorithm::Hs256,
        &claims_with(vec![("aud", "orders-api".into())]),
        SECRET,
    )
    .unwrap();
    let array_aud = encode_hmac(
        Algorithm::Hs256,
        &claims_with(vec![(
            "aud",
            Value::Array(vec!["billing-api".into(), "orders-api".into()]),
        )]),
        SECRET,
    )
    .unwrap();
    let no_aud = encode_hmac(Algorithm::Hs256, &claims_with(vec![]), SECRET).unwrap();

    for token in [&string_aud, &array_aud] {
        assert!(decode_hs256_with(token, SECRET, &at_now().audience("orders-api")).is_ok());
        // Any one of several accepted audiences is enough.
        assert!(decode_hs256_with(
            token,
            SECRET,
            &at_now().audience("unrelated").audience("orders-api")
        )
        .is_ok());
        assert_eq!(
            decode_hs256_with(token, SECRET, &at_now().audience("admin-api")),
            Err(JwtError::InvalidAudience)
        );
        // Comparison is exact: no prefix, no case folding.
        for near in ["orders", "orders-api ", "Orders-API", "orders-api/v2", ""] {
            assert_eq!(
                decode_hs256_with(token, SECRET, &at_now().audience(near)),
                Err(JwtError::InvalidAudience),
                "{near:?}"
            );
        }
        // Not configured: the claim is not inspected.
        assert!(decode_hs256_with(token, SECRET, &at_now()).is_ok());
    }
    assert!(decode_hs256_with(&array_aud, SECRET, &at_now().audience("billing-api")).is_ok());
    assert_eq!(
        decode_hs256_with(&no_aud, SECRET, &at_now().audience("orders-api")),
        Err(JwtError::MissingClaim("aud".into()))
    );

    // Wrong JSON types.
    for aud in [
        Value::Int(7),
        Value::Bool(true),
        Value::Array(vec!["orders-api".into(), Value::Int(1)]),
        object(vec![("orders-api", Value::Bool(true))]),
    ] {
        let token =
            encode_hmac(Algorithm::Hs256, &claims_with(vec![("aud", aud)]), SECRET).unwrap();
        assert_eq!(
            decode_hs256_with(&token, SECRET, &at_now().audience("orders-api")),
            Err(JwtError::Malformed)
        );
    }
    let empty_array = encode_hmac(
        Algorithm::Hs256,
        &claims_with(vec![("aud", Value::Array(vec![]))]),
        SECRET,
    )
    .unwrap();
    assert_eq!(
        decode_hs256_with(&empty_array, SECRET, &at_now().audience("orders-api")),
        Err(JwtError::InvalidAudience)
    );
}

#[test]
fn issuer_and_subject_are_matched_exactly() {
    let token = encode_hmac(
        Algorithm::Hs256,
        &claims_with(vec![
            ("iss", "https://issuer.example".into()),
            ("sub", "user-1".into()),
        ]),
        SECRET,
    )
    .unwrap();
    let ok = at_now().issuer("https://issuer.example").subject("user-1");
    assert!(decode_hs256_with(&token, SECRET, &ok).is_ok());
    assert!(decode_hs256_with(
        &token,
        SECRET,
        &at_now()
            .issuer("https://other.example")
            .issuer("https://issuer.example")
    )
    .is_ok());
    for issuer in [
        "https://issuer.example/",
        "https://issuer.example.evil.test",
        "HTTPS://ISSUER.EXAMPLE",
        "issuer.example",
    ] {
        assert_eq!(
            decode_hs256_with(&token, SECRET, &at_now().issuer(issuer)),
            Err(JwtError::InvalidIssuer),
            "{issuer}"
        );
    }
    assert_eq!(
        decode_hs256_with(&token, SECRET, &at_now().subject("user-2")),
        Err(JwtError::InvalidSubject)
    );

    let bare = encode_hmac(Algorithm::Hs256, &claims_with(vec![]), SECRET).unwrap();
    assert_eq!(
        decode_hs256_with(&bare, SECRET, &at_now().issuer("x")),
        Err(JwtError::MissingClaim("iss".into()))
    );
    assert_eq!(
        decode_hs256_with(&bare, SECRET, &at_now().subject("x")),
        Err(JwtError::MissingClaim("sub".into()))
    );
    let typed = encode_hmac(
        Algorithm::Hs256,
        &claims_with(vec![
            ("iss", Value::Array(vec!["x".into()])),
            ("sub", Value::Int(1)),
        ]),
        SECRET,
    )
    .unwrap();
    assert_eq!(
        decode_hs256_with(&typed, SECRET, &at_now().issuer("x")),
        Err(JwtError::InvalidIssuer)
    );
    assert_eq!(
        decode_hs256_with(&typed, SECRET, &at_now().subject("1")),
        Err(JwtError::InvalidSubject)
    );
}

#[test]
fn required_claims_must_be_present_and_non_null() {
    let token = encode_hmac(
        Algorithm::Hs256,
        &claims_with(vec![
            ("sub", "user-1".into()),
            ("jti", "abc".into()),
            ("tenant", Value::Null),
            ("flag", Value::Bool(false)),
        ]),
        SECRET,
    )
    .unwrap();
    assert!(decode_hs256_with(
        &token,
        SECRET,
        &at_now().require_claims(["sub", "jti", "flag"])
    )
    .is_ok());
    assert_eq!(
        decode_hs256_with(&token, SECRET, &at_now().require_claim("nonce")),
        Err(JwtError::MissingClaim("nonce".into()))
    );
    assert_eq!(
        decode_hs256_with(&token, SECRET, &at_now().require_claim("tenant")),
        Err(JwtError::MissingClaim("tenant".into()))
    );
    assert_eq!(
        JwtError::MissingClaim("nonce".into()).to_string(),
        "missing required claim `nonce`"
    );
}

// ---- header -----------------------------------------------------------------------

#[test]
fn typ_is_checked_when_configured() {
    let payload = format!(r#"{{"exp":{}}}"#, NOW + 60);
    let with_typ = |typ: &str| {
        sign_raw(
            format!(r#"{{"alg":"HS256","typ":"{typ}"}}"#).as_bytes(),
            payload.as_bytes(),
            SECRET,
        )
    };
    for typ in ["JWT", "jwt", "application/jwt", "APPLICATION/JWT"] {
        assert!(decode_hs256_with(&with_typ(typ), SECRET, &at_now().typ("JWT")).is_ok());
        assert!(
            decode_hs256_with(&with_typ(typ), SECRET, &at_now().typ("application/jwt")).is_ok()
        );
        assert_eq!(
            decode_hs256_with(&with_typ(typ), SECRET, &at_now().typ("at+jwt")),
            Err(JwtError::InvalidType)
        );
    }
    assert!(decode_hs256_with(&with_typ("at+jwt"), SECRET, &at_now().typ("at+JWT")).is_ok());
    // No typ header at all.
    let untyped = sign_raw(br#"{"alg":"HS256"}"#, payload.as_bytes(), SECRET);
    assert_eq!(
        decode_hs256_with(&untyped, SECRET, &at_now().typ("JWT")),
        Err(JwtError::InvalidType)
    );
    // Not configured: any (or no) typ is accepted.
    assert!(decode_hs256_with(&untyped, SECRET, &at_now()).is_ok());
    assert!(decode_hs256_with(&with_typ("anything"), SECRET, &at_now()).is_ok());
}

#[test]
fn custom_headers_round_trip() {
    let header = Header::new(Algorithm::Hs384)
        .with_kid("key-2024-06")
        .with_typ("at+jwt");
    let token = encode_with_header(&header, &claims_with(vec![]), REFERENCE_SECRET).unwrap();
    let decoded = decode_header(&token).unwrap();
    assert_eq!(decoded, header);
    assert_eq!(decoded.kid.as_deref(), Some("key-2024-06"));
    assert!(decode_hmac_with(
        Algorithm::Hs384,
        &token,
        REFERENCE_SECRET,
        &at_now().typ("at+jwt")
    )
    .is_ok());
    // Header values are JSON-escaped, not concatenated.
    let odd = Header::new(Algorithm::Hs256).with_kid("a\"b\\c\n");
    let token = encode_with_header(&odd, &claims_with(vec![]), SECRET).unwrap();
    assert_eq!(
        decode_header(&token).unwrap().kid.as_deref(),
        Some("a\"b\\c\n")
    );
    assert!(decode_hs256_with(&token, SECRET, &at_now()).is_ok());
    assert_eq!(
        encode_with_header(&Header::new(Algorithm::Rs256), &claims_with(vec![]), SECRET),
        Err(JwtError::UnsupportedAlg)
    );
}

#[test]
fn decode_header_does_not_verify_anything() {
    let forged = sign_raw(
        br#"{"alg":"HS256","kid":"k1"}"#,
        br#"{"sub":"mallory"}"#,
        b"attacker-chosen-key",
    );
    // The header is readable (to pick a key)...
    assert_eq!(decode_header(&forged).unwrap().kid.as_deref(), Some("k1"));
    // ...but the token does not verify.
    assert_eq!(
        decode_hs256_with(&forged, SECRET, &at_now()),
        Err(JwtError::InvalidSignature)
    );
    assert_eq!(decode_header("a.b"), Err(JwtError::Malformed));
    assert_eq!(
        decode_header(&format!("{}.e30.", b64(br#"{"alg":"none"}"#))),
        Err(JwtError::UnsupportedAlg)
    );
}

#[test]
fn algorithm_names_round_trip() {
    for alg in [
        Algorithm::Hs256,
        Algorithm::Hs384,
        Algorithm::Hs512,
        Algorithm::Rs256,
        Algorithm::Es256,
    ] {
        assert_eq!(Algorithm::from_name(alg.as_str()), Some(alg));
        assert_eq!(alg.to_string(), alg.as_str());
    }
    for unknown in ["none", "hs256", "HS256 ", "PS256", "EdDSA", ""] {
        assert_eq!(Algorithm::from_name(unknown), None);
    }
}

#[test]
fn scopes_read_from_verified_claims() {
    let token = encode_hmac(
        Algorithm::Hs256,
        &claims_with(vec![
            ("scope", "orders:read orders:write".into()),
            ("roles", Value::Array(vec!["admin".into()])),
        ]),
        SECRET,
    )
    .unwrap();
    let claims = decode_hs256_with(&token, SECRET, &at_now()).unwrap();
    let scopes = Scopes::from_claims(&claims);
    assert!(scopes.has_scope("orders:read"));
    assert!(scopes.require_all(&["orders:read", "orders:write"]).is_ok());
    assert!(scopes.require_any(&["x", "orders:write"]).is_ok());
    assert!(scopes.require_all(&["orders:delete"]).is_err());
    assert!(Scopes::roles(&claims).has_scope("admin"));
}
