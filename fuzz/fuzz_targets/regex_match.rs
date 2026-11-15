#![no_main]
//! The schema `pattern` matcher must never panic, and must compile and match
//! in bounded time, on arbitrary pattern text and subjects. The engine is
//! crate-private, so it is driven through the public schema API.
//!
//! Input layout: the bytes before the first NUL are the pattern, the rest is
//! the subject string (both lossily decoded as UTF-8).

use ferroly::codec::schema::{self, Schema};
use ferroly::codec::Value;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let split = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    let pattern = String::from_utf8_lossy(&data[..split]).into_owned();
    let subject = String::from_utf8_lossy(data.get(split + 1..).unwrap_or_default()).into_owned();

    let schema_value = Value::Object(vec![("pattern".into(), Value::Str(pattern))]);
    let subject = Value::Str(subject);
    match Schema::compile(&schema_value) {
        Ok(schema) => {
            // The compiled and one-shot forms must agree.
            let compiled = schema.validate(&subject);
            assert_eq!(compiled, schema::validate(&schema_value, &subject));
        }
        // A rejected pattern fails closed in the one-shot form.
        Err(_) => assert!(schema::validate(&schema_value, &subject).is_err()),
    }
});
