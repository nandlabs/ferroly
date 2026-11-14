//! Span builders that follow the OpenTelemetry semantic conventions, for the
//! layers that instrument HTTP, messaging and model calls. They depend on no
//! other module: everything is passed in as strings and numbers.

/// HTTP spans (OpenTelemetry HTTP semantic conventions).
pub mod http {
    use ferroly::obs::{ActiveSpan, Span, SpanKind, Status};

    /// The standard methods; anything else is reported as `_OTHER`.
    const KNOWN_METHODS: [&str; 9] = [
        "GET", "HEAD", "POST", "PUT", "DELETE", "CONNECT", "OPTIONS", "TRACE", "PATCH",
    ];

    fn method_name(method: &str) -> &str {
        if KNOWN_METHODS.contains(&method) {
            method
        } else {
            "_OTHER"
        }
    }

    /// A [`SpanKind::Server`] span for an inbound request.
    ///
    /// `route` is the **route template** (`/users/:id`), never the raw path —
    /// span names must be low-cardinality. Pass an empty route when it is not
    /// known yet; the span is then named after the method alone, and
    /// [`set_route`] fills it in after routing.
    ///
    /// Name: `"{method} {route}"`. Fields: `http.request.method`,
    /// `http.route`.
    ///
    /// ```
    /// use ferroly::obs::{self, http};
    ///
    /// # fn header(_name: &str) -> Option<&'static str> { None }
    /// let parent = obs::extract(header);
    /// let span = http::server_span("GET", "/users/:id")
    ///     .child_of_opt(parent.as_ref())
    ///     .start();
    /// http::record_server_status(&span, 200);
    /// ```
    pub fn server_span(method: &str, route: &str) -> Span {
        let method = method_name(method);
        let span = if route.is_empty() {
            Span::new(method)
        } else {
            Span::new(format!("{method} {route}")).field("http.route", route)
        };
        span.kind(SpanKind::Server)
            .field("http.request.method", method)
    }

    /// Names a server span after its route once routing has happened:
    /// sets the span name to `"{method} {route}"` and records `http.route`.
    pub fn set_route(span: &ActiveSpan, method: &str, route: &str) {
        span.set_name(format!("{} {route}", method_name(method)));
        span.record("http.route", route);
    }

    /// Records `http.response.status_code` on a server span. A `5xx` response
    /// marks the span as an error; `4xx` does not (the server behaved
    /// correctly).
    pub fn record_server_status(span: &ActiveSpan, status: u16) {
        span.record("http.response.status_code", status);
        if status >= 500 {
            span.set_status(Status::error(format!("HTTP {status}")));
        }
    }

    /// A [`SpanKind::Client`] span for an outbound request.
    ///
    /// Name: `"{method}"` (client spans carry no route). Fields:
    /// `http.request.method`, `server.address`, `server.port`, and `url.full`
    /// with any `user:password@` credentials, the query string and the
    /// fragment removed.
    pub fn client_span(method: &str, url: &str) -> Span {
        let method = method_name(method);
        let (clean, host, port) = split_url(url);
        let mut span = Span::new(method)
            .kind(SpanKind::Client)
            .field("http.request.method", method)
            .field("url.full", clean);
        if !host.is_empty() {
            span = span.field("server.address", host);
        }
        if let Some(port) = port {
            span = span.field("server.port", port);
        }
        span
    }

    /// Records `http.response.status_code` on a client span. Any `4xx` or `5xx`
    /// response marks the span as an error.
    pub fn record_client_status(span: &ActiveSpan, status: u16) {
        span.record("http.response.status_code", status);
        if status >= 400 {
            span.set_status(Status::error(format!("HTTP {status}")));
        }
    }

