//! The per-request context handed to handlers, filters and middleware.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};

use ferroly::codec::{schema, CodecError, Decode, Encode, Value};
use ferroly::http::{normalize_path, HeaderMap, HttpResponse, Method, Request, StatusCode};

use super::cookie::parse_cookies;
use super::error::ParamError;
use super::form::{self, Form, FormError, MultipartLimits, Part};
use super::respond::Problem;
use super::router::Resolution;

type AnyMap = HashMap<TypeId, Arc<dyn Any + Send + Sync>>;

/// Application state registered with [`Router::with_state`](super::Router::with_state).
pub(crate) type StateMap = AnyMap;

/// A typed, request-scoped map for passing values from middleware and filters
/// to the handlers that run after them — the authenticated identity, verified
/// token claims, a request id.
///
/// At most one value is stored per type; wrap a plain value in your own
/// newtype to give it a distinct slot. The handle is cheap to clone and all
/// clones refer to the same map, so an asynchronous filter can move a clone
/// into its future and insert a value once its lookup has finished.
///
/// Values are handed out as `Arc<T>`, which is why they need not be `Clone`.
#[derive(Clone, Default)]
pub struct Extensions {
    inner: Arc<Mutex<AnyMap>>,
}

impl std::fmt::Debug for Extensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Extensions")
            .field("len", &self.lock().len())
            .finish()
    }
}

impl Extensions {
    /// An empty map.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, AnyMap> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Stores `value`, returning the value of the same type it replaced.
    pub fn insert<T: Send + Sync + 'static>(&self, value: T) -> Option<Arc<T>> {
        self.lock()
            .insert(TypeId::of::<T>(), Arc::new(value))
            .and_then(|old| old.downcast::<T>().ok())
    }

    /// The stored value of type `T`.
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.lock()
            .get(&TypeId::of::<T>())
            .cloned()
            .and_then(|v| v.downcast::<T>().ok())
    }

    /// Removes and returns the stored value of type `T`.
    pub fn remove<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.lock()
            .remove(&TypeId::of::<T>())
            .and_then(|v| v.downcast::<T>().ok())
    }

    /// Whether a value of type `T` is stored.
    pub fn contains<T: Send + Sync + 'static>(&self) -> bool {
        self.lock().contains_key(&TypeId::of::<T>())
    }
}

/// Response headers queued on the context, applied when the response leaves
/// the router. `(name, value, append)`.
pub(crate) type PendingHeaders = Arc<Mutex<Vec<(String, String, bool)>>>;

pub(crate) fn apply_pending(pending: &PendingHeaders, mut resp: HttpResponse) -> HttpResponse {
    let queued: Vec<(String, String, bool)> = {
        let mut list = pending.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *list)
    };
    for (name, value, append) in queued {
        if append {
            resp.headers.append(name, value);
        } else if !resp.headers.contains(&name) {
            resp.headers.set(name, value);
        }
    }
    resp
}

/// The per-request context passed to handlers.
pub struct Ctx {
    request: Request,
    params: HashMap<String, String>,
    tail: Vec<String>,
    query: Vec<(String, String)>,
    route: Option<Arc<str>>,
    extensions: Extensions,
    state: Arc<StateMap>,
    pending: PendingHeaders,
    pub(crate) resolution: Option<Resolution>,
}

impl std::fmt::Debug for Ctx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx")
            .field("method", &self.request.method)
            .field("path", &self.request.uri.path)
            .field("route", &self.route)
            .finish_non_exhaustive()
    }
}

/// Why a request could not be turned into a context.
pub(crate) enum Reject {
    Path,
    Query,
}

impl Ctx {
    /// Normalises the request path and decodes the query — once, before
    /// anything looks at either.
    pub(crate) fn prepare(mut request: Request, state: Arc<StateMap>) -> Result<Ctx, Reject> {
        request.uri.path = normalize_path(&request.uri.path).map_err(|_| Reject::Path)?;
        let query = match request.uri.query.as_deref() {
            Some(q) => form::parse_pairs(q).ok_or(Reject::Query)?,
            None => Vec::new(),
        };
        Ok(Ctx {
            request,
            params: HashMap::new(),
            tail: Vec::new(),
            query,
            route: None,
            extensions: Extensions::new(),
            state,
            pending: PendingHeaders::default(),
            resolution: None,
        })
    }

