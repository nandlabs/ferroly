//! Negative cases: every tampering, malformed encoding and edge case must fail
//! closed, with the documented coarse error class.

#![cfg(feature = "sig")]

use ferroly::hash::hex;
use ferroly::sig::{
    verify_ed25519, Ed25519PublicKey, Ed25519Signature, Ed25519Verifier, Ed25519phVerifier,
    SigError, MAX_CONTEXT_LEN,
};

fn unhex(s: &str) -> Vec<u8> {
    hex::decode(s).unwrap()
}

fn arr<const N: usize>(s: &str) -> [u8; N] {
    unhex(s).try_into().unwrap()
}

// RFC 8032 §7.1 TEST 3.
const PK: &str = "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025";
const MSG: &[u8] = &[0xaf, 0x82];
const SIG: &str = "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac\
                   18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a";
// RFC 8032 §7.1 TEST 2 (a different key).
const OTHER_PK: &str = "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c";

/// The group order L, little-endian.
const L: &str = "edd3f55c1a631258d69cf7a2def9de1400000000000000000000000000000010";

/// Every point of order dividing 8, canonically encoded.
const SMALL_ORDER: [&str; 8] = [
    "0100000000000000000000000000000000000000000000000000000000000000",
    "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
    "0000000000000000000000000000000000000000000000000000000000000000",
    "0000000000000000000000000000000000000000000000000000000000000080",
    "26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc05",
    "26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc85",
    "c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a",
    "c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac03fa",
];

fn key() -> Ed25519PublicKey {
    Ed25519PublicKey::from_bytes(&arr(PK)).unwrap()
}

fn sig() -> [u8; 64] {
    arr(SIG)
}

/// Adds two 256-bit little-endian integers (wrapping at 2^256).
fn add_le(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut carry = 0u16;
    for i in 0..32 {
        let v = u16::from(a[i]) + u16::from(b[i]) + carry;
        out[i] = v as u8;
        carry = v >> 8;
    }
    out
}

/// Non-canonical field encodings: y = p + k for k in 0..19 (y >= p).
fn non_canonical_y() -> Vec<[u8; 32]> {
    (0u8..19)
        .flat_map(|k| {
            let mut b = [0xffu8; 32];
            b[0] = 0xed + k;
            b[31] = 0x7f;
            let mut signed = b;
            signed[31] |= 0x80;
            [b, signed]
        })
        .collect()
}

#[test]
fn baseline_is_valid() {
    assert_eq!(
        key().verify(MSG, &Ed25519Signature::from_bytes(&sig())),
        Ok(())
    );
}

#[test]
fn tampered_message_fails() {
    let k = key();
    let s = Ed25519Signature::from_bytes(&sig());
    for i in 0..MSG.len() {
        for bit in 0..8 {
            let mut m = MSG.to_vec();
            m[i] ^= 1 << bit;
            assert_eq!(k.verify(&m, &s), Err(SigError::VerificationFailed));
        }
    }
    assert_eq!(k.verify(&MSG[..1], &s), Err(SigError::VerificationFailed));
    assert_eq!(k.verify(b"", &s), Err(SigError::VerificationFailed));
    assert_eq!(
        k.verify(&[MSG, b"\0"].concat(), &s),
        Err(SigError::VerificationFailed)
    );
}

#[test]
fn every_signature_bit_flip_fails() {
    let k = key();
    for i in 0..64 {
        for bit in 0..8 {
            let mut s = sig();
            s[i] ^= 1 << bit;
            let result = k.verify(MSG, &Ed25519Signature::from_bytes(&s));
            assert!(result.is_err(), "byte {i} bit {bit}");
            assert!(!verify_ed25519(&arr::<32>(PK), MSG, &s));
            // Streaming agrees (a malformed signature may be refused up front).
            let streamed =
                Ed25519Verifier::new(&k, &Ed25519Signature::from_bytes(&s)).and_then(|mut v| {
                    v.update(MSG);
                    v.verify()
                });
            assert_eq!(streamed, result, "byte {i} bit {bit}");
        }
    }
}

#[test]
fn every_key_bit_flip_fails() {
    let s = Ed25519Signature::from_bytes(&sig());
    for i in 0..32 {
        for bit in 0..8 {
            let mut pk: [u8; 32] = arr(PK);
            pk[i] ^= 1 << bit;
            match Ed25519PublicKey::from_bytes(&pk) {
                Ok(k) => assert_eq!(k.verify(MSG, &s), Err(SigError::VerificationFailed)),
                Err(e) => assert_eq!(e, SigError::MalformedKey),
            }
            assert!(!verify_ed25519(&pk, MSG, &sig()));
        }
    }
}

#[test]
fn wrong_key_fails() {
    let other = Ed25519PublicKey::from_bytes(&arr(OTHER_PK)).unwrap();
    assert_eq!(
        other.verify(MSG, &Ed25519Signature::from_bytes(&sig())),
        Err(SigError::VerificationFailed)
    );
    assert_ne!(other, key());
}

