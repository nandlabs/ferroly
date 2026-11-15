//! Unit tests for the field, scalar and curve layers, plus a test-only signer
//! (signing is not part of the public API) used to build edge-case signatures.

use super::edwards::{self, Point};
use super::field::{self, Fe};
use super::scalar;
use super::*;
use crate::hash::sha512;

fn unhex<const N: usize>(s: &str) -> [u8; N] {
    let v = crate::hash::hex::decode(s).unwrap();
    v.try_into().unwrap()
}

/// p = 2^255 - 19, little-endian.
fn p_bytes() -> [u8; 32] {
    let mut b = [0xff; 32];
    b[0] = 0xed;
    b[31] = 0x7f;
    b
}

fn fe(v: u64) -> Fe {
    Fe::from_u64(v)
}

/// A deterministic xorshift64* stream for test inputs.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn bytes<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0u8; N];
        for b in out.iter_mut() {
            *b = self.next() as u8;
        }
        out
    }
}

// ---------- field ----------

#[test]
fn field_constants() {
    // d * 121666 = -121665
    assert!(Fe::d().mul(&fe(121_666)).ct_eq(&fe(121_665).neg()));
    // sqrt(-1)^2 = -1
    assert!(Fe::sqrt_m1().square().ct_eq(&Fe::ONE.neg()));
}

#[test]
fn field_encoding_round_trip_and_reduction() {
    let mut rng = Rng(0x1234_5678);
    for _ in 0..200 {
        let mut b: [u8; 32] = rng.bytes();
        b[31] &= 0x7f;
        if field::is_canonical(&b) {
            assert_eq!(Fe::from_bytes(&b).to_bytes(), b);
        }
    }
    // p encodes zero; p + k encodes k; 2^255 - 1 encodes 18.
    let p = p_bytes();
    assert!(!field::is_canonical(&p));
    assert_eq!(Fe::from_bytes(&p).to_bytes(), [0u8; 32]);
    for k in 0u8..19 {
        let mut b = p;
        b[0] += k;
        assert!(!field::is_canonical(&b));
        let mut expect = [0u8; 32];
        expect[0] = k;
        assert_eq!(Fe::from_bytes(&b).to_bytes(), expect);
    }
    // p - 1 is the largest canonical value.
    let mut pm1 = p;
    pm1[0] -= 1;
    assert!(field::is_canonical(&pm1));
    assert_eq!(Fe::from_bytes(&pm1).to_bytes(), pm1);
    // The top (sign) bit is ignored by both.
    let mut signed = pm1;
    signed[31] |= 0x80;
    assert!(field::is_canonical(&signed));
    assert_eq!(Fe::from_bytes(&signed).to_bytes(), pm1);
    // Fully-saturated limbs (2^255 - 1) still reduce.
    assert_eq!(Fe([(1 << 51) - 1; 5]).to_bytes()[0], 18);
}

#[test]
fn field_arithmetic_identities() {
    let minus_one = Fe::ONE.neg();
    assert!(minus_one.mul(&minus_one).ct_eq(&Fe::ONE));
    assert!(fe(7).sub(&fe(9)).ct_eq(&fe(2).neg()));
    assert!(fe(3).add(&fe(4)).mul(&fe(5)).ct_eq(&fe(35)));
    assert!(Fe::ZERO.neg().is_zero());
    assert!(Fe::ZERO.invert().is_zero());

    let mut rng = Rng(42);
    for _ in 0..64 {
        let a = Fe::from_bytes(&rng.bytes());
        let b = Fe::from_bytes(&rng.bytes());
        let c = Fe::from_bytes(&rng.bytes());
        // Distributivity and commutativity.
        assert!(a.mul(&b.add(&c)).ct_eq(&a.mul(&b).add(&a.mul(&c))));
        assert!(a.mul(&b).ct_eq(&b.mul(&a)));
        assert!(a.sub(&b).add(&b).ct_eq(&a));
        // Inverse.
        if !a.is_zero() {
            assert!(a.mul(&a.invert()).ct_eq(&Fe::ONE));
        }
        // Repeated operations stay within the limb bounds.
        let mut x = a;
        for _ in 0..100 {
            x = x.sub(&b).mul(&c).add(&a).neg();
        }
        let _ = x.to_bytes();
    }
}