    /// Builds a context from a request, outside a router — for unit-testing a
    /// handler or filter, or for a middleware that rewrites the request
    /// (`into_request`, modify, `from_request`, then [`Next::run`]).
    ///
    /// The path is normalised exactly as the router does it
    /// ([`normalize_path`]) and the query is decoded; either failing is an
    /// error. The context has no route parameters and no application state; a
    /// context passed on to [`Next::run`] is routed afresh, with the state of
    /// that router.
    ///
    /// [`Next::run`]: super::Next::run
    pub fn from_request(request: Request) -> Result<Ctx, ParamError> {
        Ctx::prepare(request, Arc::new(StateMap::new())).map_err(|reject| ParamError::Invalid {
            name: match reject {
                Reject::Path => "path".to_string(),
                Reject::Query => "query".to_string(),
            },
            reason: "invalid percent-encoding, UTF-8 or path structure".to_string(),
        })
    }

    pub(crate) fn set_match(
        &mut self,
        route: Option<Arc<str>>,
        params: HashMap<String, String>,
        tail: Vec<String>,
    ) {
        self.route = route;
        self.params = params;
        self.tail = tail;
    }

    pub(crate) fn set_state(&mut self, state: Arc<StateMap>) {
        self.state = state;
    }

    pub(crate) fn pending(&self) -> PendingHeaders {
        self.pending.clone()
    }

    // ---- request line ----------------------------------------------------

    /// The request method.
    pub fn method(&self) -> &Method {
        &self.request.method
    }

    /// The request path, **normalised**: `.`/`..` segments resolved, escapes
    /// of unreserved characters decoded, the remaining escapes kept (in
    /// upper-case hex) so an encoded `/` (`%2F`) stays distinct from a real
    /// one. Middleware, filters, routing and handlers all see this same
    /// value. Use [`param`](Self::param) for decoded parameter values.
    pub fn path(&self) -> &str {
        &self.request.uri.path
    }

    /// The template of the matched route — `/users/:id`, never the concrete
    /// path — or `None` when no route matched. Available to middleware and
    /// filters as well as handlers; use it (not [`path`](Self::path)) as a
    /// metrics or span label.
    pub fn route(&self) -> Option<&str> {
        self.route.as_deref()
    }

    // ---- path parameters -------------------------------------------------

