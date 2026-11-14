# `ferroly::hash` — hashing, MACs, key derivation, encodings, randomness

Public, dependency-free primitives for content-addressing, integrity checks,
request signing and token handling. Every hasher is **incremental**: feed bytes
as they arrive (a download, a large file read) and finalize once, without
buffering the whole input in memory.

Enable with the `hash` feature (std-only, no other feature or crate required):

```toml
ferroly = { version = "0.4", features = ["hash"] }
```

> **Hand-rolled, not FIPS-validated.** Every algorithm here is implemented
> in-house in safe Rust — FIPS 180-4 SHA-2, FIPS 180-1 SHA-1, RFC 1321 MD5,
> RFC 2104 HMAC, RFC 5869 HKDF, RFC 8018 PBKDF2, RFC 4648 base64 — with no
> external crypto dependency. They are tested against the published vectors and
> against an independent implementation, but they are **not FIPS 140
> validated**, have had **no independent security audit**, use **no hardware
> acceleration**, and — apart from the explicit constant-time comparisons —
> make **no side-channel guarantees**. If your environment mandates validated
> cryptography, do not use this module for that purpose.

## Contents

| Area | Items |
|---|---|
| [Cryptographic hashes](#hashes) | `Sha256`, `Sha384`, `Sha512`, legacy `Sha1` |
| [Checksums — **not** cryptographic](#checksums-md5-and-crc-32) | `Md5`, `Crc32` |
| [HMAC](#hmac) | `Hmac<H>`, `HmacSha256`, `HmacSha384`, `HmacSha512`, `HmacSha1`, `HmacMd5` |
| [Key derivation](#key-derivation-kdf) | `kdf::hkdf_sha256`, `kdf::pbkdf2_hmac_sha256`, generic forms |
| [Base64](#base64) | `base64::{encode, decode, …}` — strict and lenient |
| [Hex](#hex) | `hex::{encode, encode_upper, decode, decode_to_slice}` |
| [Constant-time comparison](#constant-time-comparison) | `ct_eq`, `Digest::ct_eq` |
| [`Secret`](#secret) | redacting, wipe-on-drop byte container |
| [Secure randomness](#secure-randomness) | `random::{fill, bytes, array, token_hex, token_base64url, …}` |
| [`Digest<N>`](#digestn) | fixed-size output: hex / bytes |
| [`HashAlgorithm`](#the-hashalgorithm-trait) | trait behind the generic HMAC and KDFs |

## Hashes

| Type | One-shot | Output | Block | Use |
|---|---|---|---|---|
| `Sha256` | `sha256(data)` | `Digest<32>` | 64 bytes | the default |
| `Sha384` | `sha384(data)` | `Digest<48>` | 128 bytes | when a protocol asks for it |
| `Sha512` | `sha512(data)` | `Digest<64>` | 128 bytes | when a protocol asks for it |
| `Sha1` | `sha1(data)` | `Digest<20>` | 64 bytes | **legacy interop only** |

All four have the same shape — `new()`, `update(&[u8])` any number of times,
`finalize()` — and implement `Clone` (fork a partially-fed state) and `Default`.

```rust
use ferroly::hash::{sha512, Sha256, Sha512};

let mut h = Sha256::new();
h.update(b"hello, ");
h.update(b"world");            // call update as many times as you like
let digest = h.finalize();     // -> Digest<32>
assert_eq!(digest.to_hex().len(), 64);

// Streaming and one-shot agree, whatever the chunking.
let mut s = Sha512::new();
s.update(b"ab");
s.update(b"c");
assert_eq!(s.finalize(), sha512(b"abc"));
```

`update` accepts arbitrary chunk sizes; partial blocks are buffered internally,
so streaming produces the same digest as a single call.

**SHA-1** is broken for collision resistance. It is here only for protocols that
mandate it (the WebSocket handshake accept key, legacy signatures). Prefer
`Sha256` for anything security-sensitive.

## Checksums: MD5 and CRC-32

These detect *accidental* corruption. They are **not cryptographic** and give
no protection against anyone who modifies data on purpose.

| Type | One-shot | Output | Notes |
|---|---|---|---|
| `Md5` | `md5(data)` | `Digest<16>` | RFC 1321. Collisions are trivial to produce. For `Content-MD5`, S3-style ETags, legacy manifests. |
| `Crc32` | `crc32(data)` | `u32` | IEEE 802.3 / ISO-HDLC — the CRC of zlib, gzip, PNG, zip. |

```rust
use ferroly::hash::{crc32, md5, Crc32};

assert_eq!(md5(b"abc").to_hex(), "900150983cd24fb0d6963f7d28e17f72");
assert_eq!(crc32(b"123456789"), 0xcbf4_3926);

let mut crc = Crc32::new();
crc.update(b"1234");
crc.update(b"56789");
assert_eq!(crc.finalize(), 0xcbf4_3926);
```

`Crc32::finalize(&self)` does not consume the state, so a running checksum can
be read mid-stream. Never use MD5 or a CRC for signatures, password storage,
deduplicating untrusted input, or any check an attacker could influence — use
`Sha256` or an HMAC.

## HMAC

`Hmac<H>` is RFC 2104 HMAC over any [`HashAlgorithm`](#the-hashalgorithm-trait).
Use the aliases and one-shot functions:

| Alias | One-shot | Tag |
|---|---|---|
| `HmacSha256` | `hmac_sha256(key, msg)` | `Digest<32>` |
| `HmacSha384` | `hmac_sha384(key, msg)` | `Digest<48>` |
| `HmacSha512` | `hmac_sha512(key, msg)` | `Digest<64>` |
| `HmacSha1` | `hmac_sha1(key, msg)` | `Digest<20>` — legacy interop (TOTP/HOTP, old signing schemes) |
| `HmacMd5` | `hmac::<Md5>(key, msg)` | `Digest<16>` — legacy interop only |

`hmac::<H>(key, msg)` is the generic one-shot.

```rust
use ferroly::hash::{hmac_sha256, HmacSha256};

// One-shot:
let tag = hmac_sha256(b"key", b"message");

// Streaming (e.g. authenticating a body as it streams in):
let mut mac = HmacSha256::new(b"key");
mac.update(b"mess");
mac.update(b"age");
assert_eq!(mac.finalize(), tag);

// Verifying a received tag — constant-time, never `==` on raw bytes:
let mut check = HmacSha256::new(b"key");
check.update(b"message");
assert!(check.verify(tag.as_bytes()));
```

| Method | Notes |
|---|---|
| `Hmac::<H>::new(key)` | Keys longer than the hash's block are hashed first; shorter keys are zero-padded. |
| `update(&[u8])` | Absorbs message bytes. |
| `finalize() -> H::Output` | The tag. |
| `verify(tag: &[u8]) -> bool` | Finishes and compares in constant time. A tag of the wrong length (including a truncated one) is `false`. |

A keyed `Hmac` is `Clone`: key it once and clone it per message to MAC many
messages cheaply. An **empty key is accepted** (RFC 2104 allows it) but
authenticates nothing — reject empty and short keys yourself when they come
from configuration. RFC 2104 recommends a key at least as long as the hash
output (32 bytes for HMAC-SHA256).

## Key derivation (`kdf`)

| Function | Purpose |
|---|---|
| `kdf::hkdf_sha256(salt, ikm, info, &mut okm)` | HKDF (RFC 5869): derive keys from **already strong** key material. |
| `kdf::hkdf_extract::<H>(salt, ikm)` / `kdf::hkdf_expand::<H>(prk, info, &mut okm)` / `kdf::hkdf::<H>(…)` | The two HKDF stages and the generic form. |
| `kdf::pbkdf2_hmac_sha256(password, salt, iterations, &mut out)` | PBKDF2 (RFC 8018): stretch a **low-entropy** secret. |
| `kdf::pbkdf2_hmac::<H>(…)` | PBKDF2 over another hash. |

Errors are `kdf::KdfError`: `OutputTooLong` (HKDF produces at most
`255 * hash_len` bytes — 8160 for SHA-256) and `ZeroIterations`.

```rust
use ferroly::hash::kdf;

// Two independent keys from one master secret (distinct `info` strings).
let (mut enc, mut mac) = ([0u8; 32], [0u8; 32]);
kdf::hkdf_sha256(b"salt", b"master key material", b"enc", &mut enc).unwrap();
kdf::hkdf_sha256(b"salt", b"master key material", b"mac", &mut mac).unwrap();
assert_ne!(enc, mac);

// A key from a passphrase: unique random salt, high iteration count.
let mut key = [0u8; 32];
kdf::pbkdf2_hmac_sha256(b"correct horse", b"per-secret-random-salt", 600_000, &mut key).unwrap();
```

- **HKDF is fast by design. Never run it on a password.**
- **PBKDF2** is the only password-based KDF here. Its cost is linear in
  `iterations` and this implementation has no hardware acceleration, so
  benchmark it; OWASP's 2023 reference point for PBKDF2-HMAC-SHA256 is 600,000
  iterations. Use a unique random salt of at least 16 bytes per secret.
- PBKDF2 is **not memory-hard**. For storing user passwords prefer Argon2id,
  scrypt or bcrypt — none of which ferroly implements (see
  [Limitations](#limitations)).

## Base64

`ferroly::hash::base64` is the single base64 implementation in the crate
(RFC 4648): two alphabets, each with and without `=` padding.

| | Encode | Strict decode |
|---|---|---|
| standard (`+/`), padded | `base64::encode` | `base64::decode` |
| standard, unpadded | `base64::encode_no_pad` | `base64::decode_no_pad` |
| URL-safe (`-_`), padded | `base64::encode_url` | `base64::decode_url` |
| URL-safe, unpadded (JWT/JOSE `base64url`) | `base64::encode_url_no_pad` | `base64::decode_url_no_pad` |

Plus the general forms `base64::encode_with(input, alphabet, pad)` /
`base64::decode_with(input, alphabet, pad)` with `base64::Alphabet::{Standard,
UrlSafe}`, `base64::encoded_len(n, pad)`, and the explicitly named
`base64::decode_lenient`. Decoders accept `&str`, `String`, `&[u8]` — anything
`AsRef<[u8]>`.

```rust
use ferroly::hash::base64;

assert_eq!(base64::encode(b"hi"), "aGk=");
assert_eq!(base64::encode_url_no_pad(&[0xfb, 0xff]), "-_8");
assert_eq!(base64::decode("aGk=").unwrap(), b"hi");

// Strict: a second spelling of the same bytes is an error...
assert_eq!(base64::decode("aGl="), Err(base64::DecodeError::TrailingBits));
// ...which the lenient decoder accepts, along with whitespace.
assert_eq!(base64::decode_lenient("aGl=\n").unwrap(), b"hi");
```

### Strict decoding

The strict decoders are **injective**: exactly one input string decodes to a
given byte string. They reject, with a `base64::DecodeError`:

| Rejected input | Error |
|---|---|
| a character outside the chosen alphabet — including whitespace, the other alphabet's `+ /` or `- _`, and an embedded `=` | `InvalidCharacter { index }` |
| a length that cannot be base64 (`4n + 1` characters) | `InvalidLength` |
| missing padding where it is required, padding where it is forbidden, more than two `=`, `=` not at the end | `InvalidPadding` / `InvalidCharacter` / `InvalidLength` |
| non-zero unused bits in the final quantum (`"Zh"` instead of `"Zg"`) | `TrailingBits` |

Use them for anything security-relevant — tokens, signatures, keys — so two
different strings can never stand for the same bytes (the JWT malleability that
lenient decoding allows). `DecodeError` never contains any of the input, so it
is safe to log.

### Lenient decoding

`base64::decode_lenient` is for human-edited input such as PEM bodies. It
accepts **either** alphabet (even mixed), optional padding, ASCII whitespace
anywhere, and non-zero trailing bits. Several inputs therefore decode to the
same bytes: never use it to compare or validate tokens or signatures. It still
rejects other non-alphabet bytes, data after padding, more than two `=`, and an
impossible length.

Decoding is table-driven and not constant-time with respect to the input.

## Hex

```rust
use ferroly::hash::hex;

assert_eq!(hex::encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
assert_eq!(hex::encode_upper(&[0xde, 0xad]), "DEAD");
assert_eq!(hex::decode("DEADbeef").unwrap(), [0xde, 0xad, 0xbe, 0xef]);

let mut out = [0u8; 2];
hex::decode_to_slice("beef", &mut out).unwrap();
```

`hex::decode` accepts either case and nothing else — no `0x` prefix, no
separators, no whitespace. `hex::DecodeError` is `OddLength`,
`InvalidCharacter { index }` or `InvalidLength` (for `decode_to_slice` and
`Digest::from_hex` when the destination size does not match).

## Constant-time comparison

```rust
use ferroly::hash::{ct_eq, hmac_sha256};

let expected = hmac_sha256(b"key", b"body");
let received: &[u8] = expected.as_bytes(); // e.g. decoded from a header
assert!(ct_eq(expected.as_bytes(), received));
```

`ct_eq(a: &[u8], b: &[u8]) -> bool` compares two byte slices without exiting at
the first difference. Use it — never `==` — to compare a MAC, signature, token
or password hash against an untrusted value; `==` lets an attacker recover the
expected value byte by byte from response timing.

Slices of different lengths compare unequal immediately, so the **length is not
hidden**. `Digest::ct_eq` is the same check for two digests of equal size;
`Hmac::verify` and `Secret::ct_eq` use it internally.

## `Secret`

A container for secret bytes — a key, token or password — that cannot be
printed by accident.

```rust
use ferroly::hash::Secret;

let key = Secret::from("s3cr3t-api-key");
assert_eq!(format!("{key:?}"), "Secret([REDACTED])");
assert_eq!(format!("{key}"), "[REDACTED]");
assert_eq!(key.expose_str(), Some("s3cr3t-api-key"));
assert!(key.ct_eq(b"s3cr3t-api-key"));
```

| Item | Notes |
|---|---|
| `Secret::new(impl Into<Vec<u8>>)`, `From<String>`, `From<Vec<u8>>`, `From<&str>`, `From<&[u8]>`, `From<&[u8; N]>` | `String` and `Vec<u8>` are taken over without a copy; borrowed data is copied. |
| `expose() -> &[u8]` | The bytes. Keep the borrow short; do not log it. |
| `expose_str() -> Option<&str>` | The bytes as text, `None` if not UTF-8. |
| `len()`, `is_empty()` | |
| `ct_eq(&[u8]) -> bool`, `==` | Constant-time in the content. |
| `Debug`, `Display` | Print `Secret([REDACTED])` / `[REDACTED]` — so a `Secret` field in a `#[derive(Debug)]` struct, a log line or an error does not leak. |
| `Clone` | An independent copy (wiped separately). |
| `Drop` | Overwrites the whole allocation with zeros. |

`Secret` is re-exported as `ferroly::auth::Secret` and
`ferroly::clients::Secret`.

**The wipe is best-effort.** ferroly forbids `unsafe`, so the wipe is an
ordinary write followed by `std::hint::black_box`: the optimiser is
*discouraged*, not prevented, from removing it, and there is no volatile write.
It also cannot reach copies that live outside the `Secret` — the `&str` you
built it from, anything you copy out of `expose()`, a `String` that
reallocated before conversion, swap files, core dumps. Treat it as defence in
depth, not a guarantee.

## Secure randomness

`ferroly::hash::random` returns cryptographically secure bytes from the
operating system.

```rust
use ferroly::hash::random;

fn make_credentials() -> Result<(), random::RandomError> {
    let mut key = [0u8; 32];
    random::fill(&mut key)?;                      // raw key material
    let nonce: [u8; 12] = random::array()?;       // fixed-size array
    let session_id = random::token_hex(16)?;      // 32 hex characters
    let api_token = random::token_base64url(32)?; // 43 URL-safe characters
    let secret = random::secret(32)?;             // wrapped in a `Secret`
    println!("{session_id} {} {secret:?}", api_token.len()); // secret prints as [REDACTED]
    let _ = nonce;
    Ok(())
}
```

| Function | Returns |
|---|---|
| `random::fill(&mut [u8])` | fills the buffer |
| `random::bytes(len)` | `Vec<u8>` |
| `random::array::<N>()` | `[u8; N]` |
| `random::u64()` | a uniform `u64` |
| `random::secret(len)` | a `Secret` |
| `random::token_hex(byte_len)` | `2 * byte_len` lowercase hex characters |
| `random::token_base64url(byte_len)` | unpadded URL-safe base64 — safe in URLs, headers, cookies |
| `random::is_supported()` | `const fn`: whether this platform has a source at all |

16 random bytes (128 bits) is a sound minimum for an unguessable identifier.

### Platform support and limits

ferroly allows neither extra dependencies nor `unsafe`, which rules out the
`getrandom(2)` and `BCryptGenRandom` system calls. The only remaining portable
route to the kernel CSPRNG is the device file.

| Platform | Source | Result |
|---|---|---|
| Unix (Linux, macOS, the BSDs, …) | `/dev/urandom` | random bytes |
| Everything else (Windows, wasm, …) | none | `Err(RandomError::Unsupported)` |

There is deliberately **no fallback** to a weaker source (time, addresses, a
userspace PRNG). An unsupported platform is an explicit error, never silently
predictable output.

On Unix:

- `/dev/urandom` must exist and be a character device (a regular file planted
  at that path is refused). In a `chroot`, container or sandbox without it,
  every call fails with `RandomError::Io`.
- The device is opened once and kept open for the life of the process, so the
  first call can fail under file-descriptor exhaustion.
- On Linux, `/dev/urandom` does not block before the kernel's pool is
  initialised. That matters only very early at boot (initramfs, first boot of
  an embedded device): do not generate long-lived keys there.

`RandomError` is `Unsupported` or `Io(std::io::Error)`. On error the buffer
contents are unspecified and must not be used.

## `Digest<N>`

A fixed-size (`N`-byte) hash output.

| Method | Returns | Notes |
|---|---|---|
| `to_hex()` | `String` | lowercase, `2 * N` chars |
| `Digest::from_hex(&str)` | `Result<Digest<N>, hex::DecodeError>` | either case, exactly `2 * N` chars |
| `as_bytes()` | `&[u8; N]` | raw bytes |
| `into_bytes()` | `[u8; N]` | consumes the digest |
| `ct_eq(&other)` | `bool` | **constant-time** equality (no early exit) |
| `from_bytes([u8; N])` | `Digest<N>` | wrap existing bytes |

`Digest` also implements `Display` (hex), `Debug`, `AsRef<[u8]>`, `PartialEq`,
`Eq`, `Hash`, `Copy`. Use `ct_eq` — not `==` — when comparing a computed MAC
against an untrusted value to avoid timing side channels.

```rust
use ferroly::hash::{sha256, Digest};

// Verify a download against a published checksum.
let expected: Digest<32> = Digest::from_hex(
    "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
).unwrap();
assert!(sha256(b"abc").ct_eq(&expected));
assert_eq!(format!("{expected}"), expected.to_hex());
```

## The `HashAlgorithm` trait

```rust
pub trait HashAlgorithm: Clone {
    const BLOCK_LEN: usize;   // 64 for SHA-256/SHA-1/MD5, 128 for SHA-384/512
    const OUTPUT_LEN: usize;  // digest size in bytes
    type Output: AsRef<[u8]>; // Digest<OUTPUT_LEN>

    fn new() -> Self;
    fn update(&mut self, data: &[u8]);
    fn finalize(self) -> Self::Output;
}
```

Implemented by `Sha256`, `Sha384`, `Sha512`, `Sha1` and `Md5`; it is what makes
`Hmac<H>`, `kdf::hkdf::<H>` and `kdf::pbkdf2_hmac::<H>` generic. The inherent
`new` / `update` / `finalize` methods on those types do the same thing without
importing the trait.

## How it is tested

Nothing is compared against itself. `tests/hash_vectors.rs` checks:

- SHA-1 / SHA-256 / SHA-384 / SHA-512 against the FIPS 180 example messages
  (empty, `"abc"`, the 448-bit and 896-bit messages, one million `a`);
- every block-boundary length (0, 1, 55, 56, 63, 64, 65, 111, 112, 119, 120,
  127, 128, 129, …) against digests from an independent implementation
  (CPython's `hashlib`), with every two-way split of the input and several
  chunk sizes;
- HMAC-SHA-256/384/512 against all seven RFC 4231 cases (including the
  131-byte long-key cases), HMAC-SHA-1 against RFC 2202;
- MD5 against the RFC 1321 suite, CRC-32 against the catalogue check value;
- HKDF-SHA256 against RFC 5869 cases 1–3, PBKDF2-HMAC-SHA256 against
  RFC 7914 §11.

## Limitations

- **Not FIPS-validated, not audited, no hardware acceleration** (see the note
  at the top). SHA-NI / AVX2 implementations are several times faster.
- **No password hashing** — no Argon2, scrypt or bcrypt. PBKDF2 is available
  but is not memory-hard.
- **No SHA-3, BLAKE2 or BLAKE3.**
- **No encryption and no signing.** Signature *verification* for JWTs (RS256,
  ES256) lives in [auth](auth.md) and is delegated to the TLS stack's crypto
  provider.
- **Secure randomness is Unix-only** (see
  [Platform support](#platform-support-and-limits)).
- `Secret`'s wipe is best-effort.
- SHA-1, MD5 and CRC-32 are included for interop, not for security.

## See also

- [auth](auth.md) — JWTs built on the HMACs and the strict base64url decoder.
- [clients](clients.md) — auth providers that hold credentials in `Secret`.
- [ws](ws.md) — the RFC 6455 handshake accept key uses `sha1`.
