#![cfg(feature = "protobuf")]
//! Hostile protobuf input: truncation, overlong varints, depth bombs, huge
//! lengths and budget exhaustion must be errors — never panics, stack
//! overflows or allocations sized by the input. Plus a seeded "fuzz-lite"
//! loop (the coverage-guided counterpart is `fuzz/fuzz_targets/proto_decode.rs`).

use std::collections::BTreeMap;

use ferroly::proto::{EnumValue, Limits, Message, ProtoError, UnknownFields};

#[derive(ferroly::proto::Enumeration, Clone, Copy, PartialEq, Debug)]
enum Kind {
    A = 0,
    B = 1,
}

#[derive(ferroly::proto::Oneof, Clone, PartialEq, Debug)]
enum Pick {
    #[proto(tag = 20)]
    Text(String),
    #[proto(tag = 21, sint64)]
    Num(i64),
    #[proto(tag = 22)]
    Node(Box<Everything>),
}

/// One field of every shape, recursive through `child`, `children`, the map
/// values and the oneof.
#[derive(ferroly::proto::Message, Clone, PartialEq, Debug, Default)]
struct Everything {
    #[proto(tag = 1)]
    i: i32,
    #[proto(tag = 2, sint64)]
    s: i64,
    #[proto(tag = 3, fixed32)]
    f: u32,
    #[proto(tag = 4)]
    d: f64,
    #[proto(tag = 5)]
    b: bool,
    #[proto(tag = 6)]
    text: String,
    #[proto(tag = 7)]
    raw: Vec<u8>,
    #[proto(tag = 8)]
    kind: EnumValue<Kind>,
    #[proto(tag = 9)]
    packed: Vec<u64>,
    #[proto(tag = 10, packed = false)]
    unpacked: Vec<i32>,
    #[proto(tag = 11)]
    names: Vec<String>,
    #[proto(tag = 12)]
    child: Option<Box<Everything>>,
    #[proto(tag = 13)]
    children: Vec<Everything>,
    #[proto(tag = 14)]
    map: BTreeMap<String, Everything>,
    #[proto(tag = 15, map(key = "sint32", value = "fixed64"))]
    nums: BTreeMap<i32, u64>,
    #[proto(tag = 16)]
    maybe: Option<f32>,
    #[proto(oneof)]
    pick: Option<Pick>,
    #[proto(unknown)]
    unknown: UnknownFields,
}

fn sample() -> Everything {
    let mut map = BTreeMap::new();
    map.insert(
        "k".to_string(),
        Everything {
            i: 3,
            ..Default::default()
        },
    );
    let mut nums = BTreeMap::new();
    nums.insert(-2, 9);
    Everything {
        i: -7,
        s: -8,
        f: 9,
        d: 1.25,
        b: true,
        text: "héllo".into(),
        raw: vec![0, 255],
        kind: Kind::B.into(),
        packed: vec![1, 300, u64::MAX],
        unpacked: vec![-1, 2],
        names: vec!["a".into(), String::new()],
        child: Some(Box::new(Everything {
            text: "inner".into(),
            pick: Some(Pick::Num(-5)),
            ..Default::default()
        })),
        children: vec![
            Everything::default(),
            Everything {
                b: true,
                ..Default::default()
            },
        ],
        map,
        nums,
        maybe: Some(-0.5),
        pick: Some(Pick::Node(Box::new(Everything {
            i: 1,
            ..Default::default()
        }))),
        unknown: UnknownFields::new(),
    }
}

/// Wraps `inner` as field 12 (`child`) of an outer message, `levels` times.
fn nested(levels: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    for _ in 0..levels {
        let mut outer = vec![0x62];
        ferroly::proto::wire::encode_varint(bytes.len() as u64, &mut outer);
        outer.extend_from_slice(&bytes);
        bytes = outer;
    }
    bytes
}

#[test]
fn every_truncation_is_an_error_or_a_shorter_message() {
    let bytes = sample().encode_to_vec();
    assert_eq!(Everything::decode(&bytes).unwrap(), sample());
    for end in 0..bytes.len() {
        // Truncation can land on a field boundary (a valid, shorter
        // message) or inside one (an error); it must never panic.
        if let Ok(m) = Everything::decode(&bytes[..end]) {
            assert!(m.encoded_len() <= bytes.len());
        }
    }
}

#[test]
fn overlong_varints_are_rejected() {
    let mut eleven = vec![0x08];
    eleven.extend_from_slice(&[0xff; 10]);
    eleven.push(0x01);
    assert_eq!(Everything::decode(&eleven), Err(ProtoError::VarintOverflow));
    // An overlong key.
    assert_eq!(
        Everything::decode(&[0xff; 11]),
        Err(ProtoError::VarintOverflow)
    );
    // A key above field number 2^29-1.
    assert!(matches!(
        Everything::decode(&[0xf8, 0xff, 0xff, 0xff, 0x7f, 0x00]),
        Err(ProtoError::InvalidTag(_))
    ));
}

