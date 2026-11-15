#![cfg(feature = "grpc")]
//! RPC-4: rapid reset against `GrpcServer::serve`, whose
//! `rapid_reset_threshold` is 10 ms. A client that opens batches of calls
//! and resets each one just after the threshold (11 ms) used to start
//! thousands of handlers per second without ever being stopped; the
//! cancel budget (`max_cancels_per_window`) now ends the connection with
//! `GOAWAY(ENHANCE_YOUR_CALM)`, as resets right after HEADERS always did.

use std::time::{Duration, Instant};

use ferroly::grpc::{GrpcServer, Request, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(ferroly::proto::Message, Default, Debug)]
struct M {
    #[proto(tag = 1)]
    n: u32,
}

const ENHANCE_YOUR_CALM: u32 = 0xb;

fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let len = payload.len();
    let mut out = vec![(len >> 16) as u8, (len >> 8) as u8, len as u8, kind, flags];
    out.extend_from_slice(&stream.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// A literal HPACK field without indexing: an indexed name (`first`) or a
/// new one.
fn lit(out: &mut Vec<u8>, first: u8, name: Option<&str>, value: &str) {
    out.push(first);
    if let Some(n) = name {
        out.push(n.len() as u8);
        out.extend_from_slice(n.as_bytes());
    }
    out.push(value.len() as u8);
    out.extend_from_slice(value.as_bytes());
}

fn headers() -> Vec<u8> {
    // :method POST, :scheme http.
    let mut h = vec![0x83, 0x86];
    lit(&mut h, 0x04, None, "/t.S/Work");
    lit(&mut h, 0x01, None, "x");
    lit(&mut h, 0x00, Some("content-type"), "application/grpc");
    lit(&mut h, 0x00, Some("te"), "trailers");
    h
}

/// Runs the attack for up to `secs` seconds; returns the GOAWAY code the
/// server sent, if any, and how many calls were opened.
async fn attack(hold: Duration, secs: u64) -> (Option<u32>, usize) {
    let server = GrpcServer::new().unary("/t.S/Work", |_req: Request<M>| async {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok(Response::new(M { n: 1 }))
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(server.serve(listener, std::future::pending()));

    let tcp = TcpStream::connect(addr).await.unwrap();
    let (mut r, mut w) = tcp.into_split();
    let (goaway_tx, mut goaway) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut chunk = vec![0u8; 65536];
        loop {
            let n = match r.read(&mut chunk).await {
                Ok(0) | Err(_) => {
                    let _ = goaway_tx.send(None);
                    return;
                }
                Ok(n) => n,
            };
            buf.extend_from_slice(&chunk[..n]);
            while buf.len() >= 9 {
                let len = (buf[0] as usize) << 16 | (buf[1] as usize) << 8 | buf[2] as usize;
                if buf.len() < 9 + len {
                    break;
                }
                if buf[3] == 0x7 {
                    let code = u32::from_be_bytes(buf[13..17].try_into().unwrap());
                    let _ = goaway_tx.send(Some(code));
                    return;
                }
                buf.drain(..9 + len);
            }
        }
    });
    let mut out = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
    out.extend(frame(4, 0, 0, &[]));
    out.extend(frame(4, 1, 0, &[]));
    w.write_all(&out).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let h = headers();
    // A gRPC message: uncompressed, 2 bytes, field 1 = 1.
    let body = [0u8, 0, 0, 0, 2, 0x08, 0x01];
    let started = Instant::now();
    let mut id = 1u32;
    let mut sent = 0usize;
    while started.elapsed() < Duration::from_secs(secs) {
        if let Ok(code) = goaway.try_recv() {
            return (code, sent);
        }
        let mut out = Vec::new();
        let first = id;
        for _ in 0..100 {
            out.extend(frame(1, 4, id, &h));
            out.extend(frame(0, 1, id, &body));
            id += 2;
        }
        if w.write_all(&out).await.is_err() {
            break;
        }
        sent += 100;
        tokio::time::sleep(hold).await;
        let mut out = Vec::new();
        for s in (first..id).step_by(2) {
            out.extend(frame(3, 0, s, &8u32.to_be_bytes()));
        }
        if w.write_all(&out).await.is_err() {
            break;
        }
        // Let the server free the slots.
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let code = tokio::time::timeout(Duration::from_secs(2), goaway)
        .await
        .ok()
        .and_then(Result::ok)
        .flatten();
    (code, sent)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn holding_calls_just_past_the_threshold_still_ends_in_goaway() {
    for hold in [11, 15] {
        let (code, sent) = attack(Duration::from_millis(hold), 5).await;
        assert_eq!(
            code,
            Some(ENHANCE_YOUR_CALM),
            "hold {hold} ms: no GOAWAY after {sent} calls"
        );
        // The budget is 1000 cancels per 10 s; allow for in-flight batches.
        assert!(
            sent <= 2000,
            "hold {hold} ms: {sent} calls before the GOAWAY"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resets_right_after_headers_still_end_in_goaway() {
    let (code, sent) = attack(Duration::ZERO, 5).await;
    assert_eq!(
        code,
        Some(ENHANCE_YOUR_CALM),
        "no GOAWAY after {sent} calls"
    );
}
