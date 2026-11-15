#![cfg(feature = "hash")]
//! The wipe-on-drop of hasher, HMAC and `Secret` state, observed from the
//! outside: a global allocator looks at each watched heap block when it is
//! freed, after `Drop` ran, so an optimiser that elided the wiping stores
//! as dead would make this fail. The stores are most at risk in an
//! optimised build, so run it with `--release` too:
//!
//! ```text
//! cargo test -p ferroly --release --features hash --test hash_wipe
//! ```
//!
//! The watched block is global state, so this file holds one test.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

use ferroly::hash::{Hmac, Md5, Secret, Sha1, Sha256, Sha384, Sha512};

/// The address of the block to inspect when it is freed (0: none).
static WATCH: AtomicUsize = AtomicUsize::new(0);
/// Non-zero bytes the watched block held when it was freed.
static NONZERO_AT_FREE: AtomicUsize = AtomicUsize::new(usize::MAX);

struct Inspecting;

// A test-only allocator: it forwards to `System` unchanged and only reads a
// block it is about to free, so the safety contract is `System`'s.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for Inspecting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Zeroed, so struct padding starts out as zeros rather than as
        // whatever an earlier allocation left there.
        System.alloc_zeroed(layout)
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        if p as usize == WATCH.load(SeqCst) {
            let bytes = std::slice::from_raw_parts(p, layout.size());
            NONZERO_AT_FREE.store(bytes.iter().filter(|b| **b != 0).count(), SeqCst);
            WATCH.store(0, SeqCst);
        }
        System.dealloc(p, layout)
    }
}

#[global_allocator]
static ALLOC: Inspecting = Inspecting;

/// Struct padding may pick up a stray byte when a value is moved; key
/// material that survived would be far more than this.
const PADDING_ALLOWANCE: usize = 16;

/// The non-zero bytes of `len` bytes at `p` (the live value, before drop).
fn nonzero(p: *const u8, len: usize) -> usize {
    #[allow(unsafe_code)]
    // SAFETY: `p` points to a live, initialised allocation of `len` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(p, len) };
    bytes.iter().filter(|b| **b != 0).count()
}

/// Drops `value` and returns (non-zero bytes before, non-zero bytes at free).
fn observe<T>(value: Box<T>) -> (usize, usize) {
    let p = &*value as *const T as *const u8;
    let before = nonzero(p, std::mem::size_of::<T>());
    NONZERO_AT_FREE.store(usize::MAX, SeqCst);
    WATCH.store(p as usize, SeqCst);
    drop(std::hint::black_box(value));
    (before, NONZERO_AT_FREE.load(SeqCst))
}

fn check<T>(name: &str, value: Box<T>) {
    let (before, after) = observe(value);
    // The state really held data, so a missing wipe would show.
    assert!(
        before > 2 * PADDING_ALLOWANCE,
        "{name}: {before} non-zero bytes before drop"
    );
    assert!(
        after <= PADDING_ALLOWANCE,
        "{name}: {after} of {} bytes still non-zero when freed",
        std::mem::size_of::<T>()
    );
}

#[test]
fn key_state_is_zero_when_its_memory_is_freed() {
    let key = std::hint::black_box([0xA5u8; 40]);

    let mut h = Box::new(Sha256::new());
    h.update(&key);
    check("Sha256", h);
    let mut h = Box::new(Sha384::new());
    h.update(&key);
    check("Sha384", h);
    let mut h = Box::new(Sha512::new());
    h.update(&key);
    check("Sha512", h);
    let mut h = Box::new(Sha1::new());
    h.update(&key);
    check("Sha1", h);
    let mut h = Box::new(Md5::new());
    h.update(&key);
    check("Md5", h);

    let mut m = Box::new(Hmac::<Sha256>::new(&key));
    m.update(b"message");
    check("Hmac<Sha256>", m);
    let mut m = Box::new(Hmac::<Sha512>::new(&key));
    m.update(b"message");
    check("Hmac<Sha512>", m);

    // `Secret`: the heap buffer it owns.
    let bytes = vec![0xA5u8; 64];
    let p = bytes.as_ptr();
    let secret = Secret::new(bytes);
    assert_eq!(nonzero(p, 64), 64);
    NONZERO_AT_FREE.store(usize::MAX, SeqCst);
    WATCH.store(p as usize, SeqCst);
    std::hint::black_box(secret.expose());
    drop(secret);
    assert_eq!(NONZERO_AT_FREE.load(SeqCst), 0, "Secret");
}
