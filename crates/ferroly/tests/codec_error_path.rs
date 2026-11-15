#![cfg(feature = "codec")]
//! Decode errors name the failing field: `CodecError::At` carries a path of
//! wire names and indexes, filled in by the derive and the std container
//! impls on both decode paths (streaming `decode_from` and the `Value` path).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use ferroly::codec::{from_value_strict, json, toml, yaml, CodecError, Decode, PathSegment, Value};

/// Decodes `text` on the streaming path and on the `Value` path and asserts
/// both fail at `path`; returns the streaming error.
fn fails_at<T: Decode + std::fmt::Debug>(text: &str, path: &str) -> CodecError {
    let streamed = json::decode::<T>(text).expect_err("streaming decode should fail");
    let tree = json::from_str(text).expect("test input must be valid JSON");
    let from_value = T::decode(&tree).expect_err("value decode should fail");
    assert_eq!(streamed.path_string(), path, "streaming: {streamed}");
    assert_eq!(from_value.path_string(), path, "value: {from_value}");
    streamed
}

// ---- the issue's examples -------------------------------------------------

#[derive(Decode, Debug)]
struct Message {
    #[allow(dead_code)]
    role: String,
    #[allow(dead_code)]
    content: String,
}

#[derive(Decode, Debug)]
#[allow(dead_code)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    temperature: Option<f32>,
}

#[test]
fn chat_request_names_the_field() {
    let err = json::decode::<ChatRequest>(
        r#"{"model":"m","messages":[{"role":"user","content":"hi"},{"role":7,"content":"x"}]}"#,
    )
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "messages[1].role: expected string, found number"
    );
    assert_eq!(err.path_string(), "messages[1].role");
    let param = err.path().first().map(|segment| segment.to_string());
    assert_eq!(param.as_deref(), Some("messages"));
    assert_eq!(
        err.path(),
        &[
            PathSegment::Key("messages".into()),
            PathSegment::Index(1),
            PathSegment::Key("role".into()),
        ]
    );

    let err = json::decode::<ChatRequest>(r#"{"model":"m","messages":[],"temperature":"0.5"}"#)
        .unwrap_err();
    assert_eq!(err.path_string(), "temperature");
    assert!(matches!(
        err.root_cause(),
        CodecError::TypeMismatch {
            expected: "number",
            found: "string"
        }
    ));
}

#[derive(Decode, Debug)]
#[allow(dead_code)]
struct B {
    b: Vec<u32>,
}

#[derive(Decode, Debug)]
#[allow(dead_code)]
struct A {
    a: B,
}

