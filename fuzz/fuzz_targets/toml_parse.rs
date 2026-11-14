#![no_main]
//! Fuzz the hand-rolled TOML parser. It must never panic (escapes are read as
//! bytes, so a multibyte character cannot be split), must bound its depth for
//! dotted keys and table headers as well as nested values, and must be able to
//! read back whatever its own encoder writes.
use ferroly::codec::{toml, CodecError};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(value) = toml::from_str(text) {
        let written = toml::encode(&value);
        match toml::from_str(&written) {
            // Re-encoding can be longer than the input, so a budget error is
            // the one acceptable failure.
            Ok(_) | Err(CodecError::LimitExceeded(_)) => {}
            Err(e) => panic!("encoder output does not parse: {e}\n{written:?}"),
        }
    }
});
