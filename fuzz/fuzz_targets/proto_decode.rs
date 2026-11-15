#![no_main]
//! Fuzz the protobuf decoder: arbitrary bytes decoded into a message with a
//! field of every shape (recursive through a boxed child, repeated children,
//! map values and a oneof) must never panic or over-allocate (the decode
//! budget and the length checks are the guards), and whatever decodes must
//! re-encode to bytes that decode and re-encode identically. Valid UTF-8
//! input is also fed to the `.proto` parser, which must never panic.
use std::collections::BTreeMap;

use ferroly::proto::{idl, EnumValue, Limits, Message, UnknownFields};
use libfuzzer_sys::fuzz_target;

#[derive(ferroly::proto::Enumeration, Clone, Copy, PartialEq, Debug)]
enum Kind {
    A = 0,
    B = 1,
}

#[derive(ferroly::proto::Oneof, Clone, PartialEq, Debug)]
enum Pick {
    #[proto(tag = 20)]
    Text(String),
    #[proto(tag = 21, sint64)]
    Num(i64),
    #[proto(tag = 22)]
    Node(Box<Everything>),
}

#[derive(ferroly::proto::Message, Clone, PartialEq, Debug, Default)]
struct Everything {
    #[proto(tag = 1)]
    i: i32,
    #[proto(tag = 2, sint64)]
    s: i64,
    #[proto(tag = 3, fixed32)]
    f: u32,
    #[proto(tag = 4)]
    d: f64,
    #[proto(tag = 5)]
    b: bool,
    #[proto(tag = 6)]
    text: String,
    #[proto(tag = 7)]
    raw: Vec<u8>,
    #[proto(tag = 8)]
    kind: EnumValue<Kind>,
    #[proto(tag = 9)]
    packed: Vec<u64>,
    #[proto(tag = 10, packed = false)]
    unpacked: Vec<i32>,
    #[proto(tag = 11)]
    names: Vec<String>,
    #[proto(tag = 12)]
    child: Option<Box<Everything>>,
    #[proto(tag = 13)]
    children: Vec<Everything>,
    #[proto(tag = 14)]
    map: BTreeMap<String, Everything>,
    #[proto(tag = 15, map(key = "sint32", value = "fixed64"))]
    nums: BTreeMap<i32, u64>,
    #[proto(tag = 16)]
    maybe: Option<f32>,
    #[proto(oneof)]
    pick: Option<Pick>,
    #[proto(unknown)]
    unknown: UnknownFields,
}

fuzz_target!(|data: &[u8]| {
    let limits = Limits::new().with_max_entries(1 << 20);
    if let Ok(m) = Everything::decode_with(data, limits) {
        let once = m.encode_to_vec();
        assert_eq!(once.len(), m.encoded_len(), "encoded_len disagrees with encode");
        let again = Everything::decode(&once).expect("re-encoded bytes must decode");
        assert_eq!(again.encode_to_vec(), once, "re-encoding is not stable");
    }
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = idl::parse("fuzz.proto", text);
    }
});
