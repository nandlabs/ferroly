#![no_main]
//! Ed25519 verification must never panic on arbitrary keys, signatures,
//! messages and contexts, the boolean and typed APIs must agree, and the
//! streaming verifier must match the one-shot one for any chunking.
//!
//! Input layout: 32 key bytes, 64 signature bytes, one context-length byte,
//! one chunk-size byte, then the context and the message. Short inputs are
//! zero-padded, so every input is exercised.

use ferroly::sig::{
    verify_ed25519, Ed25519PublicKey, Ed25519Signature, Ed25519Verifier, Ed25519phVerifier,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut head = [0u8; 98];
    let n = data.len().min(head.len());
    head[..n].copy_from_slice(&data[..n]);
    let rest = &data[n..];
    let (key, sig) = (&head[..32], &head[32..96]);
    let ctx_len = usize::from(head[96]).min(rest.len());
    let chunk = usize::from(head[97]).max(1);
    let (ctx, msg) = rest.split_at(ctx_len);

    let ok = verify_ed25519(key, msg, sig);
    let (Ok(k), Ok(s)) = (Ed25519PublicKey::from_slice(key), Ed25519Signature::from_slice(sig))
    else {
        assert!(!ok);
        return;
    };
    let one_shot = k.verify(msg, &s);
    assert_eq!(ok, one_shot.is_ok());

    let streamed = Ed25519Verifier::new(&k, &s).and_then(|mut v| {
        for part in msg.chunks(chunk) {
            v.update(part);
        }
        v.verify()
    });
    assert_eq!(streamed, one_shot);

    let _ = k.verify_ctx(msg, ctx, &s);
    let ph = k.verify_ph(msg, ctx, &s);
    if let Ok(mut v) = Ed25519phVerifier::new(&k, ctx) {
        for part in msg.chunks(chunk) {
            v.update(part);
        }
        assert_eq!(v.verify(&s), ph);
    }
});
