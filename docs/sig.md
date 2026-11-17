# `ferroly::sig` — Ed25519 signature verification (verify-only)

Confirm that a payload (a release binary, model weights, a container layer, a
config bundle) matches a detached signature and a public key before you trust
it. Pair it with [`hash`](hash.md) and the resumable
[`download_to_file`](http.md) helper: fetch, hash, verify.

- **Ed25519 (RFC 8032)**, with the Ed25519ctx and Ed25519ph variants.
- **Verify-only.** There is no signing and no key generation.
- **Fails closed.** A malformed key, a malformed signature or a mismatch is an
  error (or `false`), never a panic.
- **Strict.** Non-canonical encodings, `S >= L` and small-order points are
  rejected (see [Strictness](#strictness-rules)).
- **Streaming.** A large input can be verified as it arrives, without
  buffering it.

Enable with the `sig` feature. It pulls in `hash` (for SHA-512) and nothing
else:

```toml
ferroly = { version = "0.7", features = ["sig"] }
```

> **Hand-rolled, not audited.** Field arithmetic modulo 2^255 − 19, scalars
> modulo the group order and the edwards25519 curve operations are implemented
> in this module in safe Rust, with no external crypto crate. They are checked
> against every RFC 8032 Ed25519 / Ed25519ctx / Ed25519ph test vector, every
> Project Wycheproof Ed25519 vector, independently generated edge cases, and
> by fuzz targets, but they have had
> **no independent security audit** and are **not FIPS-validated**.

## Quick start

```rust
use ferroly::sig::{verify_ed25519, Ed25519PublicKey, Ed25519Signature, SigError};

fn check(key_bytes: &[u8], payload: &[u8], sig_bytes: &[u8]) -> Result<(), SigError> {
    // Typed API: decode the key once, verify many signatures with it.
    let key = Ed25519PublicKey::from_slice(key_bytes)?;     // 32 bytes
    let sig = Ed25519Signature::from_slice(sig_bytes)?;     // 64 bytes
    key.verify(payload, &sig)
}

// Boolean API: `false` on any failure, wrong lengths included.
fn trusted(key_bytes: &[u8], payload: &[u8], sig_bytes: &[u8]) -> bool {
    verify_ed25519(key_bytes, payload, sig_bytes)
}
```

### Verifying a download as it streams

Pure Ed25519 hashes `R || A || M`. A detached signature is normally fetched
before the payload, so `R` is known first and the payload can be absorbed chunk
by chunk:

```rust
use ferroly::sig::{Ed25519PublicKey, Ed25519Signature, Ed25519Verifier, SigError};

fn verify_stream(
    key: &Ed25519PublicKey,
    sig: &Ed25519Signature,
    chunks: impl IntoIterator<Item = Vec<u8>>,
) -> Result<(), SigError> {
    // A malformed signature is refused here, before any payload is read.
    let mut v = Ed25519Verifier::new(key, sig)?;
    for chunk in chunks {
        v.update(&chunk);
    }
    v.verify()
}
```

For **Ed25519ph** (the message is hashed with SHA-512 first), use
`Ed25519phVerifier`, which takes the signature at the end. You can also feed
your own `ferroly::hash::Sha512`, for example the one you already use to
checksum the file, and pass its digest to `verify_prehashed`:

```rust
use ferroly::hash::Sha512;
use ferroly::sig::{Ed25519PublicKey, Ed25519Signature, Ed25519phVerifier, SigError};

fn verify_ph(key: &Ed25519PublicKey, sig: &Ed25519Signature) -> Result<(), SigError> {
    // Streaming verifier...
    let mut v = Ed25519phVerifier::new(key, b"")?; // optional context, ≤ 255 bytes
    v.update(b"a");
    v.update(b"bc");
    let streamed = v.verify(sig);

    // ...or bring your own SHA-512.
    let mut h = Sha512::new();
    h.update(b"abc");
    let prehashed = key.verify_prehashed(&h.finalize(), b"", sig);

    streamed.and(prehashed)
}
```

## API

| Item | Purpose |
|---|---|
| `verify_ed25519(public_key: &[u8], message: &[u8], signature: &[u8]) -> bool` | One call, fails closed. Any error, wrong lengths included, is `false`. |
| `Ed25519PublicKey::from_bytes(&[u8; 32]) -> Result<Self, SigError>` | Strict decode: canonical, on the curve, not small-order. Does the point decompression once. |
| `Ed25519PublicKey::from_slice(&[u8])` / `TryFrom<&[u8]>` | The same, after checking the length is 32. |
| `Ed25519PublicKey::from_spki_der(&[u8])` / `key.to_spki_der()` | A DER `SubjectPublicKeyInfo` (RFC 8410 §4, exactly 44 bytes), as in X.509 and `openssl pkey -pubout -outform DER`. |
| `Ed25519PublicKey::from_pem(&str)` | A PEM `-----BEGIN PUBLIC KEY-----` block holding that structure. |
| `Ed25519PublicKey::from_openssh(&str)` | An OpenSSH `ssh-ed25519 AAAA… [comment]` line (RFC 8709), as in `id_ed25519.pub`. |
| `key.as_bytes()` / `key.to_bytes()` | The 32-byte encoding. |
| `key.verify(message, &sig)` | Pure Ed25519. |
| `key.verify_ctx(message, context, &sig)` | Ed25519ctx. `context` must be 1 to 255 bytes (an empty one is refused; see [Strictness](#strictness-rules)). |
| `key.verify_ph(message, context, &sig)` | Ed25519ph. The message is SHA-512-hashed here. `context` is 0 to 255 bytes. |
| `key.verify_prehashed(&Digest<64>, context, &sig)` | Ed25519ph given `SHA-512(message)`. |
| `Ed25519Signature::from_bytes(&[u8; 64])` / `From<[u8; 64]>` | Wraps 64 bytes. `R` and `S` are validated at verification time. |
| `Ed25519Signature::from_slice(&[u8])` / `TryFrom<&[u8]>` | The same, after checking the length is 64. |
| `Ed25519Verifier::new(&key, &sig)?` → `update(&[u8])`… → `verify()` | Streaming pure Ed25519. |
| `Ed25519phVerifier::new(&key, context)?` → `update(&[u8])`… → `verify(&sig)` | Streaming Ed25519ph. |
| `MAX_CONTEXT_LEN` | 255, the longest context RFC 8032 allows. |

Keys and signatures implement `Clone`, `Debug` (as hex), `PartialEq`, `Eq` and
`Hash`. `Ed25519Signature` is also `Copy`.

### `SigError`

`#[non_exhaustive]`, `Copy`, implements `std::error::Error`. It is deliberately
coarse: it names the input that was unusable, never which internal check
failed.

| Variant | Meaning |
|---|---|
| `MalformedKey` | Wrong length, a non-canonical encoding, not a curve point, or a small-order point. |
| `MalformedSignature` | Wrong length, `S >= L`, or `R` not a canonical, non-small-order curve point. A tampered signature lands here or in `VerificationFailed` depending on the bits that changed. |
| `InvalidContext` | Context longer than 255 bytes, or empty for Ed25519ctx. |
| `VerificationFailed` | Everything decoded, but the signature does not match this key and message. |

Treat every variant the same way: **do not trust the payload**. Converted into
`ferroly::Error`, both `MalformedSignature` and `VerificationFailed` classify as
`ErrorKind::Unauthenticated` (a tampered signature can produce either), while
`MalformedKey` and `InvalidContext` are `InvalidInput`.

## Strictness rules

RFC 8032 §5.1.7 leaves some choices to the verifier. This module makes the
strict choice. Its accept/reject answer matches libsodium on every edge case
of "Taming the many EdDSAs" (checked in `tests/sig_edge_vectors.rs`), and it is
at least as strict as `ed25519-dalek`'s `verify_strict`: dalek does not enforce
`y < p` when it decompresses a key, so it accepts some non-canonical encodings
of mixed-order keys that are rejected here (not exploitable either way).

| Rule | Behaviour | Why |
|---|---|---|
| `S` canonical | `S >= L` is `MalformedSignature`. | Required by RFC 8032. Without it, `(R, S + L)` is a second valid signature for the same message (malleability). |
| `A`, `R` canonical | An encoding with `y >= p` is rejected. | RFC 8032 decoding. One point has only one accepted encoding. |
| No "negative zero" | `x = 0` with the sign bit set is rejected. | RFC 8032 §5.1.3 step 4. |
| On the curve | An encoding with no matching `x` is rejected. | RFC 8032 §5.1.3 step 3. |
| Small-order `A` rejected | `MalformedKey` for any `A` with `[8]A` the identity (the eight torsion points). | A small-order key, with `R` small-order and `S = 0`, "verifies" many messages. No honest key is small-order. |
| Small-order `R` rejected | `MalformedSignature`. | An honest signer's `R = [r]B` is never small-order. |
| **Cofactorless equation** | Checks `[S]B = R + [k]A` by computing `[S]B − [k]A` and comparing its encoding with the signature's `R` bytes (in constant time), with `k` reduced mod `L` first. | See below. |
| Empty Ed25519ctx context | `InvalidContext`, even for a correctly signed signature. | **Stricter than RFC 8032**, which only says the context SHOULD NOT be empty. An empty context is what plain Ed25519 is for; a signer that used Ed25519ctx with an empty context cannot be verified here. |

### Why cofactorless

RFC 8032 lets a verifier check either the cofactored equation
`[8][S]B = [8]R + [8][k]A` or the cofactorless one `[S]B = R + [k]A`. For
every signature an honest signer produces, both give the same answer. They
can differ only when `A` or `R` carries a small torsion component `T`, which
an honest signer never produces:

- **Mixed-order key** (`A' = A + T`): the key decodes, since it is not
  small-order. A signature made with the torsion-free secret passes the
  cofactorless check only when `[k]T` vanishes (for `T` of order 8, when
  `k ≡ 0 (mod 8)`). The cofactored check always passes it.
- **Mixed-order `R`** (`R' = [r]B + T`): rejected whenever the key is
  torsion-free, which every honestly generated key is. With a crafted
  mixed-order key, a crafted mixed-order `R` whose torsion cancels the key's
  (`T_R = −[k]T_A`) is **accepted**, exactly as by libsodium, `ed25519-dalek`
  and Go. The cofactored check accepts every mixed-order `R`.
- **`k` is reduced mod `L` before `[k]A`**, as in the RFC 8032 §6 reference
  code, libsodium and `ed25519-dalek`. For a mixed-order key this differs from
  a literal reading of §5.1.7 (multiplying by the full 512-bit digest): a
  signature with `(h mod L) ≡ 0 (mod 8)` but `h ≢ 0 (mod 8)` verifies here.

The cofactorless check is the one most deployed verifiers use (libsodium,
`ed25519-dalek`'s `verify`/`verify_strict`, Go's `crypto/ed25519`, BoringSSL).
Using it here gives the same accept/reject answer on these edge cases. This
module has no batch verification, and batch verification is the main reason to
prefer the cofactored check (a batch and a single verify must agree). Every
case above is pinned by tests (`src/sig/tests.rs`, `tests/sig_edge_vectors.rs`).

## Performance and side channels

A verification costs one double-scalar multiplication (interleaved 4-bit
windows, variable time), one decompression of `R`, one inversion (addition
chains for the inversion and the square root) plus SHA-512 over the message.
That is roughly 65 µs per signature in an optimised build on a current laptop
core (about 10 % less than before the addition chains), a few times slower
than assembly-optimised libraries. Decoding a key (point decompression and
the small-order check) costs about 12 µs, so decode once and reuse the key. `Ed25519Verifier` validates `R` and `S` once, in `new`.

Verification only handles **public** values (key, message, signature), so the
arithmetic is **variable-time**: the field, scalar and curve code branch on
and index tables by their inputs. The final comparison of `R` is
constant-time anyway. There are no secrets to leak because there is no
signing, and the arithmetic must never be reused with secret scalars (for
signing or key generation) as it stands.

## Limits

- Messages have no length limit. Memory use is constant: the streaming
  verifiers hold only a SHA-512 state.
- Contexts are at most 255 bytes (`MAX_CONTEXT_LEN`, an RFC 8032 limit).
  `Ed25519phVerifier` copies the context, so it holds at most 255 bytes.
- Hostile input never panics. Every length is checked, and every decode and
  arithmetic path is total. `tests/sig_fuzz.rs` (always-on) and the
  `ed25519_verify` libFuzzer target exercise this.

## How it is tested

- `tests/sig_vectors.rs`: all RFC 8032 §7.1 Ed25519 vectors (TEST 1, 2, 3,
  1024, SHA(abc)), the four §7.2 Ed25519ctx vectors and the §7.3 Ed25519ph
  vector, through the typed API, `verify_ed25519` and both streaming verifiers
  (for every two-way split of the message). Also checks that a signature for
  one variant never verifies as another.
- `tests/sig_negative.rs`: tampered messages; every single-bit flip of the
  signature and of the key; the wrong key; `S = L`, `S = L + 1`,
  `S = 2^256 − 1` and the malleated `S + L`; non-canonical `R` and `A`
  (`y = p … 2^255 − 1`, negative zero); off-curve points; all eight
  small-order points as `A` and as `R`; mixed-order `A` and `R` built
  independently in Python; wrong lengths; context limits.
- `tests/sig_fuzz.rs`: seeded random and mutated inputs. They must not panic
  or verify, the boolean and typed APIs must agree, and streaming must match
  one-shot.
- `tests/sig_containers.rs`: the RFC 8410 §10.1 PEM example, an
  `ssh-keygen` public key, and malformed containers (wrong OID, unused bits,
  truncation, trailing bytes, another OpenSSH key type).
- `tests/sig_edge_vectors.rs`: every Project Wycheproof `ed25519_test.json`
  vector (117 valid, 64 invalid, 29 "acceptable" small- or mixed-order keys,
  whose outcome is pinned); the 12 "Taming the many EdDSAs" edge cases
  (rebuilt independently) against libsodium's verdict; and every single-byte
  edit of valid signatures, keys and messages, which must be rejected (and
  agree between the streaming and one-shot verifiers).
- Unit tests in `src/sig/tests.rs`: field encoding and reduction edge cases
  (`p`, `p + k`, `2^255 − 1`), inverses, square roots and non-squares, the
  constants `d` and `sqrt(−1)`, scalar reduction (`L`, `L ± 1`,
  `2^512 − 1`, `2^256`), the group order (`[L]B` is the identity), the windowed
  multiplication against a naive ladder, the inversion and square-root
  addition chains against square-and-multiply, worst-case limb inputs against
  values computed independently with big integers, and a test-only signer
  that reproduces the RFC 8032 TEST 1 signature byte for byte. The signer also
  pins the edge cases above: a cancelling mixed-order `R`, the `k mod L`
  reduction, and the refused empty Ed25519ctx context.
- `fuzz/fuzz_targets/ed25519_verify.rs`: coverage-guided fuzzing, seeded with
  the RFC vectors.
- `fuzz/fuzz_targets/ed25519_structured.rs`: the fuzzer edits valid RFC 8032
  triples; the oracle is that an edited triple verifies if and only if it is
  unchanged, so the accept path is exercised.

## Limitations and out of scope

- **Verification only.** No signing, no key generation, no private-key
  handling.
- **Ed25519 only.** No Ed448, RSA (PKCS#1 v1.5 / PSS) or ECDSA (P-256 and
  others) here. JWT RS\*, PS\* and ES\* verification exists in
  [auth](auth.md), delegated to the TLS stack's crypto provider; JWT `EdDSA`
  (Ed25519) verification in `auth` uses this module.
- **No batch verification**, and no option to check the cofactored equation
  instead.
- **Few container formats.** Public keys can be read from raw bytes, DER / PEM
  `SubjectPublicKeyInfo` and OpenSSH `ssh-ed25519` lines. Signatures are raw
  64 bytes; minisign (whose default mode prehashes with BLAKE2b, which ferroly
  does not implement), signify and OpenSSH `SSHSIG` framing are up to you.
  Decode base64 or hex with `ferroly::hash::{base64, hex}`.
- **Not audited, not FIPS-validated** (see the note at the top).

### Possible follow-ups

- RSA / ECDSA verification as separate features, if there is demand.
- Batch verification (with the cofactored equation it requires).
- Signed windows and a wider base-point table (about 20–30 % faster).

## See also

- [hash](hash.md): SHA-512 and the strict base64 / hex decoders used with
  this module.
- [http](http.md): `download_to_file`, for fetching the artifacts you verify.
- [auth](auth.md): JWT verification (HS\*, RS\*, PS\*, ES\*, and EdDSA through
  this module).
