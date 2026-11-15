//! gRPC interop with grpc-go: a ferroly server for grpc-go's client, and a
//! ferroly client that checks a grpc-go server.
//!
//! The Go side lives in `interop/grpc-go/`; `interop/grpc-go.sh` builds both
//! and runs them in both directions. By hand:
//!
//! ```sh
//! cargo run -p ferroly --features grpc --example grpc_interop -- server 50051
//! (cd interop/grpc-go && go run . client 127.0.0.1:50051)
//!
//! (cd interop/grpc-go && go run . server 50052)
//! cargo run -p ferroly --features grpc --example grpc_interop -- client 127.0.0.1:50052
//! ```
//!
//! The service, `/p.S/*`, is defined by both programs; every message is a
//! single `bytes` field (tag 1). The client prints one PASS or FAIL line per
//! check and exits with status 1 if any failed.

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use ferroly::grpc::health::{
    HealthCheckRequest, HealthCheckResponse, HealthReporter, ServingStatus, CHECK_PATH, WATCH_PATH,
};
use ferroly::grpc::{Channel, Code, GrpcServer, Metadata, Request, Response, Status, Streaming};

/// The one message type: field 1, `bytes`.
#[derive(ferroly::proto::Message, Clone, PartialEq, Debug, Default)]
pub struct B {
    #[proto(tag = 1)]
    pub data: Vec<u8>,
}

fn b(s: &str) -> B {
    B {
        data: s.as_bytes().to_vec(),
    }
}

fn text(m: &B) -> String {
    String::from_utf8_lossy(&m.data).into_owned()
}

/// The error message both servers return for "err"; it exercises the
/// percent-encoding of `grpc-message`.
const ERR_MESSAGE: &str = "bad ✓ 100% \r\n\t end";

/// The type URL of the single `Any` detail attached to that error.
const DETAIL_TYPE_URL: &str = "type.googleapis.com/p.Detail";

/// Appends a length-delimited protobuf field (wire type 2).
fn put_bytes(out: &mut Vec<u8>, tag: u8, value: &[u8]) {
    out.push(tag << 3 | 2);
    let mut n = value.len();
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
    out.extend_from_slice(value);
}

/// An encoded `google.rpc.Status { code, message, details: [Any] }`.
fn rpc_status(code: Code, message: &str) -> Vec<u8> {
    let mut any = Vec::new();
    put_bytes(&mut any, 1, DETAIL_TYPE_URL.as_bytes());
    put_bytes(&mut any, 2, b"x");
    let mut out = vec![0x08, code.as_i32() as u8];
    put_bytes(&mut out, 2, message.as_bytes());
    put_bytes(&mut out, 3, &any);
    out
}

// ---- server ------------------------------------------------------------------

