//! Arithmetic in GF(p), p = 2^255 - 19.
//!
//! An element is five 51-bit limbs (radix 2^51), little-endian. Products are
//! accumulated in `u128`, so no operation can overflow for the limb bounds the
//! functions keep (every result is weakly reduced: each limb is below
//! 2^51 + 2^18).
//!
//! **Variable-time; never use with secret values.** Verification only handles
//! public values, so nothing here is constant-time: comparisons go through
//! the canonical encoding with data-dependent results, and callers branch on
//! them (`sqrt_ratio`, point decoding). Do not reuse this code for signing,
//! key generation or anything else that touches a secret scalar.

/// Mask for one 51-bit limb.
const MASK51: u64 = (1 << 51) - 1;

/// An element of GF(2^255 - 19), weakly reduced (not necessarily canonical).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Fe(pub(crate) [u64; 5]);

/// `p - 2`, little-endian: the exponent of a Fermat inverse. Only the
/// generic [`Fe::pow`] reference (tests) uses it; [`Fe::invert`] uses an
/// addition chain.
#[cfg(test)]
pub(crate) const P_MINUS_2: [u8; 32] = {
    let mut e = [0xff; 32];
    e[0] = 0xeb;
    e[31] = 0x7f;
    e
};

/// `(p - 5) / 8 = 2^252 - 3`, little-endian: the exponent of the square root
/// candidate in RFC 8032 §5.1.3 (reference for [`Fe::pow_p58`] in tests).
#[cfg(test)]
pub(crate) const P_MINUS_5_OVER_8: [u8; 32] = {
    let mut e = [0xff; 32];
    e[0] = 0xfd;
    e[31] = 0x0f;
    e
};

/// The curve constant `d = -121665 / 121666`, little-endian.
const D_BYTES: [u8; 32] = [
    0xa3, 0x78, 0x59, 0x13, 0xca, 0x4d, 0xeb, 0x75, 0xab, 0xd8, 0x41, 0x41, 0x4d, 0x0a, 0x70, 0x00,
    0x98, 0xe8, 0x79, 0x77, 0x79, 0x40, 0xc7, 0x8c, 0x73, 0xfe, 0x6f, 0x2b, 0xee, 0x6c, 0x03, 0x52,
];

/// `sqrt(-1) = 2^((p - 1) / 4)`, little-endian.
const SQRT_M1_BYTES: [u8; 32] = [
    0xb0, 0xa0, 0x0e, 0x4a, 0x27, 0x1b, 0xee, 0xc4, 0x78, 0xe4, 0x2f, 0xad, 0x06, 0x18, 0x43, 0x2f,
    0xa7, 0xd7, 0xfb, 0x3d, 0x99, 0x00, 0x4d, 0x2b, 0x0b, 0xdf, 0xc1, 0x4f, 0x80, 0x24, 0x83, 0x2b,
];

impl Fe {
    pub(crate) const ZERO: Fe = Fe([0; 5]);
    pub(crate) const ONE: Fe = Fe([1, 0, 0, 0, 0]);

    /// The curve constant `d`.
    pub(crate) fn d() -> Fe {
        Fe::from_bytes(&D_BYTES)
    }

    /// `sqrt(-1)`.
    pub(crate) fn sqrt_m1() -> Fe {
        Fe::from_bytes(&SQRT_M1_BYTES)
    }

    /// A small constant.
    #[cfg(test)]
    pub(crate) fn from_u64(v: u64) -> Fe {
        Fe::reduce([v & MASK51, v >> 51, 0, 0, 0])
    }

    /// Decodes 32 little-endian bytes, **ignoring the top bit** (bit 255).
    /// Values in `p..2^255` are accepted and reduced; callers that need a
    /// canonical encoding check it with [`is_canonical`] first.
    pub(crate) fn from_bytes(b: &[u8; 32]) -> Fe {
        let load = |i: usize| {
            let mut w = [0u8; 8];
            w.copy_from_slice(&b[i..i + 8]);
            u64::from_le_bytes(w)
        };
        Fe::reduce([
            load(0) & MASK51,
            (load(6) >> 3) & MASK51,
            (load(12) >> 6) & MASK51,
            (load(19) >> 1) & MASK51,
            (load(24) >> 12) & MASK51,
        ])
    }

