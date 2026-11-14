#![cfg(feature = "codec")]
//! Dependency-free generative ("fuzz-lite") tests for the hand-rolled codecs:
//! parsers must never panic on arbitrary/mutated input, and a `Value` must
//! survive a JSON encode→decode round-trip. Uses a seeded PRNG so failures are
//! reproducible in CI without a `proptest`/`arbitrary` dependency.
//!
//! The coverage-guided counterparts live in the detached `fuzz/` workspace;
//! every crash they (or a review) found is pinned here as a seed or a
//! regression input.

use ferroly::codec::{json, toml, xml, yaml, CodecError, Limits, Value};

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

/// Structurally interesting bytes that stress the parsers' branches. The two
/// trailing bytes are the UTF-8 encoding of `é`, so multibyte characters land
/// next to escapes and delimiters.
const STRUCTURAL: &[u8] =
    b"{}[]\":,-.eE+0123456789tfn \t\r\n\\/<>?&;=abc\x00\x01\x7f'#!|*%_uUxX\xc3\xa9";

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
        match rng.below(4) {
            0 => {
                let i = rng.below(v.len());
                v[i] = rng.byte();
            }
            1 => v.truncate(rng.below(v.len())),
            2 => {
                let i = rng.below(v.len());
                v[i] = STRUCTURAL[rng.below(STRUCTURAL.len())];
            }
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
    // JSON
    r#"{"a":1,"b":[true,null,"x"],"c":{"d":-1.5e3}}"#,
    r#"[[[[1]]]]"#,
    r#""\uD800\uDC00 \t \n \\ \" ""#,
    r#"[0, -0, 1.5, 1e999, 01, 1., -.5, "\u00e9"]"#,
    // XML — including the truncated prolog / comment / CDATA forms that used
    // to index past the end of the input.
    "<root><a>1</a><b>x</b><b>y</b></root>",
    "<?xml version=\"1.0\"?><!-- c --><root a=\">\" b='/>'><k>&#x41;&amp;</k></root>",
    "<root type=\"array\"><item xml:space=\"preserve\"> x </item><item/></root>",
    "<r><![CDATA[a < b]]><!-- c --><?pi x?></r>",
    "<?",
    "<?xml",
    "<!--",
    "<!-- x --",
    "<root><!--",
    "<root><![CDATA[",
    "<root>&#x+41;&#+65;</root>",
    "<_x0041_ _x_=\"1\"><_x_/></_x0041_>",
    // YAML
    "name: svc\nport: 9000\ntags:\n  - x\n  - y\nnested:\n  on: true\n",
    "tags:\n- a\n- b\nport: 1 # c\nm: {a: 1, b: [2, 3]}\n",
    "- name: a\n  port: 1\n- name: b\n- - x\n  - \"q: r\"\n",
    "---\na: |\n  text\n  more\nb: >-\n  folded\n  line\n...\n",
    "a: 'it''s'\nb: \"\\u00e9\\x41\\U0001F600\"\nc: &x 1\nd: *x\n",
    // TOML — including the escapes that used to slice a character in half and
    // the dotted keys that used to nest without limit.
    "name = \"svc\"\nport = 9000\ntags = [\"x\", \"y\"]\n[nested]\non = true\n",
    "[[s]]\nh = \"a\"\n[[s]]\nh = 'b'\n[t.u]\nv.w = { x = 1, y = [1, 2] }\n",
    "a = \"\\u000\u{e9}\"\n",
    "a = \"\\U0000000\u{e9}\"\n",
    "a = \"\\u+041\"\n",
    "a.a.a.a.a.a.a.a.a.a.a.a.a.a.a.a = 1\n",
    "a = \"\"\"\nmulti \\\n  line\"\"\"\nb = '''raw'''\nc = 1979-05-27T07:32:00Z\n",
    "a = 0x_1\nb = 1__0\nc = 1e999\nd = +inf\ne = 01\n",
];

/// Structural equality that treats `NaN` as equal to itself, so a parsed
/// `.nan` does not fail a round-trip comparison.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => x == y || (x.is_nan() && y.is_nan()),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|((k, p), (l, q))| k == l && same(p, q))
        }
        _ => a == b,
    }
}

/// Checks that a codec can read back what it just wrote. The only acceptable
/// failure is a budget one: re-encoding may be longer (or, for XML's explicit
/// array form, one level deeper) than the input that fit the limits.
fn reparsed(format: &str, written: &str, result: Result<Value, CodecError>) -> Option<Value> {
    match result {
        Ok(v) => Some(v),
        Err(CodecError::LimitExceeded(_)) => None,
        Err(e) => panic!("{format}: the encoder's own output does not parse: {e}\n{written:?}"),
    }
}

/// Runs every parser over `bytes`. Each must return `Ok` or `Err` — never
/// panic, never hang — and whatever one accepts, its encoder must write in a
/// form the same parser accepts again.
fn parse_all(bytes: &[u8], limits: Limits) {
    let _ = json::decode_from_slice_with::<Value>(bytes, limits);
    let _ = json::decode_from_slice_with::<Vec<Option<u8>>>(bytes, limits);
    let text = String::from_utf8_lossy(bytes);
    let mut accepted = Vec::new();

    if let Ok(v) = json::from_slice_with(bytes, limits) {
        // JSON is full-fidelity: the value itself must survive, compact or
        // indented.
        for written in [json::to_string(&v), json::to_string_pretty(&v)] {
            if let Some(back) = reparsed("json", &written, json::from_str_with(&written, limits)) {
                assert!(same(&back, &v), "json changed {v:?} via {written:?}");
            }
        }
        accepted.push(v);
    }
    if let Ok(v) = yaml::from_str_with(&text, limits) {
        // So is YAML at the `Value` level.
        let written = yaml::encode(&v);
        if let Some(back) = reparsed("yaml", &written, yaml::from_str_with(&written, limits)) {
            assert!(same(&back, &v), "yaml changed {v:?} via {written:?}");
        }
        accepted.push(v);
    }
    if let Ok(v) = xml::from_str_with(&text, limits) {
        let written = xml::encode(&v);
        reparsed("xml", &written, xml::from_str_with(&written, limits));
        accepted.push(v);
    }
    if let Ok(v) = toml::from_str_with(&text, limits) {
        let written = toml::encode(&v);
        reparsed("toml", &written, toml::from_str_with(&written, limits));
        accepted.push(v);
    }

    // Whatever any parser accepted, every encoder must be able to write.
    for v in &accepted {
        let _ = json::to_string(v);
        let _ = xml::encode(v);
        let _ = yaml::encode(v);
        let _ = toml::encode(v);
    }
}

