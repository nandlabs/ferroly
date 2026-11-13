#![cfg(feature = "ws")]
//! Capabilities: the upgrade hook and origin policy (C1), the hub (C2), the
//! connection handle, subprotocols and custom headers, `wss` and mutual TLS,
//! the reconnecting client, the observer, and the masking randomness (W7).

mod ws_common;

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::http::{ClientTlsConfig, StatusCode, TlsConfig};
use ferroly::lifecycle::BoxFuture;
use ferroly::ws::{
    close_code, Hub, Message, OriginPolicy, ReconnectOptions, ReconnectingClient,
    SlowConsumerPolicy, UpgradeAccept, UpgradeHook, UpgradeReject, UpgradeRequest, WsClient,
    WsConn, WsError, WsEvent, WsOptions, WsSender, WsServerConfig,
};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use ws_common::{
    echo, fast_config, fast_options, http_exchange, start, status_of, upgrade_request, within, Raw,
    Recorder, FIN, OP_TEXT,
};

const CA: &[u8] = include_bytes!("../src/http/testdata/test_ca_cert.pem");
const SERVER_CERT: &[u8] = include_bytes!("../src/http/testdata/test_server_cert.pem");
const SERVER_KEY: &[u8] = include_bytes!("../src/http/testdata/test_server_key.pem");
const CLIENT_CERT: &[u8] = include_bytes!("../src/http/testdata/test_client_cert.pem");
const CLIENT_KEY: &[u8] = include_bytes!("../src/http/testdata/test_client_key.pem");

fn close_of(msg: Option<Message>) -> (u16, String) {
    match msg {
        Some(Message::Close { code, reason }) => (code, reason),
        other => panic!("expected a close, got {other:?}"),
    }
}

// ---- C1: the upgrade hook --------------------------------------------------

struct User(String);

/// Replies with what the connection knows about itself.
async fn describe(mut conn: WsConn) {
    let user = conn.context::<User>().map(|u| u.0.clone());
    let line = format!(
        "path={} query={:?} user={:?} tls={} peer={} proto={:?} auth={:?}",
        conn.path(),
        conn.request().query(),
        user,
        conn.request().is_tls(),
        conn.peer_addr().is_some(),
        conn.subprotocol(),
        conn.headers().get("authorization"),
    );
    let _ = conn.send(Message::text(line)).await;
    while let Some(msg) = conn.recv().await {
        if msg.is_close() {
            break;
        }
    }
}

#[tokio::test]
async fn c1_upgrade_hook_can_reject_before_the_upgrade() {
    let recorder = Recorder::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let config =
        fast_config()
            .observer(recorder.clone())
            .on_upgrade(move |req: &UpgradeRequest| {
                log.lock().unwrap().push(format!(
                    "{} {:?} tls={} peer={}",
                    req.path(),
                    req.query(),
                    req.is_tls(),
                    req.peer_addr().is_some()
                ));
                if req.path() == "/admin" {
                    return Err(UpgradeReject::forbidden().body("admins only"));
                }
                match req.header("authorization") {
                    Some("Bearer good") => Ok(UpgradeAccept::new()
                        .context(User("ada".into()))
                        .header("X-Session", "s1")),
                    _ => Err(UpgradeReject::unauthorized()
                        .header("WWW-Authenticate", "Bearer realm=\"ws\"")
                        .body("token required")),
                }
            });
    let server = start(config, describe).await;
    let host = server.addr.to_string();

    // No credentials: a real HTTP 401, with the hook's header and body, and
    // no upgrade.
    let response = http_exchange(server.addr, &upgrade_request(&host, "/feed", "")).await;
    assert_eq!(status_of(&response), 401);
    assert!(response.contains("WWW-Authenticate: Bearer realm=\"ws\"\r\n"));
    assert!(response.ends_with("token required"));
    assert!(!response.contains("Sec-WebSocket-Accept"));

    // The client surfaces the status.
    let denied = WsClient::dial(&server.url("/feed"), fast_options()).await;
    assert!(matches!(denied, Err(WsError::Rejected { status: 401 })));
    let wrong = fast_options().header("Authorization", "Bearer bad");
    assert!(matches!(
        WsClient::dial(&server.url("/feed"), wrong).await,
        Err(WsError::Rejected { status: 401 })
    ));
    let admin = fast_options().header("Authorization", "Bearer good");
    assert!(matches!(
        WsClient::dial(&server.url("/admin"), admin).await,
        Err(WsError::Rejected { status: 403 })
    ));

    // With the token: accepted, the context reaches the handler, the hook's
    // response header reaches the client, and the handler sees the request.
    let authed = fast_options().header("Authorization", "Bearer good");
    let mut client = WsClient::dial(&server.url("/feed?since=42"), authed)
        .await
        .unwrap();
    assert_eq!(client.response_headers().get("x-session"), Some("s1"));
    assert_eq!(
        client.recv().await,
        Some(Message::text(
            "path=/feed query=Some(\"since=42\") user=Some(\"ada\") tls=false peer=true \
             proto=None auth=Some(\"Bearer good\")"
        ))
    );
    client.close().await.unwrap();

    assert_eq!(
        seen.lock().unwrap().last().unwrap(),
        "/feed Some(\"since=42\") tls=false peer=true"
    );
    assert_eq!(
        recorder.count(|e| matches!(e, WsEvent::HandshakeRejected { status: 401 })),
        3
    );
    assert_eq!(
        recorder.count(|e| matches!(e, WsEvent::HandshakeRejected { status: 403 })),
        1
    );
    assert_eq!(recorder.count(|e| matches!(e, WsEvent::Opened { .. })), 1);
    server.stop().await;
}

