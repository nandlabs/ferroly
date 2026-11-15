//! Verify-only digital signatures: Ed25519 (RFC 8032), hand-rolled.
//!
//! Confirm that a payload — a release binary, model weights, a config bundle —
//! matches a detached signature and a public key before you trust it. Every
//! check **fails closed**: a malformed key, a malformed signature or a
//! mismatch is an error (or `false`), never a panic.
//!
//! | Variant | One-shot | Streaming |
//! |---|---|---|
//! | Ed25519 | [`Ed25519PublicKey::verify`], [`verify_ed25519`] | [`Ed25519Verifier`] (signature known up front) |
//! | Ed25519ctx | [`Ed25519PublicKey::verify_ctx`] | — |
//! | Ed25519ph | [`Ed25519PublicKey::verify_ph`], [`Ed25519PublicKey::verify_prehashed`] | [`Ed25519phVerifier`] |
//!
//! ```
//! use ferroly::sig::{verify_ed25519, Ed25519PublicKey, Ed25519Signature};
//! # fn unhex(s: &str) -> Vec<u8> {
//! #     (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
//! # }
//! // RFC 8032 §7.1, TEST 2.
//! let key = unhex("3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c");
//! let sig = unhex(
//!     "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da\
//!      085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
//! );
//! let msg = [0x72];
//!
//! // Typed API: decode once, verify many.
//! let key = Ed25519PublicKey::from_slice(&key)?;
//! let signature = Ed25519Signature::from_slice(&sig)?;
//! key.verify(&msg, &signature)?;
//! assert!(key.verify(b"tampered", &signature).is_err());
//!
//! // Boolean API: any failure is `false`.
//! assert!(verify_ed25519(key.as_bytes(), &msg, &sig));
//! assert!(!verify_ed25519(key.as_bytes(), b"tampered", &sig));
//! # Ok::<(), ferroly::sig::SigError>(())
//! ```
//!
//! # Strictness
//!
//! Verification follows RFC 8032 §5.1.7 and resolves its options the strict
//! way. The accept/reject answer matches libsodium on every edge case of
//! "Taming the many EdDSAs" (Chalkias et al.), and is at least as strict as
//! `ed25519-dalek`'s `verify_strict` (which, unlike this module, accepts some
//! non-canonical `y >= p` encodings of mixed-order keys):
//!
//! - **`S` must be canonical** (`S < L`); `S >= L` is rejected (signature
//!   malleability).
//! - **`A` and `R` must be canonical encodings** (`y < p`, and no "negative
//!   zero" `x`), and must decode to curve points.
//! - **Small-order `A` and `R` are rejected** (points `P` with `[8]P` the
//!   identity). A small-order public key would accept forged signatures on
//!   many messages.
//! - **The cofactorless equation `[S]B = R + [k]A`** is checked, by comparing
//!   the encoding of `[S]B - [k]A` with the signature's `R` bytes, where `k`
//!   is the SHA-512 challenge **reduced mod `L`** first (as in the RFC 8032
//!   §6 reference code, libsodium and `ed25519-dalek`). Honest signatures
//!   satisfy both this and the cofactored equation. The two can differ only
//!   when `A` or `R` carries a small torsion component: a crafted
//!   mixed-order `R` is rejected whenever the key is torsion-free (every
//!   honestly generated key), but can be accepted for a crafted mixed-order
//!   key whose torsion cancels it (see the docs page).
//! - **Ed25519ctx refuses an empty context** ([`SigError::InvalidContext`]),
//!   which is stricter than RFC 8032 (it only says the context SHOULD NOT be
//!   empty). Verify such signatures as Ed25519ctx elsewhere, or ask the signer
//!   to use plain Ed25519, which is what an empty context is meant to be.
//!
//! # Not constant-time
//!
//! Verification handles only public values (key, message, signature), so the
//! arithmetic here (field, scalar and curve layers alike) is **variable-time**:
//! it branches on and indexes tables by its inputs. The final comparison is
//! constant-time. There is **no signing** — there are no secrets to protect —
//! and the arithmetic must never be reused with secret scalars.
//!
//! # Hand-rolled, not audited
//!
//! Field arithmetic modulo `2^255 - 19`, scalars modulo the group order and
//! the curve operations are implemented here in safe Rust, using SHA-512 from
//! [`crate::hash`]. They are checked against every RFC 8032 Ed25519 vector,
//! but have had **no independent security audit**.

