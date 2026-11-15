//! RFC 8032 test vectors for Ed25519 (§7.1), Ed25519ctx (§7.2) and
//! Ed25519ph (§7.3), through every public entry point.

#![cfg(feature = "sig")]

use ferroly::hash::{hex, Sha512};
use ferroly::sig::{
    verify_ed25519, Ed25519PublicKey, Ed25519Signature, Ed25519Verifier, Ed25519phVerifier,
    SigError,
};

struct Vector {
    name: &'static str,
    public_key: &'static str,
    message: &'static str,
    signature: &'static str,
}

struct CtxVector {
    name: &'static str,
    public_key: &'static str,
    message: &'static str,
    context: &'static str,
    signature: &'static str,
}

/// RFC 8032 §7.1 (TEST 1, 2, 3, 1024, SHA(abc)).
const ED25519: &[Vector] = &[
    Vector {
        name: "TEST 1",
        public_key: "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
        message: "",
        signature: "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155\
            5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
    },
    Vector {
        name: "TEST 2",
        public_key: "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
        message: "72",
        signature: "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da\
            085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
    },
    Vector {
        name: "TEST 3",
        public_key: "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
        message: "af82",
        signature: "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac\
            18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
    },
    Vector {
        name: "TEST 1024",
        public_key: "278117fc144c72340f67d0f2316e8386ceffbf2b2428c9c51fef7c597f1d426e",
        message: "08b8b2b733424243760fe426a4b54908632110a66c2f6591eabd3345e3e4eb98\
            fa6e264bf09efe12ee50f8f54e9f77b1e355f6c50544e23fb1433ddf73be84d8\
            79de7c0046dc4996d9e773f4bc9efe5738829adb26c81b37c93a1b270b20329d\
            658675fc6ea534e0810a4432826bf58c941efb65d57a338bbd2e26640f89ffbc\
            1a858efcb8550ee3a5e1998bd177e93a7363c344fe6b199ee5d02e82d522c4fe\
            ba15452f80288a821a579116ec6dad2b3b310da903401aa62100ab5d1a36553e\
            06203b33890cc9b832f79ef80560ccb9a39ce767967ed628c6ad573cb116dbef\
            efd75499da96bd68a8a97b928a8bbc103b6621fcde2beca1231d206be6cd9ec7\
            aff6f6c94fcd7204ed3455c68c83f4a41da4af2b74ef5c53f1d8ac70bdcb7ed1\
            85ce81bd84359d44254d95629e9855a94a7c1958d1f8ada5d0532ed8a5aa3fb2\
            d17ba70eb6248e594e1a2297acbbb39d502f1a8c6eb6f1ce22b3de1a1f40cc24\
            554119a831a9aad6079cad88425de6bde1a9187ebb6092cf67bf2b13fd65f270\
            88d78b7e883c8759d2c4f5c65adb7553878ad575f9fad878e80a0c9ba63bcbcc\
            2732e69485bbc9c90bfbd62481d9089beccf80cfe2df16a2cf65bd92dd597b07\
            07e0917af48bbb75fed413d238f5555a7a569d80c3414a8d0859dc65a46128ba\
            b27af87a71314f318c782b23ebfe808b82b0ce26401d2e22f04d83d1255dc51a\
            ddd3b75a2b1ae0784504df543af8969be3ea7082ff7fc9888c144da2af58429e\
            c96031dbcad3dad9af0dcbaaaf268cb8fcffead94f3c7ca495e056a9b47acdb7\
            51fb73e666c6c655ade8297297d07ad1ba5e43f1bca32301651339e22904cc8c\
            42f58c30c04aafdb038dda0847dd988dcda6f3bfd15c4b4c4525004aa06eeff8\
            ca61783aacec57fb3d1f92b0fe2fd1a85f6724517b65e614ad6808d6f6ee34df\
            f7310fdc82aebfd904b01e1dc54b2927094b2db68d6f903b68401adebf5a7e08\
            d78ff4ef5d63653a65040cf9bfd4aca7984a74d37145986780fc0b16ac451649\
            de6188a7dbdf191f64b5fc5e2ab47b57f7f7276cd419c17a3ca8e1b939ae49e4\
            88acba6b965610b5480109c8b17b80e1b7b750dfc7598d5d5011fd2dcc5600a3\
            2ef5b52a1ecc820e308aa342721aac0943bf6686b64b2579376504ccc493d97e\
            6aed3fb0f9cd71a43dd497f01f17c0e2cb3797aa2a2f256656168e6c496afc5f\
            b93246f6b1116398a346f1a641f3b041e989f7914f90cc2c7fff357876e506b5\
            0d334ba77c225bc307ba537152f3f1610e4eafe595f6d9d90d11faa933a15ef1\
            369546868a7f3a45a96768d40fd9d03412c091c6315cf4fde7cb68606937380d\
            b2eaaa707b4c4185c32eddcdd306705e4dc1ffc872eeee475a64dfac86aba41c\
            0618983f8741c5ef68d3a101e8a3b8cac60c905c15fc910840b94c00a0b9d0",
        signature: "0aab4c900501b3e24d7cdf4663326a3a87df5e4843b2cbdb67cbf6e460fec350\
            aa5371b1508f9f4528ecea23c436d94b5e8fcd4f681e30a6ac00a9704a188a03",
    },
    Vector {
        name: "TEST SHA(abc)",
        public_key: "ec172b93ad5e563bf4932c70e1245034c35467ef2efd4d64ebf819683467e2bf",
        message: "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
            2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
        signature: "dc2a4459e7369633a52b1bf277839a00201009a3efbf3ecb69bea2186c26b589\
            09351fc9ac90b3ecfdfbc7c66431e0303dca179c138ac17ad9bef1177331a704",
    },
];

