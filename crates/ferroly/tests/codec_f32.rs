#![cfg(feature = "codec")]
//! `f32` values encode as the shortest text that reads back as the same `f32`
//! (`0.1`, not the `0.10000000149011612` of its `f64` widening), in every
//! format, while `f64` output and custom encoders are unchanged.
//!
//! The property tests are seeded and dependency-free, like `codec_fuzz.rs`;
//! `FERROLY_FUZZ_ITERS` raises the sample count. The `#[ignore]`d test checks
//! all 2^32 bit patterns (run it in release:
//! `cargo test --release --test codec_f32 -- --ignored`).

use ferroly::codec::{json, toml, xml, yaml, CodecError, Decode, Encode, Encoder, Value};

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
}

fn iterations(default: usize) -> usize {
    std::env::var("FERROLY_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[derive(Encode, Decode, PartialEq, Debug)]
struct One {
    x: f32,
}

#[derive(Encode, Decode, PartialEq, Debug)]
struct Embedding {
    index: u32,
    embedding: Vec<f32>,
}

/// The issue's table: value, shortest text.
const TABLE: [(f32, &str); 3] = [
    (0.1, "0.1"),
    (-0.020566292, "-0.020566292"),
    (1.0e-7, "0.0000001"),
];

#[test]
fn shortest_text_in_json_yaml_toml_and_xml() {
    assert_eq!(json::encode(&0.1f32), "0.1");
    assert_eq!(Value::from(0.1f32), 0.1f32.encode());
    assert_eq!(Value::from(0.1f32), Value::Float(0.1));
    for (v, text) in TABLE {
        assert_eq!(json::encode(&v), text);
        assert_eq!(json::encode_pretty(&v), text);
        assert_eq!(json::to_string(&v.encode()), text, "value path");
        assert_eq!(json::encode(&One { x: v }), format!(r#"{{"x":{text}}}"#));
        assert_eq!(yaml::encode(&One { x: v }).trim_end(), format!("x: {text}"));
        assert_eq!(
            toml::encode(&One { x: v }).trim_end(),
            format!("x = {text}")
        );
        assert!(
            xml::encode(&One { x: v }).contains(&format!("<x>{text}</x>")),
            "{}",
            xml::encode(&One { x: v })
        );
        // Each reads back as the same value.
        assert_eq!(json::decode::<f32>(text).unwrap(), v);
        assert_eq!(yaml::decode::<One>(&format!("x: {text}\n")).unwrap().x, v);
        assert_eq!(toml::decode::<One>(&format!("x = {text}\n")).unwrap().x, v);
    }
    // Integer-valued floats keep a `.0` so they read back as floats.
    assert_eq!(json::encode(&250.0f32), "250.0");
    assert_eq!(json::encode(&-0.0f32), "-0.0");
    assert_eq!(toml::encode(&One { x: 3.0 }).trim_end(), "x = 3.0");
}

#[test]
fn the_embedding_example() {
    let e = Embedding {
        index: 0,
        embedding: vec![0.1, -0.020566292, 1.0e-7],
    };
    assert_eq!(
        json::encode(&e),
        r#"{"index":0,"embedding":[0.1,-0.020566292,0.0000001]}"#
    );
    let back: Vec<f32> = json::decode("[0.1,-0.020566292,0.0000001]").unwrap();
    assert_eq!(back, e.embedding);
    // The long form still decodes to the same value.
    let long: Vec<f32> = json::decode("[0.10000000149011612]").unwrap();
    assert_eq!(long, vec![0.1]);

    // As shown in docs/codec.md.
    #[derive(Encode)]
    struct Only {
        embedding: Vec<f32>,
    }
    let e = Only {
        embedding: vec![0.1, -0.020566292],
    };
    assert_eq!(json::encode(&e), r#"{"embedding":[0.1,-0.020566292]}"#);
    assert_eq!(yaml::encode(&e), "embedding:\n  - 0.1\n  - -0.020566292\n");
    assert_eq!(toml::encode(&e), "embedding = [0.1, -0.020566292]\n");
}

/// Encodes `v` to JSON on both paths and checks it reads back bit-for-bit
/// and is no longer than the `f64` widening's text.
fn check_json(v: f32) {
    let text = json::encode(&v);
    let back: f32 = json::decode(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
    assert_eq!(back.to_bits(), v.to_bits(), "{v:?} -> {text}");
    assert_eq!(json::to_string(&v.encode()), text, "value path for {v:?}");
    let value_back = f32::decode(&v.encode()).unwrap();
    assert_eq!(value_back.to_bits(), v.to_bits(), "Value path for {v:?}");
    assert!(text.len() <= json::encode(&f64::from(v)).len(), "{text}");
}

/// The edges: zeros, subnormals, the normal boundary, the extremes, and
/// their neighbours.
fn edge_cases() -> Vec<f32> {
    let mut out = vec![
        0.0,
        -0.0,
        f32::MIN_POSITIVE,
        f32::MAX,
        f32::MIN,
        f32::EPSILON,
        1.0,
        16_777_216.0,
        16_777_217.0,
    ];
    for bits in [
        1u32,
        2,
        3,
        0x0000_0400,
        0x007F_FFFE,
        0x007F_FFFF,
        0x0080_0001,
        // 7.038531e-26: its shortest text reads back as an `f64` exactly
        // halfway between two `f32`s (the one double-rounding case the
        // exhaustive test found), so it is written in its exact form.
        0x15AE_43FD,
    ] {
        out.push(f32::from_bits(bits));
    }
    for v in [f32::MIN_POSITIVE, f32::MAX, 1.0, 0.1] {
        out.push(f32::from_bits(v.to_bits() - 1));
        out.push(f32::from_bits(v.to_bits() + 1).min(f32::MAX));
    }
    let negated: Vec<f32> = out.iter().map(|v| -v).collect();
    out.extend(negated);
    out
}

#[test]
fn edge_values_round_trip_exactly() {
    for v in edge_cases() {
        check_json(v);
    }
    // The double-rounding case keeps its exact (`f64`) text on both paths.
    let odd = f32::from_bits(0x15AE_43FD);
    assert_eq!(json::encode(&odd), format!("{}", f64::from(odd)));
    assert_eq!(odd.encode(), Value::Float(f64::from(odd)));
    assert_ne!(json::encode(&odd), format!("{odd}"));
    // Every subnormal magnitude below 2^-140 (the shortest texts are longest
    // here), exhaustively.
    for bits in 1u32..0x0000_2000 {
        check_json(f32::from_bits(bits));
    }
}

#[test]
fn random_bit_patterns_round_trip_exactly() {
    let mut rng = Rng(0xF32F_32F3_2F32_F32F);
    let mut checked = 0;
    let target = iterations(200_000);
    while checked < target {
        let v = f32::from_bits(rng.next_u64() as u32);
        if v.is_finite() {
            check_json(v);
            checked += 1;
        }
    }
}

#[test]
fn random_values_round_trip_through_yaml_and_toml() {
    let mut rng = Rng(7);
    let mut checked = 0;
    while checked < iterations(5_000) {
        let x = f32::from_bits(rng.next_u64() as u32);
        if !x.is_finite() {
            continue;
        }
        let one = One { x };
        let y = yaml::encode(&one);
        assert_eq!(
            yaml::decode::<One>(&y).unwrap().x.to_bits(),
            x.to_bits(),
            "{y}"
        );
        let t = toml::encode(&one);
        assert_eq!(
            toml::decode::<One>(&t).unwrap().x.to_bits(),
            x.to_bits(),
            "{t}"
        );
        let xm = xml::encode(&one);
        assert_eq!(
            xml::decode::<One>(&xm).unwrap().x.to_bits(),
            x.to_bits(),
            "{xm}"
        );
        checked += 1;
    }
}

#[test]
#[ignore = "exhaustive over all 2^32 bit patterns; run in release"]
fn every_finite_f32_round_trips_exactly() {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()) as u64;
    let span = (1u64 << 32) / threads + 1;
    std::thread::scope(|s| {
        for t in 0..threads {
            s.spawn(move || {
                let start = t * span;
                let end = ((t + 1) * span).min(1 << 32);
                for bits in start..end {
                    let v = f32::from_bits(bits as u32);
                    if v.is_finite() {
                        let text = json::encode(&v);
                        let back: f32 = json::decode(&text).unwrap();
                        assert_eq!(back.to_bits(), v.to_bits(), "{text}");
                        let value_back = f32::decode(&v.encode()).unwrap();
                        assert_eq!(value_back.to_bits(), v.to_bits(), "{text}");
                    }
                }
            });
        }
    });
}

#[test]
fn non_finite_values_follow_the_existing_rules() {
    for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let wide = f64::from(v);
        // JSON: `null` from `encode`, an error from `try_encode`.
        assert_eq!(json::encode(&v), "null");
        assert_eq!(json::encode(&v), json::encode(&wide));
        assert!(matches!(
            json::try_encode(&v),
            Err(CodecError::Unrepresentable(_))
        ));
        assert!(matches!(
            json::try_encode(&vec![1.0f32, v]),
            Err(CodecError::Unrepresentable(_))
        ));
        // The `Value` path widens them unchanged.
        let value = v.encode();
        match value {
            Value::Float(f) => assert!(f.to_bits() == wide.to_bits() || (f.is_nan() && v.is_nan())),
            other => panic!("{other:?}"),
        }
        // YAML, TOML and XML spell them exactly as they spell an `f64`.
        assert_eq!(yaml::encode(&One { x: v }), yaml::encode(&Wide { x: wide }));
        assert_eq!(toml::encode(&One { x: v }), toml::encode(&Wide { x: wide }));
        assert_eq!(xml::encode(&One { x: v }), xml::encode(&Wide { x: wide }));
    }
    assert_eq!(
        yaml::encode(&One { x: f32::INFINITY }).trim_end(),
        "x: .inf"
    );
    assert_eq!(
        yaml::encode(&One {
            x: f32::NEG_INFINITY
        })
        .trim_end(),
        "x: -.inf"
    );
    assert_eq!(yaml::encode(&One { x: f32::NAN }).trim_end(), "x: .nan");
    assert!(json::try_encode(&0.1f32).is_ok());
}

#[derive(Encode, PartialEq, Debug)]
struct Wide {
    x: f64,
}

#[test]
fn f64_output_is_unchanged() {
    // An `f64` still prints its own shortest form, so the widened `f32`
    // prints long, exactly as before.
    assert_eq!(json::encode(&0.1f64), "0.1");
    assert_eq!(json::encode(&f64::from(0.1f32)), "0.10000000149011612");
    assert_eq!(
        json::encode(&f64::from(1.0e-7f32)),
        "0.00000010000000116860974"
    );
    assert_eq!(json::encode(&250.0f64), "250.0");
    assert_eq!(json::encode(&f64::MAX), format!("{}.0", f64::MAX));
    assert_eq!(f64::from(0.1f32).encode(), Value::Float(f64::from(0.1f32)));
    assert_eq!(
        yaml::encode(&Wide {
            x: f64::from(0.1f32)
        })
        .trim_end(),
        "x: 0.10000000149011612"
    );
    let mut rng = Rng(64);
    for _ in 0..iterations(20_000) {
        let v = f64::from_bits(rng.next_u64());
        if v.is_finite() {
            let mut expected = format!("{v}");
            if !expected.contains(['.', 'e', 'E']) {
                expected.push_str(".0");
            }
            assert_eq!(json::encode(&v), expected);
        }
    }
}

/// A custom encoder that implements only `encode_f64`.
#[derive(Default)]
struct Recorder {
    floats: Vec<f64>,
}

impl Encoder for Recorder {
    fn encode_null(&mut self) {}
    fn encode_bool(&mut self, _: bool) {}
    fn encode_i64(&mut self, _: i64) {}
    fn encode_u64(&mut self, _: u64) {}
    fn encode_f64(&mut self, v: f64) {
        self.floats.push(v);
    }
    fn encode_str(&mut self, _: &str) {}
    fn encode_bytes(&mut self, _: &[u8]) {}
    fn begin_seq(&mut self, _: usize) {}
    fn seq_entry<V: Encode + ?Sized>(&mut self, v: &V) {
        v.encode_to(self);
    }
    fn end_seq(&mut self) {}
    fn begin_map(&mut self, _: usize) {}
    fn map_entry<V: Encode + ?Sized>(&mut self, _: &str, v: &V) {
        v.encode_to(self);
    }
    fn end_map(&mut self) {}
}

#[test]
fn custom_encoders_without_encode_f32_are_unchanged() {
    let mut r = Recorder::default();
    Embedding {
        index: 0,
        embedding: vec![0.1, -0.020566292],
    }
    .encode_to(&mut r);
    0.5f64.encode_to(&mut r);
    // The exact widening, as `encode_f64(*self as f64)` gave before.
    assert_eq!(
        r.floats,
        vec![f64::from(0.1f32), f64::from(-0.020566292f32), 0.5]
    );
}