#![deny(missing_docs)]

use std::fmt;

use crate::hash::{ct_eq, Digest, Sha512};

mod edwards;
mod field;
mod scalar;

/// Unit tests, and a test-only signer other modules' tests use (`auth`).
#[cfg(test)]
pub(crate) mod tests;

use edwards::Point;

/// The `dom2` prefix of RFC 8032 §2 (32 ASCII octets).
const DOM2_PREFIX: &[u8; 32] = b"SigEd25519 no Ed25519 collisions";

/// The longest context RFC 8032 allows (it is length-prefixed with one octet).
pub const MAX_CONTEXT_LEN: usize = 255;

/// The DER prefix of an Ed25519 `SubjectPublicKeyInfo` (RFC 8410 §4):
/// `SEQUENCE { SEQUENCE { OID 1.3.101.112 }, BIT STRING (0 unused bits) }`,
/// followed by the 32 key bytes.
const SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// Why a signature did not verify.
///
/// Deliberately coarse: it says *which input* was unusable, never which
/// internal check failed. Every variant means "do not trust the payload".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SigError {
    /// The public key is not a usable Ed25519 key: wrong length, not a
    /// canonical point encoding, not on the curve, or of small order.
    MalformedKey,
    /// The signature is structurally invalid: wrong length, a non-canonical
    /// `S` (`S >= L`), or an `R` that is not a canonical, non-small-order
    /// curve point. Like [`VerificationFailed`](Self::VerificationFailed),
    /// this is what a tampered signature produces (which of the two depends on
    /// the bits flipped), so treat both as a failed authentication.
    MalformedSignature,
    /// The context string is unusable: longer than [`MAX_CONTEXT_LEN`] bytes,
    /// or empty for Ed25519ctx (which RFC 8032 says should not be empty —
    /// use plain Ed25519 instead).
    InvalidContext,
    /// Everything decoded, but the signature does not match the key and
    /// message.
    VerificationFailed,
}

impl fmt::Display for SigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SigError::MalformedKey => "malformed Ed25519 public key",
            SigError::MalformedSignature => "malformed Ed25519 signature",
            SigError::InvalidContext => "invalid Ed25519 context string",
            SigError::VerificationFailed => "Ed25519 signature verification failed",
        })
    }
}

impl std::error::Error for SigError {}

/// An Ed25519 public key: a strictly decoded, non-small-order curve point.
///
/// Decoding does the expensive point decompression once, so keep the key
/// around to verify many signatures.
#[derive(Clone)]
pub struct Ed25519PublicKey {
    bytes: [u8; 32],
    /// `-A`, so verification computes `[S]B + [k](-A)`.
    neg_a: Point,
}

impl Ed25519PublicKey {
    /// The encoded length of a public key, in bytes.
    pub const LENGTH: usize = 32;

    /// Decodes a 32-byte public key.
    ///
    /// Fails with [`SigError::MalformedKey`] if the bytes are not the
    /// canonical encoding of a curve point, or the point has small order.
    pub fn from_bytes(bytes: &[u8; 32]) -> Result<Self, SigError> {
        let a = Point::decompress(bytes).ok_or(SigError::MalformedKey)?;
        if a.is_small_order() {
            return Err(SigError::MalformedKey);
        }
        Ok(Ed25519PublicKey {
            bytes: *bytes,
            neg_a: a.neg(),
        })
    }

    /// Decodes a public key from a slice, which must be exactly
    /// [`LENGTH`](Self::LENGTH) bytes ([`SigError::MalformedKey`] otherwise).
    pub fn from_slice(bytes: &[u8]) -> Result<Self, SigError> {
        let array: &[u8; 32] = bytes.try_into().map_err(|_| SigError::MalformedKey)?;
        Self::from_bytes(array)
    }

