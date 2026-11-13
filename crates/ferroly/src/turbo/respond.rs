//! Turning handler return values — including typed errors — into responses.

use ferroly::codec::{json, CodecError, Value};
use ferroly::http::{HttpResponse, StatusCode};

use super::error::ParamError;

/// The media type of an RFC 9457 problem document.
pub const PROBLEM_CONTENT_TYPE: &str = "application/problem+json";

/// Converts a handler's return value into an [`HttpResponse`].
///
/// Handlers may return anything that implements this trait. It is implemented
/// for [`HttpResponse`] itself and for `Result<T, E>` where both sides
/// implement it, so a handler can return `Result<HttpResponse, E>` and use `?`:
///
/// ```
/// use ferroly::http::{HttpResponse, StatusCode};
/// use ferroly::turbo::{Ctx, IntoResponse, Problem, Router};
///
/// enum OrderError { NotFound(u64), Closed }
///
/// impl IntoResponse for OrderError {
///     fn into_response(self) -> HttpResponse {
///         match self {
///             OrderError::NotFound(id) => Problem::new(StatusCode::NOT_FOUND)
///                 .detail(format!("order {id} does not exist"))
///                 .into_response(),
///             OrderError::Closed => Problem::new(StatusCode(409))
///                 .detail("the order is closed")
///                 .into_response(),
///         }
///     }
/// }
///
/// async fn show(ctx: Ctx) -> Result<HttpResponse, OrderError> {
///     let id: u64 = ctx.param_as("id").map_err(|_| OrderError::NotFound(0))?;
///     if id == 0 {
///         return Err(OrderError::Closed);
///     }
///     Ok(HttpResponse::text(StatusCode::OK, format!("order {id}")))
/// }
///
/// let _router = Router::new().get("/orders/:id", show);
/// ```
pub trait IntoResponse {
    /// Builds the response.
    fn into_response(self) -> HttpResponse;
}

impl IntoResponse for HttpResponse {
    fn into_response(self) -> HttpResponse {
        self
    }
}

impl<T: IntoResponse, E: IntoResponse> IntoResponse for Result<T, E> {
    fn into_response(self) -> HttpResponse {
        match self {
            Ok(value) => value.into_response(),
            Err(error) => error.into_response(),
        }
    }
}

impl IntoResponse for std::convert::Infallible {
    fn into_response(self) -> HttpResponse {
        match self {}
    }
}

/// An empty response with the given status.
impl IntoResponse for StatusCode {
    fn into_response(self) -> HttpResponse {
        HttpResponse::new(self)
    }
}

/// A `text/plain` response with the given status.
impl IntoResponse for (StatusCode, String) {
    fn into_response(self) -> HttpResponse {
        HttpResponse::text(self.0, self.1)
    }
}

/// A `text/plain` response with the given status.
impl IntoResponse for (StatusCode, &'static str) {
    fn into_response(self) -> HttpResponse {
        HttpResponse::text(self.0, self.1)
    }
}

/// `200 OK` with a `text/plain` body.
impl IntoResponse for String {
    fn into_response(self) -> HttpResponse {
        HttpResponse::text(StatusCode::OK, self)
    }
}

/// `200 OK` with a `text/plain` body.
impl IntoResponse for &'static str {
    fn into_response(self) -> HttpResponse {
        HttpResponse::text(StatusCode::OK, self)
    }
}

/// An RFC 9457 "problem details" error body (`application/problem+json`) — the
/// default rendering for errors in this module.
///
/// ```
/// use ferroly::http::StatusCode;
/// use ferroly::turbo::{IntoResponse, Problem};
///
/// let resp = Problem::new(StatusCode::NOT_FOUND)
///     .detail("no such order")
///     .with("order_id", 42)
///     .into_response();
/// assert_eq!(resp.status, StatusCode::NOT_FOUND);
/// assert_eq!(resp.headers.get("content-type"), Some("application/problem+json"));
/// assert_eq!(
///     String::from_utf8(resp.body).unwrap(),
///     r#"{"type":"about:blank","title":"Not Found","status":404,"detail":"no such order","order_id":42}"#
/// );
/// ```
#[derive(Debug, Clone, PartialEq)]
#[must_use]
pub struct Problem {
    // Boxed so that `Result<T, Problem>` stays small.
    inner: Box<ProblemInner>,
}