#[test]
fn depth_bombs_stop_at_the_limit() {
    assert!(Everything::decode(&nested(100)).is_ok());
    assert_eq!(
        Everything::decode(&nested(101)),
        Err(ProtoError::DepthExceeded(100))
    );
    assert_eq!(
        Everything::decode(&nested(5000)),
        Err(ProtoError::DepthExceeded(100))
    );
    let tight = Limits::new().with_max_depth(3);
    assert_eq!(
        Everything::decode_with(&nested(4), tight),
        Err(ProtoError::DepthExceeded(3))
    );

    // Through map values and oneof members too.
    let mut bytes = Vec::new();
    for i in 0..200 {
        let mut outer = Vec::new();
        if i % 2 == 0 {
            // map entry: field 14 { key = 1: "", value = 2: bytes }
            let mut entry = vec![0x0a, 0x00, 0x12];
            ferroly::proto::wire::encode_varint(bytes.len() as u64, &mut entry);
            entry.extend_from_slice(&bytes);
            outer.push(0x72);
            ferroly::proto::wire::encode_varint(entry.len() as u64, &mut outer);
            outer.extend_from_slice(&entry);
        } else {
            outer.extend_from_slice(&[0xb2, 0x01]); // field 22 (oneof Node)
            ferroly::proto::wire::encode_varint(bytes.len() as u64, &mut outer);
            outer.extend_from_slice(&bytes);
        }
        bytes = outer;
    }
    assert!(matches!(
        Everything::decode(&bytes),
        Err(ProtoError::DepthExceeded(_))
    ));

    // Unknown groups nested without end.
    let groups = [0xfbu8, 0x01]; // field 31, start group
    let bomb: Vec<u8> = groups.iter().copied().cycle().take(20_000).collect();
    assert!(matches!(
        Everything::decode(&bomb),
        Err(ProtoError::DepthExceeded(_))
    ));
}

#[test]
fn huge_lengths_do_not_allocate() {
    let huge = [0xff, 0xff, 0xff, 0xff, 0x0f];
    for key in [0x32u8, 0x3a, 0x4a, 0x5a, 0x62, 0x72, 0x7a] {
        let mut bytes = vec![key];
        bytes.extend_from_slice(&huge);
        bytes.extend_from_slice(b"short");
        assert!(
            matches!(
                Everything::decode(&bytes),
                Err(ProtoError::LengthOutOfBounds { remaining: 5, .. })
            ),
            "key {key:#x}"
        );
    }
    // A length larger than the enclosing message, though smaller than the
    // whole input.
    let bytes = [0x62, 0x02, 0x32, 0x05, b'a', b'b', b'c', b'd', b'e'];
    assert!(matches!(
        Everything::decode(&bytes),
        Err(ProtoError::LengthOutOfBounds { .. })
    ));
}

#[test]
fn budgets_bound_expansion() {
    // 50k empty children: 100 KB of input, 50k structs in memory.
    let bytes: Vec<u8> = [0x6a, 0x00].iter().copied().cycle().take(100_000).collect();
    assert_eq!(Everything::decode(&bytes).unwrap().children.len(), 50_000);
    let limits = Limits::new().with_max_entries(1_000);
    assert!(matches!(
        Everything::decode_with(&bytes, limits),
        Err(ProtoError::LimitExceeded(_))
    ));

    // Packed elements count too.
    let mut packed = vec![0x4a, 0xa0, 0x8d, 0x06]; // field 9, 100_000 bytes
    packed.extend(std::iter::repeat(0x01).take(100_000));
    assert_eq!(Everything::decode(&packed).unwrap().packed.len(), 100_000);
    assert!(matches!(
        Everything::decode_with(&packed, limits),
        Err(ProtoError::LimitExceeded(_))
    ));

    let small = Limits::new().with_max_input_bytes(10);
    assert!(matches!(
        Everything::decode_with(&bytes, small),
        Err(ProtoError::LimitExceeded(_))
    ));
}

