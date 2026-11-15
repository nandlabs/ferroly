#![cfg(feature = "turbo")]
//! Schema validation of a hostile request body stays bounded (MISC-TV-1): an
//! 8 MiB body of failing array items yields a small `422`, and validating it
//! allocates little beyond the decoded `Value` itself.
//!
//! The test binary counts allocations with its own global allocator, so the
//! peak heap use of each step can be compared.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use ferroly::codec::schema::{Schema, DEFAULT_MAX_VIOLATIONS};
use ferroly::codec::{json, Decode};
use ferroly::http::{HttpResponse, Method, Request, Uri};
use ferroly::turbo::Ctx;

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// Test-only: a counting wrapper around the system allocator, which is the
// one way to observe peak heap use. Each method forwards its arguments to
// `System` unchanged, so it upholds `GlobalAlloc`'s contract exactly as
// `System` does.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Runs `f`, returning its result and the peak heap growth while it ran.
fn peak_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = f();
    (out, PEAK.load(Ordering::Relaxed).saturating_sub(base))
}

fn ctx_with_body(body: Vec<u8>) -> Ctx {
    let mut request = Request::new(Method::Post, Uri::parse("http://127.0.0.1/items").unwrap());
    request.headers.set("content-type", "application/json");
    request.body = body;
    Ctx::from_request(request).unwrap()
}

/// `[1,1,1,…]` (or `["","",…]`), just under 8 MiB.
fn hostile_array(item: &str) -> Vec<u8> {
    let limit = 8 * 1024 * 1024 - 2;
    let count = limit / (item.len() + 1);
    let mut body = Vec::with_capacity(limit + 2);
    body.push(b'[');
    for i in 0..count {
        if i > 0 {
            body.push(b',');
        }
        body.extend_from_slice(item.as_bytes());
    }
    body.push(b']');
    body
}

// Both scenarios run in one test: the allocation counters are process-wide,
// and the test harness runs separate tests on parallel threads.
#[test]
fn an_8_mib_body_of_failing_items_yields_a_small_bounded_422() {
    let email = r"^[a-z0-9._%+-]{1,64}@[a-z0-9-]{1,63}(?:\.[a-z0-9-]{1,63})*\.[a-z]{2,24}$";
    let cases = [
        (r#"{"items":{"type":"string"}}"#.to_string(), "1"),
        (
            format!(
                r#"{{"items":{{"type":"string","pattern":{}}}}}"#,
                json::encode(email)
            ),
            r#""""#,
        ),
    ];
    for (schema_src, item) in cases {
        let schema = Schema::compile(&json::from_str(&schema_src).unwrap()).unwrap();
        let body = hostile_array(item);
        let body_len = body.len();

        // What decoding alone costs (the request body becomes a Value tree).
        let ctx = ctx_with_body(body);
        let (decoded, decode_peak) = peak_during(|| ctx.read::<ferroly::codec::Value>().unwrap());
        drop(decoded);

        let started = std::time::Instant::now();
        let (problem, validate_peak) = peak_during(|| ctx.validate_with(&schema).unwrap_err());
        let elapsed = started.elapsed();
        let response = HttpResponse::from(problem);
        assert_eq!(response.status.as_u16(), 422);

        // A small response: the errors array is capped and marked truncated.
        assert!(
            response.body.len() < 64 * 1024,
            "{} byte response for a {body_len} byte body",
            response.body.len()
        );
        let report = json::from_slice(&response.body).unwrap();
        let errors = report.get("errors").and_then(|e| e.as_array()).unwrap();
        assert_eq!(errors.len(), DEFAULT_MAX_VIOLATIONS);
        assert_eq!(
            report.get("truncated"),
            Some(&ferroly::codec::Value::Bool(true))
        );

        // Bounded memory: validating adds at most a few MiB to the decode.
        assert!(
            validate_peak <= decode_peak + 4 * 1024 * 1024,
            "validation peaked at {validate_peak} bytes vs {decode_peak} for decoding alone"
        );
        // And it is quick (the report measured 2.7 s and a 219-514 MB body);
        // generous, as debug builds are slow.
        assert!(elapsed.as_secs() < 30, "validation took {elapsed:?}");
    }
}

#[test]
fn read_valid_decodes_the_body_once() {
    // MISC-TV-2: `read_valid*` used to parse the body twice. Decoding goes
    // through the validated `Value`, by the rules of the body's format.
    #[derive(Decode, Debug, PartialEq)]
    struct Item {
        qty: u32,
    }
    let schema = Schema::compile(&json::from_str(r#"{"required":["qty"]}"#).unwrap()).unwrap();

    let ctx = ctx_with_body(br#"{"qty": 3}"#.to_vec());
    assert_eq!(
        ctx.read_valid_with::<Item>(&schema).unwrap(),
        Item { qty: 3 }
    );

    // JSON stays strict: a string is not a number, even after validation.
    let ctx = ctx_with_body(br#"{"qty": "3"}"#.to_vec());
    let problem = ctx.read_valid_with::<Item>(&schema).unwrap_err();
    assert_eq!(HttpResponse::from(problem).status.as_u16(), 400);

    // YAML stays lenient.
    let mut request = Request::new(Method::Post, Uri::parse("http://127.0.0.1/").unwrap());
    request.headers.set("content-type", "application/yaml");
    request.body = b"qty: \"3\"\n".to_vec();
    let ctx = Ctx::from_request(request).unwrap();
    let schema_value = json::from_str(r#"{"required":["qty"]}"#).unwrap();
    assert_eq!(
        ctx.read_valid::<Item>(&schema_value).unwrap(),
        Item { qty: 3 }
    );
}

#[test]
fn read_valid_rejects_wide_integer_literals_never_rounding_them() {
    // CORE-15 (documented limit): decoding from the validated `Value` sees an
    // integer literal wider than 64 bits as a float, so `read_valid` rejects
    // it where `read` decodes it exactly; a decimal string works for both.
    #[derive(Decode, Debug, PartialEq)]
    struct Tx {
        amount: u128,
    }
    let schema = Schema::compile(&json::from_str(r#"{"required":["amount"]}"#).unwrap()).unwrap();
    let wide = br#"{"amount": 340282366920938463463374607431768211455}"#.to_vec();

    let ctx = ctx_with_body(wide.clone());
    let problem = ctx.read_valid_with::<Tx>(&schema).unwrap_err();
    assert_eq!(HttpResponse::from(problem).status.as_u16(), 400);
    assert_eq!(
        ctx_with_body(wide).read::<Tx>().unwrap(),
        Tx { amount: u128::MAX }
    );

    let ctx = ctx_with_body(br#"{"amount": "340282366920938463463374607431768211455"}"#.to_vec());
    assert_eq!(
        ctx.read_valid_with::<Tx>(&schema).unwrap(),
        Tx { amount: u128::MAX }
    );
    // Within 64 bits the literal is exact on both paths.
    let ctx = ctx_with_body(br#"{"amount": 18446744073709551615}"#.to_vec());
    assert_eq!(
        ctx.read_valid_with::<Tx>(&schema).unwrap(),
        Tx {
            amount: u128::from(u64::MAX)
        }
    );
}
