#![cfg(feature = "codec")]
//! `#[derive(Encode)]` / `#[derive(Decode)]` across struct shapes (named,
//! tuple, newtype, unit) and enum representations (externally tagged,
//! internally tagged, untagged). Every case is checked on both the streaming
//! and the `Value` path, which must agree.

use std::fmt::Debug;

use ferroly::codec::{json, CodecError, Decode, Encode, Value};

/// Checks one value against its JSON text on all four generated paths.
fn round_trip<T: Encode + Decode + PartialEq + Debug>(value: &T, text: &str) {
    assert_eq!(json::encode(value), text, "streaming encode");
    assert_eq!(json::to_string(&value.encode()), text, "value encode");
    assert_eq!(&decode_both::<T>(text).expect("decode"), value);
}

/// Decodes `text` on the streaming path and on the `Value` path, asserts the
/// two agree (same value, or both failing), and returns the streaming result.
fn decode_both<T: Decode + PartialEq + Debug>(text: &str) -> Result<T, CodecError> {
    let streamed = json::decode::<T>(text);
    let tree = json::from_str(text).expect("test input must be valid JSON");
    let from_value = T::decode(&tree);
    match (&streamed, &from_value) {
        (Ok(a), Ok(b)) => assert_eq!(a, b, "paths decoded different values from {text}"),
        (Err(_), Err(_)) => {}
        _ => panic!("paths disagree on {text}: streaming {streamed:?}, value {from_value:?}"),
    }
    streamed
}

// ---- struct shapes -------------------------------------------------------

#[derive(Encode, Decode, PartialEq, Debug)]
struct Marker;

#[derive(Encode, Decode, PartialEq, Debug)]
struct UserId(u64);

#[derive(Encode, Decode, PartialEq, Debug)]
struct Tags(Vec<String>);

#[derive(Encode, Decode, PartialEq, Debug)]
struct Point(i32, i32);

#[derive(Encode, Decode, PartialEq, Debug)]
struct Triple(u8, String, Option<bool>);

#[derive(Encode, Decode, PartialEq, Debug)]
struct NoFields();

#[derive(Encode, Decode, PartialEq, Debug)]
struct EmptyBraces {}

#[derive(Encode, Decode, PartialEq, Debug)]
struct Single {
    only: u8,
}

#[derive(Encode, Decode, PartialEq, Debug)]
struct Holder {
    id: UserId,
    at: Point,
    marker: Marker,
    tags: Tags,
}

#[test]
fn unit_struct_is_null() {
    round_trip(&Marker, "null");
    assert!(decode_both::<Marker>("0").is_err());
    assert!(decode_both::<Marker>("{}").is_err());
}

