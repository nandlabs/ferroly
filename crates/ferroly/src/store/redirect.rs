//! Redirect following for the HTTP adapters (HF resolves and OCI blobs
//! answer with redirects to a CDN or object store).
//!
//! Done here rather than through the client's `max_redirects`, so it works
//! with any `Client` and the credential handling is explicit: an
//! `Authorization` header is never sent to a different origin, and an
//! `https` → `http` downgrade is refused.

use ferroly::http::{Client, Method, Request, Response};

use super::StoreError;

/// Redirect hops followed before giving up.
pub(crate) const MAX_REDIRECTS: usize = 10;

/// What to do with a redirect to another origin.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CrossOrigin {
    /// Follow it (without credentials).
    Follow,
    /// Return the redirect response itself.
    Stop,
}

/// Sends `req`, following up to [`MAX_REDIRECTS`] redirects.
pub(crate) async fn send_following(
    client: &Client,
    mut req: Request,
    cross_origin: CrossOrigin,
) -> Result<Response, StoreError> {
    let mut hops = 0;
    loop {
        let resp = client.send(req.clone()).await?;
        let code = resp.status().as_u16();
        if !matches!(code, 301 | 302 | 303 | 307 | 308) {
            return Ok(resp);
        }
        let Some(location) = resp.headers().get("location").map(str::to_string) else {
            return Ok(resp);
        };
        let next = req.uri.resolve(&location)?;
        let same_origin = req.uri.same_origin(&next);
        if !same_origin && cross_origin == CrossOrigin::Stop {
            return Ok(resp);
        }
        if req.uri.is_tls() && !next.is_tls() {
            return Err(StoreError::Protocol(
                "refusing a redirect from https to http".into(),
            ));
        }
        hops += 1;
        if hops > MAX_REDIRECTS {
            return Err(StoreError::Protocol("too many redirects".into()));
        }
        drop(resp);
        if !same_origin {
            for name in ["authorization", "cookie", "proxy-authorization", "host"] {
                req.headers.remove(name);
            }
        }
        if code == 303 && !matches!(req.method, Method::Head) {
            req.method = Method::Get;
            req.body.clear();
        }
        req.uri = next;
    }
}