fn server() -> GrpcServer {
    let health = HealthReporter::new();
    GrpcServer::new()
        .unary("/p.S/Unary", |req: Request<B>| async move {
            let data = req.into_inner().data;
            if data == b"err" {
                let mut md = Metadata::new();
                md.insert("x-why", "because")?;
                md.insert_bin("x-why-bin", vec![0u8, 1, 2, 0xff])?;
                return Err(Status::new(Code::InvalidArgument, ERR_MESSAGE)
                    .with_details(rpc_status(Code::InvalidArgument, ERR_MESSAGE))
                    .with_metadata(md));
            }
            let mut resp = Response::new(B { data });
            resp.metadata_mut().insert("x-h", "hv")?;
            resp.trailers_mut().insert_bin("x-t-bin", vec![9u8, 9])?;
            Ok(resp)
        })
        .server_streaming("/p.S/ServerStream", |req: Request<B>| async move {
            let arg = text(req.get_ref());
            if arg == "midfail" {
                let (tx, stream) = Streaming::channel(4);
                tokio::spawn(async move {
                    tx.send(b("one")).await?;
                    tx.fail(Status::aborted("mid-stream failure ✓")).await
                });
                return Ok(Response::new(stream));
            }
            Ok(Response::new(Streaming::from_messages(
                (0..arg.len()).map(|i| b(&format!("m{i}"))),
            )))
        })
        .client_streaming(
            "/p.S/ClientStream",
            |req: Request<Streaming<B>>| async move {
                let mut input = req.into_inner();
                let mut all = Vec::new();
                while let Some(m) = input.message().await? {
                    all.extend(m.data);
                }
                Ok(Response::new(B { data: all }))
            },
        )
        .client_streaming(
            "/p.S/ClientStreamEarly",
            // Answers without reading the request stream.
            |_req: Request<Streaming<B>>| async move { Ok(Response::new(b("early"))) },
        )
        .bidi_streaming("/p.S/Bidi", |req: Request<Streaming<B>>| async move {
            let mut input = req.into_inner();
            let (tx, out) = Streaming::channel(4);
            tokio::spawn(async move {
                while let Some(m) = input.message().await? {
                    tx.send(m).await?;
                }
                tx.finish().await
            });
            Ok(Response::new(out))
        })
        .unary("/p.S/Sleep", |_req: Request<B>| async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(Response::new(B::default()))
        })
        .unary("/p.S/Panic", |_req: Request<B>| async move { boom() })
        .server_streaming("/p.S/Infinite", |_req: Request<B>| async move {
            let (tx, stream) = Streaming::channel(4);
            tokio::spawn(async move { while tx.send(b("x")).await.is_ok() {} });
            Ok(Response::new(stream))
        })
        .add_service(health.service())
}

/// The `/p.S/Panic` handler: the server must turn this into `INTERNAL`.
fn boom() -> Result<Response<B>, Status> {
    panic!("handler panic (expected by the interop check)")
}

// ---- client ------------------------------------------------------------------

#[derive(Default)]
struct Checker {
    failed: usize,
}

impl Checker {
    fn check(&mut self, name: &str, ok: bool, detail: impl std::fmt::Display) {
        if ok {
            println!("PASS {name}: {detail}");
        } else {
            self.failed += 1;
            println!("FAIL {name}: {detail}");
        }
    }
}

fn describe<T>(r: &Result<T, Status>) -> String {
    match r {
        Ok(_) => "OK".to_string(),
        Err(s) => format!("code={:?} msg={:?}", s.code(), s.message()),
    }
}

fn code_of<T>(r: &Result<T, Status>) -> Option<Code> {
    r.as_ref().err().map(Status::code)
}

/// Reads a server stream to its end: the messages, then `Ok(())` or the error.
async fn drain(mut s: Streaming<B>) -> (Vec<String>, Result<(), Status>) {
    let mut got = Vec::new();
    loop {
        match s.message().await {
            Ok(Some(m)) => got.push(text(&m)),
            Ok(None) => return (got, Ok(())),
            Err(e) => return (got, Err(e)),
        }
    }
}