/// A hook that awaits before deciding (for example a token introspection
/// call), implemented on a type.
struct SlowCheck {
    delay: Duration,
}

impl UpgradeHook for SlowCheck {
    fn on_upgrade<'a>(
        &'a self,
        request: &'a UpgradeRequest,
    ) -> BoxFuture<'a, Result<UpgradeAccept, UpgradeReject>> {
        Box::pin(async move {
            tokio::time::sleep(self.delay).await;
            match request.query() {
                Some("ticket=ok") => Ok(UpgradeAccept::new()),
                _ => Err(UpgradeReject::new(StatusCode(429)).body("slow down")),
            }
        })
    }
}

#[tokio::test]
async fn c1_async_hooks_work_and_are_bounded_by_the_handshake_timeout() {
    let config = fast_config().upgrade_hook(SlowCheck {
        delay: Duration::from_millis(30),
    });
    let server = start(config, echo).await;
    assert!(WsClient::dial(&server.url("/?ticket=ok"), fast_options())
        .await
        .is_ok());
    assert!(matches!(
        WsClient::dial(&server.url("/?ticket=no"), fast_options()).await,
        Err(WsError::Rejected { status: 429 })
    ));
    server.stop().await;

    // A hook slower than the handshake timeout is cut off with a 503.
    let config = WsServerConfig {
        handshake_timeout: Duration::from_millis(400),
        ..fast_config()
    }
    .upgrade_hook(SlowCheck {
        delay: Duration::from_secs(30),
    });
    let server = start(config, echo).await;
    assert!(matches!(
        within(
            "dial",
            WsClient::dial(&server.url("/?ticket=ok"), fast_options())
        )
        .await,
        Err(WsError::Rejected { status: 503 })
    ));
    server.stop().await;
}

// ---- C1: the origin policy -------------------------------------------------

async fn origin_status(server: &ws_common::TestServer, origin: Option<&str>) -> u16 {
    let extra = origin.map_or(String::new(), |o| format!("Origin: {o}\r\n"));
    let request = upgrade_request(&server.addr.to_string(), "/", &extra);
    let mut stream = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    tokio::io::AsyncWriteExt::write_all(&mut stream, request.as_bytes())
        .await
        .unwrap();
    status_of(&ws_common::read_head(&mut stream).await)
}

#[tokio::test]
async fn c1_origin_default_is_same_origin() {
    let server = start(fast_config(), echo).await;
    let own = format!("http://{}", server.addr);

    // Non-browser clients send no Origin and are not affected.
    assert_eq!(origin_status(&server, None).await, 101);
    // A page served from the same host and port may connect.
    assert_eq!(origin_status(&server, Some(&own)).await, 101);
    // Any other origin is refused before the upgrade.
    assert_eq!(
        origin_status(&server, Some("https://evil.example")).await,
        403
    );
    assert_eq!(origin_status(&server, Some("null")).await, 403);
    let other_port = format!("http://127.0.0.1:{}", server.addr.port().wrapping_add(1));
    assert_eq!(origin_status(&server, Some(&other_port)).await, 403);

    let cross = fast_options().header("Origin", "https://evil.example");
    assert!(matches!(
        WsClient::dial(&server.url("/"), cross).await,
        Err(WsError::Rejected { status: 403 })
    ));
    server.stop().await;
}

