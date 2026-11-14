#![cfg(feature = "codec")]
//! Typed round-trip property tests: `decode(encode(x)) == x` for YAML, XML and
//! TOML, over generated structs whose strings and map keys are drawn from a
//! pool of adversarial text (YAML/XML/TOML syntax, padding, control
//! characters, empty strings) and whose collections hit the awkward shapes
//! (empty, single-element, nested, optional elements).
//!
//! Seeded and dependency-free, like `codec_fuzz.rs`. `FERROLY_FUZZ_ITERS`
//! raises the iteration count for a longer soak.

use std::collections::BTreeMap;
use std::fmt::Debug;

use ferroly::codec::{json, toml, xml, yaml, Decode, Encode};

// ---- generator -----------------------------------------------------------

/// A tiny deterministic SplitMix64 PRNG.
struct Rng(u64);

impl Rng {
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
    fn coin(&mut self) -> bool {
        self.below(2) == 0
    }
}

fn iterations(default: usize) -> usize {
    std::env::var("FERROLY_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Strings that are syntax, keywords, or otherwise awkward in at least one of
/// the formats.
const ADVERSARIAL: &[&str] = &[
    "",
    " ",
    "  ",
    " padded ",
    "trailing ",
    " leading",
    "\ttab",
    "tab\tinside",
    "line\nbreak",
    "\nleading newline",
    "trailing newline\n",
    "cr\rlf\r\n",
    "\r",
    "a: b",
    "a : b",
    "key: value # comment",
    "a #b",
    "#hash",
    "x#y",
    "- item",
    "-",
    "-5",
    "+5",
    "- -5",
    "? q",
    ": colon",
    "colon:",
    "[a, b]",
    "{a: 1}",
    "[]",
    "{}",
    "a, b",
    "null",
    "Null",
    "~",
    "true",
    "False",
    "yes",
    "No",
    "on",
    "off",
    "1",
    "007",
    "1.5",
    ".5",
    "5.",
    "1e3",
    "0x10",
    "0o17",
    "1_000",
    ".inf",
    "-.inf",
    ".nan",
    "inf",
    "nan",
    "---",
    "...",
    "--- x",
    "&anchor",
    "*alias",
    "!tag",
    "!!str",
    "|",
    ">",
    "|-",
    "%directive",
    "@at",
    "`tick",
    "'",
    "''",
    "\"",
    "\"\"",
    "it's",
    "say \"hi\"",
    "'single'",
    "\"double\"",
    "\\",
    "\\n",
    "back\\slash",
    "\\u0041",
    "<tag>",
    "</root>",
    "<root>",
    "<!-- c -->",
    "<![CDATA[x]]>",
    "]]>",
    "&amp;",
    "&",
    "&#x41;",
    "a < b > c",
    "<?pi?>",
    "_x0041_",
    "_x_",
    "a_x41_b",
    "= eq",
    "a = 1",
    "[table]",
    "[[array]]",
    "\"\"\"",
    "'''",
    "1979-05-27",
    "1979-05-27T07:32:00Z",
    "é",
    "中文",
    "😀",
    "é: ü",
    "\u{0}",
    "\u{1}",
    "\u{1f}",
    "\u{7f}",
    "\u{85}",
    "\u{a0}nbsp\u{a0}",
    "\u{2028}",
    "\u{2029}",
    "\u{feff}bom",
    "\u{fffe}",
    "\u{ffff}",
    "plain",
    "two words",
    "snake_case",
    "kebab-case",
    "dotted.key",
    "x:y",
    "1st",
];

fn text(rng: &mut Rng) -> String {
    match rng.below(8) {
        // Mostly straight from the pool…
        0..=4 => ADVERSARIAL[rng.below(ADVERSARIAL.len())].to_string(),
        // …sometimes two entries glued together…
        5 => format!(
            "{}{}",
            ADVERSARIAL[rng.below(ADVERSARIAL.len())],
            ADVERSARIAL[rng.below(ADVERSARIAL.len())]
        ),
        // …and sometimes random characters from a nasty alphabet.
        _ => {
            const ALPHABET: &[char] = &[
                'a', 'B', '0', '9', ' ', '\t', '\n', '\r', ':', '#', '-', '?', ',', '[', ']', '{',
                '}', '&', '*', '!', '|', '>', '<', '\'', '"', '%', '@', '`', '\\', '/', '=', '_',
                '.', '+', '~', 'é', '中', '😀', '\u{1}', '\u{7f}', '\u{2028}',
            ];
            let len = rng.below(10);
            (0..len)
                .map(|_| ALPHABET[rng.below(ALPHABET.len())])
                .collect()
        }
    }
}

fn int(rng: &mut Rng) -> i64 {
    match rng.below(6) {
        0 => 0,
        1 => -1,
        2 => i64::MIN,
        3 => i64::MAX,
        4 => -(rng.below(1000) as i64),
        _ => rng.next_u64() as i64,
    }
}

fn float(rng: &mut Rng) -> f64 {
    match rng.below(9) {
        0 => 0.0,
        1 => -2.5,
        2 => 250.0,
        3 => 1e300,
        4 => -1e-300,
        5 => f64::INFINITY,
        6 => f64::NEG_INFINITY,
        7 => f64::MAX,
        // Any finite bit pattern.
        _ => loop {
            let f = f64::from_bits(rng.next_u64());
            if f.is_finite() {
                break f;
            }
        },
    }
}

fn list<T>(rng: &mut Rng, mut item: impl FnMut(&mut Rng) -> T) -> Vec<T> {
    // Weight the awkward lengths: empty and single-element.
    let len = match rng.below(5) {
        0 => 0,
        1 | 2 => 1,
        3 => 2,
        _ => 3 + rng.below(3),
    };
    (0..len).map(|_| item(rng)).collect()
}

fn option<T>(rng: &mut Rng, item: impl FnOnce(&mut Rng) -> T) -> Option<T> {
    if rng.below(3) == 0 {
        None
    } else {
        Some(item(rng))
    }
}

// ---- the types under test ------------------------------------------------

#[derive(Encode, Decode, PartialEq, Debug, Clone)]
struct Inner {
    label: String,
    score: i64,
    flag: bool,
    alias: Option<String>,
}

#[derive(Encode, Decode, PartialEq, Debug, Clone)]
struct Doc {
    name: String,
    count: i64,
    big: u64,
    small: i8,
    ratio: f64,
    on: bool,
    letter: char,
    note: Option<String>,
    limit: Option<u32>,
    tags: Vec<String>,
    numbers: Vec<i64>,
    grid: Vec<Vec<i32>>,
    cube: Vec<Vec<Vec<String>>>,
    maybe: Vec<Option<i32>>,
    maybe_text: Vec<Option<String>>,
    opt_list: Option<Vec<String>>,
    pair: (String, i32),
    triple: [i16; 3],
    labels: BTreeMap<String, String>,
    counters: BTreeMap<String, Vec<i32>>,
    inner: Inner,
    items: Vec<Inner>,
    opt_inner: Option<Inner>,
}

fn inner(rng: &mut Rng) -> Inner {
    Inner {
        label: text(rng),
        score: int(rng),
        flag: rng.coin(),
        alias: option(rng, text),
    }
}

/// Builds a random `Doc`. `null_items` controls whether sequences may hold
/// `None` — TOML has no null, so it cannot represent one inside an array.
fn doc(rng: &mut Rng, null_items: bool) -> Doc {
    let opt_i32 = |rng: &mut Rng| {
        if null_items {
            option(rng, |r| int(r) as i32)
        } else {
            Some(int(rng) as i32)
        }
    };
    let opt_text = |rng: &mut Rng| {
        if null_items {
            option(rng, text)
        } else {
            Some(text(rng))
        }
    };
    Doc {
        name: text(rng),
        count: int(rng),
        big: match rng.below(3) {
            0 => u64::MAX,
            1 => 0,
            _ => rng.next_u64(),
        },
        small: int(rng) as i8,
        ratio: float(rng),
        on: rng.coin(),
        letter: {
            const LETTERS: &[char] = &[
                'a', ' ', '\n', '\t', ':', '#', '-', '<', '&', '"', '\'', 'é', '😀', '\u{1}', '~',
            ];
            LETTERS[rng.below(LETTERS.len())]
        },
        note: option(rng, text),
        limit: option(rng, |r| r.next_u64() as u32),
        tags: list(rng, text),
        numbers: list(rng, int),
        grid: list(rng, |r| list(r, |r| int(r) as i32)),
        cube: list(rng, |r| list(r, |r| list(r, text))),
        maybe: list(rng, opt_i32),
        maybe_text: list(rng, opt_text),
        opt_list: option(rng, |r| list(r, text)),
        pair: (text(rng), int(rng) as i32),
        triple: [int(rng) as i16, int(rng) as i16, int(rng) as i16],
        labels: list(rng, |r| (text(r), text(r))).into_iter().collect(),
        counters: list(rng, |r| (text(r), list(r, |r| int(r) as i32)))
            .into_iter()
            .collect(),
        inner: inner(rng),
        items: list(rng, inner),
        opt_inner: option(rng, inner),
    }
}

/// Asserts a round trip, printing the encoded document on failure.
fn assert_round_trip<T>(
    format: &str,
    value: &T,
    encode: impl Fn(&T) -> String,
    decode: impl Fn(&str) -> Result<T, ferroly::codec::CodecError>,
) where
    T: PartialEq + Debug,
{
    let encoded = encode(value);
    match decode(&encoded) {
        Ok(back) => assert!(
            back == *value,
            "{format} round-trip mismatch\n value: {value:?}\n  back: {back:?}\n  text: {encoded:?}"
        ),
        Err(e) => panic!(
            "{format} failed to decode its own output: {e}\n value: {value:?}\n  text: {encoded:?}"
        ),
    }
}

// ---- generated structs ---------------------------------------------------

#[test]
fn yaml_typed_round_trip() {
    let mut rng = Rng(0x7A31_0001);
    for _ in 0..iterations(400) {
        assert_round_trip("yaml", &doc(&mut rng, true), yaml::encode, yaml::decode);
    }
}

#[test]
fn xml_typed_round_trip() {
    let mut rng = Rng(0x7A31_0002);
    for _ in 0..iterations(400) {
        assert_round_trip("xml", &doc(&mut rng, true), xml::encode, xml::decode);
    }
}

#[test]
fn toml_typed_round_trip() {
    let mut rng = Rng(0x7A31_0003);
    for _ in 0..iterations(400) {
        let value = doc(&mut rng, false);
        assert_round_trip("toml", &value, toml::encode, toml::decode);
        // Without nulls in arrays, the fallible encoder agrees.
        assert_eq!(toml::try_encode(&value).unwrap(), toml::encode(&value));
    }
}

#[test]
fn json_typed_round_trip() {
    // JSON is the reference: compact, indented, and via the `Value` tree.
    // (Non-finite floats have no JSON form, so they are swapped out.)
    let mut rng = Rng(0x7A31_0004);
    for _ in 0..iterations(400) {
        let mut value = doc(&mut rng, true);
        if !value.ratio.is_finite() {
            value.ratio = 0.5;
        }
        assert_round_trip("json", &value, json::encode, json::decode);
        assert_round_trip("json (pretty)", &value, json::encode_pretty, json::decode);
        let via_value = Doc::decode(&json::from_str(&json::encode(&value)).unwrap()).unwrap();
        assert_eq!(via_value, value);
    }
}

#[test]
fn every_adversarial_string_round_trips_in_every_position() {
    #[derive(Encode, Decode, PartialEq, Debug)]
    struct S {
        value: String,
        opt: Option<String>,
        one: Vec<String>,
        two: Vec<String>,
        nested: Vec<Vec<String>>,
        map: BTreeMap<String, String>,
    }
    for s in ADVERSARIAL {
        let value = S {
            value: s.to_string(),
            opt: Some(s.to_string()),
            one: vec![s.to_string()],
            two: vec![s.to_string(), s.to_string()],
            nested: vec![vec![s.to_string()], vec![]],
            map: [(s.to_string(), s.to_string())].into_iter().collect(),
        };
        assert_round_trip("yaml", &value, yaml::encode, yaml::decode);
        assert_round_trip("xml", &value, xml::encode, xml::decode);
        assert_round_trip("toml", &value, toml::encode, toml::decode);
        assert_round_trip("json", &value, json::encode, json::decode);
        // And as a bare top-level string where the format allows one.
        let top = s.to_string();
        assert_round_trip("yaml (top-level)", &top, yaml::encode, yaml::decode);
        assert_round_trip("xml (top-level)", &top, xml::encode, xml::decode);
    }
}

// ---- the specific shapes the review found broken ---------------------------

#[test]
fn yaml_review_cases() {
    // A string containing ": " inside a Vec<String> used to fail to decode.
    let v = vec!["a: b".to_string(), "c".to_string()];
    assert_round_trip("yaml", &v, yaml::encode, yaml::decode);
    // The same text as a map key.
    let m: BTreeMap<String, String> = [("a: b".to_string(), "c: d".to_string())]
        .into_iter()
        .collect();
    assert_round_trip("yaml", &m, yaml::encode, yaml::decode);
    // A top-level negative number used to decode as the sequence `[5]`.
    assert_round_trip("yaml", &-5i64, yaml::encode, yaml::decode);
    assert_round_trip("yaml", &-2.5f64, yaml::encode, yaml::decode);
    assert_round_trip("yaml", &i64::MIN, yaml::encode, yaml::decode);
    // Top-level empty collections used to encode as `null`.
    assert_round_trip(
        "yaml",
        &BTreeMap::<String, i32>::new(),
        yaml::encode,
        yaml::decode,
    );
    assert_round_trip("yaml", &Vec::<i32>::new(), yaml::encode, yaml::decode);
    assert_eq!(yaml::encode(&BTreeMap::<String, i32>::new()), "{}\n");
    assert_eq!(yaml::encode(&Vec::<i32>::new()), "[]\n");
    // Nested and optional sequences.
    assert_round_trip(
        "yaml",
        &vec![vec![1, 2], vec![], vec![3]],
        yaml::encode,
        yaml::decode,
    );
    assert_round_trip(
        "yaml",
        &vec![None, Some(1), None],
        yaml::encode,
        yaml::decode,
    );
    assert_round_trip("yaml", &vec![None::<i32>], yaml::encode, yaml::decode);
    assert_round_trip("yaml", &vec![String::new()], yaml::encode, yaml::decode);
    assert_round_trip("yaml", &Some(String::new()), yaml::encode, yaml::decode);
    assert_round_trip("yaml", &None::<String>, yaml::encode, yaml::decode);
    assert_round_trip("yaml", &f64::INFINITY, yaml::encode, yaml::decode);
}

#[test]
fn xml_review_cases() {
    // The derive does not take generics, so spell the wrappers out.
    #[derive(Encode, Decode, PartialEq, Debug)]
    struct Text {
        v: String,
    }
    #[derive(Encode, Decode, PartialEq, Debug)]
    struct Grid {
        v: Vec<Vec<i32>>,
    }
    #[derive(Encode, Decode, PartialEq, Debug)]
    struct Maybe {
        v: Vec<Option<i32>>,
    }
    #[derive(Encode, Decode, PartialEq, Debug)]
    struct OptText {
        v: Option<String>,
    }
    #[derive(Encode, Decode, PartialEq, Debug)]
    struct OptList {
        v: Option<Vec<String>>,
    }
    #[derive(Encode, Decode, PartialEq, Debug)]
    struct Map {
        v: BTreeMap<String, String>,
    }
    // The empty string and padded strings.
    for s in ["", " ", " a ", "\ta", "a\n", "\r\n"] {
        assert_round_trip("xml", &Text { v: s.into() }, xml::encode, xml::decode);
    }
    // `[[1,2],[3]]` used to flatten to `[[1],[2],[3]]`.
    for grid in [
        vec![vec![1, 2], vec![3]],
        vec![vec![1, 2]],
        vec![vec![1]],
        vec![vec![]],
        vec![vec![], vec![]],
        vec![],
    ] {
        assert_round_trip("xml", &Grid { v: grid }, xml::encode, xml::decode);
    }
    // `vec![None]` used to come back as an empty vec.
    for maybe in [
        vec![None],
        vec![None, None],
        vec![Some(1)],
        vec![None, Some(1)],
        vec![Some(1), None],
        vec![],
    ] {
        assert_round_trip("xml", &Maybe { v: maybe }, xml::encode, xml::decode);
    }
    // `None`, `Some("")` and `Some(vec![])` are three different documents.
    for opt in [None, Some(String::new()), Some("x".to_string())] {
        assert_round_trip("xml", &OptText { v: opt }, xml::encode, xml::decode);
    }
    for opt in [
        None,
        Some(vec![]),
        Some(vec![String::new()]),
        Some(vec!["x".to_string()]),
    ] {
        assert_round_trip("xml", &OptList { v: opt }, xml::encode, xml::decode);
    }
    // Map keys that are not XML names, and the empty map.
    for keys in [vec![], vec!["a b"], vec!["", "1", "<x>", "a/b", "k"]] {
        let v = keys
            .into_iter()
            .map(|k| (k.to_string(), format!("<{k}>")))
            .collect();
        assert_round_trip("xml", &Map { v }, xml::encode, xml::decode);
    }
    // Top-level values of every kind.
    assert_round_trip("xml", &vec![1, 2, 3], xml::encode, xml::decode);
    assert_round_trip("xml", &Vec::<i32>::new(), xml::encode, xml::decode);
    assert_round_trip("xml", &vec![vec![1], vec![]], xml::encode, xml::decode);
    assert_round_trip("xml", &vec![None::<i32>], xml::encode, xml::decode);
    assert_round_trip("xml", &-5i64, xml::encode, xml::decode);
    assert_round_trip("xml", &String::new(), xml::encode, xml::decode);
    assert_round_trip("xml", &None::<String>, xml::encode, xml::decode);
    assert_round_trip("xml", &(1u8, "two".to_string()), xml::encode, xml::decode);
    assert_round_trip("xml", &("solo".to_string(),), xml::encode, xml::decode);
    assert_round_trip(
        "xml",
        &BTreeMap::<String, i32>::new(),
        xml::encode,
        xml::decode,
    );
}

#[test]
fn toml_review_cases() {
    #[derive(Encode, Decode, PartialEq, Debug)]
    struct T {
        text: String,
        key_like: BTreeMap<String, i64>,
        grid: Vec<Vec<i64>>,
        none: Option<i64>,
        some: Option<i64>,
        neg: i64,
        float: f64,
        whole_float: f64,
    }
    let value = T {
        text: "a: b # c \"q\" \\ \n\t\u{1}\u{7f}é".into(),
        key_like: [
            ("".to_string(), 1),
            ("a.b".to_string(), 2),
            ("a b".to_string(), 3),
            ("\"q\"".to_string(), 4),
            ("[t]".to_string(), 5),
            ("é".to_string(), 6),
            ("plain".to_string(), -7),
        ]
        .into_iter()
        .collect(),
        grid: vec![vec![1, 2], vec![], vec![3]],
        none: None,
        some: Some(-1),
        neg: i64::MIN,
        float: -0.5,
        whole_float: 3.0,
    };
    assert_round_trip("toml", &value, toml::encode, toml::decode);
    // A whole-number float keeps its `.0`, so it stays a float.
    assert!(toml::encode(&value).contains("whole_float = 3.0"));
}
