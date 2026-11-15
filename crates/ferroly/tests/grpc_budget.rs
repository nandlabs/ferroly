#![cfg(feature = "grpc")]
//! PB-1 over gRPC: a 4 MiB request of empty sub-messages (`0a 00`, two
//! bytes each on the wire, one wide struct each in memory) is rejected with
//! `RESOURCE_EXHAUSTED` by the server's decoded-memory budget, with peak
//! memory near that budget instead of the gigabyte it expands to without
//! one. Measured with a counting global allocator, so this file holds one
//! test.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use ferroly::grpc::{GrpcServer, Request, Response};
use ferroly::http::{Client, Method};
use ferroly::proto::UnknownFields;
use tokio::net::TcpListener;

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

const MIB: usize = 1024 * 1024;

/// About 480 bytes in memory, two on the wire when empty.
#[derive(ferroly::proto::Message, Clone, Default, Debug)]
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
    #[proto(unknown)]
    unknown: UnknownFields,
}

#[derive(ferroly::proto::Message, Default, Debug)]
struct Batch {
    #[proto(tag = 1)]
    items: Vec<Wide>,
}

#[tokio::test(flavor = "current_thread")]
async fn an_amplifying_request_is_rejected_with_bounded_memory() {
    assert!(std::mem::size_of::<Wide>() >= 400);
    let server = GrpcServer::new().unary("/t.S/Batch", |req: Request<Batch>| async move {
        let n = req.into_inner().items.len() as u32;
        Ok(Response::new(Batch {
            items: vec![Wide::default(); n.min(1) as usize],
        }))
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(server.serve(listener, std::future::pending()));
    let client = Client::builder()
        .http2_prior_knowledge(true)
        .build()
        .unwrap();

    // Just under the default 4 MiB message limit.
    let payload_len = 4 * MIB - 16;
    let mut frame = vec![0];
    frame.extend_from_slice(&(payload_len as u32).to_be_bytes());
    frame.extend((0..payload_len / 2).flat_map(|_| [0x0a, 0x00]));
    let request = |body: Vec<u8>| {
        ferroly::http::Request::builder(Method::Post, &format!("http://{addr}/t.S/Batch"))
            .unwrap()
            .header("content-type", "application/grpc")
            .header("te", "trailers")
            .body(body)
            .build()
    };

    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let mut resp = client.send(request(frame)).await.unwrap();
    while resp.chunk().await.unwrap().is_some() {}
    let status = resp
        .trailers()
        .and_then(|t| t.get("grpc-status").map(str::to_string))
        .or_else(|| resp.headers().get("grpc-status").map(str::to_string));
    let peak = PEAK.load(Ordering::Relaxed) - base;

    assert_eq!(status.as_deref(), Some("8"), "RESOURCE_EXHAUSTED");
    let message = resp.headers().get("grpc-message").unwrap_or("");
    assert!(message.contains("max_decoded_bytes"), "{message}");
    // The budget is 64 MiB (16 x the 4 MiB limit); the request body is held
    // a few times over on its way. Without the budget this is about 1 GB.
    assert!(peak < 112 * MIB, "peak {} MiB", peak / MIB);

    // A legitimate batch of the same kind still decodes.
    let mut small = vec![0];
    let body: Vec<u8> = (0..1000).flat_map(|_| [0x0a, 0x00]).collect();
    small.extend_from_slice(&(body.len() as u32).to_be_bytes());
    small.extend(body);
    let mut resp = client.send(request(small)).await.unwrap();
    while resp.chunk().await.unwrap().is_some() {}
    let status = resp
        .trailers()
        .and_then(|t| t.get("grpc-status").map(str::to_string));
    assert_eq!(status.as_deref(), Some("0"));
}