/// RFC 8032 §7.2.
const ED25519CTX: &[CtxVector] = &[
    CtxVector {
        name: "foo",
        public_key: "dfc9425e4f968f7f0c29f0259cf5f9aed6851c2bb4ad8bfb860cfee0ab248292",
        message: "f726936d19c800494e3fdaff20b276a8",
        context: "666f6f",
        signature: "55a4cc2f70a54e04288c5f4cd1e45a7bb520b36292911876cada7323198dd87a\
            8b36950b95130022907a7fb7c4e9b2d5f6cca685a587b4b21f4b888e4e7edb0d",
    },
    CtxVector {
        name: "bar",
        public_key: "dfc9425e4f968f7f0c29f0259cf5f9aed6851c2bb4ad8bfb860cfee0ab248292",
        message: "f726936d19c800494e3fdaff20b276a8",
        context: "626172",
        signature: "fc60d5872fc46b3aa69f8b5b4351d5808f92bcc044606db097abab6dbcb1aee3\
            216c48e8b3b66431b5b186d1d28f8ee15a5ca2df6668346291c2043d4eb3e90d",
    },
    CtxVector {
        name: "foo2",
        public_key: "dfc9425e4f968f7f0c29f0259cf5f9aed6851c2bb4ad8bfb860cfee0ab248292",
        message: "508e9e6882b979fea900f62adceaca35",
        context: "666f6f",
        signature: "8b70c1cc8310e1de20ac53ce28ae6e7207f33c3295e03bb5c0732a1d20dc6490\
            8922a8b052cf99b7c4fe107a5abb5b2c4085ae75890d02df26269d8945f84b0b",
    },
    CtxVector {
        name: "foo3",
        public_key: "0f1d1274943b91415889152e893d80e93275a1fc0b65fd71b4b0dda10ad7d772",
        message: "f726936d19c800494e3fdaff20b276a8",
        context: "666f6f",
        signature: "21655b5f1aa965996b3f97b3c849eafba922a0a62992f73b3d1b73106a84ad85\
            e9b86a7b6005ea868337ff2d20a7f5fbd4cd10b0be49a68da2b2e0dc0ad8960f",
    },
];

/// RFC 8032 §7.3, TEST abc.
const ED25519PH: Vector = Vector {
    name: "TEST abc",
    public_key: "ec172b93ad5e563bf4932c70e1245034c35467ef2efd4d64ebf819683467e2bf",
    message: "616263",
    signature: "98a70222f0b8121aa9d30f813d683f809e462b469c7ff87639499bb94e6dae41\
        31f85042463c2a355a2003d062adf5aaa10b8c61e636062aaad11c2a26083406",
};

fn unhex(s: &str) -> Vec<u8> {
    hex::decode(s).unwrap()
}

fn parts(public_key: &str, signature: &str) -> (Ed25519PublicKey, Ed25519Signature) {
    (
        Ed25519PublicKey::from_slice(&unhex(public_key)).unwrap(),
        Ed25519Signature::from_slice(&unhex(signature)).unwrap(),
    )
}