    /// The canonical (fully reduced, `0 <= x < p`) little-endian encoding.
    pub(crate) fn to_bytes(self) -> [u8; 32] {
        let mut l = Fe::reduce(self.0).0;
        // Each limb is now below 2^52, so the value is below 2p. Compute
        // q = 1 if value >= p (i.e. value + 19 >= 2^255), else 0.
        let mut q = (l[0] + 19) >> 51;
        q = (l[1] + q) >> 51;
        q = (l[2] + q) >> 51;
        q = (l[3] + q) >> 51;
        q = (l[4] + q) >> 51;
        // value - q*p = value + 19q - q*2^255: add 19q, carry, drop bit 255.
        l[0] += 19 * q;
        l[1] += l[0] >> 51;
        l[0] &= MASK51;
        l[2] += l[1] >> 51;
        l[1] &= MASK51;
        l[3] += l[2] >> 51;
        l[2] &= MASK51;
        l[4] += l[3] >> 51;
        l[3] &= MASK51;
        l[4] &= MASK51;

        let mut out = [0u8; 32];
        let mut acc: u128 = 0;
        let mut bits = 0u32;
        let mut idx = 0usize;
        for limb in l {
            acc |= u128::from(limb) << bits;
            bits += 51;
            while bits >= 8 && idx < 32 {
                out[idx] = acc as u8;
                acc >>= 8;
                bits -= 8;
                idx += 1;
            }
        }
        if idx < 32 {
            out[idx] = acc as u8;
        }
        out
    }

    /// Carries every limb into the next, folding the top carry back with
    /// `2^255 = 19 (mod p)`. Accepts limbs up to 2^63.
    fn reduce(mut l: [u64; 5]) -> Fe {
        let c0 = l[0] >> 51;
        let c1 = l[1] >> 51;
        let c2 = l[2] >> 51;
        let c3 = l[3] >> 51;
        let c4 = l[4] >> 51;
        l[0] &= MASK51;
        l[1] &= MASK51;
        l[2] &= MASK51;
        l[3] &= MASK51;
        l[4] &= MASK51;
        l[0] += c4 * 19;
        l[1] += c0;
        l[2] += c1;
        l[3] += c2;
        l[4] += c3;
        Fe(l)
    }

    pub(crate) fn add(&self, rhs: &Fe) -> Fe {
        let (a, b) = (self.0, rhs.0);
        Fe::reduce([
            a[0] + b[0],
            a[1] + b[1],
            a[2] + b[2],
            a[3] + b[3],
            a[4] + b[4],
        ])
    }

    pub(crate) fn sub(&self, rhs: &Fe) -> Fe {
        // Add 16p (limb-wise: 16 * (2^51 - 19), 16 * (2^51 - 1), ...) so no
        // limb underflows for a weakly reduced `rhs` (limbs < 2^52).
        const P16_0: u64 = 16 * ((1 << 51) - 19);
        const P16_N: u64 = 16 * ((1 << 51) - 1);
        let (a, b) = (self.0, rhs.0);
        Fe::reduce([
            (a[0] + P16_0) - b[0],
            (a[1] + P16_N) - b[1],
            (a[2] + P16_N) - b[2],
            (a[3] + P16_N) - b[3],
            (a[4] + P16_N) - b[4],
        ])
    }

    pub(crate) fn neg(&self) -> Fe {
        Fe::ZERO.sub(self)
    }

    pub(crate) fn mul(&self, rhs: &Fe) -> Fe {
        #[inline(always)]
        fn m(x: u64, y: u64) -> u128 {
            u128::from(x) * u128::from(y)
        }
        let (a, b) = (self.0, rhs.0);
        let b1 = b[1] * 19;
        let b2 = b[2] * 19;
        let b3 = b[3] * 19;
        let b4 = b[4] * 19;

        let c0 = m(a[0], b[0]) + m(a[4], b1) + m(a[3], b2) + m(a[2], b3) + m(a[1], b4);
        let mut c1 = m(a[1], b[0]) + m(a[0], b[1]) + m(a[4], b2) + m(a[3], b3) + m(a[2], b4);
        let mut c2 = m(a[2], b[0]) + m(a[1], b[1]) + m(a[0], b[2]) + m(a[4], b3) + m(a[3], b4);
        let mut c3 = m(a[3], b[0]) + m(a[2], b[1]) + m(a[1], b[2]) + m(a[0], b[3]) + m(a[4], b4);
        let mut c4 = m(a[4], b[0]) + m(a[3], b[1]) + m(a[2], b[2]) + m(a[1], b[3]) + m(a[0], b[4]);

        let mask = u128::from(MASK51);
        c1 += c0 >> 51;
        c2 += c1 >> 51;
        c3 += c2 >> 51;
        c4 += c3 >> 51;
        let t0 = (c0 & mask) + (c4 >> 51) * 19;
        let l0 = (t0 & mask) as u64;
        let l1 = ((c1 & mask) + (t0 >> 51)) as u64;
        Fe::reduce([
            l0,
            l1,
            (c2 & mask) as u64,
            (c3 & mask) as u64,
            (c4 & mask) as u64,
        ])
    }