    /// Decodes a DER `SubjectPublicKeyInfo` holding an Ed25519 key
    /// (RFC 8410 §4, algorithm `id-Ed25519` with absent parameters): exactly
    /// 44 bytes. Anything else — another algorithm, parameters, trailing
    /// bytes — is [`SigError::MalformedKey`].
    pub fn from_spki_der(der: &[u8]) -> Result<Self, SigError> {
        match der.strip_prefix(&SPKI_PREFIX[..]) {
            Some(key) => Self::from_slice(key),
            None => Err(SigError::MalformedKey),
        }
    }

    /// Decodes a PEM `-----BEGIN PUBLIC KEY-----` block (the format of
    /// `openssl pkey -pubout`) holding an Ed25519 `SubjectPublicKeyInfo`.
    /// Text around the block is ignored; the base64 body must be strict
    /// (padded, canonical) apart from line breaks.
    pub fn from_pem(pem: &str) -> Result<Self, SigError> {
        const BEGIN: &str = "-----BEGIN PUBLIC KEY-----";
        const END: &str = "-----END PUBLIC KEY-----";
        let start = pem.find(BEGIN).ok_or(SigError::MalformedKey)? + BEGIN.len();
        let len = pem[start..].find(END).ok_or(SigError::MalformedKey)?;
        let body: String = pem[start..start + len]
            .chars()
            .filter(|c| !matches!(c, '\r' | '\n' | ' ' | '\t'))
            .collect();
        let der = crate::hash::base64::decode(&body).map_err(|_| SigError::MalformedKey)?;
        Self::from_spki_der(&der)
    }

    /// Decodes an OpenSSH public key line, `ssh-ed25519 AAAA… [comment]`
    /// (the format of `~/.ssh/id_ed25519.pub` and `authorized_keys` entries
    /// without options). The blob must be exactly the `ssh-ed25519` type
    /// string and a 32-byte key (RFC 8709).
    pub fn from_openssh(line: &str) -> Result<Self, SigError> {
        let mut fields = line.split_ascii_whitespace();
        if fields.next() != Some("ssh-ed25519") {
            return Err(SigError::MalformedKey);
        }
        let blob = fields.next().ok_or(SigError::MalformedKey)?;
        let blob = crate::hash::base64::decode(blob).map_err(|_| SigError::MalformedKey)?;
        // string "ssh-ed25519", string key (each a u32 big-endian length).
        let mut expected = Vec::with_capacity(51);
        expected.extend_from_slice(&11u32.to_be_bytes());
        expected.extend_from_slice(b"ssh-ed25519");
        expected.extend_from_slice(&32u32.to_be_bytes());
        match blob.strip_prefix(&expected[..]) {
            Some(key) => Self::from_slice(key),
            None => Err(SigError::MalformedKey),
        }
    }

    /// The DER `SubjectPublicKeyInfo` encoding (RFC 8410 §4), 44 bytes.
    pub fn to_spki_der(&self) -> Vec<u8> {
        let mut der = SPKI_PREFIX.to_vec();
        der.extend_from_slice(&self.bytes);
        der
    }

