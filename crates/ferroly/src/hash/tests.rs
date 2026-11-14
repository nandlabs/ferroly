//! Unit tests for the module root. The exhaustive known-answer vectors live in
//! `tests/hash_vectors.rs`.

use super::*;

#[test]
fn sha256_known_vectors() {
    assert_eq!(
        sha256(b"").to_hex(),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256(b"abc").to_hex(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn sha256_streaming_matches_one_shot() {
    // A message spanning several 64-byte blocks, fed in awkward chunks.
    let data: Vec<u8> = (0u8..=255).cycle().take(1000).collect();
    let mut h = Sha256::new();
    for chunk in data.chunks(7) {
        h.update(chunk);
    }
    assert_eq!(h.finalize(), sha256(&data));
}

#[test]
fn hmac_sha256_rfc4231_case2() {
    let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
    assert_eq!(
        mac.to_hex(),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
}

#[test]
fn hmac_streaming_matches_one_shot() {
    let key = b"a-very-long-key-that-exceeds-the-sha256-block-size-of-64-bytes!!!";
    assert!(key.len() > 64);
    let mut h = HmacSha256::new(key);
    h.update(b"hello ");
    h.update(b"world");
    assert_eq!(h.finalize(), hmac_sha256(key, b"hello world"));
}

#[test]
fn sha1_known_vectors() {
    assert_eq!(
        sha1(b"").to_hex(),
        "da39a3ee5e6b4b0d3255bfef95601890afd80709"
    );
    assert_eq!(
        sha1(b"abc").to_hex(),
        "a9993e364706816aba3e25717850c26c9cd0d89d"
    );
}

#[test]
fn sha512_and_sha384_abc() {
    assert_eq!(
        sha512(b"abc").to_hex(),
        "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
         2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
    );
    assert_eq!(
        sha384(b"abc").to_hex(),
        "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed\
         8086072ba1e7cc2358baeca134c825a7"
    );
}

#[test]
fn digest_ct_eq_and_display() {
    let a = sha256(b"x");
    let b = sha256(b"x");
    let c = sha256(b"y");
    assert!(a.ct_eq(&b));
    assert!(!a.ct_eq(&c));
    assert_eq!(format!("{a}"), a.to_hex());
    assert_eq!(format!("{a:?}"), format!("Digest({})", a.to_hex()));
}

#[test]
fn trait_constants_match_the_digest_sizes() {
    fn check<H: HashAlgorithm>() {
        let mut h = H::new();
        h.update(b"abc");
        assert_eq!(h.finalize().as_ref().len(), H::OUTPUT_LEN);
        assert!(H::OUTPUT_LEN <= H::BLOCK_LEN);
    }
    check::<Sha1>();
    check::<Sha256>();
    check::<Sha384>();
    check::<Sha512>();
    check::<Md5>();
    assert_eq!(<Sha512 as HashAlgorithm>::BLOCK_LEN, 128);
    assert_eq!(<Sha256 as HashAlgorithm>::BLOCK_LEN, 64);
}