    pub(crate) fn square(&self) -> Fe {
        self.mul(self)
    }

    /// `self^(2^k)`: `k` successive squarings.
    fn pow2k(&self, k: u32) -> Fe {
        let mut r = *self;
        for _ in 0..k {
            r = r.square();
        }
        r
    }

    /// `(self^(2^250 - 1), self^11)`, the shared prefix of the inversion and
    /// square-root addition chains (the ref10 / curve25519-dalek chain:
    /// 250 squarings and 11 multiplications).
    fn pow22501(&self) -> (Fe, Fe) {
        let t0 = self.square(); // 2
        let t1 = t0.square().square(); // 8
        let t2 = self.mul(&t1); // 9
        let t3 = t0.mul(&t2); // 11
        let t4 = t3.square(); // 22
        let t5 = t2.mul(&t4); // 2^5 - 1
        let t7 = t5.pow2k(5).mul(&t5); // 2^10 - 1
        let t9 = t7.pow2k(10).mul(&t7); // 2^20 - 1
        let t11 = t9.pow2k(20).mul(&t9); // 2^40 - 1
        let t13 = t11.pow2k(10).mul(&t7); // 2^50 - 1
        let t15 = t13.pow2k(50).mul(&t13); // 2^100 - 1
        let t17 = t15.pow2k(100).mul(&t15); // 2^200 - 1
        let t19 = t17.pow2k(50).mul(&t13); // 2^250 - 1
        (t19, t3)
    }

    /// `self^e` for a little-endian exponent (square-and-multiply, MSB
    /// first): the obviously-correct reference the addition chains are
    /// tested against.
    #[cfg(test)]
    pub(crate) fn pow(&self, e: &[u8; 32]) -> Fe {
        let mut r = Fe::ONE;
        for byte in e.iter().rev() {
            for bit in (0..8).rev() {
                r = r.square();
                if (byte >> bit) & 1 == 1 {
                    r = r.mul(self);
                }
            }
        }
        r
    }

    /// The multiplicative inverse (`0` maps to `0`): `self^(p - 2)`, with
    /// `p - 2 = (2^250 - 1) * 2^5 + 11`.
    pub(crate) fn invert(&self) -> Fe {
        let (t19, t3) = self.pow22501();
        t19.pow2k(5).mul(&t3)
    }

    /// `self^((p - 5) / 8) = self^(2^252 - 3)`, with
    /// `2^252 - 3 = (2^250 - 1) * 4 + 1`.
    pub(crate) fn pow_p58(&self) -> Fe {
        let (t19, _) = self.pow22501();
        t19.pow2k(2).mul(self)
    }

    /// Equality of the represented values: one canonical encoding of the
    /// difference instead of one per side.
    pub(crate) fn ct_eq(&self, rhs: &Fe) -> bool {
        self.sub(rhs).is_zero()
    }

    pub(crate) fn is_zero(&self) -> bool {
        let bytes = self.to_bytes();
        let mut acc = 0u8;
        for b in bytes {
            acc |= b;
        }
        acc == 0
    }

    /// The "sign" of RFC 8032: the least-significant bit of the canonical value.
    pub(crate) fn is_negative(&self) -> bool {
        self.to_bytes()[0] & 1 == 1
    }

    /// Computes a square root of `u / v` (RFC 8032 §5.1.3 step 2–3).
    ///
    /// Returns `None` when `u / v` is not a square (including `v = 0` with
    /// `u != 0`). For `u = 0` the result is `Some(0)`.
    pub(crate) fn sqrt_ratio(u: &Fe, v: &Fe) -> Option<Fe> {
        let v3 = v.square().mul(v);
        let v7 = v3.square().mul(v);
        // x = u v^3 (u v^7)^((p-5)/8)
        let x = u.mul(&v3).mul(&u.mul(&v7).pow_p58());
        let vxx = v.mul(&x.square());
        if vxx.ct_eq(u) {
            Some(x)
        } else if vxx.ct_eq(&u.neg()) {
            Some(x.mul(&Fe::sqrt_m1()))
        } else {
            None
        }
    }
}

/// Whether 32 bytes encode a field element canonically, i.e. the low 255 bits
/// are below `p` (the top bit, the x sign in a point encoding, is ignored).
pub(crate) fn is_canonical(b: &[u8; 32]) -> bool {
    // p = 2^255 - 19: little-endian ed ff .. ff 7f. A 255-bit value is >= p
    // iff bytes 1..=30 are all 0xff, the top byte (sans sign bit) is 0x7f and
    // byte 0 is >= 0xed.
    let top = b[31] & 0x7f;
    if top != 0x7f {
        return true;
    }
    if b[1..31].iter().any(|&x| x != 0xff) {
        return true;
    }
    b[0] < 0xed
}
