#![cfg(feature = "http")]
//! The client's per-host active-connection cap: concurrent requests beyond
//! it queue for a slot, the wait honours the request timeout, and a slot is
//! released when the response body is finished or dropped.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::http::{
    serve, BoxFuture, Client, HttpError, HttpHandler, HttpResponse, Method, Request, StatusCode,
};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

#[derive(Default)]
struct Server {
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    peers: Mutex<HashSet<u16>>,
    /// Keeps `/hold` response streams open.
    held: Mutex<Vec<mpsc::Sender<Vec<u8>>>>,
}

impl HttpHandler for Server {
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
        if let Some(peer) = req.peer_addr() {
            self.peers.lock().unwrap().insert(peer.port());
        }
        Box::pin(async move {
            if req.uri.path == "/close" {
                // Not kept alive: the connection ends with this response.
                return HttpResponse::text(StatusCode::OK, "closed").header("connection", "close");
            }
            if req.uri.path == "/hold" {
                let (tx, rx) = mpsc::channel(1);
                let _ = tx.send(b"start".to_vec()).await;
                self.held.lock().unwrap().push(tx);
                return HttpResponse::stream(StatusCode::OK, rx);
            }
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(60)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            HttpResponse::text(StatusCode::OK, "done")
        })
    }
}

async fn start() -> (String, Arc<Server>) {
    let server = Arc::new(Server::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handler = server.clone();
    tokio::spawn(async move {
        let _ = serve(listener, handler, std::future::pending::<()>()).await;
    });
    (format!("http://{addr}"), server)
}

fn get(url: &str) -> Request {
    Request::builder(Method::Get, url).unwrap().build()
}

#[tokio::test]
async fn concurrent_requests_beyond_the_cap_wait_for_a_slot() {
    let (base, server) = start().await;
    let client = Client::builder()
        .pool_max_active_per_host(Some(2))
        .build()
        .unwrap();
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let client = client.clone();
        let url = format!("{base}/work");
        tasks.push(tokio::spawn(async move {
            client.send(get(&url)).await?.text().await
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap().unwrap(), "done");
    }
    assert_eq!(server.max_in_flight.load(Ordering::SeqCst), 2);
    // Finished connections are reused, so no more than two were opened.
    assert!(server.peers.lock().unwrap().len() <= 2);

    // Without a cap the same burst runs all at once.
    let (base, server) = start().await;
    let client = Client::new();
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let client = client.clone();
        let url = format!("{base}/work");
        tasks.push(tokio::spawn(async move {
            client.send(get(&url)).await?.text().await
        }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert!(server.max_in_flight.load(Ordering::SeqCst) > 2);
}

#[tokio::test]
async fn the_wait_for_a_slot_honours_the_timeout_and_a_dropped_response_frees_it() {
    let (base, _server) = start().await;
    let client = Client::builder()
        .pool_max_active_per_host(Some(1))
        .timeout(Some(Duration::from_millis(200)))
        .build()
        .unwrap();
    // An unfinished response body keeps its connection, and its slot, busy.
    let mut held = client.send(get(&format!("{base}/hold"))).await.unwrap();
    assert_eq!(held.chunk().await.unwrap().unwrap(), b"start");
    let err = client.send(get(&format!("{base}/work"))).await.unwrap_err();
    assert!(matches!(err, HttpError::Timeout), "{err:?}");
    // Another host is not affected.
    let (other, _server2) = start().await;
    let text = client.send(get(&format!("{other}/work"))).await.unwrap();
    assert_eq!(text.text().await.unwrap(), "done");
    // Dropping the response releases the slot.
    drop(held);
    let resp = client.send(get(&format!("{base}/work"))).await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "done");
    // A finished body releases it too: two sequential requests in a row.
    for _ in 0..2 {
        let resp = client.send(get(&format!("{base}/work"))).await.unwrap();
        assert_eq!(resp.text().await.unwrap(), "done");
    }
}

/// HTTP-7: a response on a connection that is not kept alive
/// (`Connection: close`) releases its slot once its body has been read to
/// the end, or at once when it has no body (`HEAD`), even while the caller
/// still holds the response.
#[tokio::test]
async fn a_finished_body_on_a_closed_connection_frees_its_slot() {
    let (base, _server) = start().await;
    let client = Client::builder()
        .pool_max_active_per_host(Some(1))
        .timeout(Some(Duration::from_millis(500)))
        .build()
        .unwrap();
    let mut first = client.send(get(&format!("{base}/close"))).await.unwrap();
    while first.chunk().await.unwrap().is_some() {}
    let next = client
        .send(get(&format!("{base}/work")))
        .await
        .expect("the slot was released at the end of the body");
    assert_eq!(next.text().await.unwrap(), "done");

    let head = Request::builder(Method::Head, &format!("{base}/close"))
        .unwrap()
        .build();
    let head = client.send(head).await.unwrap();
    assert_eq!(head.status(), StatusCode::OK);
    let next = client
        .send(get(&format!("{base}/work")))
        .await
        .expect("a response without a body holds no slot");
    assert_eq!(next.text().await.unwrap(), "done");
    drop((first, head));
}