    /// Returns `(url without credentials, query and fragment; host; port)`.
    fn split_url(url: &str) -> (String, String, Option<u16>) {
        let end = url.find(['?', '#']).unwrap_or(url.len());
        let url = &url[..end];
        let (scheme, rest) = match url.split_once("://") {
            Some((scheme, rest)) => (Some(scheme), rest),
            None => (None, url),
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => rest.split_at(i),
            None => (rest, ""),
        };
        // Drop `user:password@`.
        let host_port = authority.rsplit('@').next().unwrap_or(authority);
        let (host, port) = match host_port.rsplit_once(':') {
            // An IPv6 literal such as `[::1]` contains colons of its own.
            Some((host, port)) if !port.contains(']') => (host, port.parse::<u16>().ok()),
            _ => (host_port, None),
        };
        let clean = match scheme {
            Some(scheme) => format!("{scheme}://{host_port}{path}"),
            None => format!("{host_port}{path}"),
        };
        (clean, host.to_string(), port)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn urls_lose_credentials_and_query() {
            assert_eq!(
                split_url("https://user:pw@api.example.com:8443/v1/items?token=abc#frag"),
                (
                    "https://api.example.com:8443/v1/items".to_string(),
                    "api.example.com".to_string(),
                    Some(8443)
                )
            );
            assert_eq!(
                split_url("http://localhost/x"),
                (
                    "http://localhost/x".to_string(),
                    "localhost".to_string(),
                    None
                )
            );
            assert_eq!(
                split_url("http://[::1]:4318/v1/traces"),
                (
                    "http://[::1]:4318/v1/traces".to_string(),
                    "[::1]".to_string(),
                    Some(4318)
                )
            );
            assert_eq!(
                split_url("http://[::1]/x"),
                ("http://[::1]/x".to_string(), "[::1]".to_string(), None)
            );
            assert_eq!(method_name("PURGE"), "_OTHER");
            assert_eq!(method_name("GET"), "GET");
        }
    }
}

/// Messaging spans (OpenTelemetry messaging semantic conventions).
pub mod messaging {
    use ferroly::obs::{Span, SpanKind};

    /// A [`SpanKind::Producer`] span for sending a message to `destination`
    /// (a queue, topic or address) on `system` (`"kafka"`, `"local"`, …).
    ///
    /// Name: `"send {destination}"`. Fields: `messaging.system`,
    /// `messaging.destination.name`, `messaging.operation.type` = `send`.
    /// [Inject](ferroly::obs::inject) the started span's context into the
    /// message headers so the consumer can continue the trace.
    pub fn publish_span(system: &str, destination: &str) -> Span {
        Span::new(format!("send {destination}"))
            .kind(SpanKind::Producer)
            .field("messaging.system", system)
            .field("messaging.destination.name", destination)
            .field("messaging.operation.type", "send")
    }

    /// A [`SpanKind::Consumer`] span for handling a message received from
    /// `destination`. Give it the context [extracted](ferroly::obs::extract) from
    /// the message headers as its parent (or as a link, for batches).
    ///
    /// Name: `"process {destination}"`. Fields: `messaging.system`,
    /// `messaging.destination.name`, `messaging.operation.type` = `process`.
    pub fn process_span(system: &str, destination: &str) -> Span {
        Span::new(format!("process {destination}"))
            .kind(SpanKind::Consumer)
            .field("messaging.system", system)
            .field("messaging.destination.name", destination)
            .field("messaging.operation.type", "process")
    }
}

/// Generative-AI spans (OpenTelemetry GenAI semantic conventions).
pub mod genai {
    use ferroly::obs::{ActiveSpan, Span, SpanKind};

    /// A [`SpanKind::Client`] span for a chat-completion call to `model` on
    /// `provider` (`"openai"`, `"anthropic"`, `"ollama"`, …).
    ///
    /// Name: `"chat {model}"`. Fields: `gen_ai.operation.name` = `chat`,
    /// `gen_ai.provider.name`, `gen_ai.request.model`. Prompt and completion
    /// text are deliberately not recorded.
    pub fn chat_span(provider: &str, model: &str) -> Span {
        Span::new(format!("chat {model}"))
            .kind(SpanKind::Client)
            .field("gen_ai.operation.name", "chat")
            .field("gen_ai.provider.name", provider)
            .field("gen_ai.request.model", model)
    }

    /// Records token usage on a model-call span: `gen_ai.usage.input_tokens`
    /// and `gen_ai.usage.output_tokens`.
    pub fn record_usage(span: &ActiveSpan, input_tokens: u64, output_tokens: u64) {
        span.record("gen_ai.usage.input_tokens", input_tokens);
        span.record("gen_ai.usage.output_tokens", output_tokens);
    }
}
