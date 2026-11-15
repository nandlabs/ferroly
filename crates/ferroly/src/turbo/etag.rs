//! Conditional `GET` for dynamic responses: an `ETag` computed over the body
//! and `If-None-Match` answered with `304 Not Modified`.

use std::sync::Arc;

use ferroly::http::{BoxFuture, HttpResponse, Method, StatusCode};

use super::ctx::Ctx;
use super::router::{Group, Next, Route, Router};

/// The `ETag` / `If-None-Match` middleware installed by [`Router::etag`],
/// [`Group::etag`] or [`Route::etag`].
///
/// For a `GET` or `HEAD` request answered `200 OK` with a fixed (not
/// streamed) body, it
///
/// 1. adds an `ETag` computed over the body bytes, unless the handler already
///    set one (a handler that knows a cheaper version tag keeps it, and still
///    gets the `304` handling below);
/// 2. answers `304 Not Modified` with no body when the request's
///    `If-None-Match` lists that tag (weak comparison, as RFC 9110 §13.1.2
///    requires) or is `*`. The `304` keeps the response's headers except the
///    content headers (`Content-Type`, `Content-Length`, `Content-Encoding`,
///    `Content-Range`), so `ETag`, `Cache-Control`, `Vary` and cookies still
///    reach the client.
///
/// Other methods, other statuses and streamed bodies pass through untouched.
/// A `HEAD` answer is tagged only when its body is the one `GET` would send
/// (the router's automatic `HEAD` runs the `GET` handler): an explicit `HEAD`
/// handler that returns an empty body with a non-zero `Content-Length` (or
/// none at all) gets no computed tag, as it would differ from `GET`'s; set
/// the `ETag` in that handler instead.
/// The handler still runs for every request: this saves bandwidth, not
/// work. A handler that can tell cheaply that nothing changed should compare
/// [`Ctx::header`]`("if-none-match")` itself and return early.
///
/// The tag is a 128-bit FNV-1a digest of the body plus its length — stable
/// across restarts and processes, so every replica behind a load balancer
/// produces the same tag for the same bytes. It is a change detector, not a
/// security token.
///
/// [`strong`](ETag::strong) tags (the default) promise byte-identical bodies
/// and are what `If-Range` needs; choose [`weak`](ETag::weak) when something
/// between this server and the client may re-encode the body (a compressing
/// proxy).
///
/// ```
/// use ferroly::http::{HttpResponse, StatusCode};
/// use ferroly::turbo::{ETag, Router};
///
/// let _router: Router = Router::new()
///     .etag(ETag::strong())
///     .get("/report", |_| async { HttpResponse::text(StatusCode::OK, "{}") });
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ETag {
    weak: bool,
}

impl Default for ETag {
    /// [`ETag::strong`].
    fn default() -> Self {
        Self::strong()
    }
}

impl ETag {
    /// Strong tags: `"…"`.
    pub fn strong() -> Self {
        Self { weak: false }
    }

    /// Weak tags: `W/"…"`.
    pub fn weak() -> Self {
        Self { weak: true }
    }

    /// The tag this configuration gives `body`, quoted and with the `W/`
    /// prefix for weak tags — what the middleware puts in the `ETag` header.
    ///
    /// ```
    /// use ferroly::turbo::ETag;
    ///
    /// let tag = ETag::weak().tag_for(b"hello");
    /// assert!(tag.starts_with("W/\"") && tag.ends_with('"'));
    /// assert_eq!(tag, ETag::weak().tag_for(b"hello"));
    /// assert_ne!(tag, ETag::weak().tag_for(b"hello!"));
    /// ```
    pub fn tag_for(&self, body: &[u8]) -> String {
        let digest = fnv1a_128(body);
        let prefix = if self.weak { "W/" } else { "" };
        format!("{prefix}\"{digest:032x}-{:x}\"", body.len())
    }

    /// Applies the middleware to one response.
    fn finish(
        &self,
        method: &Method,
        if_none_match: Option<&str>,
        resp: HttpResponse,
    ) -> HttpResponse {
        if !matches!(method, Method::Get | Method::Head)
            || resp.status != StatusCode::OK
            || resp.has_stream()
        {
            return resp;
        }
        if *method == Method::Head
            && !resp.headers.contains("etag")
            && !head_body_is_complete(&resp)
        {
            return resp;
        }
        let mut resp = resp;
        let tag = match resp.headers.get("etag") {
            Some(existing) => existing.to_string(),
            None => {
                let tag = self.tag_for(&resp.body);
                resp.headers.set("ETag", tag.clone());
                tag
            }
        };
        if !if_none_match.is_some_and(|candidates| none_match_lists(candidates, &tag)) {
            return resp;
        }
        let mut not_modified = HttpResponse::new(StatusCode::NOT_MODIFIED);
        for (name, value) in resp.headers.iter() {
            const CONTENT: [&str; 4] = [
                "content-type",
                "content-length",
                "content-encoding",
                "content-range",
            ];
            if !CONTENT.iter().any(|c| name.eq_ignore_ascii_case(c)) {
                not_modified.headers.append(name, value);
            }
        }
        not_modified
    }

