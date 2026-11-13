#![cfg(feature = "http")]
//! `download_to_file` against scripted raw-TCP servers that misbehave in the
//! ways a resumable download has to survive: a `Content-Range` that does not
//! match the request, a server that ignores `Range`, and a body cut short.

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ferroly::http::{download_to_file, Client, HttpError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const FULL: &[u8] = b"0123456789abcdefghij"; // 20 bytes

/// A unique scratch file, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "ferroly-dl-strict-{}-{name}.bin",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        Scratch(path)
    }
    fn write(&self, data: &[u8]) {
        std::fs::write(&self.0, data).unwrap();
    }
    fn read(&self) -> Vec<u8> {
        std::fs::read(&self.0).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Reads a request head and returns the value of its `Range` header, if any.
async fn read_range(sock: &mut TcpStream) -> Option<Option<String>> {
    let mut buf = Vec::new();
    while !buf.ends_with(b"\r\n\r\n") {
        let mut byte = [0u8; 1];
        if sock.read_exact(&mut byte).await.is_err() {
            return None;
        }
        buf.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&buf).to_ascii_lowercase();
    Some(
        head.lines()
            .find_map(|l| l.strip_prefix("range:"))
            .map(|v| v.trim().to_string()),
    )
}

/// Serves each connection with `respond(range_header) -> raw response bytes`,
/// closing after the response. Records the `Range` headers received.
async fn server<F, Fut>(respond: F) -> (String, Arc<Mutex<Vec<Option<String>>>>)
where
    F: Fn(Option<String>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Vec<u8>> + Send,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let ranges: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let seen = ranges.clone();
    let respond = Arc::new(respond);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            let respond = respond.clone();
            tokio::spawn(async move {
                if let Some(range) = read_range(&mut sock).await {
                    seen.lock().unwrap().push(range.clone());
                    let reply = respond(range).await;
                    let _ = sock.write_all(&reply).await;
                }
            });
        }
    });
    (format!("http://{addr}/file"), ranges)
}

fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\nConnection: close\r\n{headers}\r\n").into_bytes();
    out.extend_from_slice(body);
    out
}

#[tokio::test]
async fn content_range_must_match_the_requested_offset() {
    // Asked for bytes from 8, the server "resumes" from 5.
    let (url, ranges) = server(|_| async {
        response(
            "206 Partial Content",
            "Content-Range: bytes 5-19/20\r\nContent-Length: 15\r\n",
            &FULL[5..],
        )
    })
    .await;
    let file = Scratch::new("wrong-offset");
    file.write(&FULL[..8]);
    let err = download_to_file(&Client::new(), &url, &file.0)
        .await
        .unwrap_err();
    assert!(matches!(err, HttpError::Protocol(_)), "{err:?}");
    assert_eq!(ranges.lock().unwrap()[0].as_deref(), Some("bytes=8-"));
    // Nothing was appended: the partial file is exactly as it was.
    assert_eq!(file.read(), &FULL[..8]);
}

#[tokio::test]
async fn partial_content_without_a_usable_content_range_is_rejected() {
    for headers in [
        "Content-Length: 12\r\n",
        "Content-Range: bytes 8-19\r\nContent-Length: 12\r\n",
        "Content-Range: items 8-19/20\r\nContent-Length: 12\r\n",
        "Content-Range: bytes 19-8/20\r\nContent-Length: 12\r\n",
        "Content-Range: bytes */20\r\nContent-Length: 12\r\n",
    ] {
        let (url, _ranges) =
            server(move |_| async move { response("206 Partial Content", headers, &FULL[8..]) })
                .await;
        let file = Scratch::new("no-range");
        file.write(&FULL[..8]);
        let err = download_to_file(&Client::new(), &url, &file.0)
            .await
            .unwrap_err();
        assert!(
            matches!(err, HttpError::Protocol(_)),
            "{headers:?}: {err:?}"
        );
        assert_eq!(file.read(), &FULL[..8], "{headers:?}");
    }
}

#[tokio::test]
async fn server_ignoring_range_restarts_the_file() {
    // A 200 with the whole body, although a range was requested.
    let (url, ranges) =
        server(|_| async { response("200 OK", "Content-Length: 20\r\n", FULL) }).await;
    let file = Scratch::new("ignored-range");
    // Stale local bytes that are *not* a prefix of the remote file.
    file.write(b"STALE-STALE");
    let total = download_to_file(&Client::new(), &url, &file.0)
        .await
        .unwrap();
    assert_eq!(total, 20);
    assert_eq!(ranges.lock().unwrap()[0].as_deref(), Some("bytes=11-"));
    // Rewritten from the start, not appended to.
    assert_eq!(file.read(), FULL);
}

