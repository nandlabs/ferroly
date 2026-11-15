#![cfg(feature = "protobuf")]
//! The decoded-memory budget (`Limits::max_decoded_bytes`): hostile inputs
//! that are small on the wire but large in memory are rejected before they
//! are allocated, measured with a counting global allocator.
//!
//! Everything runs in one test function, so no other test allocates while a
//! peak is being measured.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};

use ferroly::proto::{Limits, Message, ProtoError, UnknownFields};

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// A test-only allocator: it forwards to `System` unchanged and only counts
// bytes, so the safety contract is `System`'s.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc(layout);
        if !p.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        System.dealloc(p, layout);
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Runs `f` and returns its result with the peak memory it allocated beyond
/// what was live when it started.
fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = f();
    (out, PEAK.load(Ordering::Relaxed) - base)
}

/// A realistic wide message: about 480 bytes in memory, two on the wire when
/// empty.
#[derive(Message, Default, Debug)]
struct Wide {
    #[proto(tag = 1)]
    a1: String,
    #[proto(tag = 2)]
    a2: String,
    #[proto(tag = 3)]
    a3: String,
    #[proto(tag = 4)]
    a4: String,
    #[proto(tag = 5)]
    a5: String,
    #[proto(tag = 6)]
    a6: String,
    #[proto(tag = 7)]
    a7: String,
    #[proto(tag = 8)]
    a8: String,
    #[proto(tag = 9)]
    a9: Vec<String>,
    #[proto(tag = 10)]
    a10: Vec<String>,
    #[proto(tag = 11)]
    a11: Vec<i64>,
    #[proto(tag = 12)]
    a12: Vec<u64>,
    #[proto(tag = 13)]
    a13: Option<String>,
    #[proto(tag = 14)]
    a14: Option<String>,
    #[proto(tag = 15)]
    a15: Option<Vec<u8>>,
    #[proto(tag = 16)]
    a16: Option<Vec<u8>>,
    #[proto(tag = 17)]
    a17: BTreeMap<String, String>,
    #[proto(tag = 18)]
    a18: HashMap<String, String>,
    #[proto(unknown)]
    unknown: UnknownFields,
}

#[derive(Message, Default, Debug)]
struct Top {
    #[proto(tag = 1)]
    items: Vec<Wide>,
    #[proto(tag = 2)]
    map: BTreeMap<u32, Wide>,
    #[proto(tag = 3)]
    boxed: Option<Box<Wide>>,
    #[proto(unknown)]
    unknown: UnknownFields,
}

/// A map whose entries are small, so the hash table's spare buckets and
/// control bytes dominate.
#[derive(Message, Default, Debug)]
struct Hashed {
    #[proto(tag = 1)]
    m: HashMap<u32, bool>,
}

const MIB: usize = 1024 * 1024;

fn repeat(pattern: &[u8], total: usize) -> Vec<u8> {
    pattern.iter().copied().cycle().take(total).collect()
}

fn is_budget_error(e: &ProtoError) -> bool {
    matches!(e, ProtoError::LimitExceeded(m) if m.contains("max_decoded_bytes"))
}