#[test]
fn field_sqrt_ratio() {
    let mut rng = Rng(7);
    for _ in 0..64 {
        let x = Fe::from_bytes(&rng.bytes());
        let v = Fe::from_bytes(&rng.bytes());
        if v.is_zero() {
            continue;
        }
        // u = x^2 v is a square times v, so sqrt(u / v) = ±x.
        let u = x.square().mul(&v);
        let r = Fe::sqrt_ratio(&u, &v).expect("square");
        assert!(r.square().ct_eq(&x.square()));
    }
    // 2 is not a square mod p (p = 5 mod 8).
    assert!(Fe::sqrt_ratio(&fe(2), &Fe::ONE).is_none());
    // -1 is a square: sqrt(-1).
    let r = Fe::sqrt_ratio(&Fe::ONE.neg(), &Fe::ONE).unwrap();
    assert!(r.square().ct_eq(&Fe::ONE.neg()));
    assert!(Fe::sqrt_ratio(&Fe::ZERO, &Fe::ONE).unwrap().is_zero());
}

// ---------- scalars ----------

const L_BYTES: &str = "edd3f55c1a631258d69cf7a2def9de1400000000000000000000000000000010";

#[test]
fn scalar_canonical_and_reduction() {
    let l: [u8; 32] = unhex(L_BYTES);
    assert!(!scalar::is_canonical(&l));
    let mut lm1 = l;
    lm1[0] -= 1;
    assert!(scalar::is_canonical(&lm1));
    let mut lp1 = l;
    lp1[0] += 1;
    assert!(!scalar::is_canonical(&lp1));
    assert!(scalar::is_canonical(&[0u8; 32]));
    assert!(!scalar::is_canonical(&[0xff; 32]));

    assert_eq!(scalar::reduce(&l), [0u8; 32]);
    assert_eq!(scalar::reduce(&lm1), lm1);
    assert_eq!(scalar::reduce(&lp1)[0], 1);
    assert_eq!(scalar::reduce(&[]), [0u8; 32]);
    // 2^512 - 1 mod L and 2^256 mod L, computed independently.
    let expect: [u8; 32] =
        unhex("000f9c44e31106a447938568a71b0ed065bef517d273ecce3d9a307c1b419903");
    assert_eq!(scalar::reduce(&[0xff; 64]), expect);
    let mut two_256 = [0u8; 33];
    two_256[32] = 1;
    let expect: [u8; 32] =
        unhex("1d95988d7431ecd670cf7d73f45befc6feffffffffffffffffffffffffffff0f");
    assert_eq!(scalar::reduce(&two_256), expect);
}

#[test]
fn scalar_mul_add() {
    let one = {
        let mut b = [0u8; 32];
        b[0] = 1;
        b
    };
    let mut lm1: [u8; 32] = unhex(L_BYTES);
    lm1[0] -= 1;
    // (L-1)(L-1) + 0 = 1 (mod L); (L-1) * 1 + 1 = 0.
    assert_eq!(scalar::mul_add(&lm1, &lm1, &[0; 32]), one);
    assert_eq!(scalar::mul_add(&lm1, &one, &one), [0u8; 32]);
}

// ---------- curve ----------

fn base_bytes() -> [u8; 32] {
    let mut b = [0x66; 32];
    b[0] = 0x58;
    b
}

