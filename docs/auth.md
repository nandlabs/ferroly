# ferroly::auth

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `auth` · **Module:** `ferroly::auth`

## Overview

`auth` provides authentication primitives. Today that is **HS256 JSON Web
Tokens** — minting ([`encode_hs256`]) and verifying ([`decode_hs256`]) — built
on a **from-scratch SHA-256 + HMAC + base64url** implementation with zero
external dependencies, in keeping with ferroly's near-zero-dependency policy.

HS256 is symmetric: signing and verifying use the same shared `secret`. That
makes it the right fit for **in-process** signing and for services that already
share a secret (e.g. between a gateway and its backends). It is used directly by
[turbo](turbo.md)'s `jwt_auth` middleware to authenticate incoming requests.

The crypto is verified against the standard test vectors:

- **SHA-256** against FIPS 180-4.
- **HMAC-SHA256** against RFC 4231.

So the primitives are not ad-hoc — they match the specifications bit for bit.

### HS256 only (by design)

Only the `HS256` algorithm is supported. **RS256 / ES256** (asymmetric,
public/private key) are intentionally **not** here yet: they need RSA / ECDSA,
which live behind the TLS boundary (`rustls`), and are planned for a later
addition. Verification explicitly rejects any token whose `alg` header is not
`HS256` (see [`JwtError::UnsupportedAlg`]) — this is a deliberate guard against
the classic JWT algorithm-confusion attacks (e.g. `alg: none`, or an RS256 token
verified as HMAC).

## Enabling

This is an **optional, non-default** feature. It is **std-only** — no `tokio`, no
runtime, no external crates.

```toml
[dependencies]
ferroly = { version = "*", features = ["auth"] }
```

The feature implies [`codec`](codec.md); claims are represented as a
[`Value`](codec.md) and encoded/decoded as JSON internally.

## Quick start

```rust
use ferroly::auth::{decode_hs256, encode_hs256};
use ferroly::codec::Value;

// Claims are a codec Value object.
let claims = Value::Object(vec![("sub".into(), "u123".into())]);

// Mint a token signed with a shared secret.
let token = encode_hs256(&claims, b"secret");

// Verify and recover the claims.
let back = decode_hs256(&token, b"secret").unwrap();
assert_eq!(back.get("sub").and_then(Value::as_str), Some("u123"));
```

## API reference

### `encode_hs256`

```rust
pub fn encode_hs256(claims: &Value, secret: &[u8]) -> String
```

Mints an HS256 JWT for `claims`, signed with `secret`. The result is the
standard three-part `base64url(header).base64url(payload).base64url(signature)`
string, where:

- the header is a fixed `{"alg":"HS256","typ":"JWT"}`,
- the payload is the JSON encoding of `claims`,
- the signature is `HMAC-SHA256(secret, "header.payload")`.

Encoding is infallible — it always returns a `String`.

### `decode_hs256`

```rust
pub fn decode_hs256(token: &str, secret: &[u8]) -> Result<Value, JwtError>
```

Verifies an HS256 JWT under `secret` and returns its claims. It performs, in
order:

1. **Structure** — the token must be exactly three dot-separated parts.
   Otherwise [`JwtError::Malformed`].
2. **Algorithm** — the decoded header's `alg` must equal `HS256`. Otherwise
   [`JwtError::UnsupportedAlg`].
3. **Signature** — recomputes `HMAC-SHA256(secret, "header.payload")` and
   compares it against the token's signature in **constant time** (a
   length-checked, timing-safe byte compare, so verification does not leak how
   many bytes matched). Mismatch → [`JwtError::InvalidSignature`].
4. **Expiry** — if the claims contain an `exp` (Unix seconds, integer **or**
   fractional per RFC 7519), it must be strictly in the future (`now < exp`). If
   `now >= exp` → [`JwtError::Expired`]. A present-but-non-numeric `exp` is
   rejected as [`JwtError::Malformed`] rather than silently ignored. Tokens with
   no `exp` never expire.
5. **Not-before** — if the claims contain an `nbf` (same numeric rules), the
   token is not yet valid until `now >= nbf`; earlier than that →
   [`JwtError::NotYetValid`]. A present-but-non-numeric `nbf` → `Malformed`.

On success it returns the claims as a [`Value`](codec.md).

### `JwtError`

```rust
pub enum JwtError {
    Malformed,          // not a well-formed header.payload.signature (or non-numeric exp/nbf)
    UnsupportedAlg,     // alg header is not HS256
    InvalidSignature,   // HMAC did not verify under the secret
    Expired,            // exp claim is in the past
    NotYetValid,        // nbf claim is in the future
}
```

Derives `Debug, PartialEq, Eq` and the crate's `FerrolyError` (so it implements
`std::error::Error` + `Display`). The `Display` messages are: `"malformed JWT"`,
`"unsupported JWT algorithm (only HS256)"`, `"invalid JWT signature"`,
`"JWT expired"`, `"JWT not yet valid"`.

## In depth

### Building claims

Claims are an ordinary codec [`Value`](codec.md) object — a list of
`(String, Value)` pairs. Anything JSON-representable is a valid claim value.
Standard registered claims like `sub`, `iss`, `aud`, `iat`, and `exp` are just
keys you set yourself:

```rust
use ferroly::auth::encode_hs256;
use ferroly::codec::Value;

let claims = Value::Object(vec![
    ("sub".into(),  "alice".into()),
    ("role".into(), "admin".into()),
    ("iss".into(),  "ferroly-app".into()),
]);
let token = encode_hs256(&claims, b"topsecret");
```

