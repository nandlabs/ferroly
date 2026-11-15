//! The twisted Edwards curve edwards25519, `-x^2 + y^2 = 1 + d x^2 y^2`.
//!
//! Points are in extended homogeneous coordinates `(X : Y : Z : T)` with
//! `x = X/Z`, `y = Y/Z`, `x y = T/Z`, using the complete addition and doubling
//! formulas of RFC 8032 §5.1.4 (complete: valid for every pair of inputs,
//! including equal points and the identity, so there are no exceptional cases
//! to branch on).
//!
//! **Variable-time; never use with secret scalars.** The scalar
//! multiplication skips zero windows and indexes its tables by scalar
//! nibbles, and decoding branches on the input. That is fine for
//! verification, where every input is public, and unsafe for signing or key
//! generation.

use std::sync::OnceLock;

use super::field::{self, Fe};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Point {
    x: Fe,
    y: Fe,
    z: Fe,
    t: Fe,
}

/// The standard base point `B` encoding: `y = 4/5`, `x` even.
const BASE_BYTES: [u8; 32] = {
    let mut b = [0x66; 32];
    b[0] = 0x58;
    b
};

/// Cached curve constants and the base-point window table.
struct Consts {
    d: Fe,
    d2: Fe,
    base_table: [Point; 16],
}

fn consts() -> &'static Consts {
    static CONSTS: OnceLock<Consts> = OnceLock::new();
    CONSTS.get_or_init(|| {
        let d = Fe::d();
        let d2 = d.add(&d);
        // The base point is a valid encoding; decoding cannot fail. Fall back
        // to the identity rather than panic so a (impossible) bug fails closed:
        // every verification would then reject.
        let base = decompress_with(&BASE_BYTES, &d).unwrap_or(Point::IDENTITY);
        Consts {
            d,
            d2,
            base_table: window_table_with(&base, &d2),
        }
    })
}

impl Point {
    pub(crate) const IDENTITY: Point = Point {
        x: Fe::ZERO,
        y: Fe::ONE,
        z: Fe::ONE,
        t: Fe::ZERO,
    };

    /// The base point `B`.
    #[cfg(test)]
    pub(crate) fn base() -> Point {
        consts().base_table[1]
    }

    /// Decodes a 32-byte point encoding (RFC 8032 §5.1.3), **strictly**:
    /// `y` must be canonical (`y < p`), the point must be on the curve, and
    /// `x = 0` with the sign bit set is rejected.
    pub(crate) fn decompress(b: &[u8; 32]) -> Option<Point> {
        decompress_with(b, &consts().d)
    }

    /// The 32-byte encoding: canonical `y` with the sign of `x` in bit 255.
    pub(crate) fn compress(&self) -> [u8; 32] {
        let zinv = self.z.invert();
        let x = self.x.mul(&zinv);
        let y = self.y.mul(&zinv);
        let mut out = y.to_bytes();
        if x.is_negative() {
            out[31] |= 0x80;
        }
        out
    }

    pub(crate) fn neg(&self) -> Point {
        Point {
            x: self.x.neg(),
            y: self.y,
            z: self.z,
            t: self.t.neg(),
        }
    }

    #[cfg(test)]
    pub(crate) fn add(&self, q: &Point) -> Point {
        add_with(self, q, &consts().d2)
    }

    /// Point doubling (RFC 8032 §5.1.4, `dbl-2008-hwcd`).
    pub(crate) fn double(&self) -> Point {
        let a = self.x.square();
        let b = self.y.square();
        let zz = self.z.square();
        let c = zz.add(&zz);
        let h = a.add(&b);
        let e = h.sub(&self.x.add(&self.y).square());
        let g = a.sub(&b);
        let f = c.add(&g);
        Point {
            x: e.mul(&f),
            y: g.mul(&h),
            t: e.mul(&h),
            z: f.mul(&g),
        }
    }

    pub(crate) fn is_identity(&self) -> bool {
        // x = 0 and y = 1, projectively: X = 0 and Y = Z.
        self.x.is_zero() && self.y.ct_eq(&self.z)
    }