#[test]
fn base_point_and_group_order() {
    let b = Point::base();
    assert_eq!(b.compress(), base_bytes());
    assert!(!b.is_small_order());
    let l: [u8; 32] = unhex(L_BYTES);
    assert!(b.mul(&l).is_identity());
    assert!(b.add(&b.neg()).is_identity());
    assert!(b.double().equals(&b.add(&b)));
    assert!(Point::IDENTITY.double().is_identity());
    let mut three = [0u8; 32];
    three[0] = 3;
    assert!(b.mul(&three).equals(&b.double().add(&b)));
}

#[test]
fn double_scalar_mul_matches_naive() {
    let mut rng = Rng(99);
    let p = Point::base().mul(&rng.bytes());
    for _ in 0..8 {
        let mut a: [u8; 32] = rng.bytes();
        let mut k: [u8; 32] = rng.bytes();
        a[31] &= 0x1f;
        k[31] &= 0x1f;
        let fast = edwards::double_scalar_mul_base(&a, &k, &p);
        let slow = Point::base().mul(&a).add(&p.mul(&k));
        assert!(fast.equals(&slow));
    }
}

#[test]
fn compress_decompress_round_trip() {
    let mut rng = Rng(5);
    for _ in 0..32 {
        let p = Point::base().mul(&rng.bytes());
        let enc = p.compress();
        let q = Point::decompress(&enc).expect("valid");
        assert!(p.equals(&q));
        assert_eq!(q.compress(), enc);
    }
}

#[test]
fn small_order_points() {
    for h in SMALL_ORDER {
        let b: [u8; 32] = unhex(h);
        let p = Point::decompress(&b).expect("on curve");
        assert!(p.is_small_order(), "{h}");
        assert_eq!(p.compress(), b);
    }
}

pub(super) const SMALL_ORDER: [&str; 8] = [
    "0100000000000000000000000000000000000000000000000000000000000000",
    "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
    "0000000000000000000000000000000000000000000000000000000000000000",
    "0000000000000000000000000000000000000000000000000000000000000080",
    "26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc05",
    "26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc85",
    "c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a",
    "c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac03fa",
];

#[test]
fn decompress_rejects_non_canonical() {
    // x = 0 with the sign bit set.
    let mut neg_zero = [0u8; 32];
    neg_zero[0] = 1;
    neg_zero[31] = 0x80;
    assert!(Point::decompress(&neg_zero).is_none());
    // y >= p, with and without the sign bit.
    for k in 0u8..19 {
        let mut b = p_bytes();
        b[0] += k;
        assert!(Point::decompress(&b).is_none());
        b[31] |= 0x80;
        assert!(Point::decompress(&b).is_none());
    }
    // y = 2 is not on the curve.
    let mut two = [0u8; 32];
    two[0] = 2;
    assert!(Point::decompress(&two).is_none());
}

// ---------- test-only signer ----------

pub(crate) struct TestKey {
    a: [u8; 32],
    prefix: [u8; 32],
    pub(crate) public: [u8; 32],
}

pub(crate) fn test_key(seed: &[u8; 32]) -> TestKey {
    let h = sha512(seed).into_bytes();
    let mut a = [0u8; 32];
    a.copy_from_slice(&h[..32]);
    a[0] &= 248;
    a[31] &= 127;
    a[31] |= 64;
    let mut prefix = [0u8; 32];
    prefix.copy_from_slice(&h[32..]);
    let public = Point::base().mul(&a).compress();
    TestKey { a, prefix, public }
}

/// Signs `msg` with nonce scalar `r` under the (possibly tweaked) encoded key
/// `public_for_hash`.
fn sign_with(
    key: &TestKey,
    public_for_hash: &[u8; 32],
    r_point: &Point,
    r: &[u8; 32],
    msg: &[u8],
) -> [u8; 64] {
    let r_enc = r_point.compress();
    let mut h = Sha512::new();
    h.update(&r_enc);
    h.update(public_for_hash);
    h.update(msg);
    let k = scalar::reduce(h.finalize().as_bytes());
    let s = scalar::mul_add(&k, &key.a, r);
    let mut sig = [0u8; 64];
    sig[..32].copy_from_slice(&r_enc);
    sig[32..].copy_from_slice(&s);
    sig
}

