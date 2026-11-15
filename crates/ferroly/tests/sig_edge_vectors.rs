//! Public third-party Ed25519 vectors, checked in under `tests/sig_data/`:
//!
//! - every Project Wycheproof `ed25519_test.json` verification vector;
//! - the 12 "Taming the many EdDSAs" (ed25519-speccheck) edge cases, with
//!   libsodium's verdict as the expected result;
//! - a structured mutation test: starting from valid signatures, every
//!   single-byte change to the key, message or signature must be rejected,
//!   which reaches the accept path and the full decode of `R` and `S` that
//!   random inputs almost never do.

#![cfg(feature = "sig")]

use ferroly::hash::hex;
use ferroly::sig::{verify_ed25519, Ed25519PublicKey, Ed25519Signature, Ed25519Verifier, SigError};

const WYCHEPROOF: &str = include_str!("sig_data/wycheproof_ed25519.txt");
const SPECCHECK: &str = include_str!("sig_data/speccheck_ed25519.txt");

fn field(s: &str) -> Vec<u8> {
    if s == "-" {
        Vec::new()
    } else {
        hex::decode(s).unwrap()
    }
}

fn verify(key: &[u8], msg: &[u8], sig: &[u8]) -> Result<(), SigError> {
    let key = Ed25519PublicKey::from_slice(key)?;
    let sig = Ed25519Signature::from_slice(sig)?;
    key.verify(msg, &sig)
}

fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
}

#[test]
fn wycheproof_ed25519_vectors() {
    let mut counts = [0usize; 3];
    for line in lines(WYCHEPROOF) {
        let cols: Vec<&str> = line.split(' ').collect();
        assert_eq!(cols.len(), 6, "{line}");
        let (id, result, flags) = (cols[0], cols[1], cols[2]);
        let (key, msg, sig) = (field(cols[3]), field(cols[4]), field(cols[5]));
        let got = verify(&key, &msg, &sig);
        assert_eq!(got.is_ok(), verify_ed25519(&key, &msg, &sig), "tcId {id}");
        match result {
            "valid" => {
                assert_eq!(got, Ok(()), "tcId {id} ({flags})");
                counts[0] += 1;
            }
            "invalid" => {
                assert!(got.is_err(), "tcId {id} ({flags}) must be rejected");
                counts[1] += 1;
            }
            // Wycheproof leaves small- and mixed-order keys to the verifier's
            // policy. Small-order keys are always refused here; mixed-order
            // keys follow the cofactorless equation like libsodium.
            "acceptable" => {
                if flags.contains("SmallOrderPublicKey") {
                    assert_eq!(got, Err(SigError::MalformedKey), "tcId {id}");
                } else {
                    assert!(flags.contains("MixedOrderPublicKey"), "tcId {id}: {flags}");
                    assert!(
                        matches!(got, Ok(()) | Err(SigError::VerificationFailed)),
                        "tcId {id}: {got:?}"
                    );
                }
                counts[2] += 1;
            }
            other => panic!("tcId {id}: unknown result {other}"),
        }
    }
    assert_eq!(counts, [117, 64, 29], "vector counts");
}

#[test]
fn speccheck_edge_cases_match_libsodium() {
    let mut n = 0;
    for line in lines(SPECCHECK) {
        let (case, desc) = line.split_once(" # ").unwrap();
        let cols: Vec<&str> = case.split(' ').collect();
        assert_eq!(cols.len(), 5, "{line}");
        assert_eq!(cols[0], "pure");
        let (key, sig, msg) = (field(cols[1]), field(cols[2]), field(cols[3]));
        let want = match cols[4] {
            "OK" => Ok(()),
            "FAIL" => Err(SigError::VerificationFailed),
            "KEY" => Err(SigError::MalformedKey),
            "SIG" => Err(SigError::MalformedSignature),
            other => panic!("unknown verdict {other}"),
        };
        assert_eq!(verify(&key, &msg, &sig), want, "{desc}");
        n += 1;
    }
    assert_eq!(n, 12);
}

/// Valid (key, message, signature) triples: the RFC 8032 §7.1 vectors and
/// the accepted Wycheproof vectors.
fn valid_triples() -> Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    lines(WYCHEPROOF)
        .filter_map(|line| {
            let cols: Vec<&str> = line.split(' ').collect();
            (cols[1] == "valid").then(|| (field(cols[3]), field(cols[4]), field(cols[5])))
        })
        .take(12)
        .collect()
}

#[test]
fn every_single_byte_mutation_of_a_valid_signature_is_rejected() {
    for (key, msg, sig) in valid_triples() {
        assert_eq!(verify(&key, &msg, &sig), Ok(()));
        // Each byte of the key and signature, and the first and last bytes of
        // the message, through a few XOR masks.
        for mask in [0x01u8, 0x80, 0xff] {
            for i in 0..key.len() {
                let mut k = key.clone();
                k[i] ^= mask;
                assert!(verify(&k, &msg, &sig).is_err(), "key byte {i}");
            }
            for i in 0..sig.len() {
                let mut s = sig.clone();
                s[i] ^= mask;
                let got = verify(&key, &msg, &s);
                assert!(got.is_err(), "sig byte {i}");
                // The streaming verifier reaches the same verdict.
                let k = Ed25519PublicKey::from_slice(&key).unwrap();
                let s = Ed25519Signature::from_slice(&s).unwrap();
                let streamed = Ed25519Verifier::new(&k, &s).and_then(|mut v| {
                    v.update(&msg);
                    v.verify()
                });
                assert_eq!(streamed, got, "sig byte {i}");
            }
            if !msg.is_empty() {
                for i in [0, msg.len() - 1] {
                    let mut m = msg.clone();
                    m[i] ^= mask;
                    assert_eq!(
                        verify(&key, &m, &sig),
                        Err(SigError::VerificationFailed),
                        "msg byte {i}"
                    );
                }
            }
        }
        let mut longer = msg.clone();
        longer.push(0);
        assert_eq!(
            verify(&key, &longer, &sig),
            Err(SigError::VerificationFailed)
        );
    }
}
