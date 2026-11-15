//! Fuzz-lite: seeded random and mutated keys, signatures, messages and
//! contexts must never panic, and must never verify by accident. The
//! coverage-guided counterpart is `fuzz/fuzz_targets/ed25519_verify.rs`.

#![cfg(feature = "sig")]

use ferroly::hash::hex;
use ferroly::sig::{
    verify_ed25519, Ed25519PublicKey, Ed25519Signature, Ed25519Verifier, Ed25519phVerifier,
};

/// xorshift64*: deterministic, so a failure reproduces from the seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn fill(&mut self, buf: &mut [u8]) {
        for b in buf {
            *b = self.next() as u8;
        }
    }
}

fn unhex<const N: usize>(s: &str) -> [u8; N] {
    hex::decode(s).unwrap().try_into().unwrap()
}

/// Exercises every entry point on one input; returns whether pure Ed25519
/// accepted it.
fn exercise(key: &[u8], msg: &[u8], sig: &[u8], ctx: &[u8]) -> bool {
    let ok = verify_ed25519(key, msg, sig);
    let typed = match (
        Ed25519PublicKey::from_slice(key),
        Ed25519Signature::from_slice(sig),
    ) {
        (Ok(k), Ok(s)) => {
            let _ = k.verify_ctx(msg, ctx, &s);
            let _ = k.verify_ph(msg, ctx, &s);
            if let Ok(mut v) = Ed25519phVerifier::new(&k, ctx) {
                v.update(msg);
                let _ = v.verify(&s);
            }
            let streamed = Ed25519Verifier::new(&k, &s).and_then(|mut v| {
                let cut = msg.len() / 2;
                v.update(&msg[..cut]);
                v.update(&msg[cut..]);
                v.verify()
            });
            let one_shot = k.verify(msg, &s);
            assert_eq!(streamed, one_shot);
            one_shot.is_ok()
        }
        _ => false,
    };
    assert_eq!(ok, typed);
    ok
}

#[test]
fn random_inputs_never_panic_or_verify() {
    let mut rng = Rng(0x5eed_0001);
    for _ in 0..300 {
        let mut key = vec![0u8; if rng.below(8) == 0 { rng.below(40) } else { 32 }];
        let mut sig = vec![0u8; if rng.below(8) == 0 { rng.below(80) } else { 64 }];
        let mut msg = vec![0u8; rng.below(64)];
        let mut ctx = vec![0u8; rng.below(300)];
        rng.fill(&mut key);
        rng.fill(&mut sig);
        rng.fill(&mut msg);
        rng.fill(&mut ctx);
        // Bias S below L sometimes, so the curve path (not just the S check)
        // runs on random R.
        if sig.len() == 64 && rng.below(2) == 0 {
            sig[63] &= 0x0f;
        }
        assert!(!exercise(&key, &msg, &sig, &ctx));
    }
}

#[test]
fn mutated_valid_signatures_never_verify() {
    // RFC 8032 §7.1 TEST 1 (empty message).
    let key: [u8; 32] = unhex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
    let sig: [u8; 64] = unhex(
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155\
         5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
    );
    assert!(exercise(&key, b"", &sig, b"c"));

    let mut rng = Rng(0x5eed_0002);
    for _ in 0..200 {
        let (mut k, mut s) = (key, sig);
        // 1-3 random byte mutations across key and signature.
        for _ in 0..1 + rng.below(3) {
            let pos = rng.below(96);
            let delta = 1 + rng.below(255) as u8;
            if pos < 32 {
                k[pos] ^= delta;
            } else {
                s[pos - 32] ^= delta;
            }
        }
        if k == key && s == sig {
            continue; // the mutations cancelled out
        }
        assert!(!exercise(&k, b"", &s, b""));
        // The untouched pair over a random non-empty message fails too.
        let mut msg = vec![0u8; 1 + rng.below(32)];
        rng.fill(&mut msg);
        assert!(!exercise(&key, &msg, &sig, b""));
    }
}