    /// The middleware as a boxed layer.
    fn layer(self) -> impl Fn(Ctx, Next) -> BoxFuture<'static, HttpResponse> + Send + Sync {
        let config = Arc::new(self);
        move |ctx: Ctx, next: Next| {
            let config = config.clone();
            let method = ctx.method().clone();
            let candidates: Vec<String> = ctx
                .headers()
                .get_all("if-none-match")
                .map(str::to_string)
                .collect();
            Box::pin(async move {
                let resp = next.run(ctx).await;
                let joined = (!candidates.is_empty()).then(|| candidates.join(","));
                config.finish(&method, joined.as_deref(), resp)
            })
        }
    }
}

/// Whether a `HEAD` response carries the body a `GET` would: a body that
/// agrees with its `Content-Length` (when set), or an empty body declared as
/// `Content-Length: 0`.
fn head_body_is_complete(resp: &HttpResponse) -> bool {
    match resp.headers.get("content-length") {
        Some(declared) => declared.trim().parse::<usize>().ok() == Some(resp.body.len()),
        None => !resp.body.is_empty(),
    }
}

/// Whether an `If-None-Match` value is `*` or lists `tag`, comparing weakly
/// (the `W/` prefix is ignored on both sides).
///
/// Entity-tags are parsed as RFC 9110 §8.8.3 defines them — optional `W/`,
/// then a quoted string that may itself contain commas — so a list is not
/// simply split on `,`. An unquoted element (not valid, but sent by some
/// clients) runs to the next comma.
fn none_match_lists(candidates: &str, tag: &str) -> bool {
    let ours = opaque_tag(tag);
    let mut rest = candidates;
    loop {
        rest = rest.trim_start_matches([',', ' ', '\t']);
        if rest.is_empty() {
            return false;
        }
        let (candidate, tail) = next_entity_tag(rest);
        if candidate == "*" || (!candidate.is_empty() && opaque_tag(candidate) == ours) {
            return true;
        }
        rest = tail;
    }
}

/// Splits the first element off a non-empty `If-None-Match` list: the
/// element (with any `W/` prefix and its quotes) and what follows it.
fn next_entity_tag(list: &str) -> (&str, &str) {
    let quoted_from = if list.starts_with("W/\"") {
        Some(2)
    } else if list.starts_with('"') {
        Some(0)
    } else {
        None
    };
    if let Some(open) = quoted_from {
        if let Some(close) = list[open + 1..].find('"') {
            let end = open + 1 + close + 1;
            return (&list[..end], &list[end..]);
        }
    }
    // Unquoted (or unterminated): up to the next comma.
    let end = list.find(',').unwrap_or(list.len());
    (list[..end].trim_end(), &list[end..])
}

/// An entity-tag without its weakness prefix, for weak comparison.
fn opaque_tag(tag: &str) -> &str {
    let tag = tag.trim();
    tag.strip_prefix("W/").unwrap_or(tag)
}

/// 128-bit FNV-1a.
fn fnv1a_128(bytes: &[u8]) -> u128 {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    bytes.iter().fold(OFFSET, |hash, &b| {
        (hash ^ u128::from(b)).wrapping_mul(PRIME)
    })
}

impl Router {
    /// Adds an `ETag` to every successful `GET`/`HEAD` response with a fixed
    /// body and answers a matching `If-None-Match` with `304 Not Modified`;
    /// see [`ETag`].
    ///
    /// Register it after layers that rewrite the body, so the tag is computed
    /// over the bytes that are sent.
    pub fn etag(self, etag: ETag) -> Self {
        self.layer(etag.layer())
    }
}

impl Group {
    /// [`Router::etag`] for the routes of this group only.
    pub fn etag(self, etag: ETag) -> Self {
        self.layer(etag.layer())
    }
}