    /// A captured path parameter (`:name`, or a `*name` catch-all),
    /// percent-decoded.
    ///
    /// Decoding happens after routing, so an encoded `/` in a value can never
    /// change which route matched: for `/files/:name`, a request for
    /// `/files/a%2Fb` yields `"a/b"` here. A catch-all yields the remaining
    /// segments joined with `/`; see [`tail_segments`](Self::tail_segments)
    /// for the individual segments.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.get(name).map(String::as_str)
    }

    /// A path parameter parsed into `T` via [`FromStr`].
    pub fn param_as<T>(&self, name: &str) -> Result<T, ParamError>
    where
        T: FromStr,
        T::Err: std::fmt::Display,
    {
        let raw = self
            .param(name)
            .ok_or_else(|| ParamError::Missing(name.to_string()))?;
        raw.parse().map_err(|e: T::Err| ParamError::Invalid {
            name: name.to_string(),
            reason: e.to_string(),
        })
    }

    /// The segments captured by a `*name` catch-all, each percent-decoded on
    /// its own (so a segment that contained `%2F` shows up as one element
    /// with a `/` in it). Empty when the route has no catch-all.
    pub fn tail_segments(&self) -> &[String] {
        &self.tail
    }

    // ---- query -----------------------------------------------------------

    /// The first query-string value for `name`. Keys and values are
    /// percent-decoded and `+` is a space.
    pub fn query_param(&self, name: &str) -> Option<String> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    }

    /// Every query-string value for `name`, in order.
    pub fn query_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.query
            .iter()
            .filter(move |(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// All decoded query `(key, value)` pairs, in order.
    pub fn query_pairs(&self) -> &[(String, String)] {
        &self.query
    }

    /// The query string exactly as sent (without the leading `?`).
    pub fn raw_query(&self) -> Option<&str> {
        self.request.uri.query.as_deref()
    }

    /// A query parameter parsed as an integer (`None` if absent or unparseable).
    pub fn query_int(&self, name: &str) -> Option<i64> {
        self.query_param(name)?.trim().parse().ok()
    }

    /// A query parameter parsed as a float (`None` if absent or unparseable).
    pub fn query_float(&self, name: &str) -> Option<f64> {
        self.query_param(name)?.trim().parse().ok()
    }

    /// A query parameter parsed as a boolean — `true`/`1`/`yes`/`on` and
    /// `false`/`0`/`no`/`off` (case-insensitive); `None` otherwise.
    pub fn query_bool(&self, name: &str) -> Option<bool> {
        match self.query_param(name)?.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        }
    }

    /// A query parameter parsed into `T` via [`FromStr`], erroring like
    /// [`param_as`](Self::param_as) when absent or invalid.
    pub fn query_as<T>(&self, name: &str) -> Result<T, ParamError>
    where
        T: FromStr,
        T::Err: std::fmt::Display,
    {
        let raw = self
            .query_param(name)
            .ok_or_else(|| ParamError::Missing(name.to_string()))?;
        raw.trim().parse().map_err(|e: T::Err| ParamError::Invalid {
            name: name.to_string(),
            reason: e.to_string(),
        })
    }

    // ---- headers, cookies, connection --------------------------------------

    /// A request header value (the first, if the header is repeated).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.request.headers.get(name)
    }

    /// Every value of a repeated request header, in order.
    pub fn header_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.request.headers.get_all(name)
    }

    /// All request headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.request.headers
    }

    /// The request's media type: the `Content-Type` without parameters,
    /// lower-cased (`application/json`).
    pub fn content_type(&self) -> Option<String> {
        let raw = self.header("content-type")?;
        let media = raw.split(';').next().unwrap_or(raw).trim();
        (!media.is_empty()).then(|| media.to_ascii_lowercase())
    }

    /// The value of the request cookie called `name` (the first, if several).
    pub fn cookie(&self, name: &str) -> Option<String> {
        self.cookies()
            .into_iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }

    /// Every request cookie as `(name, value)`, in order. Values are not
    /// decoded; see [`parse_cookies`].
    pub fn cookies(&self) -> Vec<(String, String)> {
        self.request
            .headers
            .get_all("cookie")
            .flat_map(parse_cookies)
            .collect()
    }

    /// The address of the directly connected peer. Behind a reverse proxy
    /// this is the proxy; see [`TrustedProxies`](super::TrustedProxies) for
    /// recovering the client address from forwarding headers. `None` only for
    /// a hand-built request.
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.request.peer_addr()
    }

    /// The local address the connection was accepted on.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.request.local_addr()
    }

    /// Whether the request arrived over TLS terminated by this server.
    pub fn is_tls(&self) -> bool {
        self.request.is_tls()
    }

    /// The client's verified certificate chain (DER, leaf first) under mutual
    /// TLS; empty otherwise.
    pub fn peer_certificates(&self) -> &[Vec<u8>] {
        self.request.peer_certificates()
    }

    /// The underlying request.
    pub fn request(&self) -> &Request {
        &self.request
    }

    // ---- body --------------------------------------------------------------

    /// The raw request body bytes.
    pub fn body(&self) -> &[u8] {
        &self.request.body
    }

    /// Decodes the body into `T`, selecting the codec from `Content-Type`
    /// (defaulting to JSON).
    ///
    /// The client chooses the codec through that header. To accept one format
    /// only, use [`read_as`](Self::read_as).
    pub fn read<T: Decode>(&self) -> Result<T, CodecError> {
        let ct = self.header("content-type").unwrap_or("application/json");
        ferroly::codec::decode(ct, &self.request.body)
    }

    /// Decodes the body into `T` with the codec for `content_type`,
    /// whatever the request's `Content-Type` says.
    pub fn read_as<T: Decode>(&self, content_type: &str) -> Result<T, CodecError> {
        ferroly::codec::decode(content_type, &self.request.body)
    }

    /// Decodes the body and validates it against a schema
    /// ([`ferroly::codec::schema`], a JSON-Schema subset), returning the
    /// decoded [`Value`].
    ///
    /// An undecodable body is a `400` (or `415`) [`Problem`]; a body that
    /// decodes but breaks the schema is a `422` whose `errors` member lists
    /// every violation as `{ "path": …, "message": … }`.
    ///
    /// ```
    /// use ferroly::codec::json;
    /// use ferroly::http::{HttpResponse, StatusCode};
    /// use ferroly::turbo::{Ctx, Problem, Router};
    ///
    /// async fn create(ctx: Ctx) -> Result<HttpResponse, Problem> {
    ///     let schema = json::from_str(
    ///         r#"{"type":"object","required":["name"],
    ///             "properties":{"name":{"type":"string","minLength":1}}}"#,
    ///     ).expect("valid schema");
    ///     let body = ctx.validate(&schema)?;
    ///     let name = body.get("name").and_then(|v| v.as_str()).unwrap_or_default();
    ///     Ok(HttpResponse::text(StatusCode::CREATED, name.to_string()))
    /// }
    ///
    /// let _router = Router::new().post("/items", create);
    /// ```
    pub fn validate(&self, schema: &Value) -> Result<Value, Problem> {
        let value: Value = self.read().map_err(|e| Problem::from(&e))?;
        match schema::validate(schema, &value) {
            Ok(()) => Ok(value),
            Err(violations) => {
                let errors = violations
                    .into_iter()
                    .map(|v| {
                        Value::Object(vec![
                            ("path".to_string(), Value::Str(v.path)),
                            ("message".to_string(), Value::Str(v.message)),
                        ])
                    })
                    .collect::<Vec<_>>();
                Err(Problem::new(StatusCode(422))
                    .detail("the request body failed validation")
                    .with("errors", Value::Array(errors)))
            }
        }
    }

    /// [`validate`](Self::validate)s the body against `schema`, then decodes
    /// it into `T`.
    pub fn read_valid<T: Decode>(&self, schema: &Value) -> Result<T, Problem> {
        self.validate(schema)?;
        self.read().map_err(|e| Problem::from(&e))
    }

    /// Parses an `application/x-www-form-urlencoded` body. A different
    /// `Content-Type` is [`FormError::ContentType`].
    pub fn form(&self) -> Result<Form, FormError> {
        match self.content_type().as_deref() {
            Some("application/x-www-form-urlencoded") => Form::parse(&self.request.body),
            other => Err(FormError::ContentType(
                other.unwrap_or("(none)").to_string(),
            )),
        }
    }

    /// Parses a `multipart/form-data` body within `limits`; see
    /// [`parse_multipart`](super::parse_multipart).
    pub fn multipart(&self, limits: &MultipartLimits) -> Result<Vec<Part>, FormError> {
        let content_type = self
            .header("content-type")
            .ok_or_else(|| FormError::ContentType("(none)".to_string()))?;
        form::parse_multipart(content_type, &self.request.body, limits)
    }

    // ---- responses ---------------------------------------------------------

    /// Encodes `value` as the response body, picking the codec from the request's
    /// `Accept` header (JSON / XML / YAML / TOML; JSON when `Accept` is absent or
    /// `*/*`). Returns `406 Not Acceptable` if no supported type matches.
    pub fn respond<T: Encode>(&self, status: StatusCode, value: &T) -> HttpResponse {
        let accept = self.header("accept").unwrap_or("");
        match negotiate_content_type(accept) {
            Some(ct) => match ferroly::codec::encode(ct, value) {
                Ok(bytes) => HttpResponse::new(status)
                    .header("content-type", ct)
                    .body(bytes),
                Err(_) => HttpResponse::text(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "500 response encode error",
                ),
            },
            None => HttpResponse::text(StatusCode::NOT_ACCEPTABLE, "406 Not Acceptable"),
        }
    }

    /// Queues a response header. It is added to the response when it leaves
    /// the router — whoever produces it (the handler, a filter, the 404
    /// fallback) — unless the response already has a header of that name.
    ///
    /// This is how a filter, which only sees `&Ctx`, contributes headers such
    /// as `RateLimit-Remaining`.
    pub fn set_response_header(&self, name: impl Into<String>, value: impl Into<String>) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((name.into(), value.into(), false));
    }

    /// Like [`set_response_header`](Self::set_response_header), but always
    /// added, next to any header of the same name (for `Set-Cookie`, `Vary`).
    pub fn append_response_header(&self, name: impl Into<String>, value: impl Into<String>) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((name.into(), value.into(), true));
    }

    // ---- extensions and state ----------------------------------------------

    /// Stores a request-scoped value for later middleware, filters and the
    /// handler; see [`Extensions`]. Replaces a previous value of the same
    /// type.
    ///
    /// ```
    /// use ferroly::http::{HttpResponse, StatusCode};
    /// use ferroly::turbo::Router;
    ///
    /// struct UserId(u64);
    ///
    /// let _router = Router::new()
    ///     .filter(|ctx| {
    ///         match ctx.header("x-user").and_then(|v| v.parse().ok()) {
    ///             Some(id) => {
    ///                 ctx.set(UserId(id));
    ///                 None
    ///             }
    ///             None => Some(HttpResponse::text(StatusCode::UNAUTHORIZED, "who are you?")),
    ///         }
    ///     })
    ///     .get("/me", |ctx| async move {
    ///         let id = ctx.get::<UserId>().map(|u| u.0).unwrap_or(0);
    ///         HttpResponse::text(StatusCode::OK, format!("user {id}"))
    ///     });
    /// ```
    pub fn set<T: Send + Sync + 'static>(&self, value: T) {
        self.extensions.insert(value);
    }

    /// The request-scoped value of type `T`, if one was stored.
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.extensions.get::<T>()
    }

    /// Removes and returns the request-scoped value of type `T`.
    pub fn remove<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.extensions.remove::<T>()
    }

    /// A handle to this request's [`Extensions`], for moving into a future.
    pub fn extensions(&self) -> Extensions {
        self.extensions.clone()
    }

    /// Application state of type `T` registered with
    /// [`Router::with_state`](super::Router::with_state).
    pub fn state<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.state
            .get(&TypeId::of::<T>())
            .cloned()
            .and_then(|v| v.downcast::<T>().ok())
    }

    /// Consumes the context, returning the underlying request (with the
    /// normalised path).
    pub fn into_request(self) -> Request {
        self.request
    }
}

