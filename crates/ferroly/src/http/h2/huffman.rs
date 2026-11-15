//! The HPACK Huffman code (RFC 7541 §5.2, Appendix B).
//!
//! The code is canonical: codewords of the same length are consecutive
//! integers and shorter codewords sort first, so the whole table is described
//! by the 257 code lengths below (256 octets plus EOS). The codewords and the
//! decoding tables are derived from them at compile time.

/// Code length in bits for each symbol (0–255, then EOS = 256), from
/// RFC 7541 Appendix B.
const CODE_LENGTHS: [u8; 257] = [
    13, 23, 28, 28, 28, 28, 28, 28, 28, 24, 30, 28, 28, 30, 28, 28, //
    28, 28, 28, 28, 28, 28, 30, 28, 28, 28, 28, 28, 28, 28, 28, 28, //
    6, 10, 10, 12, 13, 6, 8, 11, 10, 10, 8, 11, 8, 6, 6, 6, //
    5, 5, 5, 6, 6, 6, 6, 6, 6, 6, 7, 8, 15, 6, 12, 10, //
    13, 6, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, //
    7, 7, 7, 7, 7, 7, 7, 7, 8, 7, 8, 13, 19, 13, 14, 6, //
    15, 5, 6, 5, 6, 5, 6, 6, 6, 5, 7, 7, 6, 6, 6, 5, //
    6, 7, 6, 5, 5, 6, 7, 7, 7, 7, 7, 15, 11, 14, 13, 28, //
    20, 22, 20, 20, 22, 22, 22, 23, 22, 23, 23, 23, 23, 23, 24, 23, //
    24, 24, 22, 23, 24, 23, 23, 23, 23, 21, 22, 23, 22, 23, 23, 24, //
    22, 21, 20, 22, 22, 23, 23, 21, 23, 22, 22, 24, 21, 22, 23, 23, //
    21, 21, 22, 21, 23, 22, 23, 23, 20, 22, 22, 22, 23, 22, 22, 23, //
    26, 26, 20, 19, 22, 23, 22, 25, 26, 26, 26, 27, 27, 26, 24, 25, //
    19, 21, 26, 27, 27, 26, 27, 24, 21, 21, 26, 26, 28, 27, 27, 27, //
    20, 24, 20, 21, 22, 21, 21, 23, 22, 22, 25, 25, 24, 24, 26, 23, //
    26, 27, 26, 26, 27, 27, 27, 27, 27, 28, 27, 27, 27, 27, 27, 26, //
    30,
];

/// The longest codeword (EOS).
const MAX_LEN: usize = 30;
/// The end-of-string symbol, which must never appear in encoded data.
const EOS: u16 = 256;

/// Canonical decoding tables: for each length, the first codeword, how many
/// codewords have that length, and where their symbols start in `symbols`.
struct Tables {
    codes: [u32; 257],
    first: [u32; MAX_LEN + 1],
    count: [u32; MAX_LEN + 1],
    offset: [u32; MAX_LEN + 1],
    symbols: [u16; 257],
}

const fn build() -> Tables {
    let mut t = Tables {
        codes: [0; 257],
        first: [0; MAX_LEN + 1],
        count: [0; MAX_LEN + 1],
        offset: [0; MAX_LEN + 1],
        symbols: [0; 257],
    };
    let mut code: u32 = 0;
    let mut next: u32 = 0;
    let mut len = 1;
    while len <= MAX_LEN {
        t.first[len] = code;
        t.offset[len] = next;
        let mut sym = 0;
        while sym < 257 {
            if CODE_LENGTHS[sym] as usize == len {
                t.codes[sym] = code;
                t.symbols[next as usize] = sym as u16;
                code += 1;
                next += 1;
                t.count[len] += 1;
            }
            sym += 1;
        }
        code <<= 1;
        len += 1;
    }
    t
}

static TABLES: Tables = build();

/// Why a Huffman string could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HuffmanError {
    /// The EOS symbol, an over-long code, or padding that is longer than
    /// 7 bits or not all ones.
    Invalid,
    /// The decoded string would exceed the caller's cap.
    TooLong,
}

