//! Integers modulo the group order L = 2^252 + 27742317777372353535851937790883648493.
//!
//! Verification needs only two operations: checking that the signature's `S`
//! is canonical (`S < L`) and reducing a 64-byte SHA-512 digest modulo `L`.
//! Both are on public values, so a simple, obviously-correct bit-serial
//! reduction is used instead of a constant-time Barrett/Montgomery one.
//!
//! **Variable-time; never use with secret scalars.** [`reduce`] branches on
//! every intermediate remainder and [`is_canonical`] returns at the first
//! differing limb.

/// `L` as four little-endian 64-bit limbs.
pub(crate) const L: [u64; 4] = [
    0x5812_631a_5cf5_d3ed,
    0x14de_f9de_a2f7_9cd6,
    0x0000_0000_0000_0000,
    0x1000_0000_0000_0000,
];

/// Little-endian limbs from 32 little-endian bytes.
pub(crate) fn limbs(b: &[u8; 32]) -> [u64; 4] {
    let mut out = [0u64; 4];
    for (i, limb) in out.iter_mut().enumerate() {
        let mut w = [0u8; 8];
        w.copy_from_slice(&b[i * 8..i * 8 + 8]);
        *limb = u64::from_le_bytes(w);
    }
    out
}

fn to_bytes(l: &[u64; 4]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, limb) in l.iter().enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&limb.to_le_bytes());
    }
    out
}

/// `a >= b` for little-endian limbs.
fn geq(a: &[u64; 4], b: &[u64; 4]) -> bool {
    for i in (0..4).rev() {
        if a[i] != b[i] {
            return a[i] > b[i];
        }
    }
    true
}

/// `a - b`, assuming `a >= b`.
fn sub(a: &[u64; 4], b: &[u64; 4]) -> [u64; 4] {
    let mut out = [0u64; 4];
    let mut borrow = 0u64;
    for i in 0..4 {
        let (d1, b1) = a[i].overflowing_sub(b[i]);
        let (d2, b2) = d1.overflowing_sub(borrow);
        out[i] = d2;
        borrow = u64::from(b1 | b2);
    }
    out
}

/// Whether 32 little-endian bytes encode an integer strictly below `L`
/// (RFC 8032 §5.1.7 step 1: `0 <= S < L`).
pub(crate) fn is_canonical(b: &[u8; 32]) -> bool {
    !geq(&limbs(b), &L)
}

/// Reduces a little-endian integer of any length modulo `L`, returning the
/// 32-byte little-endian residue.
pub(crate) fn reduce(bytes: &[u8]) -> [u8; 32] {
    // Bit-serial long division: r = (2r + bit) mod L, most significant bit
    // first. r < L < 2^253 throughout, so 2r + 1 < 2^254 fits in 256 bits.
    let mut r = [0u64; 4];
    for byte in bytes.iter().rev() {
        for bit in (0..8).rev() {
            let carry_in = u64::from((byte >> bit) & 1);
            r = [
                (r[0] << 1) | carry_in,
                (r[1] << 1) | (r[0] >> 63),
                (r[2] << 1) | (r[1] >> 63),
                (r[3] << 1) | (r[2] >> 63),
            ];
            if geq(&r, &L) {
                r = sub(&r, &L);
            }
        }
    }
    to_bytes(&r)
}

/// `(a * b + c) mod L` — used only by the test-only signer.
#[cfg(test)]
pub(crate) fn mul_add(a: &[u8; 32], b: &[u8; 32], c: &[u8; 32]) -> [u8; 32] {
    let (a, b, c) = (limbs(a), limbs(b), limbs(c));
    // 256 x 256 -> 512-bit schoolbook product, plus c.
    let mut prod = [0u128; 9];
    for i in 0..4 {
        for j in 0..4 {
            let p = u128::from(a[i]) * u128::from(b[j]);
            prod[i + j] += p & u128::from(u64::MAX);
            prod[i + j + 1] += p >> 64;
        }
        prod[i] += u128::from(c[i]);
    }
    let mut out = [0u8; 72];
    let mut carry = 0u128;
    for (i, limb) in prod.iter().enumerate() {
        let v = limb + carry;
        out[i * 8..i * 8 + 8].copy_from_slice(&(v as u64).to_le_bytes());
        carry = v >> 64;
    }
    reduce(&out)
}