#[test]
fn hostile_inputs_stay_within_the_decoded_bytes_budget() {
    assert!(
        std::mem::size_of::<Wide>() >= 400,
        "the struct is realistic"
    );

    // The reported attack: 4 MiB of empty sub-messages (`0a 00`), two
    // million `Wide::default()`s, about 1 GB without a memory budget.
    let empty_items = repeat(&[0x0a, 0x00], 4 * MIB);

    // With a gRPC-sized budget (16 MiB), the decode fails early and the peak
    // stays near the budget.
    let limits = Limits::new().with_max_decoded_bytes(16 * MIB);
    let (result, peak) = peak_of(|| Top::decode_with(&empty_items, limits).map(drop));
    let err = result.unwrap_err();
    assert!(is_budget_error(&err), "{err}");
    assert!(peak <= 24 * MIB, "peak {} MiB", peak / MIB);

    // The default budget (256 MiB) also stops it, well short of 1 GB.
    let (result, peak) = peak_of(|| Top::decode(&empty_items).map(drop));
    assert!(is_budget_error(&result.unwrap_err()));
    assert!(
        peak <= Limits::DEFAULT_MAX_DECODED_BYTES * 3 / 2,
        "peak {} MiB",
        peak / MIB
    );

    // Map entries with distinct keys and empty message values
    // (`12 len 08 key 12 00`): one entry per seven bytes or so.
    let mut map_entries = Vec::new();
    let mut key = 0u64;
    while map_entries.len() < 4 * MIB {
        let mut entry = vec![0x08];
        ferroly::proto::wire::encode_varint(key, &mut entry);
        entry.extend_from_slice(&[0x12, 0x00]);
        map_entries.push(0x12);
        map_entries.push(entry.len() as u8);
        map_entries.extend_from_slice(&entry);
        key += 1;
    }
    let (result, peak) = peak_of(|| Top::decode_with(&map_entries, limits).map(drop));
    assert!(is_budget_error(&result.unwrap_err()));
    assert!(peak <= 24 * MIB, "peak {} MiB", peak / MIB);

    // Small hash-map entries (`0a len 08 key 10 01`): the table peaks at
    // about three slots and control bytes per entry while it grows.
    let mut small_entries = Vec::new();
    let mut key = 0u64;
    while small_entries.len() < 4 * MIB {
        let mut entry = vec![0x08];
        ferroly::proto::wire::encode_varint(key, &mut entry);
        entry.extend_from_slice(&[0x10, 0x01]);
        small_entries.push(0x0a);
        small_entries.push(entry.len() as u8);
        small_entries.extend_from_slice(&entry);
        key += 1;
    }
    let small = Limits::new().with_max_decoded_bytes(6 * MIB);
    let (result, peak) = peak_of(|| Hashed::decode_with(&small_entries, small).map(drop));
    assert!(is_budget_error(&result.unwrap_err()));
    assert!(peak <= 9 * MIB, "peak {} MiB", peak / MIB);

    // Unknown fields: `08 00` keeps a 40-odd byte record per two bytes.
    let unknown = repeat(&[0x20, 0x00], 4 * MIB);
    let (result, peak) = peak_of(|| Top::decode_with(&unknown, limits).map(drop));
    assert!(is_budget_error(&result.unwrap_err()));
    assert!(peak <= 24 * MIB, "peak {} MiB", peak / MIB);

    // A repeated packed varint field: one byte becomes an eight-byte `i64`.
    let mut packed = vec![0x0a, 0x85, 0x80, 0x80, 0x02]; // items: len 4 MiB + 5
    packed.extend_from_slice(&[0x5a, 0x80, 0x80, 0x80, 0x02]); // a11: packed, 4 MiB
    packed.extend(std::iter::repeat(0x01).take(4 * MIB));
    let (result, peak) = peak_of(|| Top::decode_with(&packed, limits).map(drop));
    assert!(is_budget_error(&result.unwrap_err()));
    assert!(peak <= 24 * MIB, "peak {} MiB", peak / MIB);

    // Legitimate messages fit: 10k wide messages with a few fields set, and a
    // boxed sub-message.
    let mut top = Top::default();
    for i in 0..10_000u32 {
        top.items.push(Wide {
            a1: format!("item {i}"),
            a11: vec![i64::from(i); 4],
            ..Wide::default()
        });
    }
    top.boxed = Some(Box::default());
    let bytes = top.encode_to_vec();
    let back = Top::decode_with(&bytes, Limits::new().with_max_decoded_bytes(16 * MIB)).unwrap();
    assert_eq!(back.items.len(), 10_000);
    assert!(back.boxed.is_some());

    // A budget smaller than the decoded value rejects it, deterministically.
    let tight = Limits::new().with_max_decoded_bytes(64 * 1024);
    assert!(is_budget_error(
        &Top::decode_with(&bytes, tight).unwrap_err()
    ));
}
