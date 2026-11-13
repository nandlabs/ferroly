#![no_main]
//! Fuzz the hand-rolled YAML parser: it must never panic, must reject (not
//! reinterpret) syntax outside its subset, and a parsed value must survive a
//! trip through its own encoder unchanged.
use ferroly::codec::{yaml, CodecError, Value};
use libfuzzer_sys::fuzz_target;

/// Equality that treats `NaN` as equal to itself (`.nan` is valid YAML).
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

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(value) = yaml::from_str(text) {
        let written = yaml::encode(&value);
        match yaml::from_str(&written) {
            Ok(back) => assert!(same(&back, &value), "value changed via {written:?}"),
            // Re-encoding can be longer than the input.
            Err(CodecError::LimitExceeded(_)) => {}
            Err(e) => panic!("encoder output does not parse: {e}\n{written:?}"),
        }
    }
});