    /// Whether `[8]P` is the identity, i.e. `P` lies in the small (order
    /// dividing 8) torsion subgroup.
    pub(crate) fn is_small_order(&self) -> bool {
        self.double().double().double().is_identity()
    }

    /// Projective equality.
    #[cfg(test)]
    pub(crate) fn equals(&self, q: &Point) -> bool {
        self.x.mul(&q.z).ct_eq(&q.x.mul(&self.z)) && self.y.mul(&q.z).ct_eq(&q.y.mul(&self.z))
    }

    /// `[s]P` for a 32-byte little-endian scalar (variable time).
    #[cfg(test)]
    pub(crate) fn mul(&self, s: &[u8; 32]) -> Point {
        let table = window_table_with(self, &consts().d2);
        let mut acc = Point::IDENTITY;
        for byte in s.iter().rev() {
            for nibble in [byte >> 4, byte & 0x0f] {
                acc = acc.double().double().double().double();
                if nibble != 0 {
                    acc = acc.add(&table[usize::from(nibble)]);
                }
            }
        }
        acc
    }
}

/// `[a]B + [b]P` for 32-byte little-endian scalars, **variable time** (table
/// lookups indexed by, and branches on, the scalar nibbles): only for public
/// scalars, as in verification. Interleaved 4-bit fixed windows (Straus):
/// 256 doublings and at most 128 additions.
pub(crate) fn double_scalar_mul_base(a: &[u8; 32], b: &[u8; 32], p: &Point) -> Point {
    let c = consts();
    let p_table = window_table_with(p, &c.d2);
    let mut acc = Point::IDENTITY;
    for i in (0..32).rev() {
        for shift in [4u8, 0] {
            acc = acc.double().double().double().double();
            let na = usize::from((a[i] >> shift) & 0x0f);
            let nb = usize::from((b[i] >> shift) & 0x0f);
            if na != 0 {
                acc = add_with(&acc, &c.base_table[na], &c.d2);
            }
            if nb != 0 {
                acc = add_with(&acc, &p_table[nb], &c.d2);
            }
        }
    }
    acc
}

/// `[0]P, [1]P, ..., [15]P`.
fn window_table_with(p: &Point, d2: &Fe) -> [Point; 16] {
    let mut table = [Point::IDENTITY; 16];
    for i in 1..16 {
        table[i] = add_with(&table[i - 1], p, d2);
    }
    table
}

/// Complete addition (RFC 8032 §5.1.4, `add-2008-hwcd-3`), with `d2 = 2d`.
fn add_with(p: &Point, q: &Point, d2: &Fe) -> Point {
    let a = p.y.sub(&p.x).mul(&q.y.sub(&q.x));
    let b = p.y.add(&p.x).mul(&q.y.add(&q.x));
    let c = p.t.mul(d2).mul(&q.t);
    let zz = p.z.mul(&q.z);
    let d = zz.add(&zz);
    let e = b.sub(&a);
    let f = d.sub(&c);
    let g = d.add(&c);
    let h = b.add(&a);
    Point {
        x: e.mul(&f),
        y: g.mul(&h),
        t: e.mul(&h),
        z: f.mul(&g),
    }
}

fn decompress_with(b: &[u8; 32], d: &Fe) -> Option<Point> {
    // Step 1: y must be canonical (y < p); bit 255 is the sign of x.
    if !field::is_canonical(b) {
        return None;
    }
    let sign = b[31] >> 7 == 1;
    let y = Fe::from_bytes(b);
    // Step 2: x^2 = (y^2 - 1) / (d y^2 + 1).
    let yy = y.square();
    let u = yy.sub(&Fe::ONE);
    let v = d.mul(&yy).add(&Fe::ONE);
    // Step 3: a square root, or the encoding is not a curve point.
    let mut x = Fe::sqrt_ratio(&u, &v)?;
    // Step 4: x = 0 with the sign bit set is a non-canonical encoding.
    if x.is_zero() && sign {
        return None;
    }
    if x.is_negative() != sign {
        x = x.neg();
    }
    Some(Point {
        x,
        y,
        z: Fe::ONE,
        t: x.mul(&y),
    })
}
