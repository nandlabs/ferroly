#![cfg(feature = "codec")]
//! Dependency-free generative ("fuzz-lite") tests for the hand-rolled codecs:
//! parsers must never panic on arbitrary/mutated input, and a `Value` must
//! survive a JSON encode→decode round-trip. Uses a seeded PRNG so failures are
//! reproducible in CI without a `proptest`/`arbitrary` dependency.

use ferroly::codec::{json, xml, yaml, Value};

/// A tiny deterministic SplitMix64 PRNG.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
    fn byte(&mut self) -> u8 {
        self.next_u64() as u8
    }
}

/// Structurally interesting bytes that stress the parsers' branches.
const STRUCTURAL: &[u8] = b"{}[]\":,-.eE+0123456789tfn \t\r\n\\/<>?&;=abc\x00\x01\x7f";

fn random_bytes(rng: &mut Rng, max_len: usize) -> Vec<u8> {
    let len = rng.below(max_len);
    (0..len)
        .map(|_| {
            // Bias toward structural characters so we reach deep parser states.
            if rng.below(2) == 0 {
                STRUCTURAL[rng.below(STRUCTURAL.len())]
            } else {
                rng.byte()
            }
        })
        .collect()
}

/// Mutates a valid seed: flip bytes, truncate, or duplicate a slice.
fn mutate(rng: &mut Rng, seed: &[u8]) -> Vec<u8> {
    let mut v = seed.to_vec();
    if v.is_empty() {
        return v;
    }
    for _ in 0..1 + rng.below(6) {
        if v.is_empty() {
            break;
        }
        match rng.below(3) {
            0 => {
                let i = rng.below(v.len());
                v[i] = rng.byte();
            }
            1 => v.truncate(rng.below(v.len())),
            _ => {
                let i = rng.below(v.len());
                let b = v[i];
                v.insert(i, b);
            }
        }
    }
    v
}

const SEEDS: &[&str] = &[
    r#"{"a":1,"b":[true,null,"x"],"c":{"d":-1.5e3}}"#,
    "<root><a>1</a><b>x</b><b>y</b></root>",
    "name: svc\nport: 9000\ntags:\n  - x\n  - y\nnested:\n  on: true\n",
    r#"[[[[1]]]]"#,
    r#""𐀀 \t \n \\ \" ""#,
];

#[test]
fn parsers_never_panic_on_arbitrary_or_mutated_input() {
    let mut rng = Rng::new(0xF311_0ACE);
    for _ in 0..4000 {
        let bytes = if rng.below(2) == 0 {
            random_bytes(&mut rng, 96)
        } else {
            let seed = SEEDS[rng.below(SEEDS.len())];
            mutate(&mut rng, seed.as_bytes())
        };
        // Each parser must return Ok or Err — never panic, never hang.
        let _ = json::from_slice(&bytes);
        let text = String::from_utf8_lossy(&bytes);
        let _ = xml::from_str(&text);
        let _ = yaml::from_str(&text);
    }
}

/// Builds a random JSON-round-trippable `Value`. Avoids `Float`/`UInt`/`Bytes`,
/// which have documented, intentional JSON asymmetries.
fn random_value(rng: &mut Rng, depth: usize) -> Value {
    let leaf = depth == 0 || rng.below(2) == 0;
    if leaf {
        match rng.below(4) {
            0 => Value::Null,
            1 => Value::Bool(rng.below(2) == 0),
            2 => Value::Int(rng.next_u64() as i64),
            _ => Value::Str(random_string(rng)),
        }
    } else if rng.below(2) == 0 {
        let n = rng.below(4);
        Value::Array((0..n).map(|_| random_value(rng, depth - 1)).collect())
    } else {
        let n = rng.below(4);
        Value::Object(
            (0..n)
                .map(|i| (format!("k{i}"), random_value(rng, depth - 1)))
                .collect(),
        )
    }
}

/// A string of characters that exercise the JSON escape/unescape paths.
fn random_string(rng: &mut Rng) -> String {
    const POOL: &[char] = &[
        'a', 'Z', '0', ' ', '"', '\\', '/', '\n', '\r', '\t', '\u{0008}', '\u{000C}', '\u{0001}',
        'é', '中', '😀', '<', '>', '&', '\'',
    ];
    let len = rng.below(12);
    (0..len).map(|_| POOL[rng.below(POOL.len())]).collect()
}

#[test]
fn json_value_encode_decode_round_trips() {
    let mut rng = Rng::new(0x5EED_1234);
    for _ in 0..3000 {
        let v = random_value(&mut rng, 4);
        let s = json::to_string(&v);
        let back = json::from_str(&s).unwrap_or_else(|e| panic!("re-parse failed for {s:?}: {e}"));
        assert_eq!(back, v, "round-trip mismatch via {s:?}");
    }
}
