#![cfg(feature = "codec")]
//! A successful decode allocates nothing for error paths.
//!
//! The derive and the std container impls locate a failure with `map_err`,
//! which runs only on the error path. This binary installs a counting global
//! allocator (it is its own test binary so the allocator affects nothing else)
//! and checks that decoding allocates exactly what the decoded value itself
//! needs: nothing for scalar-only shapes, and the same as building the value
//! by hand for strings, vectors and maps.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;

use ferroly::codec::{from_value_strict, json, Decode};

struct Counting;

thread_local! {
    // Per thread, so the harness's own threads do not disturb a measurement.
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

// A test-only allocator, as in `grpc_budget.rs`.
// SAFETY: every method forwards to `System` unchanged; the only addition is
// bumping a thread-local counter, which neither allocates nor unwinds.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.with(|n| n.set(n.get() + 1));
        System.alloc(layout)
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.with(|n| n.set(n.get() + 1));
        System.alloc_zeroed(layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.with(|n| n.set(n.get() + 1));
        System.realloc(ptr, layout, new_size)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// How many allocations (including reallocations) `f` makes on this thread.
fn allocations<R>(f: impl FnOnce() -> R) -> (usize, R) {
    let before = ALLOCATIONS.with(Cell::get);
    let out = f();
    (ALLOCATIONS.with(Cell::get) - before, out)
}

#[derive(Decode, Debug, PartialEq)]
#[ferroly(rename_all = "camelCase")]
struct Inner {
    max_tokens: u32,
    offset: i64,
    enabled: bool,
    ratio: Option<f64>,
}

#[derive(Decode, Debug, PartialEq)]
struct Id(u32);

#[derive(Decode, Debug, PartialEq)]
struct Pair(u8, i16);

#[derive(Decode, Debug, PartialEq)]
enum Shape {
    Circle { r: f64 },
    Line(u8, u8),
}

#[derive(Decode, Debug, PartialEq)]
struct Scalars {
    inner: Inner,
    id: Id,
    pair: Pair,
    shape: Shape,
    tuple: (u8, bool),
    array: [u16; 3],
    missing: Option<u8>,
}

const SCALARS: &str = r#"{
    "inner": {"maxTokens": 5, "offset": -3, "enabled": true, "ratio": 0.5},
    "id": 7,
    "pair": [1, -2],
    "shape": {"Line": [3, 4]},
    "tuple": [9, false],
    "array": [1, 2, 3],
    "missing": null
}"#;

#[derive(Decode, Debug, PartialEq)]
struct Message {
    role: String,
    tokens: Vec<u32>,
}

#[derive(Decode, Debug, PartialEq)]
struct Chat {
    messages: Vec<Message>,
    counts: BTreeMap<String, u32>,
}

#[test]
fn successful_decodes_allocate_only_for_the_value() {
    // Warm up anything lazily initialised on first use.
    let _ = json::decode::<Scalars>(SCALARS).unwrap();
    let tree = json::from_str(SCALARS).unwrap();

    // Scalar-only shapes: named, renamed, tuple and newtype structs, an
    // externally tagged enum, tuples, arrays and options allocate nothing on
    // either path. (`[T; N]` decodes through a `Vec`, so it costs exactly one.)
    let (n, _) = allocations(|| json::decode::<Scalars>(SCALARS).unwrap());
    assert_eq!(n, 1, "streaming decode of scalar shapes");
    let (n, _) = allocations(|| Scalars::decode(&tree).unwrap());
    assert_eq!(n, 1, "value decode of scalar shapes");
    let (n, _) = allocations(|| from_value_strict::<Scalars>(&tree).unwrap());
    assert_eq!(n, 1, "strict value decode of scalar shapes");
    let (n, _) = allocations(|| {
        json::decode::<Inner>(r#"{"maxTokens":1,"offset":2,"enabled":false}"#).unwrap()
    });
    assert_eq!(n, 0, "a struct of scalars");

    // Strings, vectors and maps: the decode allocates exactly what building
    // the same value by hand does (pushing element by element, as the
    // streaming decoder does).
    let text = r#"{"messages":[{"role":"user","tokens":[1,2,3]},{"role":"bot","tokens":[4]}],
                   "counts":{"a":1,"b":2}}"#;
    let _ = json::decode::<Chat>(text).unwrap();
    let (decoded, chat) = allocations(|| json::decode::<Chat>(text).unwrap());
    #[allow(clippy::vec_init_then_push)]
    let (by_hand, built) = allocations(|| {
        let mut messages = Vec::new();
        let mut tokens = Vec::new();
        tokens.push(1);
        tokens.push(2);
        tokens.push(3);
        messages.push(Message {
            role: String::from("user"),
            tokens,
        });
        let mut tokens = Vec::new();
        tokens.push(4);
        messages.push(Message {
            role: String::from("bot"),
            tokens,
        });
        let mut counts = BTreeMap::new();
        counts.insert(String::from("a"), 1);
        counts.insert(String::from("b"), 2);
        Chat { messages, counts }
    });
    assert_eq!(chat, built);
    assert_eq!(decoded, by_hand, "streaming decode vs building by hand");

    // On the `Value` path too (no tree is built: the tree is borrowed).
    let tree = json::from_str(text).unwrap();
    let (n, _) = allocations(|| Chat::decode(&tree).unwrap());
    // The `Value` path collects each container from an iterator (a vector in
    // one allocation each; a `BTreeMap` through a sorted staging vector), so
    // the hand-built baseline collects too.
    let (by_hand, built) = allocations(|| {
        let tokens: Vec<u32> = [1, 2, 3].into_iter().collect();
        let first = Message {
            role: String::from("user"),
            tokens,
        };
        let tokens: Vec<u32> = [4].into_iter().collect();
        let second = Message {
            role: String::from("bot"),
            tokens,
        };
        let messages: Vec<Message> = [first, second].into_iter().collect();
        let counts: BTreeMap<String, u32> = [(String::from("a"), 1), (String::from("b"), 2)]
            .into_iter()
            .collect();
        Chat { messages, counts }
    });
    assert_eq!(chat, built);
    assert_eq!(n, by_hand, "value decode vs collecting by hand");

    // A failure, by contrast, does allocate its path.
    let bad = r#"{"messages":[{"role":7,"tokens":[]}],"counts":{}}"#;
    let (n, err) = allocations(|| json::decode::<Chat>(bad).unwrap_err());
    assert!(n > 0);
    assert_eq!(err.path_string(), "messages[0].role");
}