#[test]
fn s_equal_to_or_above_l_is_rejected() {
    let k = key();
    let l: [u8; 32] = arr(L);
    let mut s_valid = [0u8; 32];
    s_valid.copy_from_slice(&sig()[32..]);

    // S = L, S = L + 1, S = 2^256 - 1, and the malleated S + L (which the
    // cofactorless equation alone would accept: [S + L]B = [S]B).
    let mut l_plus_1 = l;
    l_plus_1[0] += 1;
    let candidates = [l, l_plus_1, [0xff; 32], add_le(&s_valid, &l)];
    for s in candidates {
        let mut bad = sig();
        bad[32..].copy_from_slice(&s);
        let signature = Ed25519Signature::from_bytes(&bad);
        assert_eq!(k.verify(MSG, &signature), Err(SigError::MalformedSignature));
        assert_eq!(
            Ed25519Verifier::new(&k, &signature).unwrap_err(),
            SigError::MalformedSignature
        );
        assert!(!verify_ed25519(&arr::<32>(PK), MSG, &bad));
    }
    // L - 1 is canonical (it is simply the wrong S here).
    let mut l_minus_1 = l;
    l_minus_1[0] -= 1;
    let mut bad = sig();
    bad[32..].copy_from_slice(&l_minus_1);
    assert_eq!(
        k.verify(MSG, &Ed25519Signature::from_bytes(&bad)),
        Err(SigError::VerificationFailed)
    );
}

#[test]
fn non_canonical_r_is_rejected() {
    let k = key();
    for r in non_canonical_y() {
        let mut bad = sig();
        bad[..32].copy_from_slice(&r);
        assert_eq!(
            k.verify(MSG, &Ed25519Signature::from_bytes(&bad)),
            Err(SigError::MalformedSignature)
        );
    }
    // "Negative zero" x: y = 1 and y = -1 with the sign bit set.
    for h in [
        "0100000000000000000000000000000000000000000000000000000000000080",
        "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
    ] {
        let mut bad = sig();
        bad[..32].copy_from_slice(&arr::<32>(h));
        assert_eq!(
            k.verify(MSG, &Ed25519Signature::from_bytes(&bad)),
            Err(SigError::MalformedSignature)
        );
    }
}

#[test]
fn r_not_on_curve_is_rejected() {
    let mut bad = sig();
    bad[..32].copy_from_slice(&[0u8; 32]);
    bad[0] = 2; // y = 2 has no x on the curve.
    assert_eq!(
        key().verify(MSG, &Ed25519Signature::from_bytes(&bad)),
        Err(SigError::MalformedSignature)
    );
}

#[test]
fn small_order_r_is_rejected() {
    for h in SMALL_ORDER {
        let mut bad = sig();
        bad[..32].copy_from_slice(&arr::<32>(h));
        // Including the classic forgery attempt S = 0.
        for s in [[0u8; 32], arr::<32>(&SIG[64..])] {
            bad[32..].copy_from_slice(&s);
            assert_eq!(
                key().verify(MSG, &Ed25519Signature::from_bytes(&bad)),
                Err(SigError::MalformedSignature),
                "{h}"
            );
        }
    }
}

#[test]
fn non_canonical_keys_are_rejected() {
    for a in non_canonical_y() {
        assert_eq!(
            Ed25519PublicKey::from_bytes(&a).unwrap_err(),
            SigError::MalformedKey
        );
    }
    for h in [
        "0100000000000000000000000000000000000000000000000000000000000080",
        "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
    ] {
        assert_eq!(
            Ed25519PublicKey::from_bytes(&arr(h)).unwrap_err(),
            SigError::MalformedKey
        );
    }
    // Not on the curve.
    let mut two = [0u8; 32];
    two[0] = 2;
    assert_eq!(
        Ed25519PublicKey::from_bytes(&two).unwrap_err(),
        SigError::MalformedKey
    );
}

#[test]
fn small_order_keys_are_rejected() {
    for h in SMALL_ORDER {
        let a: [u8; 32] = arr(h);
        assert_eq!(
            Ed25519PublicKey::from_bytes(&a).unwrap_err(),
            SigError::MalformedKey,
            "{h}"
        );
        // A small-order key with R = identity, S = 0 would "verify" every
        // message under a naive implementation.
        let mut forged = [0u8; 64];
        forged[..32].copy_from_slice(&arr::<32>(SMALL_ORDER[0]));
        assert!(!verify_ed25519(&a, b"anything", &forged));
        assert!(!verify_ed25519(&a, b"", &forged));
    }
}