### Minting a token with an expiry

`exp` is a Unix timestamp in seconds — an integer is the norm, though a
fractional value is also accepted on verify. Compute it from the current time
plus a lifetime:

```rust
use std::time::{SystemTime, UNIX_EPOCH};
use ferroly::auth::{decode_hs256, encode_hs256};
use ferroly::codec::Value;

let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
let exp = now + 3600; // valid for one hour

let claims = Value::Object(vec![
    ("sub".into(), "u123".into()),
    ("exp".into(), Value::Int(exp)),
]);

let token = encode_hs256(&claims, b"topsecret");

// Within the hour this verifies and returns the claims.
let claims_back = decode_hs256(&token, b"topsecret").unwrap();
assert_eq!(claims_back.get("sub").and_then(Value::as_str), Some("u123"));
```

An already-expired token is rejected:

```rust
use ferroly::auth::{decode_hs256, encode_hs256, JwtError};
use ferroly::codec::Value;

let claims = Value::Object(vec![("exp".into(), Value::Int(1))]); // 1970
let token = encode_hs256(&claims, b"s");
assert_eq!(decode_hs256(&token, b"s"), Err(JwtError::Expired));
```

### Not-before (`nbf`)

An `nbf` (not-before) claim marks the earliest instant a token becomes valid.
Like `exp`, it is a Unix timestamp in seconds and is checked automatically on
decode: while `now < nbf` the token is rejected as
[`JwtError::NotYetValid`]; from `nbf` onward it verifies normally. Use it to mint
tokens that only activate in the future:

```rust
use std::time::{SystemTime, UNIX_EPOCH};
use ferroly::auth::{decode_hs256, encode_hs256, JwtError};
use ferroly::codec::Value;

let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;

// A token that is not valid until an hour from now.
let claims = Value::Object(vec![("nbf".into(), Value::Int(now + 3600))]);
let token = encode_hs256(&claims, b"topsecret");
assert_eq!(decode_hs256(&token, b"topsecret"), Err(JwtError::NotYetValid));

// A token whose nbf is already in the past verifies fine.
let claims = Value::Object(vec![("nbf".into(), Value::Int(1))]); // 1970
let token = encode_hs256(&claims, b"topsecret");
assert!(decode_hs256(&token, b"topsecret").is_ok());
```

Both `exp` and `nbf` accept integer **or** fractional seconds; a present-but-
non-numeric value for either is rejected as [`JwtError::Malformed`] rather than
silently ignored.

### What tampering looks like

Any change to the secret, header, or payload invalidates the signature:

```rust
use ferroly::auth::{decode_hs256, encode_hs256, JwtError};
use ferroly::codec::Value;

let claims = Value::Object(vec![("sub".into(), "alice".into())]);
let token = encode_hs256(&claims, b"topsecret");

// Wrong secret -> InvalidSignature.
assert_eq!(decode_hs256(&token, b"wrong"), Err(JwtError::InvalidSignature));

// Not three parts -> Malformed.
assert_eq!(decode_hs256("not.a.jwt.x", b"s"), Err(JwtError::Malformed));
```

Because `decode_hs256` verifies the signature over `header.payload`, editing the
payload bytes (even to a validly-base64url-encoded different JSON) fails the
signature check rather than being silently accepted.

### Use with turbo's `jwt_auth` middleware

[turbo](turbo.md)'s `jwt_auth` layer calls `decode_hs256` under the hood: it
pulls the bearer token from the `Authorization` header, verifies it against the
configured secret, and either rejects the request or makes the claims available
to handlers. Reuse the same secret on both sides. See [turbo](turbo.md) for
wiring details.

## Error handling

`decode_hs256` returns `Result<Value, JwtError>`. Match on the variant to decide
the HTTP response — typically `401 Unauthorized` for `Malformed`,
`UnsupportedAlg`, `InvalidSignature`, and `Expired` alike (avoid leaking which
check failed to callers):

```rust
use ferroly::auth::{decode_hs256, JwtError};

fn authorize(token: &str, secret: &[u8]) -> Result<(), &'static str> {
    match decode_hs256(token, secret) {
        Ok(_claims) => Ok(()),
        Err(JwtError::Expired) => Err("token expired"),
        Err(_) => Err("unauthorized"),
    }
}
```

## Limitations

- **HS256 only.** No RS256/ES256 (asymmetric) yet — those are deferred until the
  RSA/ECDSA primitives land. `decode_hs256` rejects any other `alg`.
- **Symmetric secret.** Anyone who can verify a token can also mint one; keep the
  secret server-side. Do not use HS256 where you would need a public verifier
  that cannot sign.
- **`exp` and `nbf` are time-checked; `iat` is not.** Expiry (`exp`) and
  not-before (`nbf`) are validated automatically on decode (see the
  [validation steps](#decode_hs256)); `iat` (issued-at) is returned untouched —
  inspect it yourself if you need to enforce a maximum token age.
- **No `aud`/`iss` validation.** Enforce audience/issuer in your own code after
  decoding.
- The header is fixed to `{"alg":"HS256","typ":"JWT"}`; custom header parameters
  (e.g. `kid`) are not emitted by `encode_hs256`.

## See also

- [turbo](turbo.md) — the `jwt_auth` middleware built on `decode_hs256`.
- [codec](codec.md) — the [`Value`](codec.md) type used for claims.

---
**Related:** [turbo](turbo.md), [codec](codec.md).
