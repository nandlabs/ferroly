#![no_main]
//! Fuzz the hand-rolled YAML parser (guarded by the block-nesting depth cap).
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = ferroly::codec::yaml::from_str(s);
    }
});