#[derive(Debug, Clone, PartialEq)]
struct ProblemInner {
    status: StatusCode,
    type_uri: Option<String>,
    title: Option<String>,
    detail: Option<String>,
    instance: Option<String>,
    extensions: Vec<(String, Value)>,
    headers: Vec<(String, String)>,
}

impl Problem {
    /// A problem with the given status. The `title` defaults to the status'
    /// reason phrase (and is omitted for a status that has none) and the
    /// `type` to `about:blank`.
    pub fn new(status: StatusCode) -> Self {
        Problem {
            inner: Box::new(ProblemInner {
                status,
                type_uri: None,
                title: None,
                detail: None,
                instance: None,
                extensions: Vec::new(),
                headers: Vec::new(),
            }),
        }
    }

    /// Sets the `type` member: a URI identifying the kind of problem.
    pub fn type_uri(mut self, uri: impl Into<String>) -> Self {
        self.inner.type_uri = Some(uri.into());
        self
    }

    /// Sets the `title` member: a short, stable summary of the problem kind.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.inner.title = Some(title.into());
        self
    }

    /// Sets the `detail` member: an explanation of this occurrence. It is
    /// sent to the client — do not put internal details in it.
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.inner.detail = Some(detail.into());
        self
    }

    /// Sets the `instance` member: a URI identifying this occurrence.
    pub fn instance(mut self, instance: impl Into<String>) -> Self {
        self.inner.instance = Some(instance.into());
        self
    }

    /// Adds an extension member. The reserved names (`type`, `title`,
    /// `status`, `detail`, `instance`) are ignored.
    pub fn with(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        let name = name.into();
        if !matches!(
            name.as_str(),
            "type" | "title" | "status" | "detail" | "instance"
        ) {
            self.inner.extensions.push((name, value.into()));
        }
        self
    }

    /// Adds a response header (for example `WWW-Authenticate` or
    /// `Retry-After`).
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.inner.headers.push((name.into(), value.into()));
        self
    }

    /// The status this problem is sent with.
    pub fn status(&self) -> StatusCode {
        self.inner.status
    }

    /// The JSON document.
    pub fn to_json(&self) -> String {
        let this = &*self.inner;
        let title = match &this.title {
            Some(title) => title.clone(),
            None => this.status.reason().to_string(),
        };
        let mut members: Vec<(String, Value)> = vec![(
            "type".into(),
            this.type_uri.as_deref().unwrap_or("about:blank").into(),
        )];
        // A status without a standard reason phrase has no default title.
        if !title.is_empty() {
            members.push(("title".into(), title.into()));
        }
        members.push(("status".into(), Value::Int(i64::from(this.status.as_u16()))));
        if let Some(detail) = &this.detail {
            members.push(("detail".into(), detail.as_str().into()));
        }
        if let Some(instance) = &this.instance {
            members.push(("instance".into(), instance.as_str().into()));
        }
        members.extend(this.extensions.iter().cloned());
        json::encode(&Value::Object(members))
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> HttpResponse {
        let mut resp = HttpResponse::new(self.inner.status)
            .header("content-type", PROBLEM_CONTENT_TYPE)
            .body(self.to_json().into_bytes());
        for (name, value) in self.inner.headers {
            resp.headers.append(name, value);
        }
        resp
    }
}

impl From<Problem> for HttpResponse {
    fn from(problem: Problem) -> HttpResponse {
        problem.into_response()
    }
}

impl From<&ParamError> for Problem {
    fn from(error: &ParamError) -> Problem {
        Problem::new(StatusCode::BAD_REQUEST).detail(error.to_string())
    }
}

impl From<ParamError> for Problem {
    fn from(error: ParamError) -> Problem {
        Problem::from(&error)
    }
}

/// `400 Bad Request` as a problem document naming the parameter.
impl IntoResponse for ParamError {
    fn into_response(self) -> HttpResponse {
        Problem::from(&self).into_response()
    }
}

impl From<&CodecError> for Problem {
    /// `415` for a content type no codec handles, otherwise `400`.
    fn from(error: &CodecError) -> Problem {
        let status = match error {
            CodecError::UnsupportedContentType(_) => StatusCode(415),
            _ => StatusCode::BAD_REQUEST,
        };
        Problem::new(status).detail(error.to_string())
    }
}

impl From<CodecError> for Problem {
    fn from(error: CodecError) -> Problem {
        Problem::from(&error)
    }
}

/// `415` for a content type no codec handles, otherwise `400`, as a problem
/// document.
impl IntoResponse for CodecError {
    fn into_response(self) -> HttpResponse {
        Problem::from(&self).into_response()
    }
}

