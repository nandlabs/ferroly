#![cfg(feature = "grpc")]
//! Compiles the code `GrpcGenerator` produced for `grpc_data/echo/v1/echo.proto`
//! and `grpc_data/tricky/v1/tricky.proto` (checked in as
//! `grpc_data/echo_generated.rs` and `grpc_data/tricky_generated.rs`, kept
//! current by `grpc_codegen_golden.rs`) and runs the methods through a
//! generated client against a generated server adapter.
//!
//! Both are included publicly under `#![deny(missing_docs)]`, as a consumer
//! with that lint would include them.

#![deny(missing_docs)]

/// The generated echo service.
pub mod protos {
    include!("grpc_data/echo_generated.rs");
}

/// The generated service with awkward names.
pub mod tricky_protos {
    include!("grpc_data/tricky_generated.rs");
}

use std::sync::Arc;

use ferroly::grpc::{BoxFuture, Channel, Code, GrpcServer, Request, Response, Status, Streaming};
use protos::echo::v1::echo_client::EchoClient;
use protos::echo::v1::echo_server::{Echo, EchoServer};
use protos::echo::v1::nothing_server::{Nothing, NothingServer};
use protos::echo::v1::{EchoRequest, EchoResponse, ECHO_DESCRIPTOR};

struct Impl {
    prefix: String,
}

fn reply(message: impl Into<String>, index: i32) -> EchoResponse {
    EchoResponse {
        message: message.into(),
        index,
        ..Default::default()
    }
}

impl Echo for Impl {
    fn unary(
        &self,
        request: Request<EchoRequest>,
    ) -> BoxFuture<'_, Result<Response<EchoResponse>, Status>> {
        Box::pin(async move {
            let m = request.into_inner();
            if m.message.is_empty() {
                return Err(Status::invalid_argument("empty message"));
            }
            Ok(Response::new(reply(
                format!("{}{}", self.prefix, m.message),
                0,
            )))
        })
    }

    fn server_stream(
        &self,
        request: Request<EchoRequest>,
    ) -> BoxFuture<'_, Result<Response<Streaming<EchoResponse>>, Status>> {
        Box::pin(async move {
            let m = request.into_inner();
            Ok(Response::new(Streaming::from_messages(
                (0..m.repeat).map(move |i| reply(m.message.clone(), i)),
            )))
        })
    }

    fn client_stream(
        &self,
        request: Request<Streaming<EchoRequest>>,
    ) -> BoxFuture<'_, Result<Response<EchoResponse>, Status>> {
        Box::pin(async move {
            let mut input = request.into_inner();
            let mut parts = Vec::new();
            while let Some(m) = input.message().await? {
                parts.push(m.message);
            }
            Ok(Response::new(reply(parts.join("+"), parts.len() as i32)))
        })
    }

    fn bidi(
        &self,
        request: Request<Streaming<EchoRequest>>,
    ) -> BoxFuture<'_, Result<Response<Streaming<EchoResponse>>, Status>> {
        let prefix = self.prefix.clone();
        Box::pin(async move {
            let mut input = request.into_inner();
            let (tx, out) = Streaming::channel(2);
            tokio::spawn(async move {
                let mut i = 0;
                while let Some(m) = input.message().await? {
                    tx.send(reply(format!("{prefix}{}", m.message), i)).await?;
                    i += 1;
                }
                tx.finish().await
            });
            Ok(Response::new(out))
        })
    }
}

struct Empty;
impl Nothing for Empty {}

fn req(message: &str, repeat: i32) -> EchoRequest {
    EchoRequest {
        message: message.into(),
        repeat,
        ..Default::default()
    }
}

