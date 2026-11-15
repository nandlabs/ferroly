#![no_main]
//! Structured Ed25519 fuzzing that reaches the accept path.
//!
//! Random bytes almost never decode to a valid `R` with a canonical `S`, so
//! `ed25519_verify` rarely gets past the structural checks. This target starts
//! from valid (key, message, signature) triples (RFC 8032 §7.1) and lets the
//! fuzzer edit them: the oracle is that the edited triple verifies **if and
//! only if** it is byte-for-byte the original (Ed25519 with strict decoding is
//! strongly unforgeable, so no edit of an honest triple may verify). The
//! boolean, typed and streaming APIs must also agree.
//!
//! Input layout: one byte picks the triple, then any number of 3-byte edits
//! `(offset_hi, offset_lo, xor)` applied to `key || signature || message`,
//! with an optional trailing byte appended to the message when the input
//! length is not a multiple of 3.

use ferroly::sig::{verify_ed25519, Ed25519PublicKey, Ed25519Signature, Ed25519Verifier};
use libfuzzer_sys::fuzz_target;

/// RFC 8032 §7.1 TEST 1, 2 and 3: (public key, message, signature), hex.
const VECTORS: [(&str, &str, &str); 3] = [
    (
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
        "",
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155\
         5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
    ),
    (
        "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
        "72",
        "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da\
         085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
    ),
    (
        "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
        "af82",
        "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac\
         18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
    ),
];

fn unhex(s: &str) -> Vec<u8> {
    ferroly::hash::hex::decode(s).unwrap()
}

fuzz_target!(|data: &[u8]| {
    let Some((&pick, edits)) = data.split_first() else {
        return;
    };
    let (key, msg, sig) = VECTORS[usize::from(pick) % VECTORS.len()];
    let mut buf = unhex(key);
    buf.extend(unhex(sig));
    buf.extend(unhex(msg));
    let original = buf.clone();

    let mut chunks = edits.chunks_exact(3);
    for edit in &mut chunks {
        let offset = (usize::from(edit[0]) << 8 | usize::from(edit[1])) % buf.len();
        buf[offset] ^= edit[2];
    }
    if let [extra] = chunks.remainder() {
        buf.push(*extra);
    }

    let (key, rest) = buf.split_at(32);
    let (sig, msg) = rest.split_at(64);
    let ok = verify_ed25519(key, msg, sig);
    assert_eq!(ok, buf == original, "an edited triple verified (or the original did not)");

    if let (Ok(k), Ok(s)) = (Ed25519PublicKey::from_slice(key), Ed25519Signature::from_slice(sig)) {
        let one_shot = k.verify(msg, &s);
        assert_eq!(one_shot.is_ok(), ok);
        let streamed = Ed25519Verifier::new(&k, &s).and_then(|mut v| {
            for part in msg.chunks(7) {
                v.update(part);
            }
            v.verify()
        });
        assert_eq!(streamed, one_shot);
    }
});