#[test]
fn newtype_struct_is_transparent() {
    round_trip(&UserId(7), "7");
    round_trip(&Tags(vec!["a".into(), "b".into()]), r#"["a","b"]"#);
    assert!(decode_both::<UserId>(r#"{"0":7}"#).is_err());
}

#[test]
fn tuple_struct_is_an_array() {
    round_trip(&Point(1, -2), "[1,-2]");
    round_trip(&Triple(1, "x".into(), None), r#"[1,"x",null]"#);
    round_trip(&Triple(1, "x".into(), Some(true)), r#"[1,"x",true]"#);
    round_trip(&NoFields(), "[]");
}

#[test]
fn tuple_struct_rejects_wrong_length_and_shape() {
    for bad in ["[1]", "[1,2,3]", "[]"] {
        let err = decode_both::<Point>(bad).unwrap_err();
        assert!(matches!(err, CodecError::Expected(_)), "{bad}: {err:?}");
    }
    for bad in [r#"{"0":1,"1":2}"#, "1", "null", r#""1,2""#] {
        assert!(decode_both::<Point>(bad).is_err(), "{bad}");
    }
    assert!(decode_both::<NoFields>("[1]").is_err());
}

#[test]
fn named_struct_stays_an_object_even_with_one_or_no_fields() {
    round_trip(&Single { only: 1 }, r#"{"only":1}"#);
    round_trip(&EmptyBraces {}, "{}");
    assert!(decode_both::<Single>("1").is_err());
    assert!(decode_both::<EmptyBraces>("[]").is_err());
}

#[test]
fn shapes_nest_inside_each_other() {
    round_trip(
        &Holder {
            id: UserId(3),
            at: Point(1, 2),
            marker: Marker,
            tags: Tags(vec!["t".into()]),
        },
        r#"{"id":3,"at":[1,2],"marker":null,"tags":["t"]}"#,
    );
}

// ---- externally tagged enums (the default) -------------------------------

#[derive(Encode, Decode, PartialEq, Debug)]
enum Shape {
    Dot,
    Circle(u32),
    Line(Point, Point),
    Rect { w: u32, h: u32 },
    Blank(),
    Tagged(UserId),
}

#[test]
fn external_enum_with_mixed_variants() {
    round_trip(&Shape::Dot, r#""Dot""#);
    round_trip(&Shape::Circle(2), r#"{"Circle":2}"#);
    round_trip(
        &Shape::Line(Point(0, 0), Point(1, 1)),
        r#"{"Line":[[0,0],[1,1]]}"#,
    );
    round_trip(&Shape::Rect { w: 3, h: 4 }, r#"{"Rect":{"w":3,"h":4}}"#);
    round_trip(&Shape::Blank(), r#"{"Blank":[]}"#);
    round_trip(&Shape::Tagged(UserId(9)), r#"{"Tagged":9}"#);
}

#[test]
fn external_enum_accepts_null_content_for_a_unit_variant() {
    assert_eq!(decode_both::<Shape>(r#"{"Dot":null}"#).unwrap(), Shape::Dot);
    assert!(decode_both::<Shape>(r#"{"Dot":1}"#).is_err());
}

#[test]
fn external_enum_rejects_malformed_input() {
    let unknown = decode_both::<Shape>(r#""Hexagon""#).unwrap_err();
    assert!(matches!(unknown, CodecError::UnknownVariant(ref v) if v == "Hexagon"));
    let unknown = decode_both::<Shape>(r#"{"Hexagon":1}"#).unwrap_err();
    assert!(matches!(unknown, CodecError::UnknownVariant(ref v) if v == "Hexagon"));

    // A data-carrying variant named as a bare string.
    assert!(matches!(
        decode_both::<Shape>(r#""Circle""#).unwrap_err(),
        CodecError::Expected(_)
    ));
    // Not exactly one key.
    assert!(decode_both::<Shape>("{}").is_err());
    assert!(decode_both::<Shape>(r#"{"Circle":1,"Dot":null}"#).is_err());
    // Wrong content shape for the variant.
    assert!(decode_both::<Shape>(r#"{"Rect":[3,4]}"#).is_err());
    assert!(decode_both::<Shape>(r#"{"Line":[[0,0]]}"#).is_err());
    assert!(decode_both::<Shape>("7").is_err());
    let missing = decode_both::<Shape>(r#"{"Rect":{"w":3}}"#).unwrap_err();
    assert!(matches!(missing, CodecError::MissingField(ref f) if f == "h"));
}

/// No unit variants: `decode_from` streams the content out of the map entry.
#[derive(Encode, Decode, PartialEq, Debug)]
enum Message {
    Text(String),
    Pair(u8, u8),
    Record { id: u8, note: Option<String> },
    Nested(Box<Message>),
}

#[test]
fn external_enum_with_only_data_variants() {
    round_trip(&Message::Text("hi".into()), r#"{"Text":"hi"}"#);
    round_trip(&Message::Pair(1, 2), r#"{"Pair":[1,2]}"#);
    round_trip(
        &Message::Record { id: 1, note: None },
        r#"{"Record":{"id":1,"note":null}}"#,
    );
    round_trip(
        &Message::Nested(Box::new(Message::Pair(3, 4))),
        r#"{"Nested":{"Pair":[3,4]}}"#,
    );
    // An absent optional field inside a struct variant.
    assert_eq!(
        decode_both::<Message>(r#"{"Record":{"id":1}}"#).unwrap(),
        Message::Record { id: 1, note: None }
    );
}

#[test]
fn data_only_enum_rejects_malformed_input_on_both_paths() {
    for bad in [
        "{}",
        r#"{"Text":"a","Pair":[1,2]}"#,
        r#""Text""#,
        r#"{"Pair":[1]}"#,
        r#"{"Pair":[1,2,3]}"#,
        r#"{"Record":{}}"#,
        "[]",
        "null",
    ] {
        assert!(decode_both::<Message>(bad).is_err(), "{bad}");
    }
    let unknown = decode_both::<Message>(r#"{"Nope":1}"#).unwrap_err();
    assert!(matches!(unknown, CodecError::UnknownVariant(ref v) if v == "Nope"));
}

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(rename_all = "snake_case")]
enum Command {
    #[ferroly(alias = "halt")]
    StopAll,
    #[ferroly(rename = "go", alias = "start")]
    Run(u8),
    #[ferroly(rename_all = "camelCase")]
    SetLimit {
        max_items: u8,
        #[ferroly(rename = "soft")]
        is_soft: bool,
    },
}

#[test]
fn variant_rename_alias_and_field_rename_all() {
    round_trip(&Command::StopAll, r#""stop_all""#);
    round_trip(&Command::Run(1), r#"{"go":1}"#);
    round_trip(
        &Command::SetLimit {
            max_items: 5,
            is_soft: true,
        },
        r#"{"set_limit":{"maxItems":5,"soft":true}}"#,
    );
    // Aliases are accepted when decoding, never written.
    assert_eq!(
        decode_both::<Command>(r#""halt""#).unwrap(),
        Command::StopAll
    );
    assert_eq!(
        decode_both::<Command>(r#"{"start":2}"#).unwrap(),
        Command::Run(2)
    );
    // The original variant name is no longer a key once renamed.
    assert!(decode_both::<Command>(r#"{"run":2}"#).is_err());
}

/// The representation that worked before data variants existed is unchanged.
#[derive(Encode, Decode, PartialEq, Debug)]
enum Level {
    Low,
    High,
}

#[test]
fn unit_only_enum_is_still_a_plain_string() {
    round_trip(&Level::Low, r#""Low""#);
    assert_eq!(Level::High.encode(), Value::Str("High".into()));
    assert!(matches!(
        decode_both::<Level>(r#"{"Low":null}"#).unwrap_err(),
        CodecError::Expected("string")
    ));
    assert!(matches!(
        decode_both::<Level>(r#""Mid""#).unwrap_err(),
        CodecError::UnknownVariant(ref v) if v == "Mid"
    ));
}

// ---- internally tagged enums ---------------------------------------------

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(tag = "type", rename_all = "snake_case")]
enum Event {
    Ping,
    UserCreated {
        id: u64,
        name: String,
    },
    #[ferroly(rename = "gone", alias = "user_deleted")]
    UserDeleted {
        id: u64,
        #[ferroly(skip_none)]
        reason: Option<String>,
    },
}

#[test]
fn internal_tag_round_trips() {
    round_trip(&Event::Ping, r#"{"type":"ping"}"#);
    round_trip(
        &Event::UserCreated {
            id: 1,
            name: "a".into(),
        },
        r#"{"type":"user_created","id":1,"name":"a"}"#,
    );
    round_trip(
        &Event::UserDeleted {
            id: 2,
            reason: None,
        },
        r#"{"type":"gone","id":2}"#,
    );
    round_trip(
        &Event::UserDeleted {
            id: 2,
            reason: Some("spam".into()),
        },
        r#"{"type":"gone","id":2,"reason":"spam"}"#,
    );
}

#[test]
fn internal_tag_may_appear_anywhere_and_extra_keys_are_ignored() {
    assert_eq!(
        decode_both::<Event>(r#"{"id":1,"name":"a","type":"user_created"}"#).unwrap(),
        Event::UserCreated {
            id: 1,
            name: "a".into()
        }
    );
    assert_eq!(
        decode_both::<Event>(r#"{"type":"ping","trace":"abc"}"#).unwrap(),
        Event::Ping
    );
    assert_eq!(
        decode_both::<Event>(r#"{"type":"user_deleted","id":3}"#).unwrap(),
        Event::UserDeleted {
            id: 3,
            reason: None
        }
    );
}

#[test]
fn internal_tag_rejects_malformed_input() {
    let missing = decode_both::<Event>(r#"{"id":1}"#).unwrap_err();
    assert!(matches!(missing, CodecError::MissingField(ref f) if f == "type"));
    let not_string = decode_both::<Event>(r#"{"type":1}"#).unwrap_err();
    assert!(matches!(not_string, CodecError::Expected(_)));
    let unknown = decode_both::<Event>(r#"{"type":"nope"}"#).unwrap_err();
    assert!(matches!(unknown, CodecError::UnknownVariant(ref v) if v == "nope"));
    let field = decode_both::<Event>(r#"{"type":"user_created","id":1}"#).unwrap_err();
    assert!(matches!(field, CodecError::MissingField(ref f) if f == "name"));
    assert!(decode_both::<Event>(r#""ping""#).is_err());
    assert!(decode_both::<Event>("[]").is_err());
}

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(tag = "op", deny_unknown_fields)]
enum StrictOp {
    Noop,
    Add { n: u8 },
}

#[test]
fn internal_tag_with_deny_unknown_fields_still_accepts_the_tag() {
    round_trip(&StrictOp::Noop, r#"{"op":"Noop"}"#);
    round_trip(&StrictOp::Add { n: 1 }, r#"{"op":"Add","n":1}"#);
    assert!(decode_both::<StrictOp>(r#"{"op":"Add","n":1,"m":2}"#).is_err());
    assert!(decode_both::<StrictOp>(r#"{"op":"Noop","n":1}"#).is_err());
}

// ---- untagged enums ------------------------------------------------------

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(untagged)]
enum Setting {
    Unset,
    Flag(bool),
    Range(u8, u8),
    Named { name: String },
    Sized { width: u8, height: u8 },
}

#[test]
fn untagged_enum_is_just_the_content() {
    round_trip(&Setting::Unset, "null");
    round_trip(&Setting::Flag(true), "true");
    round_trip(&Setting::Range(1, 9), "[1,9]");
    round_trip(&Setting::Named { name: "n".into() }, r#"{"name":"n"}"#);
    round_trip(
        &Setting::Sized {
            width: 2,
            height: 3,
        },
        r#"{"width":2,"height":3}"#,
    );
}

#[test]
fn untagged_enum_takes_the_first_variant_that_decodes() {
    // Both struct variants are objects; `Named` is declared first and matches
    // whenever its field is present, whatever else the object holds.
    assert_eq!(
        decode_both::<Setting>(r#"{"name":"n","width":2,"height":3}"#).unwrap(),
        Setting::Named { name: "n".into() }
    );
    let err = decode_both::<Setting>("[1,2,3]").unwrap_err();
    match err {
        CodecError::Message(m) => assert!(m.contains("untagged enum Setting"), "{m}"),
        other => panic!("unexpected error: {other:?}"),
    }
    assert!(decode_both::<Setting>(r#"{"other":1}"#).is_err());
}

#[derive(Encode, Decode, PartialEq, Debug)]
struct Config {
    primary: Setting,
    events: Vec<Event>,
    shape: Option<Shape>,
}

#[test]
fn representations_compose_inside_a_struct() {
    round_trip(
        &Config {
            primary: Setting::Range(0, 1),
            events: vec![
                Event::Ping,
                Event::UserCreated {
                    id: 1,
                    name: "a".into(),
                },
            ],
            shape: Some(Shape::Rect { w: 1, h: 2 }),
        },
        concat!(
            r#"{"primary":[0,1],"#,
            r#""events":[{"type":"ping"},{"type":"user_created","id":1,"name":"a"}],"#,
            r#""shape":{"Rect":{"w":1,"h":2}}}"#
        ),
    );
}

// ---- other formats go through the same `Value` ---------------------------

#[test]
fn value_path_feeds_the_other_codecs() {
    // The `Value` a derive builds is what YAML/XML/TOML encode from.
    let value = Shape::Rect { w: 3, h: 4 }.encode();
    assert_eq!(
        value,
        Value::Object(vec![(
            "Rect".into(),
            Value::Object(vec![
                ("w".into(), Value::UInt(3)),
                ("h".into(), Value::UInt(4))
            ])
        )])
    );
    assert_eq!(UserId(7).encode(), Value::UInt(7));
    assert_eq!(Marker.encode(), Value::Null);
    assert_eq!(
        Point(1, 2).encode(),
        Value::Array(vec![Value::Int(1), Value::Int(2)])
    );
    assert_eq!(
        Event::Ping.encode(),
        Value::Object(vec![("type".into(), Value::Str("ping".into()))])
    );
}