#[tokio::test]
async fn c1_origin_allow_list_and_any() {
    let config = fast_config().origin_policy(OriginPolicy::allow_list([
        "https://app.example.com",
        "http://localhost:3000",
    ]));
    let server = start(config, echo).await;
    let own = format!("http://{}", server.addr);
    assert_eq!(origin_status(&server, None).await, 101);
    assert_eq!(
        origin_status(&server, Some("https://app.example.com")).await,
        101
    );
    assert_eq!(
        origin_status(&server, Some("HTTPS://APP.EXAMPLE.COM")).await,
        101
    );
    assert_eq!(
        origin_status(&server, Some("http://localhost:3000")).await,
        101
    );
    // Not on the list — including the server's own origin.
    assert_eq!(
        origin_status(&server, Some("https://evil.example")).await,
        403
    );
    assert_eq!(
        origin_status(&server, Some("https://app.example.com.evil.example")).await,
        403
    );
    assert_eq!(origin_status(&server, Some(&own)).await, 403);
    server.stop().await;

    let server = start(fast_config().origin_policy(OriginPolicy::Any), echo).await;
    assert_eq!(
        origin_status(&server, Some("https://evil.example")).await,
        101
    );
    server.stop().await;
}

// ---- subprotocols and request info -----------------------------------------

#[tokio::test]
async fn subprotocols_are_negotiated_on_both_sides() {
    let config = fast_config().subprotocols(["v2", "v1"]);
    let server = start(config, describe).await;
    let url = server.url("/");

    // The server's preference order decides among the offered ones.
    let offered = fast_options().subprotocol("v1").subprotocol("v2");
    let mut client = WsClient::dial(&url, offered).await.unwrap();
    assert_eq!(client.subprotocol(), Some("v2"));
    let line = client.recv().await.unwrap();
    assert!(line.as_text().unwrap().contains("proto=Some(\"v2\")"));

    let only_old = fast_options().subprotocol("v1");
    let client = WsClient::dial(&url, only_old).await.unwrap();
    assert_eq!(client.subprotocol(), Some("v1"));

    // Nothing in common, or nothing offered: no subprotocol is selected.
    let unknown = fast_options().subprotocol("mqtt");
    let client = WsClient::dial(&url, unknown).await.unwrap();
    assert_eq!(client.subprotocol(), None);
    let client = WsClient::dial(&url, fast_options()).await.unwrap();
    assert_eq!(client.subprotocol(), None);

    // The raw response carries exactly one protocol header.
    let (_raw, head) = Raw::connect_with(
        server.addr,
        "/",
        "Sec-WebSocket-Protocol: v1, v2\r\nSec-WebSocket-Extensions: permessage-deflate\r\n",
    )
    .await;
    assert_eq!(head.matches("Sec-WebSocket-Protocol:").count(), 1);
    assert!(head.contains("Sec-WebSocket-Protocol: v2\r\n"));
    // No extension is ever accepted.
    assert!(!head
        .to_ascii_lowercase()
        .contains("sec-websocket-extensions"));
    server.stop().await;
}

#[tokio::test]
async fn a_hook_may_choose_the_subprotocol_but_only_an_offered_one() {
    let config = fast_config()
        .subprotocols(["v2", "v1"])
        .on_upgrade(|req: &UpgradeRequest| {
            let wanted = req.query().unwrap_or("v1").to_string();
            Ok(UpgradeAccept::new().subprotocol(wanted))
        });
    let server = start(config, echo).await;
    let offered = || fast_options().subprotocol("v1").subprotocol("v2");

    let client = WsClient::dial(&server.url("/?v1"), offered())
        .await
        .unwrap();
    assert_eq!(client.subprotocol(), Some("v1"));
    // Selecting something the client never offered would make it fail the
    // connection, so the server refuses to do it.
    assert!(matches!(
        WsClient::dial(&server.url("/?v9"), offered()).await,
        Err(WsError::Rejected { status: 500 })
    ));
    server.stop().await;
}

// ---- the connection handle -------------------------------------------------

