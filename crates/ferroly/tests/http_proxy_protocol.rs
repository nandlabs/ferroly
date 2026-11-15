#![cfg(feature = "http")]
//! PROXY protocol v1 and v2 on the server listener: a trusted balancer's
//! header sets the peer address handlers see; untrusted sources are served
//! directly and cannot spoof one; a trusted source without a valid header is
//! dropped and reported.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::http::{
    serve_with_config, BoxFuture, ErrorHook, HttpError, HttpHandler, HttpResponse, ProxyProtocol,
    Request, ServerConfig, StatusCode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Answers with the peer and local addresses it was given.
struct Addrs;
impl HttpHandler for Addrs {
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
        Box::pin(async move {
            let peer = req.peer_addr().map(|a| a.to_string()).unwrap_or_default();
            let local = req.local_addr().map(|a| a.to_string()).unwrap_or_default();
            HttpResponse::text(StatusCode::OK, format!("peer={peer} local={local}"))
        })
    }
}

type Errors = Arc<Mutex<Vec<String>>>;

async fn start(pp: ProxyProtocol) -> (SocketAddr, Errors) {
    let errors: Errors = Arc::default();
    let seen = errors.clone();
    let config = ServerConfig {
        proxy_protocol: Some(pp),
        head_timeout: Duration::from_millis(500),
        on_error: Some(ErrorHook::new(move |_, e: &HttpError| {
            seen.lock().unwrap().push(e.to_string());
        })),
        ..ServerConfig::default()
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve_with_config(
            listener,
            Arc::new(Addrs),
            std::future::pending::<()>(),
            config,
        )
        .await;
    });
    (addr, errors)
}

/// Sends `prefix` then one GET, and returns the whole answer.
async fn exchange(addr: SocketAddr, prefix: &[u8]) -> String {
    let mut sock = TcpStream::connect(addr).await.unwrap();
    let mut out = prefix.to_vec();
    out.extend_from_slice(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    sock.write_all(&out).await.unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), sock.read_to_end(&mut buf)).await;
    String::from_utf8_lossy(&buf).into_owned()
}

fn local_trusted() -> ProxyProtocol {
    ProxyProtocol::from_cidrs(["127.0.0.0/8", "::1"]).unwrap()
}

fn v2(command: u8, family: u8, block: &[u8]) -> Vec<u8> {
    let mut out = b"\r\n\r\n\0\r\nQUIT\n".to_vec();
    out.push(0x20 | command);
    out.push(family);
    out.extend_from_slice(&(block.len() as u16).to_be_bytes());
    out.extend_from_slice(block);
    out
}

#[tokio::test]
async fn v1_header_from_a_trusted_balancer_sets_the_client_address() {
    let (addr, _) = start(local_trusted()).await;
    let resp = exchange(addr, b"PROXY TCP4 203.0.113.9 198.51.100.1 40000 443\r\n").await;
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(
        resp.ends_with("peer=203.0.113.9:40000 local=198.51.100.1:443"),
        "{resp}"
    );
    let resp = exchange(addr, b"PROXY TCP6 2001:db8::9 2001:db8::1 40001 8443\r\n").await;
    assert!(
        resp.ends_with("peer=[2001:db8::9]:40001 local=[2001:db8::1]:8443"),
        "{resp}"
    );
}

#[tokio::test]
async fn v2_header_from_a_trusted_balancer_sets_the_client_address() {
    let (addr, _) = start(local_trusted()).await;
    let mut block = vec![203, 0, 113, 9, 198, 51, 100, 1];
    block.extend_from_slice(&40000u16.to_be_bytes());
    block.extend_from_slice(&443u16.to_be_bytes());
    block.extend_from_slice(&[0x03, 0x00, 0x04, 1, 2, 3, 4]); // a CRC32C TLV, ignored
    let resp = exchange(addr, &v2(0x1, 0x11, &block)).await;
    assert!(
        resp.ends_with("peer=203.0.113.9:40000 local=198.51.100.1:443"),
        "{resp}"
    );

    // LOCAL (a health check) keeps the socket's own addresses.
    let resp = exchange(addr, &v2(0x0, 0x00, &[])).await;
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(resp.contains("peer=127.0.0.1:"), "{resp}");
}

