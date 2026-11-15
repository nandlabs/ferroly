//! A minimal HTTP/2 server for the h2spec conformance suite
//! (<https://github.com/summerwind/h2spec>).
//!
//! It serves cleartext HTTP/2 with prior knowledge (h2c) on 127.0.0.1 and
//! answers every request with a short `200`, reading the request body first
//! — except `/bytes/<n>`, which answers `n` bytes. `interop/h2spec.sh` starts
//! it and runs h2spec against it, and `interop/go-http.sh` drives it with Go's
//! `net/http` client; by hand:
//!
//! ```sh
//! cargo run -p ferroly --features http2 --example h2spec_server -- 8080
//! h2spec -h 127.0.0.1 -p 8080
//! ```

use std::process::ExitCode;
use std::sync::Arc;

use ferroly::http::{
    serve_with_config, BoxFuture, Http2Config, HttpHandler, HttpResponse, Request, ServerConfig,
    StatusCode,
};

struct Hello;

impl HttpHandler for Hello {
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
        Box::pin(async move {
            if let Some(n) = req.uri.path.strip_prefix("/bytes/") {
                let n: usize = n.parse().unwrap_or(0).min(64 << 20);
                return HttpResponse::new(StatusCode::OK).body(vec![b'x'; n]);
            }
            let body = format!(
                "{} {} ({} body bytes)\n",
                req.method.as_str(),
                req.uri.path,
                req.body.len()
            );
            HttpResponse::text(StatusCode::OK, body)
        })
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let port: u16 = match std::env::args().nth(1).map(|p| p.parse()) {
        None => 8080,
        Some(Ok(p)) => p,
        Some(Err(_)) => {
            eprintln!("usage: h2spec_server [port]");
            return ExitCode::from(2);
        }
    };
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("bind 127.0.0.1:{port}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let config = ServerConfig {
        http2: Some(Http2Config::default()),
        ..ServerConfig::default()
    };
    eprintln!("[h2spec_server] listening on 127.0.0.1:{port} (h2c, prior knowledge)");
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    match serve_with_config(listener, Arc::new(Hello), shutdown, config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("server: {e}");
            ExitCode::FAILURE
        }
    }
}