pub(crate) fn sign(key: &TestKey, msg: &[u8]) -> [u8; 64] {
    let mut h = Sha512::new();
    h.update(&key.prefix);
    h.update(msg);
    let r = scalar::reduce(h.finalize().as_bytes());
    sign_with(key, &key.public, &Point::base().mul(&r), &r, msg)
}

#[test]
fn test_signer_reproduces_rfc8032_test1() {
    // Cross-checks the signer (and so the curve arithmetic) against the RFC.
    let key = test_key(&unhex(
        "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
    ));
    assert_eq!(
        crate::hash::hex::encode(&key.public),
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
    );
    assert_eq!(
        crate::hash::hex::encode(&sign(&key, b"")),
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155\
         5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
    );
}

#[test]
fn random_sign_verify_round_trips() {
    let mut rng = Rng(0xfeed_beef);
    for i in 0..24 {
        let key = test_key(&rng.bytes());
        let len = (rng.next() % 300) as usize;
        let msg: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        let sig = sign(&key, &msg);
        let pk = Ed25519PublicKey::from_bytes(&key.public).unwrap();
        let s = Ed25519Signature::from_bytes(&sig);
        assert_eq!(pk.verify(&msg, &s), Ok(()), "round {i}");
        let mut bad = msg.clone();
        bad.push(0);
        assert_eq!(pk.verify(&bad, &s), Err(SigError::VerificationFailed));
        // Streaming agrees for an arbitrary split.
        let cut = if len == 0 {
            0
        } else {
            (rng.next() as usize) % len
        };
        let mut v = Ed25519Verifier::new(&pk, &s).unwrap();
        v.update(&msg[..cut]);
        v.update(&msg[cut..]);
        assert_eq!(v.verify(), Ok(()));
    }
}

/// A key with a torsion component (`A' = A + T`, `T` of order 8) decodes —
/// it is not small-order — but a signature made with the torsion-free secret
/// verifies under the cofactorless equation only when `[k]T` vanishes, i.e.
/// when `k = 0 (mod 8)`. The cofactored equation would accept all of them.
#[test]
fn mixed_order_key_follows_the_cofactorless_equation() {
    let key = test_key(&unhex(
        "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
    ));
    let t = Point::decompress(&unhex(SMALL_ORDER[4])).unwrap();
    let a_mixed = Point::decompress(&key.public).unwrap().add(&t).compress();
    let pk = Ed25519PublicKey::from_bytes(&a_mixed).expect("mixed-order keys decode");

    let mut saw = [false; 2];
    for i in 0..64u32 {
        let msg = i.to_le_bytes();
        let mut r = [0u8; 32];
        r[..4].copy_from_slice(&(i + 1000).to_le_bytes());
        let sig = sign_with(&key, &a_mixed, &Point::base().mul(&r), &r, &msg);

        let mut h = Sha512::new();
        h.update(&sig[..32]);
        h.update(&a_mixed);
        h.update(&msg);
        let k = scalar::reduce(h.finalize().as_bytes());
        let k_div_8 = k[0] % 8 == 0;
        saw[usize::from(k_div_8)] = true;

        let result = pk.verify(&msg, &Ed25519Signature::from_bytes(&sig));
        if k_div_8 {
            assert_eq!(result, Ok(()));
        } else {
            assert_eq!(result, Err(SigError::VerificationFailed));
        }
    }
    assert!(saw[0] && saw[1], "both cases exercised");
}

