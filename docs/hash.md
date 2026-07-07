# `ferroly::hash` — streaming cryptographic hashes

Public, dependency-free hashing primitives for content-addressing, integrity
checks, and caching. Every hasher is **incremental**: feed bytes as they arrive
(a download, a large file read) and finalize once, without buffering the whole
input in memory.

Enable with the `hash` feature:

```toml
ferroly = { version = "0.1", features = ["hash"] }
```

All algorithms are implemented in-house — FIPS 180-4 SHA-256, FIPS 180-1 SHA-1,
RFC 2104 HMAC — with no external crypto dependency. (`auth` and `ws` build on
this module, so their SHA lives here too.)

## Contents

| Item | Kind | Output |
|---|---|---|
| [`Sha256`](#sha256) | incremental hasher | `Digest<32>` |
| [`Sha1`](#sha1) | incremental hasher (legacy) | `Digest<20>` |
| [`HmacSha256`](#hmac-sha256) | incremental keyed hasher | `Digest<32>` |
| [`sha256`](#one-shot-helpers) / [`sha1`](#one-shot-helpers) / [`hmac_sha256`](#one-shot-helpers) | one-shot fns | `Digest<N>` |
| [`Digest<N>`](#digest) | fixed-size output | hex / bytes |

## `Sha256`

```rust
use ferroly::hash::Sha256;

let mut h = Sha256::new();
h.update(b"hello, ");
h.update(b"world");            // call update as many times as you like
let digest = h.finalize();     // -> Digest<32>

assert_eq!(digest.to_hex().len(), 64);
```

`update` accepts arbitrary chunk sizes; the hasher buffers partial 64-byte
blocks internally, so streaming produces the same digest as a single call.

## `Sha1`

Provided only for legacy interop (e.g. the WebSocket handshake accept key).
SHA-1 is broken for collision resistance — prefer [`Sha256`](#sha256) for
anything security-sensitive.

```rust
use ferroly::hash::sha1;
assert_eq!(sha1(b"abc").to_hex(), "a9993e364706816aba3e25717850c26c9cd0d89d");
```

## HMAC-SHA256

```rust
use ferroly::hash::{hmac_sha256, HmacSha256};

// One-shot:
let tag = hmac_sha256(b"key", b"message");

// Streaming (e.g. authenticating a body as it streams in):
let mut mac = HmacSha256::new(b"key");
mac.update(b"mess");
mac.update(b"age");
assert_eq!(mac.finalize(), tag);
```

Keys longer than the 64-byte block are hashed first; shorter keys are
zero-padded, per RFC 2104.

## One-shot helpers

For when you already have the whole input in memory:

```rust
use ferroly::hash::{sha256, sha1, hmac_sha256};

let a = sha256(b"abc");
let b = sha1(b"abc");
let c = hmac_sha256(b"key", b"msg");
```

## `Digest<N>`

A fixed-size (`N`-byte) hash output.

| Method | Returns | Notes |
|---|---|---|
| `to_hex()` | `String` | lowercase, `2 * N` chars |
| `as_bytes()` | `&[u8; N]` | raw bytes |
| `into_bytes()` | `[u8; N]` | consumes the digest |
| `ct_eq(&other)` | `bool` | **constant-time** equality (no early exit) |
| `from_bytes([u8; N])` | `Digest<N>` | wrap existing bytes |

`Digest` also implements `Display` (hex), `Debug`, `AsRef<[u8]>`, `PartialEq`,
`Eq`, `Hash`, `Copy`. Use `ct_eq` — not `==` — when comparing a computed MAC
against an untrusted value to avoid timing side channels.

```rust
use ferroly::hash::sha256;

let expected = sha256(b"payload");
let actual = sha256(b"payload");
assert!(actual.ct_eq(&expected));
assert_eq!(format!("{actual}"), actual.to_hex());
```

## Limitations

- **No BLAKE3** (yet) — SHA-256 is the recommended content-address hash here.
- **No signing/verification** — hashing and HMAC only.
- SHA-1 is included for interop, not for security.

## See also

- [auth](auth.md) — HS256 JWTs built on `hmac_sha256`.
- [ws](ws.md) — the RFC 6455 handshake accept key uses `sha1`.
