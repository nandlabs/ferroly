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

#[test]
fn keyed_and_hasher_state_is_wiped_on_drop() {
    // Every type that can hold key-derived state runs a wiping `Drop`.
    assert!(std::mem::needs_drop::<Sha256>());
    assert!(std::mem::needs_drop::<Sha384>());
    assert!(std::mem::needs_drop::<Sha512>());
    assert!(std::mem::needs_drop::<Sha1>());
    assert!(std::mem::needs_drop::<Md5>());
    assert!(std::mem::needs_drop::<HmacSha256>());
    assert!(std::mem::needs_drop::<HmacSha512>());

    // What the SHA-256 `Drop` runs: state, buffer and length all zero.
    let mut h = Sha256::new();
    h.update(b"key-derived input that stays in the buffer");
    assert!(h.buf_len > 0 && h.total > 0);
    h.zeroize();
    assert_eq!(h.h, [0; 8]);
    assert_eq!(h.buf, [0; 64]);
    assert_eq!((h.buf_len, h.total), (0, 0));

    let mut words = [0xdead_beefu32; 4];
    wipe(&mut words);
    assert_eq!(words, [0; 4]);
    let mut digest = sha256(b"x");
    wipe(digest.as_mut());
    assert_eq!(digest, Digest::from_bytes([0; 32]));
}

/// SEC-10: a hash implemented outside the crate needs only `AsRef<[u8]>`
/// output, as before 0.6, and still works with the generic HMAC and KDFs.
#[test]
fn an_external_hash_needs_no_writable_output() {
    #[derive(Clone)]
    struct Wrapped(Sha256);
    struct Out(Vec<u8>);
    impl AsRef<[u8]> for Out {
        fn as_ref(&self) -> &[u8] {
            &self.0
        }
    }
    impl HashAlgorithm for Wrapped {
        const BLOCK_LEN: usize = 64;
        const OUTPUT_LEN: usize = 32;
        type Output = Out;
        fn new() -> Self {
            Wrapped(Sha256::new())
        }
        fn update(&mut self, data: &[u8]) {
            self.0.update(data);
        }
        fn finalize(self) -> Out {
            Out(self.0.finalize().as_bytes().to_vec())
        }
    }
    let key = [0xaau8; 131];
    let message = b"Test Using Larger Than Block-Size Key - Hash Key First";
    assert_eq!(
        hmac::<Wrapped>(&key, message).as_ref(),
        hmac_sha256(&key, message).as_bytes()
    );
    let (mut a, mut b) = ([0u8; 42], [0u8; 42]);
    kdf::hkdf::<Wrapped>(b"salt", b"ikm", b"info", &mut a).unwrap();
    kdf::hkdf_sha256(b"salt", b"ikm", b"info", &mut b).unwrap();
    assert_eq!(a, b);
    // The crate's own hashes wipe their outputs.
    let mut digest = sha256(b"x");
    <Sha256 as HashAlgorithm>::wipe_output(&mut digest);
    assert_eq!(digest, Digest::from_bytes([0; 32]));
}

#[test]
fn wiping_intermediates_does_not_change_results() {
    // HMAC with an over-long key (hashed, then wiped) and the KDFs still
    // produce the published values (RFC 4231 case 6, RFC 5869 case 1).
    let key = [0xaau8; 131];
    assert_eq!(
        hmac_sha256(
            &key,
            b"Test Using Larger Than Block-Size Key - Hash Key First"
        )
        .to_hex(),
        "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
    );
    let mut okm = [0u8; 42];
    kdf::hkdf_sha256(
        &hex::decode("000102030405060708090a0b0c").unwrap(),
        &[0x0b; 22],
        &hex::decode("f0f1f2f3f4f5f6f7f8f9").unwrap(),
        &mut okm,
    )
    .unwrap();
    assert_eq!(
        hex::encode(&okm),
        "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
    );
    let mut dk = [0u8; 32];
    kdf::pbkdf2_hmac_sha256(b"password", b"salt", 2, &mut dk).unwrap();
    assert_eq!(
        hex::encode(&dk),
        "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
    );
}