/// Adapter that renders any [`Classify`](ferroly::errutils::Classify) error as
/// a problem document. Requires the `errutils` feature.
///
/// The status comes from
/// [`ErrorKind::http_status`](ferroly::errutils::ErrorKind::http_status), and
/// the document carries a `kind` member with the kind's name. The error's
/// `Display` text is sent as `detail` **only for caller errors** (4xx kinds);
/// for everything else the detail is omitted, so internal failure messages are
/// not disclosed to clients.
///
/// `Classified<E>` implements `From<E>`, so `?` converts automatically:
///
/// ```
/// use ferroly::errutils::{Classify, ErrorKind};
/// use ferroly::http::{HttpResponse, StatusCode};
/// use ferroly::turbo::{Classified, Ctx, Router};
///
/// #[derive(Debug)]
/// struct Missing;
/// impl std::fmt::Display for Missing {
///     fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
///         f.write_str("no such item")
///     }
/// }
/// impl Classify for Missing {
///     fn kind(&self) -> ErrorKind { ErrorKind::NotFound }
/// }
///
/// fn load(_id: &str) -> Result<String, Missing> { Err(Missing) }
///
/// async fn show(ctx: Ctx) -> Result<HttpResponse, Classified<Missing>> {
///     let item = load(ctx.param("id").unwrap_or(""))?; // Missing -> 404 problem
///     Ok(HttpResponse::text(StatusCode::OK, item))
/// }
///
/// let _router = Router::new().get("/items/:id", show);
/// ```
#[cfg(feature = "errutils")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified<E>(pub E);

#[cfg(feature = "errutils")]
impl<E> From<E> for Classified<E> {
    fn from(error: E) -> Self {
        Classified(error)
    }
}

#[cfg(feature = "errutils")]
impl<E: ferroly::errutils::Classify + std::fmt::Display> Classified<E> {
    /// The problem document this error renders as.
    pub fn to_problem(&self) -> Problem {
        let kind = self.0.kind();
        let problem = Problem::new(StatusCode(kind.http_status())).with("kind", kind.as_str());
        if kind.is_user_error() || kind == ferroly::errutils::ErrorKind::RateLimited {
            problem.detail(self.0.to_string())
        } else {
            problem
        }
    }
}

#[cfg(feature = "errutils")]
impl<E: ferroly::errutils::Classify + std::fmt::Display> IntoResponse for Classified<E> {
    fn into_response(self) -> HttpResponse {
        self.to_problem().into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn problem_renders_rfc9457_members() {
        let resp = Problem::new(StatusCode(422))
            .type_uri("https://example.test/problems/validation")
            .title("Validation failed")
            .detail("age must be positive")
            .instance("/orders/7")
            .with("status", 1) // reserved: ignored
            .header("retry-after", "3")
            .into_response();
        assert_eq!(resp.status.as_u16(), 422);
        assert_eq!(resp.headers.get("retry-after"), Some("3"));
        let body = String::from_utf8(resp.body).unwrap();
        assert_eq!(
            body,
            r#"{"type":"https://example.test/problems/validation","title":"Validation failed","status":422,"detail":"age must be positive","instance":"/orders/7"}"#
        );
    }

    #[test]
    fn title_defaults_to_the_reason_phrase_or_is_omitted() {
        assert_eq!(
            Problem::new(StatusCode(409)).to_json(),
            r#"{"type":"about:blank","title":"Conflict","status":409}"#
        );
        assert_eq!(
            Problem::new(StatusCode(499)).to_json(),
            r#"{"type":"about:blank","status":499}"#
        );
    }

    #[test]
    fn result_and_plain_values_convert() {
        let ok: Result<&'static str, Problem> = Ok("fine");
        assert_eq!(ok.into_response().status, StatusCode::OK);
        let err: Result<HttpResponse, Problem> = Err(Problem::new(StatusCode::FORBIDDEN));
        assert_eq!(err.into_response().status, StatusCode::FORBIDDEN);
        assert_eq!(
            (StatusCode::CREATED, "made").into_response().status,
            StatusCode::CREATED
        );
    }

    #[test]
    fn codec_error_statuses() {
        let unsupported = CodecError::UnsupportedContentType("text/csv".into());
        assert_eq!(unsupported.into_response().status.as_u16(), 415);
        assert_eq!(
            CodecError::parse("bad").into_response().status,
            StatusCode::BAD_REQUEST
        );
    }
}
