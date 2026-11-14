#![cfg(feature = "codec")]
//! `#[ferroly(...)]` attributes on `#[derive(Encode)]` / `#[derive(Decode)]`:
//! raw identifiers, `rename_all` with acronyms, `default`, `skip`, `skip_none`,
//! `alias`, `flatten` and `deny_unknown_fields`.

use std::collections::HashMap;
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

// ---- raw identifiers -----------------------------------------------------

#[derive(Encode, Decode, PartialEq, Debug)]
struct Token {
    r#type: String,
    r#match: u8,
    plain: bool,
}

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(rename_all = "SCREAMING_SNAKE_CASE")]
struct Keyword {
    r#final: u8,
    r#loop_count: u8,
}

#[derive(Encode, Decode, PartialEq, Debug)]
enum Kind {
    r#Static,
    Other { r#ref: u8 },
}

#[test]
fn raw_identifiers_use_the_bare_name_as_key() {
    round_trip(
        &Token {
            r#type: "ident".into(),
            r#match: 1,
            plain: true,
        },
        r#"{"type":"ident","match":1,"plain":true}"#,
    );
    round_trip(
        &Keyword {
            r#final: 1,
            r#loop_count: 2,
        },
        r#"{"FINAL":1,"LOOP_COUNT":2}"#,
    );
    round_trip(&Kind::r#Static, r#""Static""#);
    round_trip(&Kind::Other { r#ref: 3 }, r#"{"Other":{"ref":3}}"#);
}

#[test]
fn raw_identifier_keys_in_errors_and_rejected_spellings() {
    let missing = decode_both::<Token>(r#"{"match":1,"plain":true}"#).unwrap_err();
    assert!(matches!(missing, CodecError::MissingField(ref f) if f == "type"));
    // The `r#` spelling is not a key.
    assert!(decode_both::<Token>(r##"{"r#type":"x","match":1,"plain":true}"##).is_err());
}

// ---- rename_all ----------------------------------------------------------

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(rename_all = "snake_case")]
enum Transport {
    HTTPServer,
    XMLHttpRequest,
    IOError,
    Sha256Sum,
    Plain,
}

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(rename_all = "kebab-case")]
enum Dashed {
    HTTPServer,
    UserID,
}

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(rename_all = "camelCase")]
enum Camel {
    HTTPServer,
    UserID,
    Plain,
}

#[test]
fn rename_all_treats_an_acronym_as_one_word() {
    round_trip(&Transport::HTTPServer, r#""http_server""#);
    round_trip(&Transport::XMLHttpRequest, r#""xml_http_request""#);
    round_trip(&Transport::IOError, r#""io_error""#);
    round_trip(&Transport::Sha256Sum, r#""sha256_sum""#);
    round_trip(&Transport::Plain, r#""plain""#);
    round_trip(&Dashed::HTTPServer, r#""http-server""#);
    round_trip(&Dashed::UserID, r#""user-id""#);
    round_trip(&Camel::HTTPServer, r#""httpServer""#);
    round_trip(&Camel::UserID, r#""userId""#);
    round_trip(&Camel::Plain, r#""plain""#);
}

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(rename_all = "PascalCase")]
struct Pascal {
    account_id: u8,
    _reserved: u8,
}

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(rename_all = "snake_case")]
struct Snake {
    account_id: u8,
    _id: u8,
}

#[test]
fn rename_all_on_fields() {
    round_trip(
        &Pascal {
            account_id: 1,
            _reserved: 2,
        },
        r#"{"AccountId":1,"Reserved":2}"#,
    );
    // The snake rules leave an already-snake field name alone, prefix included.
    round_trip(
        &Snake {
            account_id: 1,
            _id: 2,
        },
        r#"{"account_id":1,"_id":2}"#,
    );
}

// ---- default -------------------------------------------------------------

type Maybe<T> = Option<T>;

fn default_port() -> u16 {
    8080
}

mod defaults {
    pub fn hosts() -> Vec<String> {
        vec!["localhost".to_string()]
    }
}

#[derive(Encode, Decode, PartialEq, Debug)]
struct Listener {
    name: String,
    #[ferroly(default)]
    retries: u32,
    #[ferroly(default = "default_port")]
    port: u16,
    #[ferroly(default = "defaults::hosts")]
    hosts: Vec<String>,
    #[ferroly(default)]
    label: Maybe<String>,
    #[ferroly(default)]
    tags: Vec<String>,
}

#[test]
fn default_fills_in_missing_fields() {
    let decoded = decode_both::<Listener>(r#"{"name":"a"}"#).unwrap();
    assert_eq!(
        decoded,
        Listener {
            name: "a".into(),
            retries: 0,
            port: 8080,
            hosts: vec!["localhost".into()],
            label: None,
            tags: vec![],
        }
    );
}

#[test]
fn default_does_not_override_present_fields() {
    let full = Listener {
        name: "a".into(),
        retries: 3,
        port: 1,
        hosts: vec![],
        label: Some("l".into()),
        tags: vec!["t".into()],
    };
    round_trip(
        &full,
        r#"{"name":"a","retries":3,"port":1,"hosts":[],"label":"l","tags":["t"]}"#,
    );
    // A field without `default` is still required.
    let missing = decode_both::<Listener>(r#"{"retries":3}"#).unwrap_err();
    assert!(matches!(missing, CodecError::MissingField(ref f) if f == "name"));
}

#[derive(Decode, PartialEq, Debug)]
struct AliasWithoutDefault {
    label: Maybe<String>,
}

#[derive(Decode, PartialEq, Debug)]
struct SpelledOption {
    label: Option<String>,
    nested: std::option::Option<u8>,
}

#[test]
fn only_a_spelled_out_option_is_implicitly_optional() {
    // The alias hides the `Option`, so the field is required unless marked.
    let err = decode_both::<AliasWithoutDefault>("{}").unwrap_err();
    assert!(matches!(err, CodecError::MissingField(ref f) if f == "label"));
    assert_eq!(
        decode_both::<AliasWithoutDefault>(r#"{"label":null}"#).unwrap(),
        AliasWithoutDefault { label: None }
    );
    assert_eq!(
        decode_both::<SpelledOption>("{}").unwrap(),
        SpelledOption {
            label: None,
            nested: None
        }
    );
}

// ---- skip / skip_none ----------------------------------------------------

fn fresh_token() -> String {
    "generated".to_string()
}

#[derive(Encode, Decode, PartialEq, Debug)]
struct Session {
    user: String,
    #[ferroly(skip)]
    hits: u32,
    #[ferroly(skip, default = "fresh_token")]
    token: String,
    #[ferroly(skip_none)]
    note: Option<String>,
    #[ferroly(skip_none)]
    alias_note: Maybe<String>,
}

#[test]
fn skip_is_never_written_and_never_read() {
    let session = Session {
        user: "u".into(),
        hits: 99,
        token: "secret".into(),
        note: None,
        alias_note: None,
    };
    let text = r#"{"user":"u"}"#;
    assert_eq!(json::encode(&session), text);
    assert_eq!(json::to_string(&session.encode()), text);

    // Skipped fields come from `Default` (or the named function), even when the
    // input carries a value under their name.
    let decoded =
        decode_both::<Session>(r#"{"user":"u","hits":5,"token":"x","note":"n"}"#).unwrap();
    assert_eq!(
        decoded,
        Session {
            user: "u".into(),
            hits: 0,
            token: "generated".into(),
            note: Some("n".into()),
            alias_note: None,
        }
    );
}

#[test]
fn skip_none_omits_none_and_makes_the_field_optional() {
    let with = Session {
        user: "u".into(),
        hits: 0,
        token: "generated".into(),
        note: Some("n".into()),
        alias_note: Some("a".into()),
    };
    round_trip(&with, r#"{"user":"u","note":"n","alias_note":"a"}"#);
    let without = Session {
        user: "u".into(),
        hits: 0,
        token: "generated".into(),
        note: None,
        alias_note: None,
    };
    round_trip(&without, r#"{"user":"u"}"#);
}

// ---- alias ---------------------------------------------------------------

#[derive(Encode, Decode, PartialEq, Debug)]
struct Profile {
    #[ferroly(alias = "user_name", alias = "login")]
    username: String,
    #[ferroly(rename = "mail", alias = "email")]
    email_address: Option<String>,
}

#[test]
fn alias_is_accepted_on_decode_only() {
    let profile = Profile {
        username: "u".into(),
        email_address: Some("e".into()),
    };
    // Encoding always uses the primary key.
    round_trip(&profile, r#"{"username":"u","mail":"e"}"#);
    for text in [
        r#"{"user_name":"u","email":"e"}"#,
        r#"{"login":"u","mail":"e"}"#,
    ] {
        assert_eq!(decode_both::<Profile>(text).unwrap(), profile, "{text}");
    }
    // The field's own Rust name is not a key once renamed.
    assert_eq!(
        decode_both::<Profile>(r#"{"username":"u","email_address":"e"}"#)
            .unwrap()
            .email_address,
        None
    );
}

// ---- flatten -------------------------------------------------------------

#[derive(Encode, Decode, PartialEq, Debug)]
struct Audit {
    created_by: String,
    revision: u32,
}

#[derive(Encode, Decode, PartialEq, Debug)]
struct Paging {
    #[ferroly(default)]
    offset: u32,
    #[ferroly(default)]
    limit: u32,
}

#[derive(Encode, Decode, PartialEq, Debug)]
struct Document {
    id: u8,
    #[ferroly(flatten)]
    audit: Audit,
    title: String,
    #[ferroly(flatten)]
    paging: Paging,
}

#[test]
fn flatten_merges_nested_struct_fields_into_the_parent() {
    let doc = Document {
        id: 1,
        audit: Audit {
            created_by: "me".into(),
            revision: 2,
        },
        title: "t".into(),
        paging: Paging {
            offset: 10,
            limit: 5,
        },
    };
    round_trip(
        &doc,
        r#"{"id":1,"created_by":"me","revision":2,"title":"t","offset":10,"limit":5}"#,
    );
    // Key order in the input does not matter.
    assert_eq!(
        decode_both::<Document>(
            r#"{"limit":5,"revision":2,"title":"t","offset":10,"id":1,"created_by":"me"}"#
        )
        .unwrap(),
        doc
    );
    // A missing key of the flattened struct is reported by that struct.
    let missing = decode_both::<Document>(r#"{"id":1,"title":"t","created_by":"me"}"#).unwrap_err();
    assert!(matches!(missing, CodecError::MissingField(ref f) if f == "revision"));
}

#[derive(Encode, Decode, PartialEq, Debug)]
struct Open {
    id: u8,
    #[ferroly(flatten)]
    extra: HashMap<String, Value>,
}

#[test]
fn flatten_map_collects_unknown_keys() {
    let decoded = decode_both::<Open>(r#"{"a":1,"id":7,"b":"x","c":[true]}"#).unwrap();
    assert_eq!(decoded.id, 7);
    assert_eq!(decoded.extra.len(), 3);
    assert_eq!(decoded.extra["a"], Value::Int(1));
    assert_eq!(decoded.extra["b"], Value::Str("x".into()));
    assert_eq!(decoded.extra["c"], Value::Array(vec![Value::Bool(true)]));

    // With one extra entry the output order is deterministic.
    let one = Open {
        id: 1,
        extra: HashMap::from([("k".to_string(), Value::Str("v".into()))]),
    };
    round_trip(&one, r#"{"id":1,"k":"v"}"#);
    round_trip(
        &Open {
            id: 1,
            extra: HashMap::new(),
        },
        r#"{"id":1}"#,
    );
}

#[derive(Encode, Decode, PartialEq, Debug)]
struct Record {
    id: u8,
    #[ferroly(flatten)]
    audit: Audit,
    #[ferroly(flatten)]
    extra: HashMap<String, Value>,
    #[ferroly(skip_none)]
    note: Option<String>,
}

#[test]
fn flatten_never_writes_a_key_twice() {
    // The catch-all map sees every key the struct's own fields did not claim,
    // including those the flattened struct beside it used.
    let text = r#"{"id":1,"created_by":"me","revision":2,"color":"red"}"#;
    let decoded = decode_both::<Record>(text).unwrap();
    assert_eq!(decoded.audit.revision, 2);
    assert_eq!(decoded.extra.len(), 3);
    // Encoding writes each key once: the earlier `flatten` field wins.
    assert_eq!(json::encode(&decoded), text);
    assert_eq!(json::to_string(&decoded.encode()), text);

    // A change to the flattened struct is what gets written, not the stale copy.
    let mut changed = decoded;
    changed.audit.revision = 3;
    assert_eq!(
        json::encode(&changed),
        r#"{"id":1,"created_by":"me","revision":3,"color":"red"}"#
    );

    // The struct's own keys win over map entries, wherever the field sits and
    // even when the field itself is omitted.
    let shadowing = Record {
        id: 1,
        audit: Audit {
            created_by: "me".into(),
            revision: 2,
        },
        extra: HashMap::from([
            ("id".to_string(), Value::Int(9)),
            ("note".to_string(), Value::Str("stale".into())),
        ]),
        note: None,
    };
    let text = r#"{"id":1,"created_by":"me","revision":2}"#;
    assert_eq!(json::encode(&shadowing), text);
    assert_eq!(json::to_string(&shadowing.encode()), text);
}

#[derive(Encode, Decode, PartialEq, Debug)]
struct Geo {
    lat: i32,
    lon: i32,
}

#[derive(Encode, Decode, PartialEq, Debug)]
struct Place {
    name: String,
    #[ferroly(flatten)]
    geo: Option<Geo>,
}

#[test]
fn flatten_option_is_absent_when_its_keys_are() {
    round_trip(
        &Place {
            name: "p".into(),
            geo: Some(Geo { lat: 1, lon: 2 }),
        },
        r#"{"name":"p","lat":1,"lon":2}"#,
    );
    round_trip(
        &Place {
            name: "p".into(),
            geo: None,
        },
        r#"{"name":"p"}"#,
    );
    // Partially present keys do not form the group either.
    assert_eq!(
        decode_both::<Place>(r#"{"name":"p","lat":1}"#).unwrap().geo,
        None
    );
}

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(tag = "kind")]
enum Entry {
    File {
        path: String,
        #[ferroly(flatten)]
        audit: Audit,
    },
    Link {
        #[ferroly(flatten)]
        rest: HashMap<String, Value>,
    },
}

#[test]
fn flatten_inside_struct_variants() {
    round_trip(
        &Entry::File {
            path: "/a".into(),
            audit: Audit {
                created_by: "me".into(),
                revision: 1,
            },
        },
        r#"{"kind":"File","path":"/a","created_by":"me","revision":1}"#,
    );
    // The tag is not one of the leftover keys handed to the flattened map.
    let link = decode_both::<Entry>(r#"{"kind":"Link","to":"/b"}"#).unwrap();
    assert_eq!(
        link,
        Entry::Link {
            rest: HashMap::from([("to".to_string(), Value::Str("/b".into()))])
        }
    );
    round_trip(&link, r#"{"kind":"Link","to":"/b"}"#);
}

// ---- deny_unknown_fields -------------------------------------------------

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(deny_unknown_fields)]
struct Closed {
    id: u8,
    #[ferroly(alias = "nick")]
    name: Option<String>,
    #[ferroly(skip)]
    cache: u8,
}

#[derive(Encode, Decode, PartialEq, Debug)]
struct Lenient {
    id: u8,
}

#[test]
fn deny_unknown_fields_rejects_unexpected_keys() {
    round_trip(
        &Closed {
            id: 1,
            name: Some("n".into()),
            cache: 0,
        },
        r#"{"id":1,"name":"n"}"#,
    );
    // Aliases are known keys; optional fields may still be absent.
    assert!(decode_both::<Closed>(r#"{"id":1,"nick":"n"}"#).is_ok());
    assert!(decode_both::<Closed>(r#"{"id":1}"#).is_ok());

    let err = decode_both::<Closed>(r#"{"id":1,"extra":true}"#).unwrap_err();
    match err {
        CodecError::Message(m) => assert_eq!(m, "unknown field: extra"),
        other => panic!("unexpected error: {other:?}"),
    }
    // A skipped field is not part of the wire format, so its name is unknown.
    assert!(decode_both::<Closed>(r#"{"id":1,"cache":3}"#).is_err());
    // Without the attribute, unknown keys are ignored.
    assert_eq!(
        decode_both::<Lenient>(r#"{"id":1,"extra":{"deep":[1,2]}}"#).unwrap(),
        Lenient { id: 1 }
    );
}

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(deny_unknown_fields)]
enum ClosedEnum {
    Unit,
    Point { x: i8, y: i8 },
    Wrap(Lenient),
}

#[test]
fn deny_unknown_fields_applies_to_struct_variants() {
    round_trip(
        &ClosedEnum::Point { x: 1, y: -1 },
        r#"{"Point":{"x":1,"y":-1}}"#,
    );
    assert!(decode_both::<ClosedEnum>(r#"{"Point":{"x":1,"y":2,"z":3}}"#).is_err());
    // It does not reach into other types.
    assert_eq!(
        decode_both::<ClosedEnum>(r#"{"Wrap":{"id":1,"extra":0}}"#).unwrap(),
        ClosedEnum::Wrap(Lenient { id: 1 })
    );
    round_trip(&ClosedEnum::Unit, r#""Unit""#);
}

// ---- hygiene -------------------------------------------------------------

/// Field names chosen to collide with the locals the generated code uses.
#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(deny_unknown_fields)]
struct Collide {
    __e: u8,
    __d: u8,
    __value: u8,
    __fields: u8,
    __k: u8,
    __v: u8,
    __obj: u8,
    __f0: u8,
    __s0: u8,
    __len: Option<u8>,
    #[ferroly(skip_none)]
    __rest: Option<u8>,
    value: u8,
    key: u8,
    d: u8,
    e: u8,
}

#[derive(Encode, Decode, PartialEq, Debug)]
enum CollideEnum {
    Value { __value: u8, __out: u8 },
    Option(Option<u8>),
    Result(u8, u8),
    Some,
    None,
    Ok { __k: u8 },
    Err,
}

#[test]
fn generated_locals_do_not_collide_with_user_names() {
    round_trip(
        &Collide {
            __e: 1,
            __d: 2,
            __value: 3,
            __fields: 4,
            __k: 5,
            __v: 6,
            __obj: 7,
            __f0: 8,
            __s0: 9,
            __len: None,
            __rest: Some(10),
            value: 11,
            key: 12,
            d: 13,
            e: 14,
        },
        concat!(
            r#"{"__e":1,"__d":2,"__value":3,"__fields":4,"__k":5,"__v":6,"__obj":7,"#,
            r#""__f0":8,"__s0":9,"__len":null,"__rest":10,"value":11,"key":12,"d":13,"e":14}"#
        ),
    );
    round_trip(
        &CollideEnum::Value {
            __value: 1,
            __out: 2,
        },
        r#"{"Value":{"__value":1,"__out":2}}"#,
    );
    round_trip(&CollideEnum::Option(None), r#"{"Option":null}"#);
    round_trip(&CollideEnum::Result(1, 2), r#"{"Result":[1,2]}"#);
    round_trip(&CollideEnum::Some, r#""Some""#);
    round_trip(&CollideEnum::None, r#""None""#);
    round_trip(&CollideEnum::Ok { __k: 1 }, r#"{"Ok":{"__k":1}}"#);
    round_trip(&CollideEnum::Err, r#""Err""#);
}