/// A mixed-order `R` (`R' = rB + T`) under a torsion-free key (every honestly
/// generated key) always fails the cofactorless equation.
#[test]
fn mixed_order_r_is_rejected() {
    let key = test_key(&unhex(
        "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
    ));
    let pk = Ed25519PublicKey::from_bytes(&key.public).unwrap();
    for (n, h) in SMALL_ORDER.iter().enumerate().skip(1) {
        let t = Point::decompress(&unhex(h)).unwrap();
        let mut r = [0u8; 32];
        r[0] = 77 + n as u8;
        let r_mixed = Point::base().mul(&r).add(&t);
        let sig = sign_with(&key, &key.public, &r_mixed, &r, b"msg");
        assert_eq!(
            pk.verify(b"msg", &Ed25519Signature::from_bytes(&sig)),
            Err(SigError::VerificationFailed)
        );
    }
}

#[test]
fn small_order_r_is_malformed() {
    let key = test_key(&[9; 32]);
    let pk = Ed25519PublicKey::from_bytes(&key.public).unwrap();
    for h in SMALL_ORDER {
        let mut sig = [0u8; 64];
        sig[..32].copy_from_slice(&unhex::<32>(h));
        assert_eq!(
            pk.verify(b"", &Ed25519Signature::from_bytes(&sig)),
            Err(SigError::MalformedSignature)
        );
    }
}

// ---------- addition chains and worst-case limbs ----------

#[test]
fn addition_chains_match_square_and_multiply() {
    let mut rng = Rng(0x0add_c4a1);
    let mut inputs: Vec<Fe> = (0..48).map(|_| Fe::from_bytes(&rng.bytes())).collect();
    inputs.extend([Fe::ZERO, Fe::ONE, Fe::ONE.neg(), fe(2), Fe::sqrt_m1()]);
    // p - 1, the largest canonical value.
    let mut pm1 = p_bytes();
    pm1[0] -= 1;
    inputs.push(Fe::from_bytes(&pm1));
    for x in inputs {
        assert!(x.invert().ct_eq(&x.pow(&field::P_MINUS_2)));
        assert!(x.pow_p58().ct_eq(&x.pow(&field::P_MINUS_5_OVER_8)));
    }
}

/// Limbs at the bounds the arithmetic promises to handle, checked against
/// values computed independently with Python big integers (the limbs are
/// summed as `sum(l[i] << 51 i)` and everything reduced mod `p`).
#[test]
fn worst_case_limbs_against_independent_values() {
    const TOP: u64 = (1 << 52) - 1; // the largest rhs limb `sub` accepts
    const WEAK: u64 = (1 << 51) + (1 << 18) - 1; // the weak-reduction bound
    let a = Fe([TOP; 5]);
    let b = Fe([WEAK; 5]);
    let c = Fe([TOP, 0, TOP, 0, TOP]);
    let hex = |x: Fe| crate::hash::hex::encode(&x.to_bytes());
    let cases: [(Fe, &str); 12] = [
        (
            a,
            "2500000000000800000000004000000000000002000000000010000000000000",
        ),
        (
            b,
            "1200040000000000200000000000000100000000000800000000004000000000",
        ),
        (
            c,
            "120000000000100000000000c0ffffffffffff030000000000f0ffffffffff7f",
        ),
        (
            a.mul(&b),
            "9a02c40100009000e00b00008004004d0000002400d80100002001400a000000",
        ),
        (
            a.square(),
            "a50500000000180400000000401c0000000000be0000000000d0040000000000",
        ),
        (
            b.square(),
            "44019000d00400008004801d0000002400a40000002001e00200000009000500",
        ),
        (
            a.mul(&c),
            "7f050000000078030000000040050000000000ba000000000030000000000000",
        ),
        (
            a.add(&b),
            "3700040000000800200000004000000100000002000800000010004000000000",
        ),
        (
            c.sub(&a),
            "edffffffffff07000000000080ffffffffffff010000000000e0ffffffffff7f",
        ),
        (
            a.neg(),
            "c8fffffffffff7ffffffffffbffffffffffffffdffffffffffefffffffffff7f",
        ),
        (
            a.sub(&b),
            "0000fcffffff0700e0ffffff3f0000ffffffff0100f8ffffff0f00c0ffffff7f",
        ),
        (
            a.invert(),
            "5c3b3b70633a127d2520fe174098c91ea9f11d4b0b09598cfb7826ad5737d516",
        ),
    ];
    for (i, (x, want)) in cases.iter().enumerate() {
        assert_eq!(hex(*x), *want, "case {i}");
    }
    // Every result is weakly reduced again, so it can feed the next step.
    for x in [a.mul(&b), a.add(&b), c.sub(&a), a.neg()] {
        assert!(x.0.iter().all(|&l| l <= WEAK), "{:?}", x.0);
    }
}