#[tokio::test]
async fn generated_client_and_server_round_trip() {
    let server = GrpcServer::new()
        .add_service(EchoServer::new(Impl {
            prefix: "> ".into(),
        }))
        .add_service(NothingServer::from_arc(Arc::new(Empty)));
    for m in ECHO_DESCRIPTOR.methods {
        assert!(server.has_method(m.path), "{}", m.path);
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(server.serve(listener, std::future::pending()));
    let client = EchoClient::new(Channel::new(&format!("http://{addr}")).unwrap());

    // Unary, with a bare message and with a Request.
    let r = client.unary(req("hi", 0)).await.unwrap();
    assert_eq!(r.into_inner().message, "> hi");
    let r = client.unary(Request::new(req("there", 0))).await.unwrap();
    assert_eq!(r.get_ref().message, "> there");
    assert_eq!(
        client.unary(req("", 0)).await.unwrap_err().code(),
        Code::InvalidArgument
    );

    let mut s = client
        .server_stream(req("x", 3))
        .await
        .unwrap()
        .into_inner();
    let mut indexes = Vec::new();
    while let Some(m) = s.message().await.unwrap() {
        assert_eq!(m.message, "x");
        indexes.push(m.index);
    }
    assert_eq!(indexes, [0, 1, 2]);

    let r = client
        .client_stream(Streaming::from_messages([
            req("a", 0),
            req("b", 0),
            req("c", 0),
        ]))
        .await
        .unwrap()
        .into_inner();
    assert_eq!((r.message.as_str(), r.index), ("a+b+c", 3));

    let (tx, input) = Streaming::channel(1);
    tx.send(req("ping", 0)).await.unwrap();
    let mut out = client.bidi(input).await.unwrap().into_inner();
    assert_eq!(out.message().await.unwrap().unwrap(), reply("> ping", 0));
    tx.send(req("pong", 0)).await.unwrap();
    assert_eq!(out.message().await.unwrap().unwrap(), reply("> pong", 1));
    tx.finish().await.unwrap();
    assert_eq!(out.message().await.unwrap(), None);

    assert_eq!(client.channel().target(), format!("http://{addr}"));
}

mod tricky {
    //! rpcs named after methods of `Arc`, `Clone`, `Debug` and the client,
    //! served through an `Arc<dyn Tricky>`.

    use std::sync::Arc;

    use ferroly::grpc::{BoxFuture, Channel, GrpcServer, Request, Response, Status, Streaming};

    use crate::tricky_protos::tricky::common::Empty;
    use crate::tricky_protos::tricky::v1::tricky_client::TrickyClient;
    use crate::tricky_protos::tricky::v1::tricky_server::{Tricky, TrickyServer};
    use crate::tricky_protos::tricky::v1::type_client::TypeClient;
    use crate::tricky_protos::tricky::v1::type_server::{Type, TypeServer};
    use crate::tricky_protos::tricky::v1::word::Inner;
    use crate::tricky_protos::tricky::v1::{
        TrickyClient as ClientMessage, TrickyServer as ServerMessage, Word,
    };

    type Reply<T> = BoxFuture<'static, Result<Response<T>, Status>>;

    fn word(text: impl Into<String>) -> Word {
        Word {
            text: text.into(),
            ..Default::default()
        }
    }

    fn tag(request: Request<Word>, tag: &str) -> Reply<Word> {
        let text = format!("{tag}:{}", request.into_inner().text);
        Box::pin(async move { Ok(Response::new(word(text))) })
    }

    async fn join(mut input: Streaming<Word>) -> Result<String, Status> {
        let mut parts = Vec::new();
        while let Some(w) = input.message().await? {
            parts.push(w.text);
        }
        Ok(parts.join("+"))
    }

    struct Svc;

    impl Tricky for Svc {
        fn clone(&self, request: Request<Word>) -> BoxFuture<'_, Result<Response<Word>, Status>> {
            tag(request, "clone")
        }
        fn clone_from(
            &self,
            request: Request<Word>,
        ) -> BoxFuture<'_, Result<Response<Word>, Status>> {
            tag(request, "clone_from")
        }
        fn deref(&self, request: Request<Inner>) -> BoxFuture<'_, Result<Response<Inner>, Status>> {
            let n = request.into_inner().n;
            Box::pin(async move {
                Ok(Response::new(Inner {
                    n: n + 1,
                    ..Default::default()
                }))
            })
        }
        fn as_ref(
            &self,
            request: Request<ServerMessage>,
        ) -> BoxFuture<'_, Result<Response<ClientMessage>, Status>> {
            let n = request.into_inner().n;
            Box::pin(async move {
                Ok(Response::new(ClientMessage {
                    n: n * 2,
                    ..Default::default()
                }))
            })
        }
        fn borrow(&self, _: Request<Empty>) -> BoxFuture<'_, Result<Response<Empty>, Status>> {
            Box::pin(async { Ok(Response::new(Empty::default())) })
        }
        fn fmt(
            &self,
            request: Request<Word>,
        ) -> BoxFuture<'_, Result<Response<Streaming<Word>>, Status>> {
            let text = request.into_inner().text;
            Box::pin(async move {
                Ok(Response::new(Streaming::from_messages(
                    text.split(' ').map(word).collect::<Vec<_>>(),
                )))
            })
        }
        fn new(
            &self,
            request: Request<Streaming<Word>>,
        ) -> BoxFuture<'_, Result<Response<Word>, Status>> {
            Box::pin(async move { Ok(Response::new(word(join(request.into_inner()).await?))) })
        }
        fn channel(
            &self,
            request: Request<Streaming<Word>>,
        ) -> BoxFuture<'_, Result<Response<Streaming<Word>>, Status>> {
            Box::pin(async move {
                let joined = join(request.into_inner()).await?;
                Ok(Response::new(Streaming::from_messages(vec![word(joined)])))
            })
        }
        fn into(&self, request: Request<Word>) -> BoxFuture<'_, Result<Response<Word>, Status>> {
            tag(request, "into")
        }
        fn into_words(
            &self,
            request: Request<Word>,
        ) -> BoxFuture<'_, Result<Response<Word>, Status>> {
            tag(request, "into_words")
        }
        fn from_words(
            &self,
            request: Request<Word>,
        ) -> BoxFuture<'_, Result<Response<Word>, Status>> {
            tag(request, "from_words")
        }
    }

    impl Type for Svc {
        fn get(&self, _: Request<Empty>) -> BoxFuture<'_, Result<Response<Word>, Status>> {
            Box::pin(async { Ok(Response::new(word("type"))) })
        }
    }

    #[tokio::test]
    async fn awkward_names_and_dyn_services_work() {
        let shared: Arc<dyn Tricky> = Arc::new(Svc);
        let typed: Arc<dyn Type> = Arc::new(Svc);
        let adapter = TrickyServer::from_arc(shared);
        assert_eq!(format!("{adapter:?}"), "TrickyServer");
        let server = GrpcServer::new()
            .add_service(adapter)
            .add_service(TypeServer::from_arc(typed));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(server.serve(listener, std::future::pending()));
        let channel = Channel::new(&format!("http://{addr}")).unwrap();
        let client = TrickyClient::new(channel.clone());
        // `clone` and `fmt` still mean the derived `Clone` and `Debug`.
        let copy = client.clone();
        assert!(format!("{copy:?}").contains("TrickyClient"));

        let text = |r: Response<Word>| r.into_inner().text;
        assert_eq!(text(client.clone_rpc(word("a")).await.unwrap()), "clone:a");
        assert_eq!(
            text(client.clone_from_rpc(word("b")).await.unwrap()),
            "clone_from:b"
        );
        assert_eq!(text(client.into_rpc(word("c")).await.unwrap()), "into:c");
        assert_eq!(
            text(client.into_words(word("d")).await.unwrap()),
            "into_words:d"
        );
        assert_eq!(
            text(client.from_words(word("e")).await.unwrap()),
            "from_words:e"
        );
        let inner = Inner {
            n: 41,
            ..Default::default()
        };
        assert_eq!(client.deref(inner).await.unwrap().into_inner().n, 42);
        let server_message = ServerMessage {
            n: 4,
            ..Default::default()
        };
        assert_eq!(
            client.as_ref(server_message).await.unwrap().into_inner().n,
            8
        );
        client.borrow(Empty::default()).await.unwrap();

        let mut words = client.fmt_rpc(word("x y z")).await.unwrap().into_inner();
        let mut got = Vec::new();
        while let Some(w) = words.message().await.unwrap() {
            got.push(w.text);
        }
        assert_eq!(got, ["x", "y", "z"]);

        let joined = client
            .new_rpc(Streaming::from_messages(vec![word("p"), word("q")]))
            .await
            .unwrap();
        assert_eq!(text(joined), "p+q");
        let mut out = copy
            .channel_rpc(Streaming::from_messages(vec![word("r"), word("s")]))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(out.message().await.unwrap().unwrap().text, "r+s");
        assert_eq!(out.message().await.unwrap(), None);
        assert_eq!(client.channel().target(), format!("http://{addr}"));

        let typed = TypeClient::new(channel);
        assert_eq!(text(typed.get(Empty::default()).await.unwrap()), "type");
    }
}