#[tokio::test]
async fn an_untrusted_source_is_served_directly_and_cannot_spoof() {
    let (addr, _) = start(ProxyProtocol::from_cidrs(["10.0.0.0/8"]).unwrap()).await;
    // A plain request from an untrusted address is served with its socket
    // address.
    let resp = exchange(addr, b"").await;
    assert!(resp.contains("peer=127.0.0.1:"), "{resp}");
    // A PROXY line from it is not believed: it is just a bad request.
    let resp = exchange(addr, b"PROXY TCP4 203.0.113.9 198.51.100.1 40000 443\r\n").await;
    assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
    assert!(!resp.contains("203.0.113.9"), "{resp}");
}

#[tokio::test]
async fn a_trusted_source_without_a_valid_header_is_dropped() {
    let (addr, errors) = start(local_trusted()).await;
    // A plain HTTP request where the header should be.
    let resp = exchange(addr, b"").await;
    assert!(resp.is_empty(), "{resp}");
    // A malformed header.
    let resp = exchange(addr, b"PROXY TCP4 203.0.113.9 nope 1 2\r\n").await;
    assert!(resp.is_empty(), "{resp}");
    // No header at all within the head timeout.
    let mut idle = TcpStream::connect(addr).await.unwrap();
    let mut buf = Vec::new();
    let n = tokio::time::timeout(Duration::from_secs(5), idle.read_to_end(&mut buf))
        .await
        .expect("closed after the head timeout")
        .unwrap();
    assert_eq!(n, 0);
    let errors = errors.lock().unwrap().clone();
    assert!(
        errors.iter().any(|e| e.contains("PROXY protocol")),
        "{errors:?}"
    );
    assert!(errors.iter().any(|e| e.contains("timed out")), "{errors:?}");
}

/// A v1 header split across TCP segments (here inside an address, and
/// between its CR and LF) is reassembled, not taken for a short header.
#[tokio::test]
async fn a_v1_header_split_across_segments_is_reassembled() {
    let (addr, errors) = start(local_trusted()).await;
    let mut sock = TcpStream::connect(addr).await.unwrap();
    let pieces: [&[u8]; 4] = [
        b"PROXY TCP4 203.0",
        b".113.9 198.51.100.1 40000 443\r",
        b"\n",
        b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    ];
    for piece in pieces {
        sock.write_all(piece).await.unwrap();
        sock.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), sock.read_to_end(&mut buf)).await;
    let resp = String::from_utf8_lossy(&buf);
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(
        resp.ends_with("peer=203.0.113.9:40000 local=198.51.100.1:443"),
        "{resp}"
    );
    assert!(errors.lock().unwrap().is_empty());
}

/// HTTP-12: shutting the server down closes a connection still sending its
/// PROXY header, instead of leaving it to outlive `serve` until the head
/// timeout.
#[tokio::test]
async fn shutdown_closes_a_connection_in_its_proxy_header() {
    let config = ServerConfig {
        proxy_protocol: Some(local_trusted()),
        head_timeout: Duration::from_secs(30),
        drain_timeout: Duration::from_millis(200),
        ..ServerConfig::default()
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let shutdown = async {
            let _ = stopped.await;
        };
        serve_with_config(listener, Arc::new(Addrs), shutdown, config).await
    });
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(b"PROXY TCP4 203.0.113.9 ").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("serve returned")
        .unwrap()
        .unwrap();
    let mut buf = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(2), sock.read_to_end(&mut buf)).await;
    assert!(
        closed.is_ok(),
        "the connection outlived serve, still waiting for its PROXY header"
    );
    assert!(buf.is_empty());
}