#[tokio::test]
async fn a_cloned_sender_pushes_from_another_task() {
    let server = start(fast_config(), |mut conn: WsConn| async move {
        let sender = conn.sender();
        let id = conn.id();
        assert_eq!(sender.id(), id);
        // A background task pushes without the handler's involvement.
        let ticker = tokio::spawn(async move {
            for n in 0..3 {
                if sender
                    .send(Message::text(format!("tick {n}")))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            let _ = sender.close(close_code::NORMAL, "done ticking");
        });
        while conn.recv().await.is_some() {}
        let _ = ticker.await;
    })
    .await;

    let mut client = WsClient::dial(&server.url("/"), fast_options())
        .await
        .unwrap();
    for n in 0..3 {
        assert_eq!(
            client.recv().await,
            Some(Message::text(format!("tick {n}")))
        );
    }
    assert_eq!(
        close_of(within("close", client.recv()).await),
        (1000, "done ticking".to_string())
    );
    server.stop().await;
}

#[tokio::test]
async fn connection_ids_are_unique() {
    let (ids_tx, mut ids_rx) = mpsc::unbounded_channel();
    let server = start(fast_config(), move |mut conn: WsConn| {
        let ids_tx = ids_tx.clone();
        async move {
            let _ = ids_tx.send(conn.id());
            while conn.recv().await.is_some() {}
        }
    })
    .await;
    let mut ids = HashSet::new();
    let mut clients = Vec::new();
    for _ in 0..5 {
        let client = WsClient::dial(&server.url("/"), fast_options())
            .await
            .unwrap();
        ids.insert(within("id", ids_rx.recv()).await.unwrap());
        ids.insert(client.id());
        clients.push(client);
    }
    // Five server-side and five client-side connections, all distinct.
    assert_eq!(ids.len(), 10);
    server.stop().await;
}

// ---- C2: the hub -----------------------------------------------------------

/// A chat-style server: `join <room>`, `leave <room>`, `say <room> <text>`,
/// `all <text>`, `to <id> <text>`.
async fn chat_server(hub: Hub, config: WsServerConfig) -> ws_common::TestServer {
    start(config, move |mut conn: WsConn| {
        let hub = hub.clone();
        async move {
            let _member = hub.member(conn.sender());
            let id = conn.id();
            let _ = conn.send(Message::text(format!("id {id}"))).await;
            while let Some(Message::Text(line)) = conn.recv().await {
                let mut parts = line.splitn(3, ' ');
                match (parts.next(), parts.next(), parts.next()) {
                    (Some("join"), Some(room), None) => {
                        hub.join(id, room);
                    }
                    (Some("leave"), Some(room), None) => {
                        hub.leave(id, room);
                    }
                    (Some("say"), Some(room), Some(text)) => {
                        hub.broadcast_room(room, &Message::text(text));
                    }
                    (Some("to"), Some(target), Some(text)) => {
                        let _ = hub.send_to(target.parse().unwrap(), Message::text(text));
                    }
                    (Some("all"), Some(a), b) => {
                        let text = [Some(a), b].into_iter().flatten().collect::<Vec<_>>();
                        hub.broadcast(&Message::text(text.join(" ")));
                    }
                    _ => {}
                }
                // Acknowledge so the test can sequence its steps.
                let _ = conn.send(Message::text(format!("ok {line}"))).await;
            }
        }
    })
    .await
}

struct Chatter {
    client: WsClient,
    id: u64,
}

impl Chatter {
    async fn join(server: &ws_common::TestServer) -> Chatter {
        let mut client = WsClient::dial(&server.url("/"), fast_options())
            .await
            .unwrap();
        let hello = within("id", client.recv()).await.unwrap();
        let id = hello
            .as_text()
            .unwrap()
            .strip_prefix("id ")
            .unwrap()
            .parse()
            .unwrap();
        Chatter { client, id }
    }

    /// Sends a command and returns everything received up to its ack.
    async fn cmd(&mut self, line: &str) -> Vec<String> {
        self.client.send(Message::text(line)).await.unwrap();
        self.until(&format!("ok {line}")).await
    }

    /// Receives until `marker`, returning what came before it.
    async fn until(&mut self, marker: &str) -> Vec<String> {
        let mut before = Vec::new();
        loop {
            let msg = within("chat message", self.client.recv()).await.unwrap();
            let text = msg.as_text().unwrap().to_string();
            if text == marker {
                return before;
            }
            before.push(text);
        }
    }

    async fn next(&mut self) -> String {
        let msg = within("chat message", self.client.recv()).await.unwrap();
        msg.as_text().unwrap().to_string()
    }
}

#[tokio::test]
async fn c2_hub_broadcasts_to_all_a_room_or_one_connection() {
    let hub = Hub::new();
    let server = chat_server(hub.clone(), fast_config()).await;
    let mut a = Chatter::join(&server).await;
    let mut b = Chatter::join(&server).await;
    let mut c = Chatter::join(&server).await;
    assert_eq!(hub.len(), 3);
    assert!(hub.contains(a.id) && hub.contains(b.id) && hub.contains(c.id));

    a.cmd("join red").await;
    b.cmd("join red").await;
    c.cmd("join blue").await;
    assert_eq!(hub.room_len("red"), 2);
    assert_eq!(hub.room_len("blue"), 1);
    assert_eq!(hub.room_len("nowhere"), 0);
    let mut rooms = hub.rooms();
    rooms.sort();
    assert_eq!(rooms, ["blue", "red"]);

    // To a room: only its members.
    assert_eq!(a.cmd("say red hello-red").await, ["hello-red"]);
    assert_eq!(b.next().await, "hello-red");
    // To everyone.
    assert_eq!(c.cmd("all hello everyone").await, ["hello everyone"]);
    assert_eq!(a.next().await, "hello everyone");
    assert_eq!(b.next().await, "hello everyone");
    // To one connection id.
    a.cmd(&format!("to {} psst", c.id)).await;
    assert_eq!(c.next().await, "psst");
    // c was never in red, so it saw nothing of "hello-red": its next
    // message after leaving the queue empty is the marker itself.
    assert!(c.cmd("join blue").await.is_empty());

    // Leaving a room stops room broadcasts.
    b.cmd("leave red").await;
    assert_eq!(hub.room_len("red"), 1);
    assert_eq!(a.cmd("say red only-a").await, ["only-a"]);
    assert!(b.cmd("join green").await.is_empty());

    // From outside any handler, with delivery counts.
    assert_eq!(hub.broadcast(&Message::text("from the hub")), 3);
    assert_eq!(hub.broadcast_room("red", &Message::text("red only")), 1);
    assert_eq!(hub.broadcast_room("nowhere", &Message::text("x")), 0);
    assert!(matches!(
        hub.send_to(u64::MAX, Message::text("x")),
        Err(WsError::Closed)
    ));
    assert_eq!(a.next().await, "from the hub");
    assert_eq!(a.next().await, "red only");

    // A connection that goes away leaves the hub and all its rooms.
    let gone = a.id;
    a.client.close().await.unwrap();
    within("member removed", async {
        while hub.contains(gone) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert_eq!(hub.len(), 2);
    assert_eq!(hub.room_len("red"), 0);
    assert!(!hub.rooms().contains(&"red".to_string()));

    // Closing everything through the hub.
    hub.close_all(close_code::GOING_AWAY, "maintenance")
        .unwrap();
    assert!(hub.is_empty());
    loop {
        let msg = within("close", b.client.recv()).await;
        if let Some(Message::Close { code, reason }) = msg {
            assert_eq!((code, reason.as_str()), (1001, "maintenance"));
            break;
        }
    }
    server.stop().await;
}

/// Registers every connection in `hub` and then sits there; the test drives
/// the hub from outside.
async fn idle_members(hub: Hub, config: WsServerConfig) -> ws_common::TestServer {
    start(config, move |mut conn: WsConn| {
        let hub = hub.clone();
        async move {
            let _member = hub.member(conn.sender());
            let _ = conn.send(Message::text("joined")).await;
            while conn.recv().await.is_some() {}
        }
    })
    .await
}

#[tokio::test]
async fn c2_hub_closes_slow_consumers_with_1008() {
    let recorder = Recorder::new();
    let hub = Hub::new(); // SlowConsumerPolicy::Close
    let config = WsServerConfig {
        send_queue_capacity: 4,
        close_timeout: Duration::from_millis(300),
        ..fast_config()
    }
    .observer(recorder.clone());
    let server = idle_members(hub.clone(), config).await;

    let mut fast = WsClient::dial(&server.url("/"), fast_options())
        .await
        .unwrap();
    assert_eq!(fast.recv().await, Some(Message::text("joined")));
    // A raw peer that stops reading after the greeting.
    let mut slow = Raw::connect(server.addr).await;
    assert_eq!(slow.read_text().await, "joined");
    assert_eq!(hub.len(), 2);

    // Broadcast big messages until the slow peer's queue overflows. The
    // fast client keeps reading, so only the slow one is affected.
    let big = Message::binary(vec![0u8; 256 * 1024]);
    let mut rounds = 0;
    while hub.len() == 2 {
        rounds += 1;
        assert!(rounds < 2000, "the slow consumer was never detected");
        hub.broadcast(&big);
        assert_eq!(
            within("fast client", fast.recv()).await.map(|m| m.len()),
            Some(256 * 1024)
        );
    }
    // The slow consumer was closed with 1008 and removed; the fast one
    // stays.
    assert_eq!(hub.len(), 1);
    let closed = recorder
        .wait_for("slow consumer closed", |e| {
            matches!(e, WsEvent::Closed { .. })
        })
        .await;
    assert!(matches!(
        closed,
        WsEvent::Closed {
            code: close_code::POLICY_VIOLATION,
            ..
        }
    ));
    assert_eq!(hub.broadcast(&Message::text("still serving")), 1);
    assert_eq!(fast.recv().await, Some(Message::text("still serving")));
    drop(slow);
    server.stop().await;
}

#[tokio::test]
async fn c2_hub_drop_policy_skips_messages_instead_of_closing() {
    let hub = Hub::with_policy(SlowConsumerPolicy::Drop);
    let config = WsServerConfig {
        send_queue_capacity: 2,
        ..fast_config()
    };
    let server = idle_members(hub.clone(), config).await;
    let mut slow = Raw::connect(server.addr).await;
    assert_eq!(slow.read_text().await, "joined");

    // No await between the broadcasts, so the writer cannot drain the queue:
    // two fit, the rest are dropped for this consumer.
    let delivered: usize = (0..10)
        .map(|n| hub.broadcast(&Message::text(format!("update {n}"))))
        .sum();
    assert_eq!(delivered, 2);
    assert_eq!(hub.len(), 1, "the consumer stays registered");
    assert_eq!(slow.read_text().await, "update 0");
    assert_eq!(slow.read_text().await, "update 1");
    // Later updates get through again.
    assert_eq!(hub.broadcast(&Message::text("latest")), 1);
    assert_eq!(slow.read_text().await, "latest");
    server.stop().await;
}

// ---- wss and mutual TLS ----------------------------------------------------

fn trust_test_ca() -> ClientTlsConfig {
    ClientTlsConfig::new()
        .without_web_pki_roots()
        .add_root_pem(CA)
        .unwrap()
}

#[tokio::test]
async fn wss_server_and_client_with_a_private_ca() {
    let config = fast_config().tls(TlsConfig::from_pem(SERVER_CERT, SERVER_KEY).unwrap());
    let server = start(config, describe).await;
    let url = format!("wss://localhost:{}/secure", server.addr.port());

    let mut client = WsClient::dial(&url, fast_options().tls(trust_test_ca()))
        .await
        .unwrap();
    let line = within("describe", client.recv()).await.unwrap();
    let line = line.as_text().unwrap();
    assert!(line.contains("path=/secure"), "{line}");
    assert!(line.contains("tls=true"), "{line}");
    client.close().await.unwrap();

    // The default trust store does not know the test CA: verification is
    // never skipped.
    let untrusted = WsClient::dial(&url, fast_options()).await;
    assert!(matches!(untrusted, Err(WsError::Connect(_))));
    // A plaintext client cannot talk to the TLS port.
    let plain = WsClient::dial(
        &format!("ws://localhost:{}/", server.addr.port()),
        WsOptions {
            handshake_timeout: Duration::from_millis(500),
            ..fast_options()
        },
    )
    .await;
    assert!(plain.is_err());
    server.stop().await;
}

#[tokio::test]
async fn wss_with_mutual_tls_exposes_the_client_certificate() {
    let tls = TlsConfig::builder()
        .cert_pem(SERVER_CERT, SERVER_KEY)
        .unwrap()
        .client_auth_required_pem(CA)
        .unwrap()
        .build()
        .unwrap();
    let certs_seen = Arc::new(AtomicUsize::new(usize::MAX));
    let seen = certs_seen.clone();
    let config = fast_config()
        .tls(tls)
        .on_upgrade(move |req: &UpgradeRequest| {
            seen.store(req.peer_certificates().len(), Ordering::SeqCst);
            if req.peer_certificates().is_empty() {
                Err(UpgradeReject::forbidden())
            } else {
                Ok(UpgradeAccept::new())
            }
        });
    let server = start(config, echo).await;
    let url = format!("wss://localhost:{}/", server.addr.port());

    let with_cert = fast_options().tls(
        trust_test_ca()
            .identity_pem(CLIENT_CERT, CLIENT_KEY)
            .unwrap(),
    );
    let mut client = WsClient::dial(&url, with_cert).await.unwrap();
    client.send(Message::text("mtls")).await.unwrap();
    assert_eq!(
        within("echo", client.recv()).await,
        Some(Message::text("echo: mtls"))
    );
    assert!(certs_seen.load(Ordering::SeqCst) >= 1);

    // Without a client certificate the TLS handshake itself fails, so the
    // upgrade never happens.
    let without = WsClient::dial(&url, fast_options().tls(trust_test_ca())).await;
    assert!(without.is_err());
    server.stop().await;
}

// ---- the reconnecting client -----------------------------------------------

#[tokio::test]
async fn reconnecting_client_redials_and_resubscribes() {
    // The server drops each connection after its first message, twice; the
    // third connection stays.
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = connections.clone();
    let server = start(fast_config(), move |mut conn: WsConn| {
        let nth = counter.fetch_add(1, Ordering::SeqCst) + 1;
        async move {
            // The subscription the client's hook sends on every connect.
            let Some(Message::Text(subscription)) = conn.recv().await else {
                return;
            };
            let _ = conn
                .send(Message::text(format!("conn {nth}: {subscription}")))
                .await;
            if nth < 3 {
                let _ = conn.close(4000 + nth as u16, "go away");
            }
            while conn.recv().await.is_some() {}
        }
    })
    .await;

    let hook_calls = Arc::new(Mutex::new(Vec::new()));
    let calls = hook_calls.clone();
    let mut client = ReconnectingClient::new(server.url("/"), fast_options())
        .with_reconnect(ReconnectOptions {
            initial_backoff: Duration::from_millis(20),
            max_backoff: Duration::from_millis(80),
            jitter: 0.5,
            max_attempts: Some(20),
            stable_after: Duration::from_secs(60),
        })
        .on_connect(move |sender: WsSender, reconnected: bool| {
            calls.lock().unwrap().push(reconnected);
            async move { sender.send(Message::text("subscribe prices")).await }
        });
    assert!(!client.is_connected());

    let mut log = Vec::new();
    while log.len() < 5 {
        let msg = within("reconnecting recv", client.recv()).await.unwrap();
        log.push(match msg {
            Message::Text(text) => text,
            Message::Close { code, .. } => format!("lost {code}"),
            other => panic!("{other:?}"),
        });
    }
    assert_eq!(
        log,
        [
            "conn 1: subscribe prices",
            "lost 4001",
            "conn 2: subscribe prices",
            "lost 4002",
            "conn 3: subscribe prices",
        ]
    );
    assert_eq!(client.reconnects(), 2);
    assert!(client.is_connected());
    // The hook ran on every connection and knew which ones were reconnects.
    assert_eq!(*hook_calls.lock().unwrap(), [false, true, true]);
    assert_eq!(connections.load(Ordering::SeqCst), 3);

    // Sending goes to the current connection.
    assert!(client.sender().is_some());
    client.send(Message::text("ping")).await.unwrap();
    client.close().await.unwrap();
    server.stop().await;
}

#[tokio::test]
async fn reconnecting_client_gives_up_after_max_attempts() {
    // A port with nothing listening.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let mut client = ReconnectingClient::new(format!("ws://{addr}/"), fast_options())
        .with_reconnect(ReconnectOptions {
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(40),
            max_attempts: Some(4),
            ..ReconnectOptions::default()
        });
    let started = std::time::Instant::now();
    let result = within("giving up", client.recv()).await;
    assert!(matches!(result, Err(WsError::Connect(_))), "{result:?}");
    // Three backoff delays separate the four attempts (jitter may halve
    // each): at least 5 + 10 + 20 ms.
    assert!(started.elapsed() >= Duration::from_millis(30));
    assert!(!client.is_connected());
    assert_eq!(client.reconnects(), 0);
    assert!(matches!(
        within("send", client.send(Message::text("x"))).await,
        Err(WsError::Connect(_))
    ));
}

#[tokio::test]
async fn reconnecting_client_gives_up_when_the_hook_keeps_failing() {
    let server = start(fast_config(), echo).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let counted = attempts.clone();
    let mut client = ReconnectingClient::new(server.url("/"), fast_options())
        .with_reconnect(ReconnectOptions {
            initial_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(10),
            max_attempts: Some(3),
            ..ReconnectOptions::default()
        })
        .on_connect(move |_sender: WsSender, _reconnected: bool| {
            counted.fetch_add(1, Ordering::SeqCst);
            async move { Err(WsError::Send("subscription refused".into())) }
        });
    let result = within("connect", client.connect()).await;
    assert!(matches!(result, Err(WsError::Send(_))), "{result:?}");
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    server.stop().await;
}

// ---- the observer ----------------------------------------------------------

#[tokio::test]
async fn observer_sees_connections_messages_pings_and_errors() {
    let server_events = Recorder::new();
    let client_events = Recorder::new();
    let server = start(fast_config().observer(server_events.clone()), echo).await;

    let mut client = WsClient::dial(
        &server.url("/"),
        fast_options().observer(client_events.clone()),
    )
    .await
    .unwrap();
    let id = client.id();
    client.send(Message::text("12345")).await.unwrap();
    assert_eq!(client.recv().await, Some(Message::text("echo: 12345")));
    client.close().await.unwrap();

    let client_log = client_events.events();
    assert_eq!(client_log.first(), Some(&WsEvent::Opened { conn_id: id }));
    assert!(client_log.contains(&WsEvent::MessageSent {
        conn_id: id,
        bytes: 5
    }));
    assert!(client_log.contains(&WsEvent::MessageReceived {
        conn_id: id,
        bytes: 11
    }));
    assert_eq!(
        client_log.last(),
        Some(&WsEvent::Closed {
            conn_id: id,
            code: 1000
        })
    );

    // A raw peer exercises the ping counter and a protocol error.
    let mut raw = Raw::connect(server.addr).await;
    raw.send(FIN | 0x9, b"p").await;
    assert!(raw.read().await.is_some());
    raw.send(FIN | 0x40 | OP_TEXT, b"rsv").await;
    assert_eq!(raw.read_close().await.0, close_code::PROTOCOL_ERROR);
    drop(raw);

    let error = server_events
        .wait_for("protocol error", |e| {
            matches!(e, WsEvent::ProtocolError { .. })
        })
        .await;
    assert!(matches!(
        error,
        WsEvent::ProtocolError {
            code: close_code::PROTOCOL_ERROR,
            ..
        }
    ));
    within("both closed", async {
        while server_events.count(|e| matches!(e, WsEvent::Closed { .. })) < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert_eq!(
        server_events.count(|e| matches!(e, WsEvent::Opened { .. })),
        2
    );
    assert_eq!(
        server_events.count(|e| matches!(e, WsEvent::PingReceived { .. })),
        1
    );
    assert_eq!(
        server_events.count(|e| matches!(e, WsEvent::MessageReceived { bytes: 5, .. })),
        1
    );
    assert_eq!(
        server_events.count(|e| matches!(e, WsEvent::MessageSent { bytes: 11, .. })),
        1
    );
    assert!(server_events.events().contains(&WsEvent::Closed {
        conn_id: match error {
            WsEvent::ProtocolError { conn_id, .. } => conn_id,
            _ => unreachable!(),
        },
        code: close_code::PROTOCOL_ERROR,
    }));
    server.stop().await;
}

// ---- W7: masking keys and the nonce ----------------------------------------

/// Reads one client frame from a raw socket and returns its masking key.
async fn read_mask_key(raw: &mut Raw) -> [u8; 4] {
    let mut head = [0u8; 2];
    within("frame head", raw.stream.read_exact(&mut head))
        .await
        .unwrap();
    assert_eq!(head[1] & 0x80, 0x80, "client frames must be masked");
    let len = (head[1] & 0x7f) as usize;
    assert!(len < 126);
    let mut key = [0u8; 4];
    raw.stream.read_exact(&mut key).await.unwrap();
    let mut payload = vec![0u8; len];
    raw.stream.read_exact(&mut payload).await.unwrap();
    key
}

#[tokio::test]
async fn w7_mask_keys_and_nonces_are_per_connection_and_unrelated() {
    // Two connections from the same process to two different "servers".
    let mut nonces = Vec::new();
    let mut streams: Vec<Vec<[u8; 4]>> = Vec::new();
    for _ in 0..2 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepting = tokio::spawn(async move { Raw::accept(&listener, "").await });
        let client = WsClient::dial(&format!("ws://{addr}/"), fast_options())
            .await
            .unwrap();
        let (mut raw, request) = accepting.await.unwrap();
        let key = request
            .lines()
            .find_map(|line| line.strip_prefix("Sec-WebSocket-Key: "))
            .unwrap()
            .to_string();
        let nonce = ferroly::hash::base64::decode(&key).unwrap();
        assert_eq!(nonce.len(), 16);
        nonces.push(nonce);

        let mut keys = Vec::new();
        for _ in 0..40 {
            client.send(Message::text("same payload")).await.unwrap();
            keys.push(read_mask_key(&mut raw).await);
        }
        streams.push(keys);
    }

    // A fresh nonce per handshake.
    assert_ne!(nonces[0], nonces[1]);
    for (nonce, keys) in nonces.iter().zip(&streams) {
        // Keys change from frame to frame (40 keys, essentially all
        // distinct).
        let distinct: HashSet<&[u8; 4]> = keys.iter().collect();
        assert!(distinct.len() >= 39, "{} distinct keys", distinct.len());
        // The nonce on the wire is not the start of the key stream, on its
        // own connection or the other one.
        for other in &streams {
            let prefix: Vec<u8> = other.iter().take(4).flatten().copied().collect();
            assert_ne!(&prefix, nonce);
        }
    }
    // One server's view of the key stream says nothing about the other's:
    // the streams share no key at any position.
    let shared = streams[0]
        .iter()
        .zip(&streams[1])
        .filter(|(a, b)| a == b)
        .count();
    assert_eq!(shared, 0);
    let first: HashSet<&[u8; 4]> = streams[0].iter().collect();
    assert!(streams[1].iter().all(|key| !first.contains(key)));
}