#[test]
fn structural_garbage_is_rejected() {
    // Wire types 6 and 7.
    assert!(matches!(
        Everything::decode(&[0x0e]),
        Err(ProtoError::InvalidWireType { tag: 1, bits: 6 })
    ));
    assert!(matches!(
        Everything::decode(&[0x0f]),
        Err(ProtoError::InvalidWireType { tag: 1, bits: 7 })
    ));
    // Field number 0.
    assert_eq!(
        Everything::decode(&[0x00, 0x00]),
        Err(ProtoError::InvalidTag(0))
    );
    // A stray end-group key.
    assert_eq!(
        Everything::decode(&[0x0c]),
        Err(ProtoError::UnexpectedEndGroup(1))
    );
    // Invalid UTF-8 in a string, in a repeated string and in a map key.
    assert_eq!(
        Everything::decode(&[0x32, 0x01, 0xff]),
        Err(ProtoError::InvalidUtf8)
    );
    assert_eq!(
        Everything::decode(&[0x5a, 0x01, 0xc0]),
        Err(ProtoError::InvalidUtf8)
    );
    assert_eq!(
        Everything::decode(&[0x72, 0x03, 0x0a, 0x01, 0xff]),
        Err(ProtoError::InvalidUtf8)
    );
    // A truncated fixed-width value.
    assert_eq!(
        Everything::decode(&[0x1d, 0x01, 0x02]),
        Err(ProtoError::Truncated)
    );
}

// ---- fuzz-lite -----------------------------------------------------------------

/// SplitMix64, as in `codec_fuzz.rs`.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn iterations() -> usize {
    std::env::var("FERROLY_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000)
}

/// Decoding must not panic; whatever decodes must re-encode to bytes that
/// decode and re-encode identically.
fn check_input(data: &[u8]) {
    let limits = Limits::new().with_max_entries(100_000);
    if let Ok(m) = Everything::decode_with(data, limits) {
        let once = m.encode_to_vec();
        assert_eq!(once.len(), m.encoded_len());
        let again = Everything::decode(&once).expect("re-encoded bytes decode");
        assert_eq!(
            again.encode_to_vec(),
            once,
            "re-encoding is not stable for {data:02x?}"
        );
    }
}

#[test]
fn fuzz_lite_random_bytes() {
    let mut rng = Rng(0x5eed_0001);
    // Bytes that are likely keys, lengths and continuation bits.
    const INTERESTING: &[u8] = &[
        0x08, 0x10, 0x1d, 0x21, 0x28, 0x32, 0x3a, 0x40, 0x4a, 0x50, 0x5a, 0x62, 0x6a, 0x72, 0x7a,
        0x85, 0x01, 0xa2, 0xaa, 0xb2, 0x0b, 0x0c, 0x80, 0xff, 0x00, 0x7f,
    ];
    for _ in 0..iterations() {
        let len = rng.below(64);
        let data: Vec<u8> = (0..len)
            .map(|_| {
                if rng.below(2) == 0 {
                    INTERESTING[rng.below(INTERESTING.len())]
                } else {
                    rng.next() as u8
                }
            })
            .collect();
        check_input(&data);
    }
}

#[test]
fn fuzz_lite_mutated_messages() {
    let mut rng = Rng(0x5eed_0002);
    let seed = sample().encode_to_vec();
    for _ in 0..iterations() {
        let mut v = seed.clone();
        for _ in 0..1 + rng.below(4) {
            if v.is_empty() {
                break;
            }
            let i = rng.below(v.len());
            match rng.below(4) {
                0 => v[i] = rng.next() as u8,
                1 => v.truncate(i),
                2 => {
                    let b = v[i];
                    v.insert(i, b);
                }
                _ => {
                    v.remove(i);
                }
            }
        }
        check_input(&v);
    }
}

// ---- required-field tracking stays linear ----------------------------------

#[derive(ferroly::proto::Message, Default, Debug, PartialEq)]
struct Sub {
    #[proto(tag = 1, required)]
    x: i32,
}

#[derive(ferroly::proto::Oneof, PartialEq, Debug)]
enum Switch {
    #[proto(tag = 3)]
    I(i32),
    #[proto(tag = 4)]
    J(i32),
}

#[derive(ferroly::proto::Message, Default, Debug, PartialEq)]
struct Alternating {
    #[proto(tag = 1)]
    a: Option<Sub>,
    #[proto(tag = 2)]
    b: Option<Sub>,
    #[proto(oneof)]
    k: Option<Switch>,
}

#[test]
fn alternating_required_messages_and_oneof_switches_stay_linear() {
    // 1M occurrences of two singular sub-messages with a required field,
    // alternating, then 1M oneof switches: each switch once scanned every
    // occurrence recorded (about 20 minutes of CPU); now a handful of records.
    let mut bytes = [0x0a, 0x02, 0x08, 0x01, 0x12, 0x02, 0x08, 0x01].repeat(500_000);
    bytes.extend([0x18, 0x00, 0x20, 0x00].repeat(500_000));
    let limits = Limits::new()
        .with_max_input_bytes(8 << 20)
        .with_max_decoded_bytes(64 << 20);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(Alternating::decode_with(&bytes, limits).map(|m| m.k));
    });
    // Generous for a debug build on a loaded runner (it takes about a
    // second); the old code took about 20 minutes in release.
    let decoded = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("decoding took over 10 s");
    assert_eq!(decoded, Ok(Some(Switch::J(0))));
}