async fn client(addr: &str) -> Result<usize, Status> {
    let ch = Channel::builder(&format!("http://{addr}"))?
        .timeout(Duration::from_secs(10))
        .build()?;
    let mut c = Checker::default();

    // Unary: echo, response headers and binary trailers.
    let r = ch
        .unary::<B, B>("/p.S/Unary", Request::new(b("hello")))
        .await;
    match &r {
        Ok(resp) => {
            c.check(
                "unary",
                text(resp.get_ref()) == "hello",
                text(resp.get_ref()),
            );
            c.check(
                "unary header",
                resp.metadata().get("x-h") == Some("hv"),
                format!("{:?}", resp.metadata().get("x-h")),
            );
            c.check(
                "unary binary trailer",
                resp.trailers().get_bin("x-t-bin") == Some(&[9u8, 9][..]),
                format!("{:?}", resp.trailers().get_bin("x-t-bin")),
            );
        }
        Err(_) => c.check("unary", false, describe(&r)),
    }

    // A gzip-compressed response (the channel advertises gzip).
    let r = ch
        .unary::<B, B>("/p.S/Unary", Request::new(b("gzip")))
        .await;
    c.check(
        "gzip response",
        matches!(&r, Ok(resp) if text(resp.get_ref()) == "gzip"),
        describe(&r),
    );

    // An error status, with binary request metadata echoed in its trailers
    // and google.rpc.Status details.
    let mut req = Request::new(b("err"));
    req.metadata_mut().insert_bin("q-bin", vec![0u8, 255])?;
    let r = ch.unary::<B, B>("/p.S/Unary", req).await;
    match &r {
        Err(s) => {
            c.check(
                "unary error status",
                s.code() == Code::InvalidArgument && s.message() == ERR_MESSAGE,
                describe(&r),
            );
            let md = s.metadata();
            c.check(
                "unary error trailers",
                md.get("x-why") == Some("because")
                    && md.get_bin("x-why-bin") == Some(&[0u8, 1, 2, 0xff][..])
                    && md.get_bin("q-bin") == Some(&[0u8, 255][..]),
                format!(
                    "x-why={:?} x-why-bin={:?} q-bin={:?}",
                    md.get("x-why"),
                    md.get_bin("x-why-bin"),
                    md.get_bin("q-bin")
                ),
            );
            let details = s.details().unwrap_or_default();
            let has_detail = details
                .windows(DETAIL_TYPE_URL.len())
                .any(|w| w == DETAIL_TYPE_URL.as_bytes());
            c.check(
                "unary error details",
                has_detail,
                format!("{} bytes of details", details.len()),
            );
        }
        Ok(_) => c.check("unary error status", false, "the call succeeded"),
    }

    // Server streaming: a clean end, then a failure after the first message.
    let r = ch
        .server_streaming::<B, B>("/p.S/ServerStream", Request::new(b("abc")))
        .await;
    match r {
        Ok(resp) => {
            let (got, end) = drain(resp.into_inner()).await;
            c.check(
                "server stream",
                end.is_ok() && got == ["m0", "m1", "m2"],
                format!("{got:?} {}", describe(&end)),
            );
        }
        Err(e) => c.check("server stream", false, describe::<()>(&Err(e))),
    }
    let r = ch
        .server_streaming::<B, B>("/p.S/ServerStream", Request::new(b("midfail")))
        .await;
    match r {
        Ok(resp) => {
            let (got, end) = drain(resp.into_inner()).await;
            let ok = got == ["one"]
                && matches!(&end, Err(s) if s.code() == Code::Aborted && s.message() == "mid-stream failure ✓");
            c.check(
                "server stream mid-failure",
                ok,
                format!("{got:?} {}", describe(&end)),
            );
        }
        Err(e) => c.check("server stream mid-failure", false, describe::<()>(&Err(e))),
    }

    // Client streaming.
    let r = ch
        .client_streaming::<B, B>(
            "/p.S/ClientStream",
            Request::new(Streaming::from_messages(vec![b("a"), b("b"), b("c")])),
        )
        .await;
    let got = r
        .as_ref()
        .map(|resp| text(resp.get_ref()))
        .unwrap_or_default();
    c.check(
        "client stream",
        got == "abc",
        format!("{} resp={got:?}", describe(&r)),
    );

    // Client streaming where the server answers without reading; the request
    // stream never ends on its own.
    let (tx, input) = Streaming::channel(4);
    let feeder = tokio::spawn(async move {
        while tx
            .send(B {
                data: vec![1u8; 1024],
            })
            .await
            .is_ok()
        {}
    });
    let r = tokio::time::timeout(
        Duration::from_secs(5),
        ch.client_streaming::<B, B>("/p.S/ClientStreamEarly", Request::new(input)),
    )
    .await;
    match r {
        Ok(r) => {
            let got = r
                .as_ref()
                .map(|resp| text(resp.get_ref()))
                .unwrap_or_default();
            c.check(
                "early answer",
                got == "early",
                format!("{} resp={got:?}", describe(&r)),
            );
        }
        Err(_) => c.check("early answer", false, "no answer within 5 s"),
    }
    let stopped = tokio::time::timeout(Duration::from_secs(2), feeder)
        .await
        .is_ok();
    c.check(
        "early answer stops the upload",
        stopped,
        if stopped {
            "feeder stopped"
        } else {
            "feeder still running after 2 s"
        },
    );

    // Bidirectional streaming, one message at a time.
    let (tx, input) = Streaming::channel(4);
    tx.send(b("ping1")).await?;
    match ch
        .bidi_streaming::<B, B>("/p.S/Bidi", Request::new(input))
        .await
    {
        Ok(resp) => {
            let mut out = resp.into_inner();
            let first = out.message().await;
            tx.send(b("ping2")).await?;
            let second = out.message().await;
            tx.finish().await?;
            let end = out.message().await;
            let ok = matches!(&first, Ok(Some(m)) if text(m) == "ping1")
                && matches!(&second, Ok(Some(m)) if text(m) == "ping2")
                && matches!(end, Ok(None));
            c.check("bidi", ok, format!("{first:?} {second:?} {end:?}"));
        }
        Err(e) => c.check("bidi", false, describe::<()>(&Err(e))),
    }

    // The deadline: DEADLINE_EXCEEDED, in time.
    let start = Instant::now();
    let r = ch
        .unary::<B, B>(
            "/p.S/Sleep",
            Request::new(b("")).with_timeout(Duration::from_millis(300)),
        )
        .await;
    let elapsed = start.elapsed();
    c.check(
        "deadline",
        code_of(&r) == Some(Code::DeadlineExceeded) && elapsed < Duration::from_secs(5),
        format!("{} after {elapsed:?}", describe(&r)),
    );

    // Unknown method and service; an INTERNAL error.
    let r = ch.unary::<B, B>("/p.S/Nope", Request::new(b(""))).await;
    c.check(
        "unknown method",
        code_of(&r) == Some(Code::Unimplemented),
        describe(&r),
    );
    let r = ch.unary::<B, B>("/p.X/Nope", Request::new(b(""))).await;
    c.check(
        "unknown service",
        code_of(&r) == Some(Code::Unimplemented),
        describe(&r),
    );
    let r = ch.unary::<B, B>("/p.S/Panic", Request::new(b(""))).await;
    c.check(
        "internal error",
        code_of(&r) == Some(Code::Internal),
        describe(&r),
    );

    // Cancelling an endless server stream; the channel stays usable.
    match ch
        .server_streaming::<B, B>("/p.S/Infinite", Request::new(b("x")))
        .await
    {
        Ok(resp) => {
            let mut s = resp.into_inner();
            let mut n = 0;
            while n < 3 && matches!(s.message().await, Ok(Some(_))) {
                n += 1;
            }
            drop(s);
            c.check(
                "cancel endless stream",
                n == 3,
                format!("received {n} before dropping"),
            );
        }
        Err(e) => c.check("cancel endless stream", false, describe::<()>(&Err(e))),
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let r = ch
        .unary::<B, B>("/p.S/Unary", Request::new(b("after")))
        .await;
    c.check("call after cancel", r.is_ok(), describe(&r));

    // The standard health service.
    let r = ch
        .unary::<HealthCheckRequest, HealthCheckResponse>(
            CHECK_PATH,
            Request::new(HealthCheckRequest::new("")),
        )
        .await;
    let status = r
        .as_ref()
        .ok()
        .and_then(|resp| resp.get_ref().status.known());
    c.check(
        "health check",
        status == Some(ServingStatus::Serving),
        format!("{status:?} {}", describe(&r)),
    );
    let r = ch
        .unary::<HealthCheckRequest, HealthCheckResponse>(
            CHECK_PATH,
            Request::new(HealthCheckRequest::new("nope")),
        )
        .await;
    c.check(
        "health check unknown",
        code_of(&r) == Some(Code::NotFound),
        describe(&r),
    );
    let r = ch
        .server_streaming::<HealthCheckRequest, HealthCheckResponse>(
            WATCH_PATH,
            Request::new(HealthCheckRequest::new("nope")).with_timeout(Duration::from_secs(2)),
        )
        .await;
    match r {
        Ok(resp) => {
            let first = resp.into_inner().message().await;
            let status = first
                .as_ref()
                .ok()
                .and_then(|m| m.as_ref())
                .and_then(|m| m.status.known());
            c.check(
                "health watch unknown",
                status == Some(ServingStatus::ServiceUnknown),
                format!("{status:?}"),
            );
        }
        Err(e) => c.check("health watch unknown", false, describe::<()>(&Err(e))),
    }

    // Many expired deadlines on one connection (each is an RST_STREAM) must
    // not look like a reset flood to either side: every call ends
    // DEADLINE_EXCEEDED, and an unrelated call in flight meanwhile is not cut
    // off by a GOAWAY.
    let victim = {
        let ch = ch.clone();
        tokio::spawn(async move {
            ch.unary::<B, B>(
                "/p.S/Sleep",
                Request::new(b("")).with_timeout(Duration::from_secs(4)),
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut set = tokio::task::JoinSet::new();
    let mut other: BTreeMap<String, usize> = BTreeMap::new();
    let mut record = |r: Result<Result<Response<B>, Status>, tokio::task::JoinError>| match r {
        Ok(r) if code_of(&r) == Some(Code::DeadlineExceeded) => {}
        Ok(r) => *other.entry(describe(&r)).or_default() += 1,
        Err(e) => *other.entry(format!("task: {e}")).or_default() += 1,
    };
    for _ in 0..400 {
        if set.len() >= 32 {
            if let Some(r) = set.join_next().await {
                record(r);
            }
        }
        let ch = ch.clone();
        set.spawn(async move {
            ch.unary::<B, B>(
                "/p.S/Sleep",
                Request::new(b("")).with_timeout(Duration::from_millis(20)),
            )
            .await
        });
    }
    while let Some(r) = set.join_next().await {
        record(r);
    }
    c.check(
        "400 cancelled calls",
        other.is_empty(),
        format!("other outcomes: {other:?}"),
    );
    let r = match victim.await {
        Ok(r) => r,
        Err(e) => Err(Status::internal(format!("task: {e}"))),
    };
    c.check(
        "call in flight during cancellations",
        code_of(&r) == Some(Code::DeadlineExceeded),
        describe(&r),
    );
    let r = ch
        .unary::<B, B>("/p.S/Unary", Request::new(b("still")))
        .await;
    c.check("call after cancellations", r.is_ok(), describe(&r));

    Ok(c.failed)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["server", port] => {
            let port: u16 = match port.parse() {
                Ok(p) => p,
                Err(_) => return usage(),
            };
            let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("bind 127.0.0.1:{port}: {e}");
                    return ExitCode::FAILURE;
                }
            };
            eprintln!("[ferroly] listening on 127.0.0.1:{port}");
            let shutdown = async {
                let _ = tokio::signal::ctrl_c().await;
            };
            match server().serve(listener, shutdown).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("server: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        ["client", addr] => match client(addr).await {
            Ok(0) => {
                println!("all checks passed");
                ExitCode::SUCCESS
            }
            Ok(n) => {
                println!("{n} check(s) failed");
                ExitCode::FAILURE
            }
            Err(e) => {
                println!("FAIL setup: {:?} {}", e.code(), e.message());
                ExitCode::FAILURE
            }
        },
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: grpc_interop (server <port> | client <host:port>)");
    ExitCode::from(2)
}