/// Iteration count for the generative tests. `FERROLY_FUZZ_ITERS` raises it
/// for a longer soak (e.g. a scheduled CI job) without touching the code.
fn iterations(default: usize) -> usize {
    std::env::var("FERROLY_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[test]
fn parsers_never_panic_on_arbitrary_or_mutated_input() {
    let mut rng = Rng::new(0xF311_0ACE);
    for _ in 0..iterations(12_000) {
        let bytes = if rng.below(3) == 0 {
            random_bytes(&mut rng, 96)
        } else {
            let seed = SEEDS[rng.below(SEEDS.len())];
            mutate(&mut rng, seed.as_bytes())
        };
        parse_all(&bytes, Limits::DEFAULT);
    }
}

#[test]
fn parsers_never_panic_on_any_prefix_of_a_seed() {
    // Truncation is the classic way to walk a scanner off the end of its
    // input, so try every byte prefix of every seed.
    for seed in SEEDS {
        let bytes = seed.as_bytes();
        for end in 0..=bytes.len() {
            parse_all(&bytes[..end], Limits::DEFAULT);
        }
    }
}

#[test]
fn parsers_never_panic_under_tight_limits() {
    let limits = Limits::new()
        .with_max_depth(3)
        .with_max_entries(4)
        .with_max_input_bytes(64);
    let mut rng = Rng::new(0x11A1_7ED5);
    for _ in 0..iterations(4000) {
        let seed = SEEDS[rng.below(SEEDS.len())];
        parse_all(&mutate(&mut rng, seed.as_bytes()), limits);
    }
    for seed in SEEDS {
        parse_all(seed.as_bytes(), limits);
    }
}

#[test]
fn review_crash_inputs_are_plain_errors() {
    // P0-1: a multibyte character right after a short `\u`/`\U` escape.
    assert!(toml::from_str("a = \"\\u000\u{e9}\"").is_err());
    assert!(toml::from_str("a = \"\\U0000000\u{e9}\"").is_err());
    // P0-2: an unterminated processing instruction or comment.
    for input in ["<?", "<!--", "<?xml", "<a><!--", "<a/><?", "<a/><!--"] {
        assert!(xml::from_str(input).is_err(), "{input:?}");
    }
    // P0-3: 100k dotted-key segments (200 KB) must not build a 100k-deep tree.
    let deep = vec!["a"; 100_000].join(".");
    assert!(toml::from_str(&format!("{deep} = 1")).is_err());
    assert!(toml::from_str(&format!("[{deep}]")).is_err());
    // Deep nesting in every format is an error, not a stack overflow.
    let n = 100_000;
    assert!(json::from_str(&"[".repeat(n)).is_err());
    assert!(json::from_str(&"{\"a\":".repeat(n)).is_err());
    assert!(json::decode::<Value>(&"[".repeat(n)).is_err());
    assert!(xml::from_str(&"<a>".repeat(n)).is_err());
    assert!(yaml::from_str(&"[".repeat(n)).is_err());
    assert!(yaml::from_str(&"- ".repeat(n)).is_err());
    assert!(yaml::from_str(&"{a: ".repeat(n)).is_err());
    assert!(toml::from_str(&format!("a = {}", "[".repeat(n))).is_err());
    assert!(toml::from_str(&format!("a = {}", "{b = ".repeat(n))).is_err());
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
        'é', '中', '😀', '<', '>', '&', '\'', ':', '#', '-', '\u{7f}', '\u{2028}',
    ];
    let len = rng.below(12);
    (0..len).map(|_| POOL[rng.below(POOL.len())]).collect()
}

#[test]
fn json_value_encode_decode_round_trips() {
    let mut rng = Rng::new(0x5EED_1234);
    for _ in 0..iterations(3000) {
        let v = random_value(&mut rng, 4);
        let s = json::to_string(&v);
        let back = json::from_str(&s).unwrap_or_else(|e| panic!("re-parse failed for {s:?}: {e}"));
        assert_eq!(back, v, "round-trip mismatch via {s:?}");
        // The indented form carries the same value.
        let pretty = json::to_string_pretty(&v);
        assert_eq!(json::from_str(&pretty).unwrap(), v, "via {pretty:?}");
    }
}

#[test]
fn yaml_value_encode_decode_round_trips() {
    // YAML keeps every `Value` kind this generator produces (it has typed
    // scalars, null, and both collection forms), so the round-trip is exact.
    let mut rng = Rng::new(0xA11C_E5ED);
    for _ in 0..iterations(3000) {
        let v = random_value(&mut rng, 4);
        let s = yaml::encode(&v);
        let back = yaml::from_str(&s).unwrap_or_else(|e| panic!("re-parse failed for {s:?}: {e}"));
        assert_eq!(back, v, "round-trip mismatch via {s:?}");
    }
}