#[test]
fn nested_sequence_element() {
    let err = fails_at::<A>(r#"{"a":{"b":[1,"x"]}}"#, "a.b[1]");
    assert_eq!(
        err.to_string(),
        "a.b[1]: expected unsigned integer, found string"
    );
    // The lenient formats locate it the same way.
    let err = yaml::decode::<A>("a:\n  b: [1, x]\n").unwrap_err();
    assert_eq!(err.path_string(), "a.b[1]");
    let err = toml::decode::<A>("[a]\nb = [1, 2.5]\n").unwrap_err();
    assert_eq!(err.path_string(), "a.b[1]");
}

// ---- names on the wire ------------------------------------------------------

#[derive(Decode, Debug)]
#[ferroly(rename_all = "camelCase")]
#[allow(dead_code)]
struct Sampling {
    max_tokens: u32,
    #[ferroly(rename = "top-p")]
    top_p: f32,
    #[ferroly(alias = "temp")]
    temperature: f32,
}

#[test]
fn paths_use_wire_names() {
    fails_at::<Sampling>(
        r#"{"maxTokens":"many","top-p":1,"temperature":1}"#,
        "maxTokens",
    );
    fails_at::<Sampling>(r#"{"maxTokens":1,"top-p":"x","temperature":1}"#, "top-p");
    // An alias is reported as the key the input actually used.
    fails_at::<Sampling>(r#"{"maxTokens":1,"top-p":1,"temp":"hot"}"#, "temp");
}

// ---- struct shapes ----------------------------------------------------------

#[derive(Decode, Debug)]
#[allow(dead_code)]
struct Pair(u8, String);

#[derive(Decode, Debug)]
#[allow(dead_code)]
struct Id(u32);

#[derive(Decode, Debug)]
#[allow(dead_code)]
struct Holder {
    pair: Pair,
    id: Id,
    ids: Vec<Id>,
    boxed: Box<Id>,
}

#[test]
fn tuple_fields_use_indexes_and_newtypes_are_transparent() {
    fails_at::<Pair>(r#"[1,2]"#, "[1]");
    fails_at::<Id>(r#""x""#, "");
    fails_at::<Holder>(r#"{"pair":["x","s"],"id":1,"ids":[],"boxed":1}"#, "pair[0]");
    fails_at::<Holder>(r#"{"pair":[1,"s"],"id":"x","ids":[],"boxed":1}"#, "id");
    fails_at::<Holder>(
        r#"{"pair":[1,"s"],"id":1,"ids":[1,-1],"boxed":1}"#,
        "ids[1]",
    );
    fails_at::<Holder>(r#"{"pair":[1,"s"],"id":1,"ids":[],"boxed":true}"#, "boxed");
}

#[test]
fn missing_and_unknown_fields_carry_the_parent_path() {
    let err = fails_at::<A>(r#"{"a":{}}"#, "a");
    assert_eq!(err.to_string(), "a: missing field: b");
    assert!(matches!(err.root_cause(), CodecError::MissingField(f) if f == "b"));

    #[derive(Decode, Debug)]
    #[ferroly(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Strict {
        x: u8,
    }
    #[derive(Decode, Debug)]
    #[allow(dead_code)]
    struct Parent {
        items: Vec<Strict>,
    }
    let err = fails_at::<Parent>(r#"{"items":[{"x":1},{"x":1,"y":2}]}"#, "items[1]");
    assert_eq!(err.to_string(), "items[1]: unknown field: y");
}

#[derive(Decode, Debug)]
#[allow(dead_code)]
struct Extra {
    port: u16,
}

#[derive(Decode, Debug)]
#[allow(dead_code)]
struct Server {
    name: String,
    #[ferroly(flatten)]
    extra: Extra,
}

#[test]
fn flatten_adds_no_segment() {
    fails_at::<Server>(r#"{"name":"n","port":"x"}"#, "port");
    let err = fails_at::<Server>(r#"{"name":"n"}"#, "");
    assert_eq!(err.to_string(), "missing field: port");
    #[derive(Decode, Debug)]
    #[allow(dead_code)]
    struct Fleet {
        servers: Vec<Server>,
    }
    fails_at::<Fleet>(
        r#"{"servers":[{"name":"a","port":1},{"name":"b","port":"x"}]}"#,
        "servers[1].port",
    );
}

#[test]
fn option_is_transparent() {
    #[derive(Decode, Debug)]
    #[allow(dead_code)]
    struct Opt {
        t: Option<f32>,
        inner: Option<B>,
    }
    fails_at::<Opt>(r#"{"t":"warm"}"#, "t");
    fails_at::<Opt>(r#"{"t":null,"inner":{"b":[true]}}"#, "inner.b[0]");
    fails_at::<Vec<Option<u8>>>(r#"[1,null,"x"]"#, "[2]");
}

// ---- enums ------------------------------------------------------------------

/// All data variants: the streaming path decodes the content in place.
#[derive(Decode, Debug)]
#[allow(dead_code)]
enum Shape {
    Circle { r: f64 },
    Line(u8, u8),
    Wrapped(Id),
}

/// A unit variant too: the streaming path goes through a `Value`.
#[derive(Decode, Debug)]
#[allow(dead_code)]
enum Mixed {
    Dot,
    Circle { r: f64 },
}

#[derive(Decode, Debug)]
#[allow(dead_code)]
struct Drawing {
    shapes: Vec<Shape>,
    mixed: Option<Mixed>,
}

#[test]
fn externally_tagged_content_is_under_the_variant_key() {
    fails_at::<Shape>(r#"{"Circle":{"r":"big"}}"#, "Circle.r");
    fails_at::<Shape>(r#"{"Line":[1,"x"]}"#, "Line[1]");
    fails_at::<Shape>(r#"{"Wrapped":"x"}"#, "Wrapped");
    fails_at::<Mixed>(r#"{"Circle":{"r":"big"}}"#, "Circle.r");
    fails_at::<Mixed>(r#"{"Dot":1}"#, "Dot");
    let err = fails_at::<Drawing>(
        r#"{"shapes":[{"Line":[1,2]},{"Circle":{}}]}"#,
        "shapes[1].Circle",
    );
    assert_eq!(err.to_string(), "shapes[1].Circle: missing field: r");
    fails_at::<Drawing>(
        r#"{"shapes":[],"mixed":{"Circle":{"r":[]}}}"#,
        "mixed.Circle.r",
    );
    // An unknown variant is reported where the enum sits.
    let err = fails_at::<Drawing>(r#"{"shapes":[{"Hexagon":1}]}"#, "shapes[0]");
    assert!(matches!(err.root_cause(), CodecError::UnknownVariant(v) if v == "Hexagon"));
}

#[derive(Decode, Debug)]
#[ferroly(tag = "type")]
#[allow(dead_code)]
enum Event {
    Click { x: i32, y: i32 },
    Key { code: u8 },
}

#[test]
fn internally_tagged_fields_sit_beside_the_tag() {
    fails_at::<Event>(r#"{"type":"Click","x":1,"y":"no"}"#, "y");
    #[derive(Decode, Debug)]
    #[allow(dead_code)]
    struct Log {
        events: Vec<Event>,
    }
    fails_at::<Log>(
        r#"{"events":[{"type":"Key","code":1},{"type":"Key","code":300}]}"#,
        "events[1].code",
    );
    let err = fails_at::<Log>(r#"{"events":[{"code":1}]}"#, "events[0]");
    assert_eq!(err.to_string(), "events[0]: missing field: type");
}

#[derive(Decode, Debug)]
#[ferroly(untagged)]
#[allow(dead_code)]
enum Setting {
    Flag(bool),
    Named { name: String },
}

#[test]
fn untagged_failure_is_reported_where_the_enum_sits() {
    #[derive(Decode, Debug)]
    #[allow(dead_code)]
    struct Settings {
        settings: BTreeMap<String, Setting>,
    }
    let err = fails_at::<Settings>(r#"{"settings":{"a":true,"b":{"name":1}}}"#, "settings.b");
    assert!(
        matches!(err.root_cause(), CodecError::Message(m) if m.contains("untagged enum Setting"))
    );
}

// ---- std containers ---------------------------------------------------------

#[test]
fn std_sequences_use_indexes() {
    fails_at::<Vec<u8>>(r#"[1,2,"x"]"#, "[2]");
    fails_at::<[u8; 3]>(r#"[1,"x",3]"#, "[1]");
    fails_at::<(u8, String, bool)>(r#"[1,"s",3]"#, "[2]");
    fails_at::<HashSet<u8>>(r#"[1,2,-3]"#, "[2]");
    fails_at::<BTreeSet<u8>>(r#"[1,"x"]"#, "[1]");
    fails_at::<Vec<Vec<u8>>>(r#"[[1],[2,"x"]]"#, "[1][1]");
    // A wrong length is about the sequence itself.
    fails_at::<[u8; 3]>(r#"[1,2]"#, "");
}

#[test]
fn std_maps_use_keys() {
    fails_at::<HashMap<String, u8>>(r#"{"a":1,"b":"x"}"#, "b");
    fails_at::<BTreeMap<String, Vec<u8>>>(r#"{"my key":[1,"x"]}"#, r#"["my key"][1]"#);
    #[derive(Decode, Debug)]
    #[allow(dead_code)]
    struct Limits {
        limits: HashMap<String, u32>,
    }
    fails_at::<Limits>(
        r#"{"limits":{"max.depth":"deep"}}"#,
        r#"limits["max.depth"]"#,
    );
}

// ---- compatibility ----------------------------------------------------------

#[test]
fn root_level_errors_take_no_path() {
    // A type error names what it found (0.6: `TypeMismatch` for every kind).
    let err = json::decode::<u32>(r#""x""#).unwrap_err();
    assert_eq!(err.to_string(), "expected unsigned integer, found string");
    assert!(err.path().is_empty());
    assert_eq!(err.path_string(), "");
    let err = json::decode::<A>("{}").unwrap_err();
    assert_eq!(err.to_string(), "missing field: a");
    assert_eq!(err, CodecError::MissingField("a".into()));
    let err = json::decode::<u16>("70000").unwrap_err();
    assert_eq!(err.to_string(), "value out of range for u16");
}

/// Asserts `text` fails as `TypeMismatch { expected, found }` on the streaming
/// path, the lenient `Value` path and the strict `Value` path.
fn mismatch_everywhere<T: Decode + std::fmt::Debug>(
    text: &str,
    expected: &'static str,
    found: &'static str,
) {
    let tree = json::from_str(text).expect("test input must be valid JSON");
    let results = [
        ("streaming", json::decode::<T>(text).unwrap_err()),
        ("value", T::decode(&tree).unwrap_err()),
        ("strict value", from_value_strict::<T>(&tree).unwrap_err()),
    ];
    for (path, err) in results {
        assert_eq!(
            err,
            CodecError::TypeMismatch { expected, found },
            "{path}: {text}"
        );
        assert_eq!(
            err.to_string(),
            format!("expected {expected}, found {found}")
        );
    }
}

#[derive(Decode, Debug)]
#[allow(dead_code)]
struct Unit;

#[test]
fn every_kind_of_type_error_names_what_it_found() {
    // CDC-6: one per kind the target can want.
    mismatch_everywhere::<String>("7", "string", "number");
    mismatch_everywhere::<i32>(r#""x""#, "integer", "string");
    mismatch_everywhere::<i32>("true", "integer", "bool");
    mismatch_everywhere::<u32>("[1]", "unsigned integer", "array");
    mismatch_everywhere::<i128>("{}", "integer", "object");
    mismatch_everywhere::<u128>("null", "unsigned integer", "null");
    mismatch_everywhere::<f64>("{}", "number", "object");
    mismatch_everywhere::<f32>(r#""warm""#, "number", "string");
    mismatch_everywhere::<bool>("null", "bool", "null");
    mismatch_everywhere::<bool>(r#""yes""#, "bool", "string");
    mismatch_everywhere::<Point2>(r#"{"x":1}"#, "array", "object");
    mismatch_everywhere::<HashMap<String, u8>>("[1]", "object", "array");
    mismatch_everywhere::<BTreeMap<String, u8>>("1", "object", "number");
    mismatch_everywhere::<A>("3", "object", "number");
    mismatch_everywhere::<Unit>("1", "null", "number");
    // Located the same way.
    let err = fails_at::<A>(r#"{"a":{"b":{}}}"#, "a.b");
    assert_eq!(err.to_string(), "a.b: expected array, found object");
    let err = fails_at::<A>(r#"{"a":[]}"#, "a");
    assert_eq!(err.to_string(), "a: expected object, found array");

    // A value of the right kind that does not fit stays `Expected`.
    for (text, what) in [("1.5", "integer"), ("-1", "unsigned integer")] {
        let tree = json::from_str(text).unwrap();
        let wanted = CodecError::Expected(what);
        if what == "integer" {
            assert_eq!(json::decode::<i32>(text).unwrap_err(), wanted);
            assert_eq!(i32::decode(&tree).unwrap_err(), wanted);
        } else {
            assert_eq!(json::decode::<u8>(text).unwrap_err(), wanted);
            assert_eq!(u8::decode(&tree).unwrap_err(), wanted);
        }
    }
}

#[derive(Decode, Debug)]
#[allow(dead_code)]
struct Point2(i32, i32);

#[derive(Decode, Debug)]
#[allow(dead_code)]
struct Named {
    name: String,
}

#[test]
fn malformed_json_is_a_parse_error_on_the_streaming_path() {
    // CDC-5: the token is lexed before a type error is built, so these are
    // not reported as `expected string, found null/bool/number`.
    for text in [
        r#"{"name": nope}"#,
        r#"{"name": tru}"#,
        r#"{"name": -}"#,
        r#"{"name": 01}"#,
        r#"{"name": @}"#,
        r#"{"name": "#,
    ] {
        let err = json::decode::<Named>(text).unwrap_err();
        assert!(matches!(err, CodecError::Parse(_)), "{text}: {err:?}");
        assert!(json::from_str(text).is_err(), "{text}");
    }
    // The same for a wanted array, object or null.
    for err in [
        json::decode::<Vec<u8>>("nope").unwrap_err(),
        json::decode::<Vec<u8>>("{").unwrap_err(),
        json::decode::<Named>("tru").unwrap_err(),
        json::decode::<HashMap<String, u8>>("[1,").unwrap_err(),
        json::decode::<Unit>("nul").unwrap_err(),
        json::decode::<Unit>("-").unwrap_err(),
    ] {
        assert!(matches!(err, CodecError::Parse(_)), "{err:?}");
    }
    // Well-formed values of the wrong kind are still type errors.
    let err = json::decode::<Named>(r#"{"name": null}"#).unwrap_err();
    assert_eq!(err.to_string(), "name: expected string, found null");
    let err = json::decode::<Named>(r#"{"name": -0.5e3}"#).unwrap_err();
    assert_eq!(err.to_string(), "name: expected string, found number");
}

#[test]
fn hostile_map_keys_do_not_forge_or_flood_the_message() {
    // CDC-4: keys come from the input.
    let text = r#"{"a\nERROR forged line":"x","b":1}"#;
    let err = fails_at::<HashMap<String, u8>>(text, r#"["a\nERROR forged line"]"#);
    assert!(!err.to_string().contains('\n'), "{err}");
    assert_eq!(
        err.path()[0],
        PathSegment::Key("a\nERROR forged line".into())
    );
    let huge = "k".repeat(1 << 20);
    let text = format!(r#"{{"{huge}":"x"}}"#);
    let err = json::decode::<BTreeMap<String, u8>>(&text).unwrap_err();
    assert!(
        err.to_string().len() < 200,
        "{} bytes",
        err.to_string().len()
    );
    assert_eq!(err.path()[0], PathSegment::Key(huge));
}

#[test]
fn a_lone_value_for_a_sequence_has_no_index_on_the_value_path() {
    // CDC-10 (documented): the lenient `Value` path reads a lone value as a
    // one-element sequence, so its error is located inside that value with no
    // index, while the strict paths reject the value itself.
    #[derive(Decode, Debug)]
    #[allow(dead_code)]
    struct Items {
        items: Vec<B>,
    }
    let text = r#"{"items":{"b":"no"}}"#;
    let streamed = json::decode::<Items>(text).unwrap_err();
    assert_eq!(streamed.to_string(), "items: expected array, found object");
    let tree = json::from_str(text).unwrap();
    let lenient = Items::decode(&tree).unwrap_err();
    assert_eq!(
        lenient.to_string(),
        "items.b: expected unsigned integer, found string"
    );
    let strict = from_value_strict::<Items>(&tree).unwrap_err();
    assert_eq!(strict, streamed);
}

#[test]
fn syntax_and_budget_errors_keep_their_variant() {
    let err = json::decode::<A>(r#"{"a":{"b":[1,}}"#).unwrap_err();
    assert!(matches!(err, CodecError::Parse(_)), "{err:?}");
    let deep = format!(r#"{{"a":{{"b":{}}}}}"#, "[".repeat(200));
    let err = json::decode::<A>(&deep).unwrap_err();
    assert!(matches!(err, CodecError::LimitExceeded(_)), "{err:?}");
}

#[test]
fn hand_written_impls_can_add_segments() {
    struct Point;
    impl Decode for Point {
        fn decode(value: &Value) -> Result<Self, CodecError> {
            let obj = value
                .as_object()
                .ok_or_else(|| CodecError::type_mismatch("object", value))?;
            for (k, v) in obj {
                i64::decode(v).map_err(|e| e.in_field(k))?;
            }
            Ok(Point)
        }
    }
    let err = Point::decode(&json::from_str(r#"{"x":1,"y":"no"}"#).unwrap())
        .err()
        .unwrap();
    assert_eq!(err.to_string(), "y: expected integer, found string");
    let err = Vec::<Point>::decode(&json::from_str("[{},3]").unwrap())
        .err()
        .unwrap();
    assert_eq!(err.to_string(), "[1]: expected object, found number");
}

/// The example in docs/codec.md ("Field paths").
#[test]
fn docs_example() {
    #[derive(Decode, Debug)]
    #[ferroly(rename_all = "camelCase")]
    #[allow(dead_code)]
    struct ChatRequest {
        model: String,
        messages: Vec<Message>,
        max_tokens: Option<u32>,
    }
    let err =
        json::decode::<ChatRequest>(r#"{"model":"m","messages":[],"maxTokens":"8"}"#).unwrap_err();
    assert_eq!(err.path_string(), "maxTokens");
    assert_eq!(
        err.to_string(),
        "maxTokens: expected unsigned integer, found string"
    );
    assert!(matches!(
        err.root_cause(),
        CodecError::TypeMismatch {
            expected: "unsigned integer",
            found: "string"
        }
    ));
    let err =
        json::decode::<ChatRequest>(r#"{"model":"m","messages":[{"role":"user"}]}"#).unwrap_err();
    assert_eq!(err.to_string(), "messages[0]: missing field: content");
}

#[cfg(feature = "config")]
#[test]
fn config_extract_names_the_dotted_key() {
    use ferroly::config::{Config, ConfigError};

    #[derive(Decode, Debug)]
    #[allow(dead_code)]
    struct Pool {
        threads: u32,
    }
    #[derive(Decode, Debug)]
    #[allow(dead_code)]
    struct AppConfig {
        name: String,
        server: Pool,
    }
    let cfg = Config::builder()
        .merge_value(json::from_str(r#"{"name":"app","server":{"threads":"many"}}"#).unwrap())
        .build();
    let err = cfg.extract::<AppConfig>().unwrap_err();
    match &err {
        ConfigError::InvalidValue { key, source } => {
            assert_eq!(key, "server.threads");
            // The path is in `key`; the source is the bare cause.
            assert!(source.path().is_empty(), "{source:?}");
            assert!(source.to_string().starts_with("expected "), "{source}");
        }
        other => panic!("expected InvalidValue, got {other:?}"),
    }
    assert!(
        err.to_string()
            .starts_with("invalid value for 'server.threads': expected "),
        "{err}"
    );
    let source = std::error::Error::source(&err)
        .and_then(|e| e.downcast_ref::<CodecError>())
        .expect("a codec error");
    assert!(source.path().is_empty(), "{source:?}");

    // An index and a quoted key join the path the way `path_string` writes
    // them; `get` on a section prefixes its own key.
    #[derive(Decode, Debug)]
    #[allow(dead_code)]
    struct Pools {
        pools: Vec<std::collections::BTreeMap<String, Pool>>,
    }
    let cfg = Config::builder()
        .merge_value(
            json::from_str(r#"{"svc":{"pools":[{"a":{"threads":1}},{"my pool":{"threads":-1}}]}}"#)
                .unwrap(),
        )
        .build();
    let key_of = |err: ConfigError| match err {
        ConfigError::InvalidValue { key, .. } => key,
        other => panic!("expected InvalidValue, got {other:?}"),
    };
    assert_eq!(
        key_of(cfg.get::<Pools>("svc").unwrap_err()),
        r#"svc.pools[1]["my pool"].threads"#
    );
    assert_eq!(
        key_of(
            cfg.get::<Vec<std::collections::BTreeMap<String, Pool>>>("svc.pools")
                .unwrap_err()
        ),
        r#"svc.pools[1]["my pool"].threads"#
    );

    // An error about the root itself has no path: still `Codec`.
    let err = cfg.extract::<AppConfig>().unwrap_err();
    assert!(matches!(err, ConfigError::Codec(_)), "{err:?}");
    let err = Config::builder()
        .merge_value(json::from_str(r#"{"name":"app","server":7}"#).unwrap())
        .build()
        .extract::<AppConfig>()
        .unwrap_err();
    assert!(
        matches!(&err, ConfigError::InvalidValue { key, .. } if key == "server"),
        "{err:?}"
    );
}
