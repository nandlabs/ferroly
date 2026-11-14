#![no_main]
//! Fuzz the hand-rolled XML parser: unterminated prologs, comments, CDATA
//! sections, attributes and entities must all be plain errors, nesting is
//! bounded by the shared budget, and the parser must be able to read back
//! whatever its own encoder writes.
use ferroly::codec::{xml, CodecError};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(value) = xml::from_str(text) {
        let written = xml::encode(&value);
        match xml::from_str(&written) {
            // Re-encoding can be longer, or one level deeper (the explicit
            // array form), than the input that fit the budget.
            Ok(_) | Err(CodecError::LimitExceeded(_)) => {}
            Err(e) => panic!("encoder output does not parse: {e}\n{written:?}"),
        }
    }
});
