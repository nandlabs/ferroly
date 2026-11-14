//! A scripted local HTTP server for the GenAI provider tests.
//!
//! It binds `127.0.0.1:0`, records every request it receives (method, path,
//! headers, body) and answers each one with the next scripted [`Reply`]. No
//! test ever talks to a real provider.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::codec::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

/// One request as the server saw it.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Recorded {
    /// The value of a header (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The body parsed as JSON.
    pub fn json(&self) -> Value {
        json::from_str(&self.body).expect("request body is JSON")
    }
}

enum Step {
    /// Write these bytes and flush.
    Send(Vec<u8>),
    Sleep(Duration),
    /// Keep the connection open without sending anything more, until the
    /// client goes away.
    Hang,
}

/// A scripted answer to one request.
pub struct Reply {
    steps: Vec<Step>,
}

impl Reply {
    /// A complete response with a `Content-Length` body and extra headers.
    pub fn with_headers(
        status: u16,
        headers: &[(&str, &str)],
        content_type: &str,
        body: &str,
    ) -> Reply {
        let mut head = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(body.as_bytes());
        Reply {
            steps: vec![Step::Send(bytes)],
        }
    }

    /// A JSON response.
    pub fn json(status: u16, body: &str) -> Reply {
        Reply::with_headers(status, &[], "application/json", body)
    }

    /// A chunked response: each piece is its own HTTP chunk, written and
    /// flushed separately (with a short pause, so pieces arrive as separate
    /// reads on the client).
    pub fn chunked(content_type: &str, pieces: &[&[u8]]) -> Reply {
        let mut steps = Reply::chunked_head(content_type, pieces);
        steps.push(Step::Send(b"0\r\n\r\n".to_vec()));
        Reply { steps }
    }

    /// Like [`chunked`](Reply::chunked) but never finishes the body: after the
    /// pieces the connection is held open and silent.
    pub fn chunked_then_hang(content_type: &str, pieces: &[&[u8]]) -> Reply {
        let mut steps = Reply::chunked_head(content_type, pieces);
        steps.push(Step::Hang);
        Reply { steps }
    }

    fn chunked_head(content_type: &str, pieces: &[&[u8]]) -> Vec<Step> {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\n\r\n"
        );
        let mut steps = vec![Step::Send(head.into_bytes())];
        for piece in pieces {
            let mut chunk = format!("{:x}\r\n", piece.len()).into_bytes();
            chunk.extend_from_slice(piece);
            chunk.extend_from_slice(b"\r\n");
            steps.push(Step::Send(chunk));
            steps.push(Step::Sleep(Duration::from_millis(15)));
        }
        steps
    }

    /// A Server-Sent Events stream: every event is sent as its own chunk.
    pub fn sse(events: &[&str]) -> Reply {
        let pieces: Vec<&[u8]> = events.iter().map(|e| e.as_bytes()).collect();
        Reply::chunked("text/event-stream", &pieces)
    }

    /// A newline-delimited JSON stream: every line is sent as its own chunk.
    pub fn ndjson(lines: &[&str]) -> Reply {
        let owned: Vec<String> = lines.iter().map(|l| format!("{l}\n")).collect();
        let pieces: Vec<&[u8]> = owned.iter().map(|l| l.as_bytes()).collect();
        Reply::chunked("application/x-ndjson", &pieces)
    }

    /// Accept the request and never answer.
    pub fn hang() -> Reply {
        Reply {
            steps: vec![Step::Hang],
        }
    }
}

/// A running scripted server.
pub struct Server {
    /// `http://127.0.0.1:<port>`.
    pub url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
    /// Connections whose client end has gone away.
    closed: Arc<AtomicUsize>,
    closed_signal: Arc<Notify>,
}

impl Server {
    /// Starts a server that answers successive requests with `replies`, in
    /// order. A request beyond the script gets a `500`.
    pub async fn start(replies: Vec<Reply>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Mutex::new(replies.into_iter().collect::<VecDeque<_>>()));
        let closed = Arc::new(AtomicUsize::new(0));
        let closed_signal = Arc::new(Notify::new());
        {
            let (requests, closed, closed_signal) =
                (requests.clone(), closed.clone(), closed_signal.clone());
            tokio::spawn(async move {
                while let Ok((sock, _)) = listener.accept().await {
                    let _ = sock.set_nodelay(true);
                    let (requests, replies, closed, closed_signal) = (
                        requests.clone(),
                        replies.clone(),
                        closed.clone(),
                        closed_signal.clone(),
                    );
                    tokio::spawn(async move {
                        serve(sock, requests, replies).await;
                        closed.fetch_add(1, Ordering::SeqCst);
                        closed_signal.notify_waiters();
                    });
                }
            });
        }
        Server {
            url,
            requests,
            closed,
            closed_signal,
        }
    }

    /// A server with a single reply.
    pub async fn one(reply: Reply) -> Server {
        Server::start(vec![reply]).await
    }

    /// Every request received so far.
    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    /// The `n`-th request (panics if it was never made).
    pub fn request(&self, n: usize) -> Recorded {
        self.requests()
            .get(n)
            .cloned()
            .unwrap_or_else(|| panic!("request #{n} was never received"))
    }

    /// How many requests were received.
    pub fn hits(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    /// Waits until at least `n` connections have been closed by the client,
    /// or `limit` passes. Returns whether they were.
    pub async fn wait_closed(&self, n: usize, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let notified = self.closed_signal.notified();
            if self.closed.load(Ordering::SeqCst) >= n {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.closed.load(Ordering::SeqCst) >= n;
            }
        }
    }
}

/// Reads one request; `None` when the client closed the connection.
async fn read_request(sock: &mut TcpStream, buf: &mut Vec<u8>) -> Option<Recorded> {
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let mut chunk = [0u8; 8192];
        match sock.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or("").split(' ');
    let method = request_line.next().unwrap_or("").to_string();
    let path = request_line.next().unwrap_or("").to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    while buf.len() < head_end + length {
        let mut chunk = [0u8; 8192];
        match sock.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let body = String::from_utf8_lossy(&buf[head_end..head_end + length]).into_owned();
    buf.drain(..head_end + length);
    Some(Recorded {
        method,
        path,
        headers,
        body,
    })
}

async fn serve(
    mut sock: TcpStream,
    requests: Arc<Mutex<Vec<Recorded>>>,
    replies: Arc<Mutex<VecDeque<Reply>>>,
) {
    let mut buf = Vec::new();
    while let Some(request) = read_request(&mut sock, &mut buf).await {
        requests.lock().unwrap().push(request);
        let reply = replies.lock().unwrap().pop_front();
        let Some(reply) = reply else {
            let _ = sock
                .write_all(b"HTTP/1.1 500 X\r\nContent-Length: 12\r\n\r\nno reply set")
                .await;
            continue;
        };
        for step in reply.steps {
            match step {
                Step::Send(bytes) => {
                    if sock.write_all(&bytes).await.is_err() || sock.flush().await.is_err() {
                        return;
                    }
                }
                Step::Sleep(d) => tokio::time::sleep(d).await,
                Step::Hang => {
                    // Wait for the client to go away (read returns 0 / error).
                    let mut sink = [0u8; 64];
                    loop {
                        match sock.read(&mut sink).await {
                            Ok(0) | Err(_) => return,
                            Ok(_) => {}
                        }
                    }
                }
            }
        }
    }
}

/// Parses a JSON literal (test convenience).
pub fn j(text: &str) -> Value {
    json::from_str(text).expect("valid JSON literal")
}