    /// The 32-byte encoding.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.bytes
    }

    /// The 32-byte encoding, by value.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.bytes
    }

    /// Verifies a pure Ed25519 signature (RFC 8032 §5.1.7) over `message`.
    pub fn verify(&self, message: &[u8], signature: &Ed25519Signature) -> Result<(), SigError> {
        let mut h = self.challenge_hasher(None, signature);
        h.update(message);
        self.finish(signature, h)
    }

    /// Verifies an Ed25519ctx signature: pure Ed25519 domain-separated by a
    /// `context` of 1 to [`MAX_CONTEXT_LEN`] bytes.
    ///
    /// An empty or over-long context is [`SigError::InvalidContext`]. Refusing
    /// the empty context is stricter than RFC 8032, which only says it SHOULD
    /// NOT be used: a correctly signed Ed25519ctx signature with an empty
    /// context does not verify here.
    pub fn verify_ctx(
        &self,
        message: &[u8],
        context: &[u8],
        signature: &Ed25519Signature,
    ) -> Result<(), SigError> {
        if context.is_empty() || context.len() > MAX_CONTEXT_LEN {
            return Err(SigError::InvalidContext);
        }
        let mut h = self.challenge_hasher(Some((0, context)), signature);
        h.update(message);
        self.finish(signature, h)
    }

    /// Verifies an Ed25519ph signature over `message`, which is hashed with
    /// SHA-512 first. `context` may be empty (the default) and is at most
    /// [`MAX_CONTEXT_LEN`] bytes.
    ///
    /// To verify a large input without holding it in memory, use
    /// [`Ed25519phVerifier`] or hash it yourself and call
    /// [`verify_prehashed`](Self::verify_prehashed).
    pub fn verify_ph(
        &self,
        message: &[u8],
        context: &[u8],
        signature: &Ed25519Signature,
    ) -> Result<(), SigError> {
        let mut h = Sha512::new();
        h.update(message);
        self.verify_prehashed(&h.finalize(), context, signature)
    }

    /// Verifies an Ed25519ph signature given the SHA-512 digest of the
    /// message (for example from [`crate::hash::Sha512`] fed while
    /// downloading). `context` may be empty and is at most
    /// [`MAX_CONTEXT_LEN`] bytes.
    pub fn verify_prehashed(
        &self,
        prehash: &Digest<64>,
        context: &[u8],
        signature: &Ed25519Signature,
    ) -> Result<(), SigError> {
        if context.len() > MAX_CONTEXT_LEN {
            return Err(SigError::InvalidContext);
        }
        let mut h = self.challenge_hasher(Some((1, context)), signature);
        h.update(prehash.as_bytes());
        self.finish(signature, h)
    }

    /// `SHA-512(dom2(F, C) || R || A)`, ready to absorb `PH(M)`.
    fn challenge_hasher(&self, dom: Option<(u8, &[u8])>, signature: &Ed25519Signature) -> Sha512 {
        let mut h = Sha512::new();
        if let Some((flag, context)) = dom {
            h.update(DOM2_PREFIX);
            // The callers cap the context at 255 bytes.
            h.update(&[flag, context.len() as u8]);
            h.update(context);
        }
        h.update(&signature.bytes[..32]);
        h.update(&self.bytes);
        h
    }

    /// Checks `S` and `R`, then the group equation, with `h` holding
    /// `SHA-512(dom2 || R || A || PH(M))` so far.
    fn finish(&self, signature: &Ed25519Signature, h: Sha512) -> Result<(), SigError> {
        let s = signature.check_structure()?;
        self.check_equation(&s, signature, h)
    }

    /// The group equation for an already validated `S` (see
    /// [`Ed25519Signature::check_structure`]).
    fn check_equation(
        &self,
        s: &[u8; 32],
        signature: &Ed25519Signature,
        h: Sha512,
    ) -> Result<(), SigError> {
        // k is reduced mod L before [k]A, as in the RFC 8032 §6 reference
        // code, libsodium and ed25519-dalek. For a key with a torsion
        // component this differs from multiplying by the full 512-bit digest
        // (a literal reading of §5.1.7); see the docs page.
        let k = scalar::reduce(h.finalize().as_bytes());
        // R' = [S]B - [k]A; the signature is valid iff R' encodes to R.
        let r_check = edwards::double_scalar_mul_base(s, &k, &self.neg_a).compress();
        if ct_eq(&r_check, &signature.bytes[..32]) {
            Ok(())
        } else {
            Err(SigError::VerificationFailed)
        }
    }
}

impl fmt::Debug for Ed25519PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Ed25519PublicKey({})",
            crate::hash::hex::encode(&self.bytes)
        )
    }
}

impl PartialEq for Ed25519PublicKey {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for Ed25519PublicKey {}

impl std::hash::Hash for Ed25519PublicKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.bytes.hash(state);
    }
}

impl TryFrom<&[u8]> for Ed25519PublicKey {
    type Error = SigError;

