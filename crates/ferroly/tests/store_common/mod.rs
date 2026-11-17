//! Shared helpers for the `store_*` suites: a loopback HTTP server built
//! from a closure, a range-aware file responder (both `store` only, they
//! need the HTTP stack), and scratch directories (also for the synchronous
//! `store_local` suite).
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "store")]
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(feature = "store")]
use ferroly::http::{serve, BoxFuture, HttpHandler, HttpResponse, Request, StatusCode};
#[cfg(feature = "store")]
use tokio::net::TcpListener;

#[cfg(feature = "store")]
struct FnHandler<F>(F);

#[cfg(feature = "store")]
impl<F> HttpHandler for FnHandler<F>
where
    F: Fn(Request) -> HttpResponse + Send + Sync + 'static,
{
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
        let resp = (self.0)(req);
        Box::pin(async move { resp })
    }
}

/// Serves `handler` on an ephemeral loopback port; returns `http://127.0.0.1:<port>`.
#[cfg(feature = "store")]
pub async fn spawn<F>(handler: F) -> String
where
    F: Fn(Request) -> HttpResponse + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handler: Arc<dyn HttpHandler> = Arc::new(FnHandler(handler));
    tokio::spawn(async move {
        let _ = serve(listener, handler, std::future::pending::<()>()).await;
    });
    format!("http://{addr}")
}

/// Answers `req` for `body`, honouring an open-ended `Range: bytes=N-`
/// (206, or 416 with `bytes */len` past the end).
#[cfg(feature = "store")]
pub fn serve_range(req: &Request, body: &[u8]) -> HttpResponse {
    match req.headers.get("range") {
        Some(r) if r.starts_with("bytes=") => {
            let spec = &r["bytes=".len()..];
            let (start, end) = spec.split_once('-').unwrap_or((spec, ""));
            let start: usize = start.parse().unwrap_or(0);
            if start >= body.len() {
                return HttpResponse::new(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header("content-range", format!("bytes */{}", body.len()));
            }
            let end: usize = end
                .parse()
                .map(|e: usize| e.min(body.len() - 1))
                .unwrap_or(body.len() - 1);
            HttpResponse::new(StatusCode::PARTIAL_CONTENT)
                .header(
                    "content-range",
                    format!("bytes {start}-{end}/{}", body.len()),
                )
                .body(body[start..=end].to_vec())
        }
        _ => HttpResponse::new(StatusCode::OK).body(body.to_vec()),
    }
}

/// Deterministic test content.
pub fn content(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 % 251) as u8).collect()
}

/// A fresh, empty directory under the system temp dir, removed on drop.
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(tag: &str) -> Scratch {
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "ferroly-store-{tag}-{}-{}-{nanos}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Number of regular files directly in `dir`.
pub fn files_in(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|d| {
            d.filter_map(Result::ok)
                .filter(|e| e.path().is_file())
                .count()
        })
        .unwrap_or(0)
}
