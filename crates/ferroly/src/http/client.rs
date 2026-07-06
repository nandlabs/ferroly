//! A minimal HTTP/1.1 client over the transport layer, with keep-alive
//! connection pooling: after a response body is fully drained, its connection is
//! returned to a per-host pool and reused by the next request to the same
//! `host:port`, avoiding a fresh TCP + TLS handshake each time.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::BufReader;

use super::message::{Body, Framing, Reclaim, Request, Response};
use super::pool::{Pool, PoolKey};
use super::transport::Conn;
use super::{io, transport, HeaderMap, HttpError};

/// A low-level HTTP/1.1 client. Higher-level ergonomics (auth, retry,
/// codec bodies) are layered on top in [`crate::clients`] / [`crate::rest`].
///
/// Cloning a `Client` shares its connection pool, so clone freely.
#[derive(Clone)]
pub struct Client {
    tls: Arc<rustls::ClientConfig>,
    timeout: Option<Duration>,
    pool: Arc<Pool>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    /// Creates a client with a 60-second default request timeout.
    pub fn new() -> Self {
        Self {
            tls: transport::tls_config(),
            timeout: Some(Duration::from_secs(60)),
            pool: Pool::new(),
        }
    }

    /// Sets the per-request timeout (`None` disables it).
    pub fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sends a request and returns the response with a streaming body.
    pub async fn send(&self, req: Request) -> Result<Response, HttpError> {
        match self.timeout {
            Some(t) => tokio::time::timeout(t, self.send_inner(req))
                .await
                .map_err(|_| HttpError::Timeout)?,
            None => self.send_inner(req).await,
        }
    }

    async fn send_inner(&self, req: Request) -> Result<Response, HttpError> {
        let key: PoolKey = (req.uri.host.clone(), req.uri.port, req.uri.is_tls());

        // Prefer a pooled connection. If it fails before we have a full response
        // head (the common "server closed the idle connection" case), fall back
        // to a fresh connection — safe because such a request never reached a
        // live server, so re-sending cannot double-process it.
        if let Some(reader) = self.pool.take(&key) {
            if let Ok(resp) = self.exchange(reader, &req, key.clone()).await {
                return Ok(resp);
            }
        }

        let conn = transport::connect(&req.uri, &self.tls).await?;
        let reader = BufReader::new(conn);
        self.exchange(reader, &req, key).await
    }

    /// Writes the request and reads the response head on `reader`, wiring the
    /// body to return the connection to the pool when it drains cleanly (unless
    /// the response is not keep-alive-able).
    async fn exchange(
        &self,
        mut reader: BufReader<Conn>,
        req: &Request,
        key: PoolKey,
    ) -> Result<Response, HttpError> {
        io::write_request(&mut reader, req).await?;
        let (status, headers) = io::read_response_head(&mut reader).await?;
        let framing = io::response_framing(status, &headers);
        let reusable = response_keep_alive(&headers) && !matches!(framing, Framing::Eof);
        let body = if reusable {
            Body::pooled(
                reader,
                framing,
                Reclaim {
                    pool: self.pool.clone(),
                    key,
                },
            )
        } else {
            Body::new(reader, framing)
        };
        Ok(Response::new(status, headers, body))
    }
}

/// Whether the response's `Connection` header permits reusing the connection.
/// Absent header defaults to keep-alive (HTTP/1.1); an explicit `close` disables.
fn response_keep_alive(headers: &HeaderMap) -> bool {
    match headers.get("connection") {
        Some(v) => !v
            .to_ascii_lowercase()
            .split(',')
            .any(|t| t.trim() == "close"),
        None => true,
    }
}