    fn try_from(bytes: &[u8]) -> Result<Self, SigError> {
        Self::from_slice(bytes)
    }
}

/// A detached 64-byte Ed25519 signature `R || S`.
///
/// Construction only checks the length; `R` and `S` are validated (and the
/// signature rejected if either is malformed) when it is verified.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ed25519Signature {
    bytes: [u8; 64],
}

impl Ed25519Signature {
    /// The encoded length of a signature, in bytes.
    pub const LENGTH: usize = 64;

    /// Wraps 64 signature bytes.
    pub fn from_bytes(bytes: &[u8; 64]) -> Self {
        Ed25519Signature { bytes: *bytes }
    }

    /// Wraps a signature from a slice, which must be exactly
    /// [`LENGTH`](Self::LENGTH) bytes ([`SigError::MalformedSignature`]
    /// otherwise).
    pub fn from_slice(bytes: &[u8]) -> Result<Self, SigError> {
        let array: &[u8; 64] = bytes.try_into().map_err(|_| SigError::MalformedSignature)?;
        Ok(Self::from_bytes(array))
    }

    /// The 64-byte encoding.
    pub fn as_bytes(&self) -> &[u8; 64] {
        &self.bytes
    }

    /// The 64-byte encoding, by value.
    pub fn to_bytes(&self) -> [u8; 64] {
        self.bytes
    }

    /// Validates `S < L` and that `R` is a canonical, non-small-order point;
    /// returns `S`.
    fn check_structure(&self) -> Result<[u8; 32], SigError> {
        let mut r = [0u8; 32];
        let mut s = [0u8; 32];
        r.copy_from_slice(&self.bytes[..32]);
        s.copy_from_slice(&self.bytes[32..]);
        if !scalar::is_canonical(&s) {
            return Err(SigError::MalformedSignature);
        }
        match Point::decompress(&r) {
            Some(point) if !point.is_small_order() => Ok(s),
            _ => Err(SigError::MalformedSignature),
        }
    }
}

impl fmt::Debug for Ed25519Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Ed25519Signature({})",
            crate::hash::hex::encode(&self.bytes)
        )
    }
}

impl From<[u8; 64]> for Ed25519Signature {
    fn from(bytes: [u8; 64]) -> Self {
        Ed25519Signature { bytes }
    }
}

impl TryFrom<&[u8]> for Ed25519Signature {
    type Error = SigError;

    fn try_from(bytes: &[u8]) -> Result<Self, SigError> {
        Self::from_slice(bytes)
    }
}

/// Verifies a pure Ed25519 signature from raw bytes, failing closed.
///
/// Returns `true` only if `public_key` is a valid 32-byte key, `signature` a
/// well-formed 64-byte signature, and the signature matches `message`. Every
/// other outcome — wrong lengths included — is `false`. Use
/// [`Ed25519PublicKey::verify`] for the reason.
pub fn verify_ed25519(public_key: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let Ok(key) = Ed25519PublicKey::from_slice(public_key) else {
        return false;
    };
    let Ok(signature) = Ed25519Signature::from_slice(signature) else {
        return false;
    };
    key.verify(message, &signature).is_ok()
}

/// Streaming verification of a **pure Ed25519** signature that is known
/// before the message (a detached `.sig` fetched ahead of the payload).
///
/// Pure Ed25519 hashes `R || A || M`, so with `R` in hand the message can be
/// absorbed chunk by chunk — no buffering of the payload, and the result is
/// identical to [`Ed25519PublicKey::verify`] on the concatenated chunks.
///
/// ```
/// use ferroly::sig::{Ed25519PublicKey, Ed25519Signature, Ed25519Verifier};
/// # fn unhex(s: &str) -> Vec<u8> {
/// #     (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
/// # }
/// // RFC 8032 §7.1, TEST 3: message af 82.
/// let key = Ed25519PublicKey::from_slice(&unhex(
///     "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
/// ))?;
/// let sig = Ed25519Signature::from_slice(&unhex(
///     "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac\
///      18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
/// ))?;
/// let mut v = Ed25519Verifier::new(&key, &sig)?;
/// v.update(&[0xaf]);
/// v.update(&[0x82]);
/// v.verify()?;
/// # Ok::<(), ferroly::sig::SigError>(())
/// ```
#[derive(Clone)]
pub struct Ed25519Verifier {
    key: Ed25519PublicKey,
    signature: Ed25519Signature,
    /// `S`, validated once in [`new`](Self::new) (with `R`).
    s: [u8; 32],
    h: Sha512,
}

