#![cfg(feature = "codec")]
//! Exercises the codec primitive impls, `Value`, errors, and parser edge cases.

use std::collections::HashMap;

use ferroly::codec::{json, CodecError, Decode, Encode, Value};

fn rt<T>(v: T)
where
    T: Encode + Decode + PartialEq + std::fmt::Debug,
{
    let s = json::encode(&v);
    let back: T = json::decode(&s).unwrap();
    assert_eq!(v, back);
}

#[test]
fn round_trips_all_primitives() {
    rt(0i8);
    rt(-5i16);
    rt(1234i32);
    rt(-99i64);
    rt(7isize);
    rt(0u8);
    rt(255u16);
    rt(70000u32);
    rt(18446744073709551615u64);
    rt(9usize);
    rt(1.5f32);
    rt(-2.25f64);
    rt(true);
    rt(false);
    rt('z');
    rt("hello".to_string());
    rt(Some(42i32));
    rt::<Option<i32>>(None);
    rt(vec![1i32, 2, 3]);
    rt(Box::new(88i64));
    rt(());
    rt(Value::Int(5));

    let mut m = HashMap::new();
    m.insert("a".to_string(), 1i32);
    m.insert("b".to_string(), 2i32);
    rt(m);
}

#[test]
fn lenient_string_decoding() {
    assert_eq!(i32::decode(&Value::Str("42".into())).unwrap(), 42);
    assert_eq!(u16::decode(&Value::Str("7".into())).unwrap(), 7);
    assert_eq!(f64::decode(&Value::Str("1.5".into())).unwrap(), 1.5);
    assert!(bool::decode(&Value::Str("true".into())).unwrap());
    assert!(!bool::decode(&Value::Str("false".into())).unwrap());
    // and the error paths
    assert!(i32::decode(&Value::Bool(true)).is_err());
    assert!(u16::decode(&Value::Str("x".into())).is_err());
    assert!(bool::decode(&Value::Int(1)).is_err());
    assert!(String::decode(&Value::Int(1)).is_err());
    assert!(u8::decode(&Value::Int(-1)).is_err());
    assert!(i8::decode(&Value::Int(9999)).is_err()); // out of range
}

#[test]
fn value_accessors_and_from() {
    let v = Value::Object(vec![
        ("i".into(), 1i64.into()),
        ("u".into(), 5u32.into()),
        ("f".into(), 2.5f64.into()),
        ("b".into(), true.into()),
        ("s".into(), "x".into()),
        (
            "arr".into(),
            Value::from(vec![Value::from(1i32), Value::from(2i32)]),
        ),
        ("nil".into(), Option::<i32>::None.into()),
    ]);
    assert_eq!(v.get("i").unwrap().as_i64(), Some(1));
    assert_eq!(v.get("u").unwrap().as_u64(), Some(5));
    assert_eq!(v.get("f").unwrap().as_f64(), Some(2.5));
    assert_eq!(v.get("b").unwrap().as_bool(), Some(true));
    assert_eq!(v.get("s").unwrap().as_str(), Some("x"));
    let arr = v.get("arr").unwrap();
    assert_eq!(arr.as_array().unwrap().len(), 2);
    assert_eq!(arr.get_index(1).unwrap().as_i64(), Some(2));
    assert!(v.get("nil").unwrap().is_null());
    assert!(v.get("missing").is_none());
    assert!(v.as_object().is_some());
    // From various numeric widths
    let _ = Value::from(1i8);
    let _ = Value::from(1u8);
    let _ = Value::from(1.5f32);
    let _ = Value::from(String::from("s"));
    assert_eq!(Value::default(), Value::Null);
}

#[test]
fn codec_errors_display() {
    let errors = [
        CodecError::parse("x"),
        CodecError::expected("object"),
        CodecError::missing_field("f"),
        CodecError::unknown_variant("v"),
        CodecError::out_of_range("u8"),
        CodecError::UnsupportedContentType("application/zip".into()),
        CodecError::Message("boom".into()),
    ];
    for e in &errors {
        assert!(!e.to_string().is_empty());
    }
    let _dyn: &dyn std::error::Error = &errors[0];
}

#[test]
fn json_escapes_and_unicode() {
    let v = json::from_str(r#""a\n\t\"\\\/\b\fA""#).unwrap();
    assert_eq!(v.as_str(), Some("a\n\t\"\\/\u{8}\u{c}A"));
    // surrogate pair -> emoji
    let e = json::from_str(r#""😀""#).unwrap();
    assert_eq!(e.as_str(), Some("😀"));
    // escaping round-trips
    let s = json::to_string(&Value::Str("tab\tquote\"".into()));
    assert_eq!(json::from_str(&s).unwrap().as_str(), Some("tab\tquote\""));
    // Bytes encode as a numeric array
    assert_eq!(json::to_string(&Value::Bytes(vec![1, 2, 3])), "[1,2,3]");
    // literals and numbers
    assert_eq!(json::from_str("  true ").unwrap(), Value::Bool(true));
    assert_eq!(json::from_str("false").unwrap(), Value::Bool(false));
    assert_eq!(json::from_str("null").unwrap(), Value::Null);
    assert_eq!(json::from_str("-2.5e2").unwrap(), Value::Float(-250.0));
}

#[test]
fn json_parse_errors() {
    assert!(json::from_str("{").is_err()); // unexpected eof
    assert!(json::from_str("\"unterminated").is_err());
    assert!(json::from_str("truX").is_err()); // bad literal
    assert!(json::from_str("1 2").is_err()); // trailing chars
    assert!(json::from_str("[1 2]").is_err()); // missing comma
    assert!(json::from_str(r#"{"a" 1}"#).is_err()); // missing colon
    assert!(json::from_str("@").is_err()); // unexpected char
    assert!(json::from_str(r#""\uZZZZ""#).is_err()); // bad hex escape
}
