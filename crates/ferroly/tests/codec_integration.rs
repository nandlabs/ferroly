#![cfg(feature = "codec")]
//! Exercises the derive macros and JSON codec end-to-end.

use ferroly::codec::{json, CodecError, Decode, Encode, FerrolyError, Value};

#[derive(Encode, Decode, PartialEq, Debug)]
struct Server {
    name: String,
    port: u16,
    tags: Vec<String>,
    #[ferroly(rename = "maxConns")]
    max_conns: u32,
    #[ferroly(skip_none)]
    note: Option<String>,
}

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(rename_all = "lowercase")]
enum Role {
    System,
    User,
    Assistant,
}

#[test]
fn struct_round_trip_with_rename_and_skip() {
    let s = Server {
        name: "api".into(),
        port: 8080,
        tags: vec!["a".into(), "b".into()],
        max_conns: 100,
        note: None,
    };
    let encoded = json::encode(&s);
    assert!(encoded.contains("\"maxConns\":100"));
    assert!(!encoded.contains("note"));

    let back: Server = json::decode(&encoded).unwrap();
    assert_eq!(back, s);
}

#[test]
fn skip_none_includes_some() {
    let s = Server {
        name: "x".into(),
        port: 1,
        tags: vec![],
        max_conns: 1,
        note: Some("hi".into()),
    };
    let encoded = json::encode(&s);
    assert!(encoded.contains("\"note\":\"hi\""));
    let back: Server = json::decode(&encoded).unwrap();
    assert_eq!(back.note.as_deref(), Some("hi"));
}

#[test]
fn enum_rename_all_lowercase() {
    assert_eq!(json::encode(&Role::Assistant), "\"assistant\"");
    let r: Role = json::decode("\"user\"").unwrap();
    assert_eq!(r, Role::User);
    assert!(json::decode::<Role>("\"nope\"").is_err());
}

#[test]
fn parses_nested_and_escapes() {
    let v = json::from_str(r#"{"a":[1,2.5,true,null],"b":{"c":"he\"llo\n"}}"#).unwrap();
    assert_eq!(v.as_object().unwrap().len(), 2);
    let c = v.get("b").unwrap().get("c").unwrap();
    assert_eq!(c.as_str(), Some("he\"llo\n"));
    let round = json::from_str(&json::to_string(&v)).unwrap();
    assert_eq!(v, round);
}

#[test]
fn number_kinds() {
    assert_eq!(json::from_str("42").unwrap(), Value::Int(42));
    assert_eq!(json::from_str("-7").unwrap(), Value::Int(-7));
    assert_eq!(json::from_str("2.5").unwrap(), Value::Float(2.5));
    assert!(matches!(
        json::from_str("18446744073709551615").unwrap(),
        Value::UInt(_)
    ));
}

#[derive(Debug, FerrolyError)]
enum MyError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("bad field '{field}': {reason}")]
    BadField { field: String, reason: String },
    #[error("wrapped codec error")]
    Codec(#[from] CodecError),
    #[error("nothing here")]
    Empty,
}

#[test]
fn error_display_positional_and_named() {
    assert_eq!(MyError::NotFound("x".into()).to_string(), "not found: x");
    assert_eq!(
        MyError::BadField {
            field: "port".into(),
            reason: "nan".into()
        }
        .to_string(),
        "bad field 'port': nan"
    );
    assert_eq!(MyError::Empty.to_string(), "nothing here");
}

#[test]
fn error_from_and_source() {
    use std::error::Error;
    let e: MyError = CodecError::expected("object").into();
    assert!(matches!(e, MyError::Codec(_)));
    assert!(e.source().is_some());
    assert!(MyError::Empty.source().is_none());
}
