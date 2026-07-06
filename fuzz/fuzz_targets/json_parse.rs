#![no_main]
//! Fuzz the hand-rolled JSON parser: it must never panic or over-allocate on
//! arbitrary input (the recursion cap and surrogate handling are the guards).
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = ferroly::codec::json::from_slice(data);
});
