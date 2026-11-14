# ferroly::auth

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `auth` · **Module:** `ferroly::auth`

## Overview

`auth` provides authentication primitives built around **JSON Web Tokens**:

- **HMAC tokens** — `HS256`, `HS384`, `HS512`: mint
  ([`encode_hmac`](#encode_hmac--encode_with_header)) and verify
  ([`decode_hs256_with`](#decode_hs256_with--decode_hmac_with) /
  `decode_hmac_with`) with a [`Validation`](#validation) policy: expiry
  required by default, clock-skew leeway, `aud`, `iss`, `sub`, required claims,
  maximum age, `typ`.
- **Asymmetric tokens** — `RS256` and `ES256`, **verify-only**, from
  [JWK / JWKS public keys](#asymmetric-tokens-rs256-es256-and-jwks), including a
  cached remote key set for issuers that rotate keys (Entra ID, Okta, Auth0,
  Cognito, Keycloak, …). Requires the `http` feature as well.
- **Authorization helpers** — [`Scopes`](#scopes-and-roles) reads `scope` /
  `scp` / `roles` claims.
- [`Secret`](hash.md#secret), re-exported from [hash](hash.md), keeps key
  material out of `Debug` output.

The original [`encode_hs256`](#legacy-encode_hs256--decode_hs256) /
`decode_hs256` pair is kept for compatibility. It is what
[turbo](turbo.md)'s JWT middleware calls.

### Where the cryptography comes from

- SHA-2 and HMAC are ferroly's own ([hash](hash.md)): hand-rolled, tested
  against FIPS 180-4 and RFC 4231 vectors, **not FIPS-validated**.
- RSA and ECDSA signature verification is **not** implemented in ferroly. It is
  delegated to the verification algorithms of the `rustls` crypto provider
  (`ring`) that the `http` feature already links for TLS; `auth` only converts
  JOSE key and signature encodings to the DER forms those verifiers expect.
  That is why asymmetric tokens need `http`.

## Enabling

An **optional, non-default** feature. On its own it is std-only — no `tokio`,
no runtime, no external crates.

```toml
[dependencies]
ferroly = { version = "*", features = ["auth"] }            # HMAC tokens, scopes
ferroly = { version = "*", features = ["auth", "http"] }    # + RS256 / ES256 / JWKS
```

The feature implies [`codec`](codec.md) (claims are a [`Value`](codec.md)) and
[`hash`](hash.md).

| Item | Needs |
|---|---|
| `encode_hmac`, `encode_with_header`, `decode_hs256_with`, `decode_hmac_with`, `decode_header`, `encode_hs256`, `decode_hs256` | `auth` |
| `Algorithm`, `Header`, `Validation`, `JwtError`, `MIN_SECRET_LEN`, `MAX_TOKEN_LEN` | `auth` |
| `Scopes`, `ScopeError`, `Secret` | `auth` |
| `Jwk`, `Jwks`, `RemoteJwks`, `KeyType` | `auth` + `http` |

## Quick start

```rust
use ferroly::auth::{decode_hs256_with, encode_hmac, Algorithm, Scopes, Validation};
use ferroly::codec::Value;
use std::time::{SystemTime, UNIX_EPOCH};

let secret = b"an-example-secret-of-32-bytes!!!"; // generate with ferroly::hash::random
let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

let claims = Value::Object(vec![
    ("iss".into(), "https://issuer.example".into()),
    ("aud".into(), "orders-api".into()),
    ("sub".into(), "u123".into()),
    ("scope".into(), "orders:read".into()),
    ("exp".into(), Value::UInt(now + 3600)),
]);
let token = encode_hmac(Algorithm::Hs256, &claims, secret).unwrap();

let validation = Validation::new()
    .issuer("https://issuer.example")
    .audience("orders-api");
let verified = decode_hs256_with(&token, secret, &validation).unwrap();

assert_eq!(verified.get("sub").and_then(Value::as_str), Some("u123"));
assert!(Scopes::from_claims(&verified).has_scope("orders:read"));
```

## HMAC tokens

### `Algorithm`

```rust
#[non_exhaustive]
pub enum Algorithm { Hs256, Hs384, Hs512, Rs256, Es256 }
```

| Method | Notes |
|---|---|
| `as_str()` / `Display` | the `alg` header value: `"HS256"`, … |
| `Algorithm::from_name(&str)` | case-sensitive; `"none"` and unknown names are `None` |
| `is_hmac()` | `true` for the three `Hs*` variants |
| `recommended_secret_len()` | 32 / 48 / 64 bytes for `Hs256` / `Hs384` / `Hs512`; `None` for asymmetric |

`Rs256` and `Es256` can only be *verified*, and only through a
[JWK](#asymmetric-tokens-rs256-es256-and-jwks). The HMAC functions reject them.

### Secrets

| | Requirement |
|---|---|
| **Recommended** (RFC 7518 §3.2) | at least the hash output size: **32 bytes** for HS256, 48 for HS384, 64 for HS512 — `Algorithm::recommended_secret_len()` |
| **Hard minimum**, validating API | `MIN_SECRET_LEN` = **16 bytes**; shorter is `JwtError::WeakSecret` |
| Legacy `decode_hs256` | any non-empty secret; empty is `JwtError::WeakSecret` |
| Legacy `encode_hs256` | no check at all |

Generate secrets with [`ferroly::hash::random`](hash.md#secure-randomness)
(`random::secret(32)`), never from a passphrase: an HMAC secret can be
brute-forced offline from a single token.

A secret that is recognisably an **asymmetric public key, certificate or JWK
document** (PEM `PUBLIC KEY` / `CERTIFICATE`, an OpenSSH public key, JSON with
`"kty"`) is refused with `JwtError::InvalidKey` by every HMAC function except
the unchecked `encode_hs256`. Using public material as an HMAC secret is the
classic algorithm-confusion mistake — anyone could forge tokens.

### `encode_hmac` / `encode_with_header`

```rust
pub fn encode_hmac(alg: Algorithm, claims: &Value, secret: &[u8]) -> Result<String, JwtError>
pub fn encode_with_header(header: &Header, claims: &Value, secret: &[u8]) -> Result<String, JwtError>
```

Mints `base64url(header).base64url(claims).base64url(signature)`.
`encode_hmac` writes the header `{"alg":"…","typ":"JWT"}`;
`encode_with_header` takes a [`Header`](#header) for a `kid` or another `typ`.

| Error | When |
|---|---|
| `UnsupportedAlg` | `alg` is `Rs256` / `Es256` (signing with RSA/ECDSA is not supported) |
| `WeakSecret` | the secret is shorter than `MIN_SECRET_LEN` |
| `InvalidKey` | the secret looks like a public key |
| `Malformed` | `claims` is not a JSON object, or repeats a member name |

```rust
use ferroly::auth::{encode_with_header, Algorithm, Header};
use ferroly::codec::Value;

let header = Header::new(Algorithm::Hs512).with_kid("2024-06").with_typ("at+jwt");
let claims = Value::Object(vec![("exp".into(), Value::Int(4_102_444_800))]);
let secret = [7u8; 64];
let token = encode_with_header(&header, &claims, &secret).unwrap();
```

### `decode_hs256_with` / `decode_hmac_with`

```rust
pub fn decode_hs256_with(token: &str, secret: &[u8], validation: &Validation) -> Result<Value, JwtError>
pub fn decode_hmac_with(alg: Algorithm, token: &str, secret: &[u8], validation: &Validation) -> Result<Value, JwtError>
```

The preferred verification entry points. `decode_hs256_with` is
`decode_hmac_with(Algorithm::Hs256, …)`. They check, in this order:

1. **Secret** — at least `MIN_SECRET_LEN` bytes, not a public key
   (`WeakSecret`, `InvalidKey`).
2. **Structure** — at most `MAX_TOKEN_LEN` (64 KiB), exactly three
   dot-separated segments with a non-empty header and payload, **strict
   canonical base64url** for the header and signature (`Malformed`).
3. **Header** — a JSON object without duplicate names; no `crit` parameter
   (`UnsupportedCritical`); `alg` **exactly** the algorithm of the function
   called and inside `Validation::algorithms` if set (`UnsupportedAlg`); the
   expected `typ` if configured (`InvalidType`). *The header never selects the
   algorithm.*
4. **Signature** — recomputed and compared in **constant time**
   (`InvalidSignature`).
5. **Claims** — only now decoded: a JSON object without duplicate names
   (`Malformed`), then the [`Validation`](#validation) checks.

On success the claims are returned as a [`Value`](codec.md).

### `Validation`

What to check, beyond the signature, before the claims are trusted. Build it
once and reuse it.

```rust
use ferroly::auth::Validation;

let validation = Validation::new()
    .issuer("https://issuer.example")
    .audience("orders-api")
    .leeway(30)
    .max_age(3600)
    .require_claim("sub");
```

| Method | Default | Effect | Error |
|---|---|---|---|
| `Validation::new()` / `default()` | — | the defaults in this table | |
| `require_exp(bool)` | **`true`** | a token without `exp` is rejected | `MissingClaim("exp")` |
| *(always)* | — | an `exp` that is present is enforced: valid while `now < exp + leeway` | `Expired` |
| *(always)* | — | an `nbf` that is present is enforced: valid once `now + leeway >= nbf` | `NotYetValid` |
| `validate_iat(bool)` | `true` | an `iat` later than `now + leeway` is rejected | `IssuedInFuture` |
| `leeway(seconds)` | **60** | clock-skew tolerance for `exp`, `nbf`, `iat`, `max_age`; `0` for exact | |
| `max_age(seconds)` | none | tokens issued more than this long ago are rejected, whatever their `exp`; makes `iat` required | `TooOld`, `MissingClaim("iat")` |
| `audience(aud)` | not checked | the `aud` claim — a string **or** an array of strings — must contain it; repeat the call to accept alternatives | `InvalidAudience`, `MissingClaim("aud")` |
| `issuer(iss)` | not checked | `iss` must equal it; repeat for alternatives | `InvalidIssuer`, `MissingClaim("iss")` |
| `subject(sub)` | not checked | `sub` must equal it | `InvalidSubject`, `MissingClaim("sub")` |
| `require_claim(name)` / `require_claims([..])` | none | the claim must be present and not `null` | `MissingClaim(name)` |
| `typ(expected)` | not checked | the `typ` header must match, ASCII case-insensitively, ignoring an `application/` prefix | `InvalidType` |
| `algorithms(&[..])` | no extra restriction | an additional allow-list on top of the entry point's own algorithm | `UnsupportedAlg` |
| `current_time(unix_seconds)` | system clock | validate against a fixed time — for tests and archived tokens | |

Details worth knowing:

- `exp`, `nbf` and `iat` may be integer **or** fractional seconds. A value that
  is present but not a finite number is `Malformed`, never silently ignored.
- String comparisons (`aud`, `iss`, `sub`) are exact — no prefix matching, no
  case folding, no trailing-slash normalisation.
- An `aud` array containing a non-string, or an `aud` of another JSON type, is
  `Malformed`.
- **`aud` is only checked when you configure an audience.** RFC 7519 says a
  recipient that does not find itself in `aud` must reject the token — so a
  service that accepts tokens from a shared issuer should always call
  `.audience(..)`, otherwise a token minted for another service is accepted.
- The 60-second default leeway extends a token's life by up to a minute. Use
  `.leeway(0)` if that matters.

### `Header`

```rust
#[non_exhaustive]
pub struct Header {
    pub alg: Algorithm,
    pub typ: Option<String>,   // "JWT" from Header::new
    pub kid: Option<String>,
    pub cty: Option<String>,
}
```

`Header::new(alg)`, `.with_kid(..)`, `.with_typ(..)`.

`decode_header(token) -> Result<Header, JwtError>` reads a token's header
**without verifying anything** — use it only to pick a key by `kid` or to route
by algorithm, never to make a decision. An unknown `alg` (including `none`) is
`UnsupportedAlg`.

### Legacy: `encode_hs256` / `decode_hs256`

```rust
pub fn encode_hs256(claims: &Value, secret: &[u8]) -> String
pub fn decode_hs256(token: &str, secret: &[u8]) -> Result<Value, JwtError>
```

Kept so existing callers keep compiling. **Prefer `encode_hmac` and
`decode_hs256_with`.**

| | `decode_hs256` (legacy) | `decode_hs256_with` |
|---|---|---|
| strict base64url, object header/claims, no duplicates, no `crit` | yes | yes |
| `alg` must be exactly `HS256` | yes | yes |
| constant-time signature check | yes | yes |
| `exp` | checked **if present**; a token without `exp` never expires | **required** by default |
| `nbf` | checked if present | checked if present |
| `iat`, `aud`, `iss`, `sub`, `typ`, max age | not checked | per `Validation` |
| leeway | 0 | 60 s by default |
| secret | any non-empty, not a public key | ≥ 16 bytes, not a public key |

`encode_hs256` is infallible and performs **no** checks: it signs with an empty
or short secret and encodes claims that are not an object (which no verifier
here accepts).

## Asymmetric tokens: RS256, ES256 and JWKS

Requires features `auth` **and** `http`. **Verification only** — ferroly
cannot sign with RSA or ECDSA.

| Algorithm | Key | Signature |
|---|---|---|
| `RS256` | RSA, 2048–8192 bits (`"kty":"RSA"`, members `n`, `e`) | RSASSA-PKCS1-v1_5 with SHA-256 |
| `ES256` | P-256 (`"kty":"EC"`, `"crv":"P-256"`, members `x`, `y`) | ECDSA with SHA-256, 64-byte `r‖s` |

Not supported: `RS384`/`RS512`, `PS256`/`PS384`/`PS512` (RSA-PSS), `ES384`,
`ES512`, `EdDSA`. Keys for those are skipped when a key set is loaded.

### `Jwk`

```rust
use ferroly::auth::{Jwk, Validation};

let jwk = Jwk::from_json(r#"{
    "kty": "EC", "crv": "P-256", "kid": "2024-06",
    "x": "f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU",
    "y": "x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0"
}"#).unwrap();

fn check(jwk: &Jwk, token: &str) -> bool {
    jwk.verify(token, &Validation::new().issuer("https://issuer.example")).is_ok()
}
```

| Item | Notes |
|---|---|
| `Jwk::from_json(&str)` / `Jwk::from_value(&Value)` | parse one JWK object |
| `Jwk::from_rsa_components(n, e)` | big-endian modulus and exponent; modulus 2048–8192 bits, exponent odd, `3 ≤ e < 2^33` |
| `Jwk::from_ec_p256_coordinates(x, y)` | two 32-byte big-endian coordinates |
| `with_kid(..)`, `kid()` | the key id |
| `key_type() -> KeyType` | `KeyType::Rsa` or `KeyType::EcP256` |
| `algorithm() -> Algorithm` | the **one** algorithm this key verifies: `Rs256` or `Es256` |
| `verify(token, &Validation) -> Result<Value, JwtError>` | verify a token and validate its claims |
| `verify_signature(message, signature) -> Result<(), JwtError>` | verify a detached JWS-format signature |

A JWK is rejected with `JwtError::InvalidKey` if it is malformed, carries
private material (`d`), has a `use` other than `"sig"` or `key_ops` without
`"verify"`, or has an RSA modulus outside 2048–8192 bits; and with
`JwtError::UnsupportedAlg` for another key type or curve, or an `alg` member
other than the one its key type implies (which rules out, say, an RSA key
published for `RSA-OAEP`). JWK members must be strict, unpadded base64url.

`Debug` on a `Jwk` shows its `kid` and type, not the key.

### Algorithm confusion is prevented by construction

- A `Jwk` verifies exactly one algorithm, fixed by its key type. The token's
  `alg` header must **match**; it never chooses. An `RS256` token offered to an
  EC key, or the reverse, is `UnsupportedAlg`.
- `Jwk::verify`, `Jwks::verify` and `RemoteJwks::verify` reject every `HS*`
  token — even if `Validation::algorithms` lists `Hs256`. There is no accessor
  for a `Jwk`'s raw bytes and no API that uses one as an HMAC secret, so an
  attacker who re-signs a token with the *public* key as an HS256 secret gets
  nowhere.
- Conversely, the HMAC functions refuse a secret that looks like a public key
  ([Secrets](#secrets)).
- `alg: none` and unknown algorithms are always `UnsupportedAlg`.

### `Jwks`

A set of keys — an issuer's `jwks.json`.

```rust
use ferroly::auth::{Jwks, Validation};

fn verify(jwks_json: &str, token: &str) -> Result<(), ferroly::auth::JwtError> {
    let jwks = Jwks::from_json(jwks_json)?;
    let claims = jwks.verify(token, &Validation::new().audience("orders-api"))?;
    println!("{:?}", claims.get("sub"));
    Ok(())
}
```

| Item | Notes |
|---|---|
| `Jwks::from_json(&str)` | parses `{"keys":[…]}`. Unsupported or invalid entries are **skipped** (RFC 7517 §5) and counted; only a document that is not a JWK Set is an error (`InvalidKey`). |
| `Jwks::from_keys(Vec<Jwk>)` | from keys built in code |
| `keys()`, `len()`, `is_empty()`, `skipped()` | |
| `find(kid) -> Option<&Jwk>` | first key with that `kid` |
| `verify(token, &Validation)` | select a key, verify, validate claims |

Key selection in `verify`: a key is a candidate if its type matches the token's
`alg` and — when the token has a `kid` — its `kid` is equal. A token **without**
`kid` is tried against every key of the right type.

| Outcome | Error |
|---|---|
| no candidate key | `UnknownKey` |
| candidates exist, none verifies | `InvalidSignature` |
| token `alg` is `HS*` | `UnsupportedAlg` |

### `RemoteJwks`

A JWKS fetched over HTTP and cached — for issuers that rotate their keys.

```rust
use std::time::Duration;
use ferroly::auth::{RemoteJwks, Validation};

async fn authenticate(keys: &RemoteJwks, token: &str) -> Result<(), ferroly::auth::JwtError> {
    let validation = Validation::new()
        .issuer("https://issuer.example")
        .audience("orders-api");
    let claims = keys.verify(token, &validation).await?;
    println!("{:?}", claims.get("sub"));
    Ok(())
}

// Build once and share (e.g. in an Arc):
let keys = RemoteJwks::new("https://issuer.example/.well-known/jwks.json")
    .refresh_interval(Duration::from_secs(900));
```

| Method | Default | Notes |
|---|---|---|
| `RemoteJwks::new(url)` | client with a 10 s timeout | |
| `with_client(Client)` | | custom timeout / shared pool |
| `refresh_interval(Duration)` | 1 hour | age after which the set is re-fetched |
| `min_refresh_interval(Duration)` | 30 s | minimum spacing between fetches triggered by an unknown `kid` or a failing endpoint |
| `max_response_bytes(usize)` | 1 MiB | larger responses are refused |
| `get().await` | | the current set (`Arc<Jwks>`), fetching if needed |
| `refresh().await` | | fetch now, regardless of age |
| `verify(token, &Validation).await` | | as `Jwks::verify`, with rotation handling |

Behaviour:

- **Rotation** — a token whose `kid` is not in the cached set triggers one
  immediate re-fetch, then verification is retried.
- **No fetch storms** — that re-fetch happens at most once per
  `min_refresh_interval`, so a flood of tokens with random `kid`s cannot make
  your service hammer the issuer; within the interval they are `UnknownKey`.
- **Single flight** — concurrent callers share one fetch.
- **Stale on error** — if a refresh fails (network error, non-2xx, not a JWK
  Set, or a set with no usable key) while a previous set is cached, the
  previous set keeps being served and the fetch is retried after
  `min_refresh_interval`. Only a failed *first* fetch is an error
  (`JwtError::KeyFetch`).

Use an `https://` URL: the key set is the root of trust for every token.
`RemoteJwks` does not perform OIDC discovery — give it the `jwks_uri`.

## Scopes and roles

`Scopes` reads authorization claims from **verified** claims and checks them.

```rust
use ferroly::auth::Scopes;
use ferroly::codec::Value;

let claims = Value::Object(vec![
    ("scope".into(), "orders:read orders:write".into()),
    ("roles".into(), Value::Array(vec!["admin".into()])),
]);

let scopes = Scopes::from_claims(&claims);
assert!(scopes.has_scope("orders:read"));
assert!(scopes.require_all(&["orders:read", "orders:write"]).is_ok());
assert!(scopes.require_any(&["billing:read", "orders:read"]).is_ok());
assert!(scopes.require_all(&["orders:delete"]).is_err());

assert!(Scopes::roles(&claims).has_scope("admin"));
```

| Constructor | Reads |
|---|---|
| `Scopes::parse("a b c")` | a space-delimited string |
| `Scopes::from_claims(&claims)` | `scope` merged with `scp` (Entra ID, Okta) — each a space-delimited string or an array |
| `Scopes::roles(&claims)` | the `roles` claim |
| `Scopes::from_claim(&claims, name)` | any claim holding a string or an array of strings |

| Check | Returns |
|---|---|
| `has_scope(s)` | `bool` |
| `has_all(&[..])` / `has_any(&[..])` | `bool` (`has_all(&[])` is true, `has_any(&[])` is false) |
| `require_all(&[..])` | `Err(ScopeError::Missing("a b"))` naming what is absent |
| `require_any(&[..])` | `Err(ScopeError::NoneOf("a b"))`; an empty list always fails |
| `iter()`, `len()`, `is_empty()`, `Display` | |

Matching is **exact and case-sensitive** — `orders:read` does not imply
`orders`, and there are no wildcards or hierarchies. A missing or mistyped
claim yields an empty set (no grant), not an error. `Scopes` performs no
signature check: build it only from claims a `decode_*` / `verify` call
returned.

## `JwtError`

```rust
#[non_exhaustive]
pub enum JwtError {
    Malformed,              // structure, base64url, JSON, duplicate names, wrong claim type
    UnsupportedAlg,         // alg missing / unknown / not the one asked for / wrong for the key
    InvalidSignature,
    Expired,                // exp
    NotYetValid,            // nbf
    WeakSecret,             // empty or shorter than MIN_SECRET_LEN
    InvalidKey,             // public key used as HMAC secret; invalid or unsupported JWK
    MissingClaim(String),
    InvalidAudience,
    InvalidIssuer,
    InvalidSubject,
    IssuedInFuture,         // iat
    TooOld,                 // max_age
    InvalidType,            // typ header
    UnsupportedCritical,    // crit header present
    UnknownKey,             // no key for the token's kid / alg
    KeyFetch(String),       // remote JWKS could not be fetched
}
```

Derives `Debug, Clone, PartialEq, Eq` and implements `std::error::Error` +
`Display`. No variant or message ever contains the token, the secret or key
bytes.

Map errors to HTTP responses coarsely — `401 Unauthorized` for nearly
everything — and do not tell the caller which check failed:

```rust
use ferroly::auth::{decode_hs256_with, JwtError, Validation};

fn authorize(token: &str, secret: &[u8], validation: &Validation) -> Result<(), u16> {
    match decode_hs256_with(token, secret, validation) {
        Ok(_claims) => Ok(()),
        // Our own misconfiguration, not the caller's fault:
        Err(JwtError::WeakSecret | JwtError::InvalidKey | JwtError::KeyFetch(_)) => Err(500),
        Err(_) => Err(401),
    }
}
```

## What is rejected, and why

| Input | Result | Why |
|---|---|---|
| a second base64url spelling of the same signature (non-zero trailing bits); `=` padding, embedded `=`, whitespace, `+` or `/` in the header or signature segment | `Malformed` | one token must have exactly one textual form (no malleability) |
| any change to the header or payload text, including a respelling that would decode to the same JSON | `InvalidSignature` (or `Malformed`) | the signature covers the exact text |
| claims that are an array, string, number or `null` | `Malformed` | a JWT claims set is a JSON object |
| a duplicated header or claim name | `Malformed` | parsers disagree on which one wins |
| `crit` in the header | `UnsupportedCritical` | no critical extension is understood (RFC 7515 §4.1.11) |
| `alg: none`, unknown `alg`, `alg` differing from the function or key | `UnsupportedAlg` | algorithm confusion |
| a truncated or over-long signature | `InvalidSignature` | no prefix matching |
| a token over 64 KiB | `Malformed` | bounded work before authentication |

## Use with turbo's JWT middleware

[turbo](turbo.md) builds its JWT authentication on this module:

| turbo API | Verifies with | Notes |
|---|---|---|
| `jwt_auth_with(secret, Validation)` | `decode_hs256_with` | the recommended form: `exp` required, 16-byte minimum secret, your audience / issuer / leeway policy |
| `jwt_auth_hmac_with(alg, secret, Validation)` | `decode_hmac_with` | HS384 / HS512 |
| `jwt_auth_jwks(...)` | `Jwks` / `RemoteJwks::verify` | RS256 / ES256 from an identity provider, following key rotation |
| `JwtAuth` | any of the above | builder form, including optional authentication and `Group::jwt` |
| `jwt_auth(secret)` | legacy `decode_hs256` | kept for compatibility: strict parsing, but **no** `Validation` policy and no `exp` requirement |

Verified claims are available to handlers through `ctx.claims()`, and
`require_scopes` / `require_any_scope` / `require_roles` / `require_any_role`
guard routes using [`Scopes`](#scopes-and-roles). A failed check answers `401`
with a `WWW-Authenticate` header, or `403` for a missing scope.

## Limitations

- **Verify-only asymmetric support**, limited to `RS256` and `ES256`. No RSA-PSS,
  no P-384/P-521, no EdDSA, no signing with private keys.
- **No OIDC discovery** (`/.well-known/openid-configuration`), no
  authorization-code or PKCE flows, no ID-token nonce handling, no token
  introspection or revocation, no sessions.
- **ECDSA signatures are inherently malleable**: for a valid `ES256` signature
  `(r, s)`, `(r, n − s)` verifies too (JWS does not require low-`s`). The
  claims cannot be altered, but do not treat the token text or its signature as
  a unique identifier — use a `jti` claim for replay tracking.
- **No JWE** (encrypted tokens) and no nested JWTs; no JWS JSON form, no
  detached-payload (`b64: false`) tokens.
- **No password hashing** (Argon2, scrypt, bcrypt) — see [hash](hash.md).
- **Not FIPS-validated.** The HMAC/SHA-2 code is ferroly's own; RSA/ECDSA
  verification comes from `ring` via `rustls`.
- `aud` is checked only when configured (see [`Validation`](#validation)).
- HMAC secrets are symmetric: anyone who can verify can mint. Use `RS256` /
  `ES256` where the verifier must not be able to sign.

For obtaining tokens as a *client* (OAuth 2.0 client credentials), see
[clients](clients.md#oauth-20-client-credentials).

## See also

- [hash](hash.md) — the HMAC, SHA-2, base64 and `Secret` primitives.
- [clients](clients.md) — outbound auth providers and OAuth 2.0 client credentials.
- [turbo](turbo.md) — `jwt_auth_with`, `jwt_auth_jwks` and the scope guards built on this module.
- [codec](codec.md) — the [`Value`](codec.md) type used for claims.

---
**Related:** [hash](hash.md), [clients](clients.md), [turbo](turbo.md), [codec](codec.md).