// ---------- pinned edge-case behaviour (SIG-1, SIG-6, SIG-8) ----------

fn mixed_key() -> (TestKey, [u8; 32], Point) {
    let key = test_key(&unhex(
        "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
    ));
    let t = Point::decompress(&unhex(SMALL_ORDER[4])).unwrap();
    let a_mixed = Point::decompress(&key.public).unwrap().add(&t).compress();
    (key, a_mixed, t)
}

/// The full 512-bit challenge digest and its reduction mod `L`.
fn challenge(r: &[u8], a: &[u8; 32], msg: &[u8]) -> ([u8; 64], [u8; 32]) {
    let mut h = Sha512::new();
    h.update(r);
    h.update(a);
    h.update(msg);
    let full = h.finalize().into_bytes();
    (full, scalar::reduce(&full))
}

/// `[j]T` for the order-8 point `T`, `j = 0..8`.
fn torsion_multiples(t: &Point) -> Vec<Point> {
    let mut out = vec![Point::IDENTITY];
    for j in 1..8 {
        out.push(out[j - 1].add(t));
    }
    out
}

/// SIG-1: a mixed-order `R` is **not** always rejected. With a mixed-order
/// key `A' = A + T`, an `R' = [r]B + [j]T` whose torsion cancels `[k]T`
/// (`j + k = 0 mod 8`) satisfies the cofactorless equation and is accepted,
/// as by libsodium, `ed25519-dalek` and Go. With a torsion-free key it is
/// always rejected (`mixed_order_r_is_rejected`).
#[test]
fn mixed_order_r_is_accepted_when_the_key_torsion_cancels_it() {
    let (key, a_mixed, t) = mixed_key();
    let pk = Ed25519PublicKey::from_bytes(&a_mixed).unwrap();
    let multiples = torsion_multiples(&t);
    let l: [u8; 32] = unhex(L_BYTES);
    let mut found = 0;
    for i in 0..400u32 {
        let msg = i.to_le_bytes();
        let mut r = [0u8; 32];
        r[..4].copy_from_slice(&(i + 7).to_le_bytes());
        let j = 1 + (i as usize % 7);
        let r_mixed = Point::base().mul(&r).add(&multiples[j]);
        let (_, k) = challenge(&r_mixed.compress(), &a_mixed, &msg);
        let cancels = (j + usize::from(k[0] % 8)) % 8 == 0;
        let sig = sign_with(&key, &a_mixed, &r_mixed, &r, &msg);
        let result = pk.verify(&msg, &Ed25519Signature::from_bytes(&sig));
        if cancels {
            assert!(!r_mixed.mul(&l).is_identity(), "R is mixed-order");
            assert_eq!(result, Ok(()), "round {i}");
            found += 1;
        } else {
            assert_eq!(result, Err(SigError::VerificationFailed), "round {i}");
        }
    }
    assert!(found >= 3, "only {found} cancelling cases");
}

