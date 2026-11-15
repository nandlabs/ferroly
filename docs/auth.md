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
- **Asymmetric tokens** — `RS256/384/512`, `PS256/384/512`, `ES256/384/512`
  and `EdDSA` (Ed25519), **verify-only**, from
  [JWK / JWKS public keys](#asymmetric-tokens-and-jwks), including a cached
  remote key set for issuers that rotate keys (Entra ID, Okta, Auth0, Cognito,
  Keycloak, …), found from its `jwks_uri` or by
  [OIDC discovery](#oidc-discovery). Requires the `http` feature as well.
- **Replay protection** — a [`jti` replay cache](#jti-replay-protection)
  plugged into `Validation` makes every token usable once.
- **Authorization helpers** — [`Scopes`](#scopes-and-roles) reads `scope` /
  `scp` / `roles` claims.
- [`Secret`](hash.md#secret), re-exported from [hash](hash.md), keeps key
  material out of `Debug` output.

The original [`encode_hs256`](#legacy-encode_hs256--decode_hs256) /
`decode_hs256` pair is kept for compatibility, **deprecated** since 0.6.0 (as
is [turbo](turbo.md)'s `Router::jwt_auth`, which calls it).

### Where the cryptography comes from

- SHA-2 and HMAC are ferroly's own ([hash](hash.md)): hand-rolled, tested
  against FIPS 180-4 and RFC 4231 vectors, **not FIPS-validated**.
- RSA (PKCS#1 v1.5 and PSS) and ECDSA signature verification is **not**
  implemented in ferroly. It is delegated to the verification algorithms of the
  `rustls` crypto provider that the `http` feature already uses for TLS; `auth`
  only converts JOSE key and signature encodings to the DER forms those
  verifiers expect. That is why asymmetric tokens need `http`.
- **EdDSA** (Ed25519) is verified by ferroly's own [`sig`](sig.md) module
  (hand-rolled, Wycheproof-tested, not audited); it needs no provider.
- The provider is the **same one TLS uses** (see
  [TLS crypto backend](http.md#tls-crypto-backend)): the process-default
  provider if your application installed one (e.g. `aws-lc-rs`), else `ring`
  with the default `tls-ring` feature. With neither, RS\* / PS\* / ES\*
  verification fails with `JwtError::NoCryptoProvider` (never a panic); a
  provider that lacks an algorithm gives `JwtError::UnsupportedAlg` (`ring`
  has no P-521, so `ES512` needs e.g. `aws-lc-rs`).
- The provider is looked up on **every** verification, never cached: a
  provider installed after the first verification (a FIPS provider installed
  lazily, say) is used from the next one on, and an algorithm the installed
  provider lacks is never silently served by `ring`.

## Enabling

An **optional, non-default** feature. On its own it is std-only — no `tokio`,
no runtime, no external crates.

```toml
[dependencies]
ferroly = { version = "*", features = ["auth"] }            # HMAC tokens, scopes
ferroly = { version = "*", features = ["auth", "http"] }    # + RS*/PS*/ES*/EdDSA, JWKS, OIDC
```

The feature implies [`codec`](codec.md) (claims are a [`Value`](codec.md)),
[`hash`](hash.md) and [`sig`](sig.md) (for EdDSA; std-only, no dependency).

| Item | Needs |
|---|---|
| `encode_hmac`, `encode_with_header`, `decode_hs256_with`, `decode_hmac_with`, `decode_header`, `encode_hs256`, `decode_hs256` | `auth` |
| `Algorithm`, `Header`, `Validation`, `JwtError`, `MIN_SECRET_LEN`, `MAX_TOKEN_LEN` | `auth` |
| `Scopes`, `ScopeError`, `Secret` | `auth` |
| `ReplayCache`, `MemoryReplayCache` | `auth` |
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
pub enum Algorithm {
    Hs256, Hs384, Hs512,          // HMAC: sign and verify
    Rs256, Rs384, Rs512,          // RSASSA-PKCS1-v1_5: verify-only
    Ps256, Ps384, Ps512,          // RSASSA-PSS: verify-only
    Es256, Es384, Es512,          // ECDSA P-256 / P-384 / P-521: verify-only
    EdDsa,                        // Ed25519 (RFC 8037): verify-only
}
```

| Method | Notes |
|---|---|
| `as_str()` / `Display` | the `alg` header value: `"HS256"`, …, `"EdDSA"` |
| `Algorithm::from_name(&str)` | case-sensitive; `"none"` and unknown names are `None` |
| `is_hmac()` | `true` for the three `Hs*` variants |
| `min_secret_len()` | 32 / 48 / 64 bytes for `Hs256` / `Hs384` / `Hs512` (enforced); `None` for asymmetric |
| `recommended_secret_len()` | the same as `min_secret_len()` |

Every non-HMAC algorithm can only be *verified*, and only through a
[JWK](#asymmetric-tokens-and-jwks). The HMAC functions reject them.

### Secrets

| | Requirement |
|---|---|
| **Validating API** (`encode_hmac`, `encode_with_header`, `decode_hs256_with`, `decode_hmac_with`) | the RFC 7518 §3.2 minimum, the hash output size: **32 bytes** for HS256, **48** for HS384, **64** for HS512 (`Algorithm::min_secret_len()`); shorter is `JwtError::WeakSecret`. `MIN_SECRET_LEN` is 32, the smallest of the three. |
| Legacy `decode_hs256` (deprecated) | any non-empty secret; empty is `JwtError::WeakSecret` |
| Legacy `encode_hs256` (deprecated) | an empty secret yields an empty string (no token); any other length signs |

Before 0.6.0 the validating API accepted 16 bytes. A deployment with a
shorter HS256 secret must rotate to a 32-byte one; until then only the
deprecated legacy functions accept the old secret.

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
| `UnsupportedAlg` | `alg` is not an HMAC algorithm (signing with RSA, ECDSA or EdDSA is not supported) |
| `WeakSecret` | the secret is shorter than `alg.min_secret_len()` (32 / 48 / 64 bytes) |
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

1. **Secret** — at least `alg.min_secret_len()` bytes (32 / 48 / 64), not a
   public key (`WeakSecret`, `InvalidKey`).
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
| `replay_cache(Arc<dyn ReplayCache>)` | none | each `jti` is accepted once (see [`jti` replay protection](#jti-replay-protection)); makes `jti` required | `Replayed`, `MissingClaim("jti")` |

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
#[deprecated] pub fn encode_hs256(claims: &Value, secret: &[u8]) -> String
#[deprecated] pub fn decode_hs256(token: &str, secret: &[u8]) -> Result<Value, JwtError>
```

Kept so existing callers keep compiling, and **deprecated** since 0.6.0: the
compiler points at the replacements. **Use `encode_hmac` and
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
| secret | any non-empty, not a public key | ≥ 32 bytes (RFC 7518), not a public key |

`encode_hs256` is infallible. It refuses only an **empty** secret, by returning
an empty string (not a token: every verifier rejects it as `Malformed`)
instead of a token anyone could forge; it still signs with a short secret and
encodes claims that are not an object (which no verifier here accepts).

## Asymmetric tokens and JWKS

Requires features `auth` **and** `http`. **Verification only** — ferroly
cannot sign with RSA, ECDSA or EdDSA.

| Algorithm | Key | Signature | Verified by |
|---|---|---|---|
| `RS256`, `RS384`, `RS512` | RSA, 2048–8192 bits (`"kty":"RSA"`, members `n`, `e`) | RSASSA-PKCS1-v1_5 with SHA-256/384/512 | rustls provider |
| `PS256`, `PS384`, `PS512` | the same RSA keys | RSASSA-PSS, MGF1 with the same hash, salt as long as the hash (RFC 7518 §3.5) | rustls provider |
| `ES256` | P-256 (`"kty":"EC"`, `"crv":"P-256"`, members `x`, `y`) | ECDSA with SHA-256, 64-byte `r‖s` | rustls provider |
| `ES384` | P-384 (`"crv":"P-384"`) | ECDSA with SHA-384, 96-byte `r‖s` | rustls provider |
| `ES512` | P-521 (`"crv":"P-521"`) | ECDSA with SHA-512, 132-byte `r‖s` | rustls provider **that has P-521** (`aws-lc-rs`; not `ring`, which gives `UnsupportedAlg`) |
| `EdDSA` | Ed25519 (`"kty":"OKP"`, `"crv":"Ed25519"`, member `x`; RFC 8037) | 64-byte Ed25519 | [`ferroly::sig`](sig.md), no provider |

Not supported: `Ed448`, `ES256K` (secp256k1), and key-agreement keys
(`X25519`, `X448`); keys for those are skipped when a key set is loaded.

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
| `Jwk::from_ec_coordinates(KeyType, x, y)` | P-256 / P-384 / P-521: 32 / 48 / 66-byte coordinates |
| `Jwk::from_ed25519_bytes(x)`, `Jwk::from(Ed25519PublicKey)` | a 32-byte Ed25519 key, strictly decoded (non-canonical or small-order is `InvalidKey`) |
| `with_algorithm(alg) -> Result<Jwk, _>` | pin the key to one algorithm (what a JWK's `alg` member does) |
| `with_kid(..)`, `kid()` | the key id |
| `key_type() -> KeyType` | `Rsa`, `EcP256`, `EcP384`, `EcP521` or `Ed25519` |
| `algorithms() -> &[Algorithm]`, `accepts(alg)` | the algorithms this key verifies: the pinned one, or all of its type's (`RS*` and `PS*` for RSA) |
| `algorithm() -> Algorithm` | the default: the pinned one, else `Rs256` / the curve's `ES*` / `EdDsa` |
| `verify(token, &Validation) -> Result<Value, JwtError>` | verify a token and validate its claims |
| `verify_signature(message, signature)` / `verify_signature_with(alg, message, signature)` | verify a detached JWS-format signature with the default or a given algorithm |

A JWK is rejected with `JwtError::InvalidKey` if it is malformed, carries
private material (`d`), has a `use` other than `"sig"` or `key_ops` without
`"verify"`, has an RSA modulus outside 2048–8192 bits, coordinates of the
wrong length for its curve, or an Ed25519 `x` that is not a valid point; and
with `JwtError::UnsupportedAlg` for another key type or curve, or an `alg`
member its key type cannot verify (which rules out, say, an RSA key published
for `RSA-OAEP`). JWK members must be strict, unpadded base64url.

`Debug` on a `Jwk` shows its `kid`, type and algorithms, not the key.

### Algorithm confusion is prevented by construction

- A `Jwk` verifies only the algorithms of its key type (`RS*`/`PS*` for RSA,
  the curve's `ES*`, `EdDSA` for Ed25519), and only its declared `alg` when
  the JWK has one. The token's `alg` header must be one of those; it never
  chooses the key type. An `RS256` token offered to an EC key, or the reverse,
  is `UnsupportedAlg`. (RS\* and PS\* sharing an RSA key is safe: both need
  the RSA private key.)
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

Key selection in `verify`: a key is a candidate if it accepts the token's
`alg` and — when the token has a `kid` — its `kid` is equal. A token **without**
`kid` is tried against every accepting key.

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
| `RemoteJwks::new(url)` | client with a 10 s timeout | `url` must be `https://` (see `allow_insecure_http`) |
| `with_client(Client)` | | custom timeout / shared pool |
| `refresh_interval(Duration)` | 1 hour | age after which the set is re-fetched |
| `min_refresh_interval(Duration)` | 30 s | minimum spacing between fetches triggered by an unknown `kid` or a failing endpoint |
| `max_stale(Duration)` | 4 hours | how long past its refresh interval a set is still served while every refresh fails; after that, verification fails with the fetch error |
| `allow_insecure_http(bool)` | `false` | permit plain `http://` issuer and key-set URLs (a local test IdP); without it they fail with `KeyFetch` and nothing is sent |
| `RemoteJwks::from_issuer(issuer)` | | find the key set by [OIDC discovery](#oidc-discovery) |
| `issuer()` | | the discovered-from issuer, if any |
| `discover().await` | | the key-set URL (runs discovery now if needed) — call at start-up to fail fast |
| `max_response_bytes(usize)` | 1 MiB | larger responses (key set or discovery metadata) are refused |
| `get().await` | | the current set (`Arc<Jwks>`), fetching if needed |
| `refresh().await` | | fetch now, regardless of age |
| `verify(token, &Validation).await` | | as `Jwks::verify`, with rotation handling |

Behaviour:

- **Rotation** — a token whose `kid` is not in the cached set triggers one
  immediate re-fetch, then verification is retried.
- **No fetch storms** — that re-fetch happens at most once per
  `min_refresh_interval`, so a flood of tokens with random `kid`s cannot make
  your service hammer the issuer; within the interval they are `UnknownKey`.
- **Single flight, never blocking** — concurrent callers share one fetch,
  and a fetch never holds up callers that can use the cached set: during an
  hourly refresh, or one triggered by an unknown `kid`, they verify against
  the set they have. Only a caller with no usable set, or one whose token
  names a `kid` the set lacks, waits for the fetch. A malformed token, or one
  with an `HS*` `alg`, is refused before any fetch.
- **Stale on error, within a bound** — if a refresh fails (network error,
  non-2xx, not a JWK Set, or a set with no usable key) while a previous set
  is cached, the previous set keeps being served and the fetch is retried
  after `min_refresh_interval` — for at most `max_stale` past the refresh
  interval. After that verification fails with `JwtError::KeyFetch` until a
  fetch succeeds, so blocking the key-set endpoint cannot keep a rotated-out
  key trusted indefinitely. A failed *first* fetch is an error too, and it
  is retried at most once per `min_refresh_interval`: in between, callers
  get the same error without a request, so a down issuer is not hit once per
  incoming token.
- **https only, by default** — the key set is the root of trust for every
  token, so a non-`https://` key-set or issuer URL fails with `KeyFetch`
  (nothing is sent) unless `allow_insecure_http(true)` is set. This matches
  the store adapters' explicit `plain_http` opt-in. *(Changed in 0.6.0:
  earlier versions fetched `http://` URLs silently.)*

### OIDC discovery

`RemoteJwks::from_issuer(issuer)` finds the key set of an OpenID Connect
provider (OpenID Connect Discovery 1.0) instead of taking the `jwks_uri`:

```rust,no_run
use ferroly::auth::{RemoteJwks, Validation};

async fn setup(token: &str) -> Result<(), ferroly::auth::JwtError> {
    let keys = RemoteJwks::from_issuer("https://login.example.com/tenant-1/v2.0");
    keys.discover().await?; // optional: fail fast at start-up
    let claims = keys.verify(token, &Validation::new().audience("orders-api")).await?;
    println!("{:?}", claims.get("sub"));
    Ok(())
}
```

1. On first use it fetches `{issuer}/.well-known/openid-configuration` (a
   trailing `/` of the issuer is dropped first), bounded by
   `max_response_bytes`.
2. The metadata must be a JSON object whose `issuer` is **exactly** the
   configured issuer (§4.3 — no trailing-slash or case tolerance; this stops
   one tenant's metadata from vouching for another), with a string
   `jwks_uri`. If the issuer is `https://`, so must the `jwks_uri` be.
   Anything else is `JwtError::KeyFetch("OIDC discovery: …")`.
3. The key set is fetched from `jwks_uri` and cached, refreshed and
   rate-limited exactly as for `RemoteJwks::new`. The metadata is fetched
   once per `RemoteJwks`, and again whenever a fetch from the discovered
   `jwks_uri` fails, so an issuer that moves its key set is followed; a
   failure is retried like a failed key-set fetch.
   The `jwks_uri` is **issuer-controlled**: it may name any host, internal
   addresses included, and the client will fetch it. Discover only from
   issuers you trust, or pin the key-set URL with `RemoteJwks::new`.
4. Every token verified through it must carry `"iss": issuer`
   (`MissingClaim("iss")` / `InvalidIssuer`), in addition to whatever the
   `Validation` requires — so you cannot forget the issuer check. Still set
   `.audience(..)`.

Not covered: the rest of the metadata (endpoints, supported algorithms),
WebFinger issuer discovery, and ID-token `nonce` / `at_hash` checks.

## `jti` replay protection

A signed token can be presented again until it expires. To make each token
single-use, give the `Validation` a replay cache:

```rust
use std::sync::Arc;
use ferroly::auth::{decode_hs256_with, encode_hmac, Algorithm, JwtError, MemoryReplayCache, Validation};
use ferroly::codec::Value;

let secret = [7u8; 32];
let claims = Value::Object(vec![
    ("jti".into(), "4f1c9d2e".into()),
    ("exp".into(), Value::Int(4_102_444_800)),
]);
let token = encode_hmac(Algorithm::Hs256, &claims, &secret).unwrap();

let validation = Validation::new().replay_cache(Arc::new(MemoryReplayCache::new(100_000)));
assert!(decode_hs256_with(&token, &secret, &validation).is_ok());
assert_eq!(decode_hs256_with(&token, &secret, &validation), Err(JwtError::Replayed));
```

- With a cache, `jti` is **required** (`MissingClaim("jti")`) and must be a
  string (`Malformed`).
- The check runs **last**, after the signature and every other claim check,
  so a forged or otherwise invalid token never consumes an identifier.
- The token's lifetime must be bounded: a token with neither an `exp` nor
  an `iat` checked by `max_age` is `MissingClaim("exp")`, even with
  `require_exp(false)` — its identifier would have to be remembered forever,
  and such entries would crowd out the ones a bounded cache can protect.
  *(Changed in 0.6.0.)*
- Identifiers are keyed by `iss` **and** `jti` and remembered until the token
  could no longer be accepted anyway: the first second at or after
  `exp + leeway`, or after `iat + max_age + leeway` if sooner (the maximum
  age is inclusive, so a token used in its very last valid second is still
  remembered).
- Clones of the `Validation` share the cache; it works with every entry point
  (`decode_*_with`, `Jwk`, `Jwks`, `RemoteJwks`, turbo's `JwtAuth`).

| Item | Notes |
|---|---|
| `trait ReplayCache: Send + Sync` | `check_and_record(issuer: Option<&str>, jti: &str, expires_at: u64, now: u64) -> bool` — record and return `true` if new, `false` for a replay. Must be atomic. |
| `MemoryReplayCache::new(capacity)` | bounded, in-process. Stores a 32-byte SHA-256 of (`iss`, `jti`) per token, drops expired entries, and by default, when full of live ones, **evicts the entry that expires soonest** (fail open: that token could then be replayed for the rest of its short remaining life) — size it above the number of tokens accepted per token lifetime. |
| `fail_closed(bool)` | `true`: a cache full of live entries **refuses** new tokens instead (`check_and_record` returns `false`, reported as `Replayed`) until entries expire — no replay gets through, at the cost of rejecting valid tokens while full. Default `false`. |
| `len()`, `is_empty()`, `capacity()` | |

`MemoryReplayCache` is per process and lost on restart. Behind a load
balancer, implement `ReplayCache` over a shared store — Redis
`SET key 1 NX EXAT expires_at`, or an insert into a table with a unique index —
so a token replayed against another instance is caught.

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
    KeyFetch(String),       // remote JWKS or OIDC metadata could not be fetched or is invalid
    NoCryptoProvider,       // RS*/PS*/ES* without a rustls provider (no `tls-ring`, none installed)
    Replayed,               // the jti was seen before (Validation::replay_cache)
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
        Err(
            JwtError::WeakSecret
            | JwtError::InvalidKey
            | JwtError::KeyFetch(_)
            | JwtError::NoCryptoProvider,
        ) => Err(500),
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
| `jwt_auth_with(secret, Validation)` | `decode_hs256_with` | the recommended form: `exp` required, 32-byte minimum secret, your audience / issuer / leeway policy |
| `jwt_auth_hmac_with(alg, secret, Validation)` | `decode_hmac_with` | HS384 / HS512 |
| `jwt_auth_jwks(...)` | `Jwks` / `RemoteJwks::verify` | RS\*, PS\*, ES\*, EdDSA from an identity provider (by `jwks_uri` or OIDC discovery), following key rotation |
| `JwtAuth` | any of the above | builder form, including optional authentication and `Group::jwt` |
| `jwt_auth(secret)` (deprecated) | legacy `decode_hs256` | kept for compatibility: strict parsing, but **no** `Validation` policy and no `exp` requirement |

Verified claims are available to handlers through `ctx.claims()`, and
`require_scopes` / `require_any_scope` / `require_roles` / `require_any_role`
guard routes using [`Scopes`](#scopes-and-roles). A failed check answers `401`
with a `WWW-Authenticate` header, or `403` for a missing scope. A token that
cannot be checked at all — the remote key set cannot be fetched (`KeyFetch`)
or no crypto provider is installed (`NoCryptoProvider`) — answers `503`, so
an outage is not reported as invalid tokens. A request with more than one
`Authorization` header answers `401`, even with optional authentication.
`Router::validate` reports a `Jwks` of RSA/EC keys when no provider is
installed.

## Limitations

- **Verify-only asymmetric support**: no signing with private keys. No Ed448
  or secp256k1; `ES512` only with a provider that has P-521.
- **OIDC discovery covers the key set only**: no authorization-code or PKCE
  flows, no ID-token `nonce` / `at_hash` handling, no token introspection or
  revocation lists, no sessions. Replay protection is per `jti`
  (`ReplayCache`), not a revocation list.
- **ECDSA signatures are inherently malleable**: for a valid `ES256` signature
  `(r, s)`, `(r, n − s)` verifies too (JWS does not require low-`s`). The
  claims cannot be altered, but do not treat the token text or its signature as
  a unique identifier — use a `jti` claim and a [replay cache](#jti-replay-protection).
- **No JWE** (encrypted tokens) and no nested JWTs; no JWS JSON form, no
  detached-payload (`b64: false`) tokens.
- **No password hashing** (Argon2, scrypt, bcrypt) — see [hash](hash.md).
- **Not FIPS-validated.** The HMAC/SHA-2 code (HS\*) and Ed25519 (EdDSA) are
  ferroly's own, with no FIPS route (see [hash](hash.md#fips-what-a-fips-provider-does-and-does-not-cover));
  RSA/ECDSA verification comes from the `rustls` crypto provider (`ring` by
  default; an installed process default such as a FIPS provider takes
  precedence, even when installed after the first verification).
- `aud` is checked only when configured (see [`Validation`](#validation)).
- HMAC secrets are symmetric: anyone who can verify can mint. Use an
  asymmetric algorithm where the verifier must not be able to sign.

For obtaining tokens as a *client* (OAuth 2.0 client credentials), see
[clients](clients.md#oauth-20-client-credentials).

## See also

- [hash](hash.md) — the HMAC, SHA-2, base64 and `Secret` primitives.
- [clients](clients.md) — outbound auth providers and OAuth 2.0 client credentials.
- [turbo](turbo.md) — `jwt_auth_with`, `jwt_auth_jwks` and the scope guards built on this module.
- [codec](codec.md) — the [`Value`](codec.md) type used for claims.

---
**Related:** [hash](hash.md), [clients](clients.md), [turbo](turbo.md), [codec](codec.md).
