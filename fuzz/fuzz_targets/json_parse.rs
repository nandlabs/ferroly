#![no_main]
//! Fuzz the hand-rolled JSON parser: it must never panic or over-allocate on
//! arbitrary input (the shared depth/size/entry budget and the surrogate
//! handling are the guards), the streaming decoder must agree with the tree
//! parser on what is valid, and a parsed value must survive re-encoding.
use ferroly::codec::{json, CodecError, Value};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let tree = json::from_slice(data);
    let streamed = json::decode_from_slice::<Value>(data);
    assert_eq!(
        tree.is_ok(),
        streamed.is_ok(),
        "tree parser and streaming decoder disagree"
    );
    if let Ok(value) = tree {
        for written in [json::to_string(&value), json::to_string_pretty(&value)] {
            match json::from_str(&written) {
                Ok(back) => assert_eq!(back, value, "value changed via {written:?}"),
                // Re-encoding can be longer than the input.
                Err(CodecError::LimitExceeded(_)) => {}
                Err(e) => panic!("encoder output does not parse: {e}\n{written:?}"),
            }
        }
    }
});