#[tokio::test]
async fn truncated_download_is_an_error_and_resumes() {
    // First connection: promise 20 bytes, deliver 7, hang up. Later ones
    // honour the Range request properly.
    let (url, ranges) = server(|range| async move {
        match range {
            None => response("200 OK", "Content-Length: 20\r\n", &FULL[..7]),
            Some(r) => {
                let start: usize = r
                    .trim_start_matches("bytes=")
                    .trim_end_matches('-')
                    .parse()
                    .unwrap();
                response(
                    "206 Partial Content",
                    &format!(
                        "Content-Range: bytes {start}-19/20\r\nContent-Length: {}\r\n",
                        20 - start
                    ),
                    &FULL[start..],
                )
            }
        }
    })
    .await;
    let file = Scratch::new("truncated");
    let client = Client::new();

    // Used to return Ok(7) for a file that is 13 bytes short.
    let err = download_to_file(&client, &url, &file.0).await.unwrap_err();
    assert!(matches!(err, HttpError::Protocol(_)), "{err:?}");
    // What did arrive is kept, so the next call can resume.
    assert_eq!(file.read(), &FULL[..7]);

    let total = download_to_file(&client, &url, &file.0).await.unwrap();
    assert_eq!(total, 20);
    assert_eq!(file.read(), FULL);
    assert_eq!(
        *ranges.lock().unwrap(),
        [None, Some("bytes=7-".to_string())]
    );
}

#[tokio::test]
async fn short_partial_content_is_detected_against_the_total() {
    // The range starts where asked but the response covers less than the rest
    // of the file, and the connection closes cleanly after it.
    let (url, _ranges) = server(|_| async {
        response(
            "206 Partial Content",
            "Content-Range: bytes 8-13/20\r\nContent-Length: 6\r\n",
            &FULL[8..14],
        )
    })
    .await;
    let file = Scratch::new("short-206");
    file.write(&FULL[..8]);
    let err = download_to_file(&Client::new(), &url, &file.0)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("incomplete"), "{err:?}");
    assert_eq!(file.read(), &FULL[..14]);
}

#[tokio::test]
async fn range_not_satisfiable_is_checked_against_the_remote_size() {
    // Complete already: 416 with a matching total.
    let (url, _ranges) = server(|_| async {
        response(
            "416 Range Not Satisfiable",
            "Content-Range: bytes */20\r\nContent-Length: 0\r\n",
            b"",
        )
    })
    .await;
    let file = Scratch::new("complete");
    file.write(FULL);
    assert_eq!(
        download_to_file(&Client::new(), &url, &file.0)
            .await
            .unwrap(),
        20
    );
    assert_eq!(file.read(), FULL);

    // The local file is longer than the remote one: not "complete".
    let bigger = Scratch::new("bigger");
    bigger.write(b"0123456789abcdefghij-and-then-some");
    let err = download_to_file(&Client::new(), &url, &bigger.0)
        .await
        .unwrap_err();
    assert!(matches!(err, HttpError::Protocol(_)), "{err:?}");

    // A 416 that does not state the size keeps the old behaviour.
    let (url, _ranges) =
        server(|_| async { response("416 Range Not Satisfiable", "Content-Length: 0\r\n", b"") })
            .await;
    assert_eq!(
        download_to_file(&Client::new(), &url, &file.0)
            .await
            .unwrap(),
        20
    );
}

#[tokio::test]
async fn download_is_not_subject_to_the_response_body_cap() {
    let body: Arc<Vec<u8>> = Arc::new((0..200_000u32).map(|i| i as u8).collect());
    let served = body.clone();
    let (url, _ranges) = server(move |_| {
        let served = served.clone();
        async move {
            response(
                "200 OK",
                &format!("Content-Length: {}\r\n", served.len()),
                &served,
            )
        }
    })
    .await;
    let file = Scratch::new("uncapped");
    // A client whose in-memory cap is far below the file size.
    let client = Client::builder()
        .max_response_body_bytes(Some(1024))
        .build()
        .unwrap();
    let total = download_to_file(&client, &url, &file.0).await.unwrap();
    assert_eq!(total, 200_000);
    assert_eq!(file.read(), *body);
}
