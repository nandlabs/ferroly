#![cfg(feature = "protobuf")]
//! Conformance-style byte vectors for the protobuf wire format, most taken
//! from the protobuf encoding guide, checked in both directions against
//! derived messages.

use std::collections::{BTreeMap, HashMap};

use ferroly::proto::{
    wire, EnumValue, Enumeration, Message, ProtoError, UnknownFields, UnknownValue,
};

fn hex(s: &str) -> Vec<u8> {
    let s: String = s.split_whitespace().collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// Encodes `m`, checks the bytes and `encoded_len`, and decodes them back.
fn check<M: Message + PartialEq + std::fmt::Debug>(m: &M, expected: &str) {
    let bytes = m.encode_to_vec();
    assert_eq!(bytes, hex(expected), "encoding of {m:?}");
    assert_eq!(m.encoded_len(), bytes.len(), "encoded_len of {m:?}");
    assert_eq!(&M::decode(&bytes).unwrap(), m, "decoding {expected}");
}

#[derive(Message, Default, PartialEq, Debug)]
struct Test1 {
    #[proto(tag = 1)]
    a: i32,
}

#[derive(Message, Default, PartialEq, Debug)]
struct Test2 {
    #[proto(tag = 2)]
    b: String,
}

#[derive(Message, Default, PartialEq, Debug)]
struct Test3 {
    #[proto(tag = 3)]
    c: Option<Test1>,
}

#[derive(Message, Default, PartialEq, Debug)]
struct Test4 {
    #[proto(tag = 1)]
    d: Option<String>,
    #[proto(tag = 6)]
    e: Vec<i32>,
}

#[derive(Message, Default, PartialEq, Debug)]
struct Unpacked {
    #[proto(tag = 6, packed = false)]
    e: Vec<i32>,
}

#[test]
fn encoding_guide_vectors() {
    check(&Test1 { a: 150 }, "08 96 01");
    check(
        &Test2 {
            b: "testing".into(),
        },
        "12 07 74 65 73 74 69 6e 67",
    );
    check(
        &Test3 {
            c: Some(Test1 { a: 150 }),
        },
        "1a 03 08 96 01",
    );
    check(
        &Test4 {
            d: Some("hello".into()),
            e: vec![3, 270, 86942],
        },
        "0a 05 68 65 6c 6c 6f 32 06 03 8e 02 9e a7 05",
    );
    check(&Unpacked { e: vec![1, 2, 3] }, "30 01 30 02 30 03");
}

#[test]
fn defaults_are_omitted_but_present_options_are_not() {
    check(&Test1 { a: 0 }, "");
    check(&Test2::default(), "");
    check(
        &Test3 {
            c: Some(Test1 { a: 0 }),
        },
        "1a 00",
    );
    check(
        &Test4 {
            d: Some(String::new()),
            e: vec![],
        },
        "0a 00",
    );
}

#[test]
fn negative_int32_is_a_ten_byte_varint() {
    check(&Test1 { a: -2 }, "08 fe ff ff ff ff ff ff ff ff 01");
    check(&Test1 { a: i32::MIN }, "08 80 80 80 80 f8 ff ff ff ff 01");
    // A writer that sign-extended only to 32 bits is still read correctly.
    assert_eq!(Test1::decode(&hex("08 fe ff ff ff 0f")).unwrap().a, -2);
}

#[derive(Message, Default, PartialEq, Debug)]
struct Scalars {
    #[proto(tag = 1, sint32)]
    s32: i32,
    #[proto(tag = 2, sint64)]
    s64: i64,
    #[proto(tag = 3, fixed32)]
    f32_: u32,
    #[proto(tag = 4, fixed64)]
    f64_: u64,
    #[proto(tag = 5, sfixed32)]
    sf32: i32,
    #[proto(tag = 6, sfixed64)]
    sf64: i64,
    #[proto(tag = 7)]
    float: f32,
    #[proto(tag = 8)]
    double: f64,
    #[proto(tag = 9)]
    flag: bool,
    #[proto(tag = 10)]
    u32_: u32,
    #[proto(tag = 11)]
    u64_: u64,
    #[proto(tag = 12)]
    i64_: i64,
    #[proto(tag = 13)]
    raw: Vec<u8>,
}

#[test]
fn zigzag_and_fixed_width_scalars() {
    check(
        &Scalars {
            s32: -1,
            ..Default::default()
        },
        "08 01",
    );
    check(
        &Scalars {
            s32: 1,
            ..Default::default()
        },
        "08 02",
    );
    check(
        &Scalars {
            s32: -2,
            ..Default::default()
        },
        "08 03",
    );
    check(
        &Scalars {
            s32: i32::MIN,
            ..Default::default()
        },
        "08 ff ff ff ff 0f",
    );
    check(
        &Scalars {
            s64: i64::MIN,
            ..Default::default()
        },
        "10 ff ff ff ff ff ff ff ff ff 01",
    );
    check(
        &Scalars {
            f32_: 1,
            ..Default::default()
        },
        "1d 01 00 00 00",
    );
    check(
        &Scalars {
            f64_: 1,
            ..Default::default()
        },
        "21 01 00 00 00 00 00 00 00",
    );
    check(
        &Scalars {
            sf32: -1,
            ..Default::default()
        },
        "2d ff ff ff ff",
    );
    check(
        &Scalars {
            sf64: -2,
            ..Default::default()
        },
        "31 fe ff ff ff ff ff ff ff",
    );
    check(
        &Scalars {
            float: 1.0,
            ..Default::default()
        },
        "3d 00 00 80 3f",
    );
    check(
        &Scalars {
            double: 1.0,
            ..Default::default()
        },
        "41 00 00 00 00 00 00 f0 3f",
    );
    check(
        &Scalars {
            flag: true,
            ..Default::default()
        },
        "48 01",
    );
    check(
        &Scalars {
            u32_: u32::MAX,
            ..Default::default()
        },
        "50 ff ff ff ff 0f",
    );
    check(
        &Scalars {
            u64_: u64::MAX,
            ..Default::default()
        },
        "58 ff ff ff ff ff ff ff ff ff 01",
    );
    check(
        &Scalars {
            i64_: -1,
            ..Default::default()
        },
        "60 ff ff ff ff ff ff ff ff ff 01",
    );
    check(
        &Scalars {
            raw: vec![0, 0xff],
            ..Default::default()
        },
        "6a 02 00 ff",
    );
    // -0.0 is not the default and is written; +0.0 is omitted.
    check(
        &Scalars {
            double: -0.0,
            ..Default::default()
        },
        "41 00 00 00 00 00 00 00 80",
    );
    // Any non-zero varint is `true`.
    assert!(Scalars::decode(&hex("48 05")).unwrap().flag);
    // A 64-bit varint for a 32-bit field is truncated, as protoc does.
    assert_eq!(
        Scalars::decode(&hex("50 ff ff ff ff ff 01")).unwrap().u32_,
        u32::MAX
    );
}

#[derive(Message, Default, PartialEq, Debug)]
struct Optionals {
    #[proto(tag = 1)]
    a: Option<i32>,
    #[proto(tag = 2, sint64)]
    b: Option<i64>,
    #[proto(tag = 3)]
    c: Option<bool>,
}

#[test]
fn explicit_presence_writes_defaults() {
    check(
        &Optionals {
            a: Some(0),
            b: None,
            c: Some(false),
        },
        "08 00 18 00",
    );
    check(
        &Optionals {
            a: None,
            b: Some(-1),
            c: None,
        },
        "10 01",
    );
    check(&Optionals::default(), "");
}

#[derive(Message, Default, PartialEq, Debug)]
struct Repeated {
    #[proto(tag = 1)]
    ints: Vec<i32>,
    #[proto(tag = 2, sint32)]
    zz: Vec<i32>,
    #[proto(tag = 3, fixed32)]
    fixed: Vec<u32>,
    #[proto(tag = 4)]
    doubles: Vec<f64>,
    #[proto(tag = 5)]
    names: Vec<String>,
    #[proto(tag = 6)]
    blobs: Vec<Vec<u8>>,
    #[proto(tag = 7)]
    items: Vec<Test1>,
    #[proto(tag = 8)]
    flags: Vec<bool>,
}

#[test]
fn packed_and_length_delimited_repeated_fields() {
    check(
        &Repeated {
            ints: vec![1, -1],
            ..Default::default()
        },
        "0a 0b 01 ff ff ff ff ff ff ff ff ff 01",
    );
    check(
        &Repeated {
            zz: vec![-1, 1],
            ..Default::default()
        },
        "12 02 01 02",
    );
    check(
        &Repeated {
            fixed: vec![1, 2],
            ..Default::default()
        },
        "1a 08 01 00 00 00 02 00 00 00",
    );
    check(
        &Repeated {
            doubles: vec![1.0],
            ..Default::default()
        },
        "22 08 00 00 00 00 00 00 f0 3f",
    );
    check(
        &Repeated {
            names: vec!["a".into(), String::new()],
            ..Default::default()
        },
        "2a 01 61 2a 00",
    );
    check(
        &Repeated {
            blobs: vec![vec![1]],
            ..Default::default()
        },
        "32 01 01",
    );
    check(
        &Repeated {
            items: vec![Test1 { a: 1 }, Test1::default()],
            ..Default::default()
        },
        "3a 02 08 01 3a 00",
    );
    check(
        &Repeated {
            flags: vec![true, false],
            ..Default::default()
        },
        "42 02 01 00",
    );
    // An empty packed field is not written at all.
    check(&Repeated::default(), "");
}

#[test]
fn packed_and_unpacked_are_both_accepted() {
    // Unpacked input for a packed field…
    let r = Repeated::decode(&hex("08 01 08 02 0a 01 03")).unwrap();
    assert_eq!(r.ints, [1, 2, 3]);
    // …and packed input for an unpacked one.
    let u = Unpacked::decode(&hex("32 02 01 02 30 03")).unwrap();
    assert_eq!(u.e, [1, 2, 3]);
    // A packed fixed32 run that is not a multiple of four is truncated.
    assert_eq!(
        Repeated::decode(&hex("1a 03 01 00 00")),
        Err(ProtoError::Truncated)
    );
}

#[test]
fn last_scalar_wins_and_messages_merge() {
    assert_eq!(Test1::decode(&hex("08 01 08 02")).unwrap().a, 2);

    #[derive(Message, Default, PartialEq, Debug)]
    struct Pair {
        #[proto(tag = 1)]
        x: i32,
        #[proto(tag = 2)]
        y: i32,
    }
    #[derive(Message, Default, PartialEq, Debug)]
    struct Outer {
        #[proto(tag = 1)]
        p: Option<Pair>,
        #[proto(tag = 2)]
        plain: Pair,
    }
    // Field 1 twice: {x: 1} then {y: 2} merge into {x: 1, y: 2}.
    let o = Outer::decode(&hex("0a 02 08 01 0a 02 10 02 12 02 08 05 12 02 10 06")).unwrap();
    assert_eq!(o.p, Some(Pair { x: 1, y: 2 }));
    assert_eq!(o.plain, Pair { x: 5, y: 6 });
    // A non-optional message field is always written, even when empty.
    check(&Outer::default(), "12 00");

    let mut m = Test4::decode(&hex("32 01 01")).unwrap();
    m.merge(&hex("32 01 02 0a 01 61")).unwrap();
    assert_eq!(
        m,
        Test4 {
            d: Some("a".into()),
            e: vec![1, 2]
        }
    );
}

#[derive(Enumeration, Clone, Copy, PartialEq, Debug)]
enum Corpus {
    #[proto(name = "CORPUS_UNSPECIFIED")]
    Unspecified = 0,
    #[proto(name = "CORPUS_WEB")]
    Web = 1,
    #[proto(name = "CORPUS_IMAGES")]
    Images = 2,
    #[proto(name = "CORPUS_NEGATIVE")]
    Negative = -1,
}

#[derive(Message, Default, PartialEq, Debug)]
struct WithEnum {
    #[proto(tag = 1)]
    corpus: EnumValue<Corpus>,
    #[proto(tag = 2)]
    list: Vec<EnumValue<Corpus>>,
    #[proto(tag = 3)]
    maybe: Option<EnumValue<Corpus>>,
}

#[test]
fn open_enums_keep_unknown_numbers() {
    check(
        &WithEnum {
            corpus: Corpus::Images.into(),
            ..Default::default()
        },
        "08 02",
    );
    check(
        &WithEnum {
            corpus: Corpus::Negative.into(),
            ..Default::default()
        },
        "08 ff ff ff ff ff ff ff ff ff 01",
    );
    check(
        &WithEnum {
            list: vec![Corpus::Web.into(), EnumValue::from_raw(9)],
            ..Default::default()
        },
        "12 02 01 09",
    );
    check(
        &WithEnum {
            maybe: Some(Corpus::Unspecified.into()),
            ..Default::default()
        },
        "18 00",
    );

    let m = WithEnum::decode(&hex("08 63")).unwrap();
    assert_eq!(m.corpus.raw(), 99);
    assert_eq!(m.corpus.known(), None);
    assert_eq!(m.encode_to_vec(), hex("08 63"));

    assert_eq!(
        Corpus::from_proto_name("CORPUS_IMAGES"),
        Some(Corpus::Images)
    );
    assert_eq!(Corpus::Negative.to_i32(), -1);
    assert_eq!(Corpus::from_i32(-1), Some(Corpus::Negative));
}

#[derive(Message, Default, PartialEq, Debug)]
struct Maps {
    #[proto(tag = 4, map(key = "string", value = "int32"))]
    counts: BTreeMap<String, i32>,
    #[proto(tag = 5, map(key = "sint64", value = "message"))]
    by_id: BTreeMap<i64, Test1>,
    #[proto(tag = 6)]
    hashed: HashMap<u32, String>,
    #[proto(tag = 7, map(key = "bool", value = "enumeration"))]
    flags: BTreeMap<bool, EnumValue<Corpus>>,
}

#[test]
fn maps_are_repeated_entry_messages() {
    let mut counts = BTreeMap::new();
    counts.insert("a".to_string(), 1);
    check(
        &Maps {
            counts,
            ..Default::default()
        },
        "22 05 0a 01 61 10 01",
    );

    let mut by_id = BTreeMap::new();
    by_id.insert(-1, Test1 { a: 150 });
    check(
        &Maps {
            by_id,
            ..Default::default()
        },
        "2a 07 08 01 12 03 08 96 01",
    );

    // Both key and value are written even when they are defaults.
    let mut counts = BTreeMap::new();
    counts.insert(String::new(), 0);
    check(
        &Maps {
            counts,
            ..Default::default()
        },
        "22 04 0a 00 10 00",
    );

    let mut hashed = HashMap::new();
    hashed.insert(7, "x".to_string());
    check(
        &Maps {
            hashed,
            ..Default::default()
        },
        "32 05 08 07 12 01 78",
    );

    let mut flags = BTreeMap::new();
    flags.insert(true, EnumValue::from_raw(42));
    check(
        &Maps {
            flags,
            ..Default::default()
        },
        "3a 04 08 01 10 2a",
    );

    // Missing key or value decode as defaults; a repeated key keeps the last
    // value; unknown fields inside an entry are skipped; entries in any order.
    let m = Maps::decode(&hex(
        "22 02 10 05 22 05 10 07 0a 01 61 22 07 0a 01 61 10 08 18 01",
    ))
    .unwrap();
    assert_eq!(m.counts.get(""), Some(&5));
    assert_eq!(m.counts.get("a"), Some(&8));
}

#[derive(ferroly::proto::Oneof, PartialEq, Debug)]
enum Choice {
    #[proto(tag = 4)]
    Name(String),
    #[proto(tag = 9, sint32)]
    Id(i32),
    #[proto(tag = 10)]
    Sub(Test1),
    #[proto(tag = 11)]
    Boxed(Box<Test2>),
}

#[derive(Message, Default, PartialEq, Debug)]
struct WithOneof {
    #[proto(tag = 1)]
    before: i32,
    #[proto(oneof)]
    choice: Option<Choice>,
}

#[test]
fn oneof_members_have_explicit_presence() {
    check(
        &WithOneof {
            before: 1,
            choice: Some(Choice::Name("n".into())),
        },
        "08 01 22 01 6e",
    );
    check(
        &WithOneof {
            before: 0,
            choice: Some(Choice::Id(0)),
        },
        "48 00",
    );
    check(
        &WithOneof {
            before: 0,
            choice: Some(Choice::Id(-1)),
        },
        "48 01",
    );
    check(
        &WithOneof {
            before: 0,
            choice: Some(Choice::Sub(Test1 { a: 150 })),
        },
        "52 03 08 96 01",
    );
    check(
        &WithOneof {
            before: 0,
            choice: Some(Choice::Boxed(Box::new(Test2 { b: "x".into() }))),
        },
        "5a 03 12 01 78",
    );
    check(&WithOneof::default(), "");

    // The last member seen wins; the same message member merges.
    let m = WithOneof::decode(&hex("22 01 6e 48 04")).unwrap();
    assert_eq!(m.choice, Some(Choice::Id(2)));
    let m = WithOneof::decode(&hex("52 02 08 01 52 00")).unwrap();
    assert_eq!(m.choice, Some(Choice::Sub(Test1 { a: 1 })));
}

#[derive(Message, Default, PartialEq, Debug)]
struct Small {
    #[proto(tag = 1)]
    id: u64,
    #[proto(unknown)]
    unknown: UnknownFields,
}

#[test]
fn unknown_fields_round_trip() {
    // Varint, fixed64, length-delimited, group (with a nested field) and
    // fixed32 fields this type does not declare.
    let input =
        hex("08 07 10 96 01 19 01 02 03 04 05 06 07 08 22 02 68 69 2b 08 01 2c 35 aa bb cc dd");
    let m = Small::decode(&input).unwrap();
    assert_eq!(m.id, 7);
    assert_eq!(m.unknown.len(), 5);
    let values: Vec<&UnknownValue> = m.unknown.iter().map(|f| &f.value).collect();
    assert_eq!(values[0], &UnknownValue::Varint(150));
    assert_eq!(values[1], &UnknownValue::Fixed64(0x0807_0605_0403_0201));
    assert_eq!(values[2], &UnknownValue::LengthDelimited(b"hi".to_vec()));
    assert_eq!(values[3], &UnknownValue::Group(hex("08 01")));
    assert_eq!(values[4], &UnknownValue::Fixed32(0xddcc_bbaa));
    assert_eq!(m.unknown.get(4).count(), 1);
    // Known fields come first, then the unknown ones in arrival order — here
    // that is the input unchanged.
    assert_eq!(m.encode_to_vec(), input);
    assert_eq!(m.encoded_len(), input.len());

    // A known field with the wrong wire type is kept as unknown too.
    let odd = hex("0a 01 41");
    let m = Small::decode(&odd).unwrap();
    assert_eq!(m.id, 0);
    assert_eq!(m.encode_to_vec(), odd);

    // Without an unknown-field set the fields are dropped.
    assert_eq!(
        Test1::decode(&hex("08 01 12 01 00"))
            .unwrap()
            .encode_to_vec(),
        hex("08 01")
    );
}

#[derive(Message, Default, PartialEq, Debug)]
struct Proto2 {
    #[proto(tag = 1, required)]
    id: i32,
    #[proto(tag = 2)]
    label: Option<String>,
}

#[test]
fn required_fields() {
    check(&Proto2 { id: 0, label: None }, "08 00");
    assert_eq!(
        Proto2::decode(&hex("12 00")),
        Err(ProtoError::MissingRequired {
            message: "Proto2",
            field: "id"
        })
    );
}

#[test]
fn length_delimited_framing_and_strings() {
    let mut buf = Vec::new();
    Test1 { a: 150 }.encode_length_delimited(&mut buf);
    assert_eq!(buf, hex("03 08 96 01"));
    assert_eq!(
        Test1::decode_length_delimited(&buf).unwrap(),
        Test1 { a: 150 }
    );
    assert!(Test1::decode_length_delimited(&hex("03 08 96 01 00")).is_err());

    assert_eq!(
        Test2::decode(&hex("12 02 c3 28")),
        Err(ProtoError::InvalidUtf8)
    );
    check(&Test2 { b: "é".into() }, "12 02 c3 a9");
}

#[test]
fn derive_output_matches_hand_built_bytes() {
    // The same message written field by field with the `wire` primitives.
    let mut hand = Vec::new();
    wire::encode_key(1, wire::WireType::Varint, &mut hand);
    wire::encode_varint(150, &mut hand);
    wire::encode_key(4, wire::WireType::Len, &mut hand);
    wire::encode_varint(1, &mut hand);
    hand.push(b'n');
    let derived = WithOneof {
        before: 150,
        choice: Some(Choice::Name("n".into())),
    };
    assert_eq!(derived.encode_to_vec(), hand);

    let mut hand = Vec::new();
    wire::encode_key(2, wire::WireType::Varint, &mut hand);
    wire::encode_varint(wire::zigzag_encode64(-75), &mut hand);
    wire::encode_key(3, wire::WireType::I32, &mut hand);
    hand.extend_from_slice(&7u32.to_le_bytes());
    let derived = Scalars {
        s64: -75,
        f32_: 7,
        ..Default::default()
    };
    assert_eq!(derived.encode_to_vec(), hand);
}

// ---- proto2 `required` across occurrences ------------------------------------

#[derive(Message, Default, PartialEq, Debug)]
struct Pair {
    #[proto(tag = 1, required)]
    x: i32,
    #[proto(tag = 2, required)]
    y: i32,
}

#[derive(Message, Default, PartialEq, Debug)]
struct HasPair {
    #[proto(tag = 1)]
    pair: Option<Pair>,
    #[proto(tag = 2)]
    pairs: Vec<Pair>,
    #[proto(tag = 3)]
    boxed: Option<Box<Pair>>,
    #[proto(oneof)]
    choice: Option<PairChoice>,
}

#[derive(ferroly::proto::Oneof, PartialEq, Debug)]
enum PairChoice {
    #[proto(tag = 4)]
    A(Pair),
    #[proto(tag = 5)]
    B(Pair),
    #[proto(tag = 6)]
    N(i32),
}

fn missing(field: &'static str) -> Result<HasPair, ProtoError> {
    Err(ProtoError::MissingRequired {
        message: "Pair",
        field,
    })
}

#[test]
fn required_fields_are_checked_after_every_occurrence_is_merged() {
    // A sub-message split across two occurrences is complete once merged.
    let split = HasPair::decode(&hex("0a02 0801 0a02 1002")).unwrap();
    assert_eq!(split.pair, Some(Pair { x: 1, y: 2 }));
    assert_eq!(HasPair::decode(&hex("0a02 0801")), missing("y"));
    // Interleaved with other fields, and in a box.
    let boxed = HasPair::decode(&hex("1a02 1002 0a04 0801 1002 1a02 0805")).unwrap();
    assert_eq!(boxed.boxed.as_deref(), Some(&Pair { x: 5, y: 2 }));
    assert_eq!(HasPair::decode(&hex("1a02 1002 1a02 1003")), missing("x"));

    // Each repeated element is its own message and must be complete.
    assert_eq!(
        HasPair::decode(&hex("1204 0801 1002 1202 0801")),
        missing("y")
    );
    assert_eq!(
        HasPair::decode(&hex("1204 0801 1002 1204 0803 1004"))
            .unwrap()
            .pairs
            .len(),
        2
    );

    // A oneof variant arriving twice merges, like a singular field.
    let oneof = HasPair::decode(&hex("2202 0801 2202 1002")).unwrap();
    assert_eq!(oneof.choice, Some(PairChoice::A(Pair { x: 1, y: 2 })));
    // A replaced variant no longer counts; the new one must be complete.
    let replaced = HasPair::decode(&hex("2202 0801 2a04 0801 1002")).unwrap();
    assert_eq!(replaced.choice, Some(PairChoice::B(Pair { x: 1, y: 2 })));
    assert_eq!(
        HasPair::decode(&hex("2202 0801 3000")).unwrap().choice,
        Some(PairChoice::N(0))
    );
    assert_eq!(HasPair::decode(&hex("3000 2a02 0801")), missing("y"));

    // Merging a partial update into a complete message keeps it complete.
    let mut full = HasPair::decode(&hex("0a04 0801 1002")).unwrap();
    full.merge(&hex("0a02 0807")).unwrap();
    assert_eq!(full.pair, Some(Pair { x: 7, y: 2 }));
    // A required field of a new repeated element is still checked on merge.
    assert!(full.merge(&hex("1202 0801")).is_err());

    // The length-delimited decoder checks too.
    assert_eq!(
        Pair::decode_length_delimited(&hex("02 0801")),
        Err(ProtoError::MissingRequired {
            message: "Pair",
            field: "y"
        })
    );
}

// A replaced oneof variant takes the records of everything it owns with it,
// boxed or not, so the final message alone decides (neither a false
// `MissingRequired` nor a missing field accepted).

#[derive(Message, Default, PartialEq, Debug)]
struct Holder {
    #[proto(tag = 1)]
    p: Option<Box<Pair>>,
}

#[derive(ferroly::proto::Oneof, PartialEq, Debug)]
enum BoxChoice {
    #[proto(tag = 4)]
    A(Box<Pair>),
    #[proto(tag = 5)]
    B(Box<Pair>),
    #[proto(tag = 6)]
    N(i32),
    #[proto(tag = 7)]
    H(Holder),
}

#[derive(Message, Default, PartialEq, Debug)]
struct HasBox {
    #[proto(oneof)]
    choice: Option<BoxChoice>,
}

fn box_missing(field: &'static str) -> Result<HasBox, ProtoError> {
    Err(ProtoError::MissingRequired {
        message: "Pair",
        field,
    })
}

#[test]
fn a_replaced_boxed_variant_is_forgotten() {
    // A{} (incomplete) replaced by N(0): no Pair is left.
    assert_eq!(
        HasBox::decode(&hex("2200 3000")).unwrap().choice,
        Some(BoxChoice::N(0))
    );
    // A{} replaced by a complete B.
    assert_eq!(
        HasBox::decode(&hex("2200 2a04 0801 1002")).unwrap().choice,
        Some(BoxChoice::B(Box::new(Pair { x: 1, y: 2 })))
    );
    // A{x} -> N -> A{y}: the last A lacks x, whatever A{x} carried.
    assert_eq!(
        HasBox::decode(&hex("2202 0801 3000 2202 1002")),
        box_missing("x")
    );
    // Many replacements in a row, ending incomplete and complete.
    let mut churn = hex("2202 0801 3000").repeat(50);
    churn.extend(hex("2202 1002"));
    assert_eq!(HasBox::decode(&churn), box_missing("x"));
    churn.extend(hex("2202 0801"));
    assert!(HasBox::decode(&churn).is_ok());
}

#[test]
fn a_replaced_inline_variant_forgets_its_boxes() {
    // H{p: {}} replaced by N(0), and by a complete A.
    assert_eq!(
        HasBox::decode(&hex("3a02 0a00 3000")).unwrap().choice,
        Some(BoxChoice::N(0))
    );
    assert!(HasBox::decode(&hex("3a02 0a00 2204 0801 1002")).is_ok());
    // H{p: {x}} -> N -> H{p: {y}}: the last box lacks x.
    assert_eq!(
        HasBox::decode(&hex("3a04 0a02 0801 3000 3a04 0a02 1002")),
        box_missing("x")
    );
    // Still merged across occurrences of the same variant.
    let h = HasBox::decode(&hex("3a04 0a02 0801 3a04 0a02 1002")).unwrap();
    assert_eq!(
        h.choice,
        Some(BoxChoice::H(Holder {
            p: Some(Box::new(Pair { x: 1, y: 2 }))
        }))
    );
}

// ---- derive hygiene: method names of traits in scope -------------------------

mod hijack {
    //! A trait in scope whose methods share names with the inherent methods
    //! the derive calls (`merge_field`, `encode_raw`, `encoded_len`) and take
    //! `&self`, which method-call syntax would try first.
    use ferroly::proto::wire::{Reader, WireType};
    use ferroly::proto::{Message, ProtoError, UnknownFields};

    // In scope for the derive below, never meant to be called.
    #[allow(dead_code)]
    pub trait Hijack {
        fn merge_field(&self, _: u32, _: WireType, _: &mut Reader<'_>) -> Result<(), ProtoError> {
            panic!("hijacked merge_field")
        }
        fn encode_raw(&self, _: &mut Vec<u8>) {
            panic!("hijacked encode_raw")
        }
        fn encoded_len(&self) -> usize {
            panic!("hijacked encoded_len")
        }
    }
    impl Hijack for UnknownFields {}

    #[derive(Message, Default, PartialEq, Debug)]
    pub struct Keeps {
        #[proto(tag = 1)]
        pub a: i32,
        #[proto(unknown)]
        pub unknown: UnknownFields,
    }
}

#[test]
fn derived_code_ignores_traits_in_scope() {
    let bytes = hex("08 01 10 02");
    let m = hijack::Keeps::decode(&bytes).unwrap();
    assert_eq!(m.unknown.len(), 1);
    assert_eq!(m.encode_to_vec(), bytes);
    assert_eq!(m.encoded_len(), 4);
}

// ---- encoding cost is linear in depth ----------------------------------------

/// A hand-written leaf that counts how often it is measured.
#[derive(Default, Debug, PartialEq)]
struct Counted {
    payload: Vec<u8>,
}

thread_local! {
    static MEASURED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl Message for Counted {
    fn encode_raw(&self, buf: &mut Vec<u8>) {
        wire::encode_key(1, wire::WireType::Len, buf);
        wire::encode_varint(self.payload.len() as u64, buf);
        buf.extend_from_slice(&self.payload);
    }
    fn encoded_len(&self) -> usize {
        MEASURED.with(|c| c.set(c.get() + 1));
        1 + wire::varint_len(self.payload.len() as u64) + self.payload.len()
    }
    fn merge_from(&mut self, r: &mut wire::Reader<'_>) -> Result<(), ProtoError> {
        while let Some((tag, wt)) = r.read_key()? {
            if tag == 1 && wt == wire::WireType::Len {
                self.payload = r.read_bytes()?.to_vec();
            } else {
                r.skip_field(tag, wt)?;
            }
        }
        Ok(())
    }
}

#[derive(Message, Default, PartialEq, Debug)]
struct Chain {
    #[proto(tag = 1)]
    next: Option<Box<Chain>>,
    #[proto(tag = 2)]
    leaf: Option<Counted>,
    #[proto(tag = 3)]
    by_key: BTreeMap<u32, Chain>,
}

#[test]
fn encoding_measures_each_embedded_message_once() {
    let mut chain = Chain {
        leaf: Some(Counted {
            payload: vec![7; 300],
        }),
        ..Chain::default()
    };
    for i in 0..60 {
        let mut outer = Chain::default();
        if i % 2 == 0 {
            outer.next = Some(Box::new(chain));
        } else {
            outer.by_key.insert(i, chain);
        }
        chain = outer;
    }
    let encoders: [fn(&Chain) -> Vec<u8>; 3] = [
        |m| m.encode_to_vec(),
        |m| {
            let mut buf = Vec::new();
            m.encode_raw(&mut buf);
            buf
        },
        |m| {
            let mut buf = Vec::new();
            m.encode(&mut buf);
            buf
        },
    ];
    for encode in encoders {
        MEASURED.with(|c| c.set(0));
        let bytes = encode(&chain);
        // Once in the length pass; the write pass reads the cached length.
        assert_eq!(MEASURED.with(|c| c.get()), 1);
        assert_eq!(bytes.len(), chain.encoded_len());
        assert_eq!(Chain::decode(&bytes).unwrap(), chain);
    }
    let mut framed = Vec::new();
    chain.encode_length_delimited(&mut framed);
    assert_eq!(Chain::decode_length_delimited(&framed).unwrap(), chain);
}

#[test]
fn length_delimited_decoding_takes_limits() {
    let mut buf = Vec::new();
    Test1 { a: 150 }.encode_length_delimited(&mut buf);
    let small = ferroly::proto::Limits::new().with_max_input_bytes(2);
    assert!(matches!(
        Test1::decode_length_delimited_with(&buf, small),
        Err(ProtoError::LimitExceeded(_))
    ));
    let roomy = ferroly::proto::Limits::new().with_max_input_bytes(4);
    assert_eq!(
        Test1::decode_length_delimited_with(&buf, roomy).unwrap(),
        Test1 { a: 150 }
    );
}