/// A key `A + T` (T of order 8) signed with the torsion-free secret, built
/// independently in Python: the cofactored equation accepts both signatures,
/// the cofactorless one used here accepts only the one where `[k]T` vanishes.
#[test]
fn mixed_order_key_uses_cofactorless_equation() {
    let a_mixed = "3b5b475c4b82dd1572799fc546f4c6c03e478c6654aa4c7f945b347ea32af60d";
    let k = Ed25519PublicKey::from_bytes(&arr(a_mixed)).expect("mixed-order keys decode");
    let k_not_div_8 = "7767a3242dc9b58bbc68488c1265cc6cc5f26c6f3c88a5d55e901b0734dca572\
                       b5ec04e6ccbc0c9627a74eb2b0084b8d2f3a93819003b10ad8fd3a431facb407";
    let k_div_8 = "7767a3242dc9b58bbc68488c1265cc6cc5f26c6f3c88a5d55e901b0734dca572\
                   e54f9a582ca7d36e523e10bfa0e63863e7dee771926be6cc106fa1d0ec6feb05";
    assert_eq!(
        k.verify(
            b"mixed-order 0",
            &Ed25519Signature::from_bytes(&arr(k_not_div_8))
        ),
        Err(SigError::VerificationFailed)
    );
    assert_eq!(
        k.verify(
            b"mixed-order 5",
            &Ed25519Signature::from_bytes(&arr(k_div_8))
        ),
        Ok(())
    );
}

/// A mixed-order R (`rB + T`) for the RFC TEST 1 key, built independently in
/// Python; the cofactored equation accepts it, this module does not.
#[test]
fn mixed_order_r_is_rejected() {
    let k = Ed25519PublicKey::from_bytes(&arr(
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
    ))
    .unwrap();
    let s = "b15bf26424d1b69dd33b7a9e57db5de4a2aadea4a2a051efd9bfd6af74165d2b\
             1119cf5ddddfad3785fced892ae1b60f46156c3f83df4e51fc64fbab40b2f004";
    assert_eq!(
        k.verify(b"mixed-order R", &Ed25519Signature::from_bytes(&arr(s))),
        Err(SigError::VerificationFailed)
    );
}

#[test]
fn wrong_lengths_are_rejected() {
    let pk = unhex(PK);
    let s = sig();
    for len in [0, 1, 31, 33, 64] {
        let mut key = pk.clone();
        key.resize(len, 0);
        assert_eq!(
            Ed25519PublicKey::from_slice(&key).unwrap_err(),
            SigError::MalformedKey
        );
        assert!(!verify_ed25519(&key, MSG, &s));
    }
    for len in [0, 1, 32, 63, 65, 128] {
        let mut signature = s.to_vec();
        signature.resize(len, 0);
        assert_eq!(
            Ed25519Signature::from_slice(&signature).unwrap_err(),
            SigError::MalformedSignature
        );
        assert!(!verify_ed25519(&pk, MSG, &signature));
    }
    // Trailing bytes are not ignored.
    let mut long_sig = s.to_vec();
    long_sig.push(0);
    assert!(!verify_ed25519(&pk, MSG, &long_sig));
    assert!(Ed25519PublicKey::try_from(&pk[..]).is_ok());
    assert!(Ed25519Signature::try_from(&s[..]).is_ok());
}

#[test]
fn context_limits() {
    let k = key();
    let s = Ed25519Signature::from_bytes(&sig());
    let max = vec![b'c'; MAX_CONTEXT_LEN];
    let over = vec![b'c'; MAX_CONTEXT_LEN + 1];
    // Ed25519ctx needs 1..=255 bytes of context.
    assert_eq!(k.verify_ctx(MSG, b"", &s), Err(SigError::InvalidContext));
    assert_eq!(k.verify_ctx(MSG, &over, &s), Err(SigError::InvalidContext));
    assert_eq!(
        k.verify_ctx(MSG, &max, &s),
        Err(SigError::VerificationFailed)
    );
    // Ed25519ph allows an empty context, up to 255 bytes.
    assert_eq!(k.verify_ph(MSG, &over, &s), Err(SigError::InvalidContext));
    assert_eq!(
        k.verify_ph(MSG, &max, &s),
        Err(SigError::VerificationFailed)
    );
    assert_eq!(
        Ed25519phVerifier::new(&k, &over).unwrap_err(),
        SigError::InvalidContext
    );
    assert!(Ed25519phVerifier::new(&k, &max).is_ok());
}

#[test]
fn errors_are_coarse_and_displayable() {
    for e in [
        SigError::MalformedKey,
        SigError::MalformedSignature,
        SigError::InvalidContext,
        SigError::VerificationFailed,
    ] {
        let text = e.to_string();
        assert!(text.contains("Ed25519"), "{text}");
        let _: &dyn std::error::Error = &e;
    }
    // Debug output of keys and signatures is hex, never panics.
    assert!(format!("{:?}", key()).contains(PK));
    assert!(format!("{:?}", Ed25519Signature::from_bytes(&sig())).starts_with("Ed25519Signature("));
}