impl Ed25519Verifier {
    /// Starts verifying `signature` under `key`.
    ///
    /// A structurally malformed signature is rejected here
    /// ([`SigError::MalformedSignature`]), before any message is read.
    pub fn new(key: &Ed25519PublicKey, signature: &Ed25519Signature) -> Result<Self, SigError> {
        let s = signature.check_structure()?;
        Ok(Ed25519Verifier {
            key: key.clone(),
            signature: *signature,
            s,
            h: key.challenge_hasher(None, signature),
        })
    }

    /// Absorbs the next chunk of the message.
    pub fn update(&mut self, chunk: &[u8]) {
        self.h.update(chunk);
    }

    /// Finishes: `Ok(())` only if the signature matches everything absorbed.
    pub fn verify(self) -> Result<(), SigError> {
        // `R` and `S` were validated by `new`; only the equation is left.
        self.key.check_equation(&self.s, &self.signature, self.h)
    }
}

impl fmt::Debug for Ed25519Verifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ed25519Verifier")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// Streaming **Ed25519ph** verification (RFC 8032 §5.1, `PH = SHA-512`): feed
/// the message in chunks, supply the signature at the end.
///
/// ```
/// use ferroly::sig::{Ed25519PublicKey, Ed25519Signature, Ed25519phVerifier};
/// # fn unhex(s: &str) -> Vec<u8> {
/// #     (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
/// # }
/// // RFC 8032 §7.3, TEST abc.
/// let key = Ed25519PublicKey::from_slice(&unhex(
///     "ec172b93ad5e563bf4932c70e1245034c35467ef2efd4d64ebf819683467e2bf",
/// ))?;
/// let sig = Ed25519Signature::from_slice(&unhex(
///     "98a70222f0b8121aa9d30f813d683f809e462b469c7ff87639499bb94e6dae41\
///      31f85042463c2a355a2003d062adf5aaa10b8c61e636062aaad11c2a26083406",
/// ))?;
/// let mut v = Ed25519phVerifier::new(&key, b"")?;
/// v.update(b"a");
/// v.update(b"bc");
/// v.verify(&sig)?;
/// # Ok::<(), ferroly::sig::SigError>(())
/// ```
#[derive(Clone)]
pub struct Ed25519phVerifier {
    key: Ed25519PublicKey,
    context: Vec<u8>,
    prehash: Sha512,
}

impl Ed25519phVerifier {
    /// Starts an Ed25519ph verification under `key` with `context` (empty by
    /// default; at most [`MAX_CONTEXT_LEN`] bytes, else
    /// [`SigError::InvalidContext`]).
    pub fn new(key: &Ed25519PublicKey, context: &[u8]) -> Result<Self, SigError> {
        if context.len() > MAX_CONTEXT_LEN {
            return Err(SigError::InvalidContext);
        }
        Ok(Ed25519phVerifier {
            key: key.clone(),
            context: context.to_vec(),
            prehash: Sha512::new(),
        })
    }

    /// Absorbs the next chunk of the message.
    pub fn update(&mut self, chunk: &[u8]) {
        self.prehash.update(chunk);
    }

    /// Finishes: `Ok(())` only if `signature` matches everything absorbed.
    pub fn verify(self, signature: &Ed25519Signature) -> Result<(), SigError> {
        self.key
            .verify_prehashed(&self.prehash.finalize(), &self.context, signature)
    }
}

impl fmt::Debug for Ed25519phVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ed25519phVerifier")
            .field("key", &self.key)
            .field("context_len", &self.context.len())
            .finish_non_exhaustive()
    }
}
