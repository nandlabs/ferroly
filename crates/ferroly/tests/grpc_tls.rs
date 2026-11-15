#![cfg(all(feature = "grpc", feature = "tls-ring"))]
//! gRPC over TLS with ALPN `h2`: the four call shapes end to end, and a
//! server that negotiates only HTTP/1.1 failing the call cleanly.

use std::net::SocketAddr;
use std::sync::Arc;

use ferroly::grpc::{Channel, Code, GrpcServer, Request, Response, Status, Streaming};
use ferroly::http::{
    serve_tls_with_config, ClientTlsConfig, Http2Config, HttpHandler, ServerConfig, TlsConfig,
};
use tokio::net::TcpListener;

const CA: &[u8] = include_bytes!("../src/http/testdata/test_ca_cert.pem");
const SERVER_CERT: &[u8] = include_bytes!("../src/http/testdata/test_server_cert.pem");
const SERVER_KEY: &[u8] = include_bytes!("../src/http/testdata/test_server_key.pem");

#[derive(ferroly::proto::Message, Clone, PartialEq, Debug, Default)]
struct Num {
    #[proto(tag = 1)]
    n: i64,
}

fn server() -> GrpcServer {
    GrpcServer::new()
        .unary("/t.Math/Double", |req: Request<Num>| async move {
            assert!(req.is_tls());
            Ok(Response::new(Num {
                n: req.into_inner().n * 2,
            }))
        })
        .server_streaming("/t.Math/Range", |req: Request<Num>| async move {
            let n = req.into_inner().n;
            Ok(Response::new(Streaming::from_messages(
                (0..n).map(|n| Num { n }),
            )))
        })
        .client_streaming("/t.Math/Sum", |req: Request<Streaming<Num>>| async move {
            let mut input = req.into_inner();
            let mut n = 0;
            while let Some(m) = input.message().await? {
                n += m.n;
            }
            Ok(Response::new(Num { n }))
        })
        .bidi_streaming(
            "/t.Math/Square",
            |req: Request<Streaming<Num>>| async move {
                let mut input = req.into_inner();
                let (tx, out) = Streaming::channel(1);
                tokio::spawn(async move {
                    while let Some(m) = input.message().await? {
                        tx.send(Num { n: m.n * m.n }).await?;
                    }
                    tx.finish().await
                });
                Ok::<_, Status>(Response::new(out))
            },
        )
}

async fn spawn_tls(handler: Arc<dyn HttpHandler>, http2: bool) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let tls = TlsConfig::from_pem(SERVER_CERT, SERVER_KEY).unwrap();
    let config = ServerConfig {
        http2: http2.then(Http2Config::default),
        ..ServerConfig::default()
    };
    tokio::spawn(async move {
        let _ = serve_tls_with_config(listener, handler, std::future::pending(), config, tls).await;
    });
    addr
}

fn channel(addr: SocketAddr) -> Channel {
    let tls = ClientTlsConfig::new()
        .without_web_pki_roots()
        .add_root_pem(CA)
        .unwrap();
    Channel::builder(&format!("https://127.0.0.1:{}", addr.port()))
        .unwrap()
        .tls(tls)
        .build()
        .unwrap()
}

#[tokio::test]
async fn all_four_shapes_over_tls_alpn() {
    let addr = spawn_tls(Arc::new(server()), true).await;
    let channel = channel(addr);

    let resp = channel
        .unary::<Num, Num>("/t.Math/Double", Request::new(Num { n: 21 }))
        .await
        .unwrap();
    assert_eq!(resp.into_inner().n, 42);

    let mut range = channel
        .server_streaming::<Num, Num>("/t.Math/Range", Request::new(Num { n: 4 }))
        .await
        .unwrap()
        .into_inner();
    let mut got = Vec::new();
    while let Some(m) = range.message().await.unwrap() {
        got.push(m.n);
    }
    assert_eq!(got, [0, 1, 2, 3]);

    let sum = channel
        .client_streaming::<Num, Num>(
            "/t.Math/Sum",
            Request::new(Streaming::from_messages((1..=10).map(|n| Num { n }))),
        )
        .await
        .unwrap();
    assert_eq!(sum.into_inner().n, 55);

    let (tx, input) = Streaming::channel(1);
    tx.send(Num { n: 3 }).await.unwrap();
    let mut squares = channel
        .bidi_streaming::<Num, Num>("/t.Math/Square", Request::new(input))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(squares.message().await.unwrap().unwrap().n, 9);
    tx.send(Num { n: 12 }).await.unwrap();
    assert_eq!(squares.message().await.unwrap().unwrap().n, 144);
    tx.finish().await.unwrap();
    assert_eq!(squares.message().await.unwrap(), None);
}

#[tokio::test]
async fn http1_only_tls_server_fails_cleanly() {
    let addr = spawn_tls(Arc::new(server()), false).await;
    let channel = channel(addr);
    let err = channel
        .unary::<Num, Num>("/t.Math/Double", Request::new(Num { n: 1 }))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unavailable, "{err}");
    assert!(err.message().contains("HTTP/2"), "{err}");
    // Streaming calls fail the same way, without hanging on the upload.
    let (_tx, input) = Streaming::<Num>::channel(1);
    let err = channel
        .bidi_streaming::<Num, Num>("/t.Math/Square", Request::new(input))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unavailable, "{err}");
}