#[test]
fn ed25519_vectors_verify() {
    for v in ED25519 {
        let (key, sig) = parts(v.public_key, v.signature);
        let msg = unhex(v.message);
        assert_eq!(key.verify(&msg, &sig), Ok(()), "{}", v.name);
        assert!(
            verify_ed25519(&unhex(v.public_key), &msg, &unhex(v.signature)),
            "{}",
            v.name
        );
        // Round trips of the encodings.
        assert_eq!(hex::encode(key.as_bytes()), v.public_key, "{}", v.name);
        assert_eq!(key.to_bytes(), *key.as_bytes());
        assert_eq!(sig.to_bytes().to_vec(), unhex(v.signature));
    }
}

#[test]
fn ed25519_vector_count() {
    assert_eq!(ED25519.len(), 5);
    assert_eq!(ED25519CTX.len(), 4);
    let long = ED25519.iter().find(|v| v.name == "TEST 1024").unwrap();
    assert_eq!(unhex(long.message).len(), 1023);
}

#[test]
fn ed25519_streaming_matches_one_shot_for_every_split() {
    for v in ED25519 {
        let (key, sig) = parts(v.public_key, v.signature);
        let msg = unhex(v.message);
        // Every two-way split for short messages; a sample for the long one.
        let step = if msg.len() > 128 { 37 } else { 1 };
        for cut in (0..=msg.len()).step_by(step) {
            let mut verifier = Ed25519Verifier::new(&key, &sig).unwrap();
            verifier.update(&msg[..cut]);
            verifier.update(&msg[cut..]);
            assert_eq!(verifier.verify(), Ok(()), "{} cut {cut}", v.name);
        }
        // Byte-at-a-time.
        let mut verifier = Ed25519Verifier::new(&key, &sig).unwrap();
        for b in &msg {
            verifier.update(std::slice::from_ref(b));
        }
        assert_eq!(verifier.clone().verify(), Ok(()), "{}", v.name);
        // One extra byte breaks it.
        verifier.update(b"x");
        assert_eq!(verifier.verify(), Err(SigError::VerificationFailed));
    }
}

#[test]
fn ed25519ctx_vectors_verify() {
    for v in ED25519CTX {
        let (key, sig) = parts(v.public_key, v.signature);
        let msg = unhex(v.message);
        let ctx = unhex(v.context);
        assert_eq!(key.verify_ctx(&msg, &ctx, &sig), Ok(()), "{}", v.name);
        // The context is bound: another context, none, or pure Ed25519 fail.
        assert_eq!(
            key.verify_ctx(&msg, b"baz", &sig),
            Err(SigError::VerificationFailed),
            "{}",
            v.name
        );
        assert_eq!(key.verify(&msg, &sig), Err(SigError::VerificationFailed));
        assert_eq!(
            key.verify_ph(&msg, &ctx, &sig),
            Err(SigError::VerificationFailed)
        );
    }
}

#[test]
fn ed25519ph_vector_verifies() {
    let v = &ED25519PH;
    let (key, sig) = parts(v.public_key, v.signature);
    let msg = unhex(v.message);
    assert_eq!(key.verify_ph(&msg, b"", &sig), Ok(()));

    // Pre-hashed with ferroly::hash, as when hashing a download as it streams.
    let mut h = Sha512::new();
    h.update(&msg);
    assert_eq!(key.verify_prehashed(&h.finalize(), b"", &sig), Ok(()));

    // Streaming verifier, every split.
    for cut in 0..=msg.len() {
        let mut verifier = Ed25519phVerifier::new(&key, b"").unwrap();
        verifier.update(&msg[..cut]);
        verifier.update(&msg[cut..]);
        assert_eq!(verifier.verify(&sig), Ok(()));
    }

    // Bound to the pre-hash flag and the (empty) context.
    assert_eq!(key.verify(&msg, &sig), Err(SigError::VerificationFailed));
    assert_eq!(
        key.verify_ph(&msg, b"ctx", &sig),
        Err(SigError::VerificationFailed)
    );
    assert_eq!(
        key.verify_ph(b"abd", b"", &sig),
        Err(SigError::VerificationFailed)
    );
    let mut verifier = Ed25519phVerifier::new(&key, b"").unwrap();
    verifier.update(b"ab");
    assert_eq!(verifier.verify(&sig), Err(SigError::VerificationFailed));
}

#[test]
fn pure_vectors_do_not_verify_as_other_variants() {
    for v in ED25519 {
        let (key, sig) = parts(v.public_key, v.signature);
        let msg = unhex(v.message);
        assert_eq!(
            key.verify_ctx(&msg, b"x", &sig),
            Err(SigError::VerificationFailed)
        );
        assert_eq!(
            key.verify_ph(&msg, b"", &sig),
            Err(SigError::VerificationFailed)
        );
    }
}