/// Picks a response content type from an `Accept` header (JSON default), or
/// `None` if nothing supported matches.
fn negotiate_content_type(accept: &str) -> Option<&'static str> {
    if accept.trim().is_empty() {
        return Some("application/json");
    }
    for part in accept.split(',') {
        let ct = part.split(';').next().unwrap_or("").trim();
        match ct {
            "*/*" | "application/*" | "application/json" | "text/json" => {
                return Some("application/json")
            }
            "application/xml" | "text/xml" => return Some("application/xml"),
            "application/yaml" | "text/yaml" | "application/x-yaml" | "text/x-yaml"
            | "application/yml" | "text/yml" => return Some("application/yaml"),
            "application/toml" | "text/toml" | "application/x-toml" => {
                return Some("application/toml")
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::http::Uri;

    fn ctx(target: &str) -> Result<Ctx, ParamError> {
        let (path, query) = ferroly::http::split_target(target);
        let mut uri = Uri::parse("http://localhost/").unwrap();
        uri.path = path;
        uri.query = query;
        Ctx::from_request(Request::new(Method::Get, uri))
    }

    #[test]
    fn decodes_query_keys_and_values() {
        let c = ctx("/q?a%20b=c+d&x=%E2%9C%93&x=2").unwrap();
        assert_eq!(c.query_param("a b").as_deref(), Some("c d"));
        assert_eq!(c.query_all("x").collect::<Vec<_>>(), ["✓", "2"]);
        assert_eq!(c.raw_query(), Some("a%20b=c+d&x=%E2%9C%93&x=2"));
    }

    #[test]
    fn rejects_bad_targets() {
        assert!(ctx("/q?a=%ff").is_err());
        assert!(ctx("/q?a=%zz").is_err());
        assert!(ctx("//double").is_err());
        assert_eq!(ctx("/a/./b/../c").unwrap().path(), "/a/c");
    }

    #[test]
    fn extensions_round_trip() {
        #[derive(Debug, PartialEq)]
        struct Id(u32);
        let c = ctx("/").unwrap();
        assert!(c.get::<Id>().is_none());
        c.set(Id(1));
        let handle = c.extensions();
        handle.insert(Id(2));
        assert_eq!(c.get::<Id>().as_deref(), Some(&Id(2)));
        assert_eq!(c.remove::<Id>().as_deref(), Some(&Id(2)));
        assert!(!handle.contains::<Id>());
    }

    #[test]
    fn pending_headers_do_not_override() {
        let c = ctx("/").unwrap();
        c.set_response_header("x-a", "queued");
        c.set_response_header("x-b", "queued");
        c.append_response_header("vary", "Origin");
        let resp = HttpResponse::ok()
            .header("x-a", "handler")
            .header("vary", "Accept");
        let resp = apply_pending(&c.pending(), resp);
        assert_eq!(resp.headers.get("x-a"), Some("handler"));
        assert_eq!(resp.headers.get("x-b"), Some("queued"));
        assert_eq!(resp.headers.get_all("vary").count(), 2);
    }
}