/// SIG-8: the challenge is reduced mod `L` before `[k]A`. For a mixed-order
/// key that is observable: a signature verifies when `(h mod L) mod 8 = 0`
/// even though the full 512-bit digest `h` is not a multiple of 8, so a
/// verifier multiplying by the unreduced `h` would reject it.
#[test]
fn the_challenge_is_reduced_mod_l_before_multiplying_the_key() {
    let (key, a_mixed, _) = mixed_key();
    let pk = Ed25519PublicKey::from_bytes(&a_mixed).unwrap();
    let mut pinned = 0;
    for i in 0..400u32 {
        let msg = (i + 5000).to_le_bytes();
        let mut r = [0u8; 32];
        r[..4].copy_from_slice(&(i + 3).to_le_bytes());
        let r_point = Point::base().mul(&r);
        let (full, k) = challenge(&r_point.compress(), &a_mixed, &msg);
        if k[0] % 8 == 0 && full[0] % 8 != 0 {
            let sig = sign_with(&key, &a_mixed, &r_point, &r, &msg);
            assert_eq!(pk.verify(&msg, &Ed25519Signature::from_bytes(&sig)), Ok(()));
            pinned += 1;
        }
    }
    assert!(pinned >= 3, "only {pinned} distinguishing cases");
}

/// Signs `msg` as Ed25519ctx (`dom2(0, context)` prefix), whatever the
/// context length, so the RFC's SHOULD NOT can be exercised.
fn sign_ctx(key: &TestKey, context: &[u8], msg: &[u8]) -> [u8; 64] {
    let dom = |h: &mut Sha512| {
        h.update(b"SigEd25519 no Ed25519 collisions");
        h.update(&[0, context.len() as u8]);
        h.update(context);
    };
    let mut h = Sha512::new();
    dom(&mut h);
    h.update(&key.prefix);
    h.update(msg);
    let r = scalar::reduce(h.finalize().as_bytes());
    let r_enc = Point::base().mul(&r).compress();
    let mut h = Sha512::new();
    dom(&mut h);
    h.update(&r_enc);
    h.update(&key.public);
    h.update(msg);
    let k = scalar::reduce(h.finalize().as_bytes());
    let mut sig = [0u8; 64];
    sig[..32].copy_from_slice(&r_enc);
    sig[32..].copy_from_slice(&scalar::mul_add(&k, &key.a, &r));
    sig
}

/// SIG-6: an Ed25519ctx signature over an **empty** context is correctly
/// formed per RFC 8032 (which only says SHOULD NOT), but is refused here.
#[test]
fn ed25519ctx_with_an_empty_context_is_refused_even_when_validly_signed() {
    let key = test_key(&[42; 32]);
    let pk = Ed25519PublicKey::from_bytes(&key.public).unwrap();
    let one = Ed25519Signature::from_bytes(&sign_ctx(&key, b"x", b"payload"));
    assert_eq!(pk.verify_ctx(b"payload", b"x", &one), Ok(()));

    let empty = Ed25519Signature::from_bytes(&sign_ctx(&key, b"", b"payload"));
    assert_eq!(
        pk.verify_ctx(b"payload", b"", &empty),
        Err(SigError::InvalidContext)
    );
    // It is not a pure Ed25519 signature either (dom2 is still hashed).
    assert_eq!(
        pk.verify(b"payload", &empty),
        Err(SigError::VerificationFailed)
    );
}

/// SIG-4: the streaming verifier validates `R` and `S` once, in `new`, and
/// still agrees with the one-shot API on every outcome.
#[test]
fn streaming_verifier_validates_once_and_agrees_with_one_shot() {
    let key = test_key(&[3; 32]);
    let pk = Ed25519PublicKey::from_bytes(&key.public).unwrap();
    let sig = sign(&key, b"hello world");
    let mut malformed = sig;
    malformed[63] = 0xff; // S >= L
    assert_eq!(
        Ed25519Verifier::new(&pk, &Ed25519Signature::from_bytes(&malformed)).err(),
        Some(SigError::MalformedSignature)
    );
    for msg in [&b"hello world"[..], b"hello worle", b""] {
        let s = Ed25519Signature::from_bytes(&sig);
        let mut v = Ed25519Verifier::new(&pk, &s).unwrap();
        v.update(msg);
        assert_eq!(v.verify(), pk.verify(msg, &s));
    }
}