impl Route {
    /// [`Router::etag`] for this route only.
    pub fn etag(self, etag: ETag) -> Self {
        self.layer(etag.layer())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_128_matches_the_reference_vectors() {
        // From the FNV reference (draft-eastlake-fnv): "" and "a".
        assert_eq!(fnv1a_128(b""), 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d);
        assert_eq!(fnv1a_128(b"a"), 0xd228_cb69_6f1a_8caf_7891_2b70_4e4a_8964);
    }

    #[test]
    fn if_none_match_compares_weakly() {
        let tag = ETag::strong().tag_for(b"x");
        assert!(none_match_lists(&tag, &tag));
        assert!(none_match_lists(&format!("W/{tag}"), &tag));
        assert!(none_match_lists("\"other\", *", &tag));
        assert!(none_match_lists(&format!("\"other\" , {tag}"), &tag));
        assert!(!none_match_lists("\"other\"", &tag));
        assert!(!none_match_lists("", &tag));
        assert!(!none_match_lists(" , ,", &tag));
    }

    #[test]
    fn if_none_match_parses_quoted_tags_holding_commas() {
        let tag = "\"a,b\"";
        assert!(none_match_lists(tag, tag));
        assert!(none_match_lists("W/\"a,b\"", tag));
        assert!(none_match_lists("\"x\", \"a,b\"", tag));
        assert!(none_match_lists("\"x,y\",W/\"a,b\" ", tag));
        assert!(!none_match_lists("\"a\", \"b\"", tag));
        assert!(!none_match_lists("\"a,b,c\"", tag));
        // A comma inside a quoted tag is not a list separator.
        assert!(!none_match_lists("\"x,*\"", "\"y\""));
        // Lenient about unquoted elements.
        assert!(none_match_lists("v7, other", "v7"));
        assert!(none_match_lists("\"x\", *", tag));
    }

    #[test]
    fn head_is_tagged_only_when_its_body_matches_get() {
        let etag = ETag::strong();
        // Automatic HEAD: the GET handler's body.
        let auto = etag.finish(
            &Method::Head,
            None,
            HttpResponse::text(StatusCode::OK, "body"),
        );
        assert_eq!(
            auto.headers.get("etag"),
            Some(etag.tag_for(b"body").as_str())
        );
        // Explicit HEAD with an empty body and the GET length: untagged.
        let explicit = HttpResponse::new(StatusCode::OK).header("Content-Length", "4");
        let explicit = etag.finish(&Method::Head, Some("*"), explicit);
        assert_eq!(explicit.status, StatusCode::OK);
        assert!(!explicit.headers.contains("etag"));
        // An empty body with no length either.
        let bare = etag.finish(&Method::Head, None, HttpResponse::new(StatusCode::OK));
        assert!(!bare.headers.contains("etag"));
        // A genuinely empty resource.
        let empty = HttpResponse::new(StatusCode::OK).header("Content-Length", "0");
        let empty = etag.finish(&Method::Head, None, empty);
        assert_eq!(empty.headers.get("etag"), Some(etag.tag_for(b"").as_str()));
        // A handler-set tag is still honoured on an explicit HEAD.
        let own = HttpResponse::new(StatusCode::OK)
            .header("Content-Length", "4")
            .header("ETag", "\"v1\"");
        let own = etag.finish(&Method::Head, Some("\"v1\""), own);
        assert_eq!(own.status, StatusCode::NOT_MODIFIED);
        // GET is unaffected by the HEAD rule.
        let get = etag.finish(&Method::Get, None, HttpResponse::new(StatusCode::OK));
        assert!(get.headers.contains("etag"));
    }

    #[test]
    fn only_get_and_head_200_fixed_bodies_are_tagged() {
        let etag = ETag::strong();
        let ok = || HttpResponse::text(StatusCode::OK, "body");
        assert!(etag
            .finish(&Method::Get, None, ok())
            .headers
            .contains("etag"));
        assert!(etag
            .finish(&Method::Head, None, ok())
            .headers
            .contains("etag"));
        assert!(!etag
            .finish(&Method::Post, None, ok())
            .headers
            .contains("etag"));
        let created = HttpResponse::text(StatusCode::CREATED, "body");
        assert!(!etag
            .finish(&Method::Get, None, created)
            .headers
            .contains("etag"));
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let streamed = HttpResponse::stream(StatusCode::OK, rx);
        assert!(!etag
            .finish(&Method::Get, None, streamed)
            .headers
            .contains("etag"));
    }

    #[test]
    fn a_matching_if_none_match_gets_a_bodiless_304() {
        let etag = ETag::weak();
        let tag = etag.tag_for(b"body");
        let resp = etag.finish(
            &Method::Get,
            Some(&tag),
            HttpResponse::text(StatusCode::OK, "body").header("Cache-Control", "max-age=60"),
        );
        assert_eq!(resp.status, StatusCode::NOT_MODIFIED);
        assert!(resp.body.is_empty());
        assert_eq!(resp.headers.get("etag"), Some(tag.as_str()));
        assert_eq!(resp.headers.get("cache-control"), Some("max-age=60"));
        assert_eq!(resp.headers.get("content-type"), None);
    }

    #[test]
    fn a_handler_set_etag_is_kept_and_compared() {
        let resp = ETag::strong().finish(
            &Method::Get,
            Some("\"v7\""),
            HttpResponse::text(StatusCode::OK, "body").header("ETag", "\"v7\""),
        );
        assert_eq!(resp.status, StatusCode::NOT_MODIFIED);
        assert_eq!(resp.headers.get("etag"), Some("\"v7\""));
    }
}
