#![cfg(feature = "turbo")]
//! Resumable/range download tests: a range-aware server driven by the in-house
//! client's `download_to_file`.

use ferroly::http::{download_to_file, serve, Client, HttpResponse, StatusCode};
use ferroly::turbo::Router;
use tokio::net::TcpListener;

/// Spawns a server on an ephemeral port that serves `body` at `/file` and
/// honors an open-ended `Range: bytes=N-` header (206 / 416).
async fn spawn_file_server(body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Router::new().get("/file", move |ctx| {
        let body = body.clone();
        async move {
            match ctx.header("range") {
                Some(r) if r.starts_with("bytes=") => {
                    let start: usize = r
                        .trim_start_matches("bytes=")
                        .trim_end_matches('-')
                        .parse()
                        .unwrap_or(0);
                    if start >= body.len() {
                        return HttpResponse::new(StatusCode::RANGE_NOT_SATISFIABLE);
                    }
                    HttpResponse::new(StatusCode::PARTIAL_CONTENT)
                        .header(
                            "content-range",
                            format!("bytes {}-{}/{}", start, body.len() - 1, body.len()),
                        )
                        .body(body[start..].to_vec())
                }
                _ => HttpResponse::new(StatusCode::OK).body(body.clone()),
            }
        }
    });
    let handler = router.into_handler();
    tokio::spawn(async move {
        let _ = serve(listener, handler, std::future::pending::<()>()).await;
    });
    format!("http://{addr}")
}

fn sample_body(n: usize) -> Vec<u8> {
    (0..n).map(|i| i as u8).collect()
}

#[tokio::test]
async fn downloads_full_file() {
    let body = sample_body(1000);
    let base = spawn_file_server(body.clone()).await;
    let path = std::env::temp_dir().join("ferroly-dl-full.bin");
    let _ = std::fs::remove_file(&path);

    let n = download_to_file(&Client::new(), &format!("{base}/file"), &path)
        .await
        .unwrap();

    assert_eq!(n, 1000);
    assert_eq!(std::fs::read(&path).unwrap(), body);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn resumes_partial_file() {
    let body = sample_body(1000);
    let base = spawn_file_server(body.clone()).await;
    let path = std::env::temp_dir().join("ferroly-dl-resume.bin");
    // A previous, interrupted transfer left the first 400 bytes on disk.
    std::fs::write(&path, &body[..400]).unwrap();

    let n = download_to_file(&Client::new(), &format!("{base}/file"), &path)
        .await
        .unwrap();

    assert_eq!(n, 1000, "resumed download should reach the full length");
    assert_eq!(std::fs::read(&path).unwrap(), body);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn complete_file_is_a_noop() {
    let body = sample_body(500);
    let base = spawn_file_server(body.clone()).await;
    let path = std::env::temp_dir().join("ferroly-dl-complete.bin");
    // File already complete -> Range bytes=500- -> 416 -> nothing to do.
    std::fs::write(&path, &body).unwrap();

    let n = download_to_file(&Client::new(), &format!("{base}/file"), &path)
        .await
        .unwrap();

    assert_eq!(n, 500);
    assert_eq!(std::fs::read(&path).unwrap(), body);
    let _ = std::fs::remove_file(&path);
}