/// Decodes a Huffman-coded string, appending to `out`. Fails without
/// growing `out` past `max` bytes.
pub(crate) fn decode(input: &[u8], max: usize, out: &mut Vec<u8>) -> Result<(), HuffmanError> {
    let t = &TABLES;
    let mut code: u32 = 0;
    let mut len: usize = 0;
    for &byte in input {
        let mut bit = 8;
        while bit > 0 {
            bit -= 1;
            code = (code << 1) | u32::from((byte >> bit) & 1);
            len += 1;
            if len > MAX_LEN {
                return Err(HuffmanError::Invalid);
            }
            let count = t.count[len];
            if count > 0 && code >= t.first[len] && code - t.first[len] < count {
                let sym = t.symbols[(t.offset[len] + code - t.first[len]) as usize];
                if sym == EOS {
                    return Err(HuffmanError::Invalid);
                }
                if out.len() >= max {
                    return Err(HuffmanError::TooLong);
                }
                out.push(sym as u8);
                code = 0;
                len = 0;
            }
        }
    }
    // Padding: at most 7 bits, all ones (a prefix of EOS).
    if len > 7 || code != (1u32 << len) - 1 {
        return Err(HuffmanError::Invalid);
    }
    Ok(())
}

/// The length in bytes of `input` once Huffman-coded.
pub(crate) fn encoded_len(input: &[u8]) -> usize {
    let bits: usize = input
        .iter()
        .map(|&b| CODE_LENGTHS[b as usize] as usize)
        .sum();
    bits.div_ceil(8)
}

/// Huffman-codes `input`, appending to `out` (padded with ones).
pub(crate) fn encode(input: &[u8], out: &mut Vec<u8>) {
    let t = &TABLES;
    let mut acc: u64 = 0;
    let mut bits: u32 = 0;
    for &b in input {
        let len = u32::from(CODE_LENGTHS[b as usize]);
        acc = (acc << len) | u64::from(t.codes[b as usize]);
        bits += len;
        while bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
        // Keep only the bits not yet written.
        acc &= (1u64 << bits) - 1;
    }
    if bits > 0 {
        let pad = 8 - bits;
        out.push(((acc << pad) | ((1u64 << pad) - 1)) as u8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_a_complete_prefix_code() {
        // Kraft equality: sum of 2^-len over all symbols is exactly 1.
        let total: u64 = CODE_LENGTHS
            .iter()
            .map(|&l| 1u64 << (MAX_LEN - l as usize))
            .sum();
        assert_eq!(total, 1u64 << MAX_LEN);
        // Spot-check codewords against RFC 7541 Appendix B.
        let t = &TABLES;
        assert_eq!(
            (t.codes[b'0' as usize], CODE_LENGTHS[b'0' as usize]),
            (0x0, 5)
        );
        assert_eq!(
            (t.codes[b'a' as usize], CODE_LENGTHS[b'a' as usize]),
            (0x3, 5)
        );
        assert_eq!(
            (t.codes[b' ' as usize], CODE_LENGTHS[b' ' as usize]),
            (0x14, 6)
        );
        assert_eq!(
            (t.codes[b'A' as usize], CODE_LENGTHS[b'A' as usize]),
            (0x21, 6)
        );
        assert_eq!((t.codes[0], CODE_LENGTHS[0]), (0x1ff8, 13));
        assert_eq!((t.codes[255], CODE_LENGTHS[255]), (0x3ffffee, 26));
        assert_eq!((t.codes[256], CODE_LENGTHS[256]), (0x3fff_ffff, 30));
    }

    #[test]
    fn round_trips_every_octet() {
        let all: Vec<u8> = (0..=255u8).collect();
        let mut enc = Vec::new();
        encode(&all, &mut enc);
        assert_eq!(enc.len(), encoded_len(&all));
        let mut dec = Vec::new();
        decode(&enc, usize::MAX, &mut dec).unwrap();
        assert_eq!(dec, all);
    }

    #[test]
    fn rfc_example_string() {
        // RFC 7541 C.4.1: "www.example.com".
        let mut enc = Vec::new();
        encode(b"www.example.com", &mut enc);
        assert_eq!(
            enc,
            [0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab, 0x90, 0xf4, 0xff]
        );
    }

    #[test]
    fn rejects_bad_padding_eos_and_caps() {
        let mut out = Vec::new();
        // 8 bits of ones: padding longer than 7 bits.
        assert_eq!(decode(&[0xff], 100, &mut out), Err(HuffmanError::Invalid));
        // Full EOS (30 ones) padded with ones.
        assert_eq!(
            decode(&[0xff, 0xff, 0xff, 0xff], 100, &mut out),
            Err(HuffmanError::Invalid)
        );
        // '0' (00000) padded with zeros instead of ones.
        out.clear();
        assert_eq!(decode(&[0x00], 100, &mut out), Err(HuffmanError::Invalid));
        // Output cap.
        let mut enc = Vec::new();
        encode(b"aaaaaaaa", &mut enc);
        out.clear();
        assert_eq!(decode(&enc, 4, &mut out), Err(HuffmanError::TooLong));
        assert!(out.len() <= 4);
        // Empty input decodes to nothing.
        out.clear();
        decode(&[], 0, &mut out).unwrap();
        assert!(out.is_empty());
    }
}
