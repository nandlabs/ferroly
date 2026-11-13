//! A first-class, in-house HTTP router and server.
//!
//! Built directly on [`ferroly::http`] — routing with path parameters and
//! catch-alls, groups and mounted sub-routers, typed request extraction,
//! filters and an authenticator hook (synchronous or asynchronous), onion
//! middleware, and a set of ready-made middleware (CORS, security headers,
//! request ids, rate limiting, static files), with no external web framework.
//!
//! ```no_run
//! use ferroly::turbo::Router;
//! use ferroly::http::{HttpResponse, StatusCode};
//!
//! # async fn ex() -> Result<(), ferroly::http::HttpError> {
//! let router = Router::new()
//!     .get("/greet/:name", |ctx| async move {
//!         let name = ctx.param("name").unwrap_or("world").to_string();
//!         HttpResponse::text(StatusCode::OK, format!("hi {name}"))
//!     });
//! router.serve("127.0.0.1:8080").await
//! # }
//! ```
//!
//! # Security model
//!
//! - **One path.** The request path is normalised once
//!   ([`normalize_path`](ferroly::http::normalize_path)) before middleware,
//!   authentication, filters and routing run; a target that cannot be
//!   normalised is answered `400`. Paths are **case-sensitive**.
//! - **One matcher.** Group filters are scoped with the same segment matcher
//!   that matches routes; see [`Group`].
//! - **Decode after routing.** Path parameters are percent-decoded after the
//!   route is chosen, so an encoded `/` cannot change the route.
//! - **Peer address, not headers.** Client addresses come from the TCP
//!   connection; forwarding headers are read only from proxies you declare
//!   with [`TrustedProxies`].

#![deny(missing_docs)]

mod cookie;
mod ctx;
mod error;
mod form;
#[cfg(feature = "auth")]
mod jwt;
mod observe;
mod pattern;
mod ratelimit;
mod respond;
mod router;
mod security;
mod static_files;

pub use cookie::{parse_cookies, CookieError, SameSite, SetCookie};
pub use ctx::{Ctx, Extensions};
pub use error::{ParamError, RouteError};
pub use form::{parse_multipart, Form, FormError, MultipartLimits, Part};
#[cfg(feature = "auth")]
pub use jwt::{
    bearer_token, require_any_role, require_any_scope, require_roles, require_scopes, Claims,
    JwtAuth,
};
#[cfg(feature = "log")]
pub use observe::ACCESS_LOG_TARGET;
#[cfg(feature = "metrics")]
pub use observe::UNMATCHED_ROUTE;
pub use ratelimit::{ForwardedHeader, RateLimit, TrustedProxies};
#[cfg(feature = "errutils")]
pub use respond::Classified;
pub use respond::{IntoResponse, Problem, PROBLEM_CONTENT_TYPE};
pub use router::{Group, Next, Route, Router};
pub use security::{
    is_valid_request_id, Cors, FrameOptions, RequestId, SecurityHeaders, REQUEST_ID_HEADER,
};
#[cfg(feature = "hash")]
pub use security::{Csrf, CsrfToken};
pub use static_files::StaticFiles;
