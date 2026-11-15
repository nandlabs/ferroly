//! [`GrpcGenerator`]: gRPC servers and clients for the services of `.proto`
//! files, generated alongside their messages by
//! [`proto::codegen`](crate::proto::codegen).
//!
//! ```no_run
//! // build.rs, with `ferroly = { version = "0.6", features = ["grpc"] }`
//! // under [build-dependencies].
//! use ferroly::grpc::codegen::GrpcGenerator;
//!
//! let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("protos.rs");
//! ferroly::proto::codegen::Config::new()
//!     .service_generator(GrpcGenerator::new())
//!     .compile_to_file(&["proto"], &["echo/v1/echo.proto"], &out)
//!     .unwrap_or_else(|e| panic!("{e}"));
//! ```
//!
//! For `service Echo` the service's package module gains two submodules, so
//! the generated names never clash with the package's messages (a message
//! may be called `EchoClient`):
//!
//! - `mod echo_server`:
//!   - `trait Echo` — one method per `rpc`, taking a
//!     [`Request`](super::Request) and returning a
//!     [`BoxFuture`](super::BoxFuture) of a [`Response`](super::Response)
//!     (with [`Streaming`](super::Streaming) for the streamed sides) or a
//!     [`Status`](super::Status). It is object-safe.
//!   - `struct EchoServer<T: Echo + ?Sized>` — a
//!     [`GrpcService`](super::GrpcService) adapter:
//!     `GrpcServer::new().add_service(EchoServer::new(my_impl))`, or
//!     `EchoServer::from_arc(shared)` with an `Arc<impl Echo>` or an
//!     `Arc<dyn Echo>`.
//! - `mod echo_client`:
//!   - `struct EchoClient` — a typed client over a
//!     [`Channel`](super::Channel), one `async fn` per `rpc` accepting the
//!     message or a `Request` of it. An `rpc` whose snake-case name is one
//!     of the client's own methods (`new`, `channel`, `clone`, `clone_from`, `fmt`,
//!     `into`, `try_into`) gets an `_rpc` suffix (`new_rpc`), and further
//!     `_`s when another `rpc` already has that name (`new_rpc_` next to
//!     `rpc NewRpc`).
//!
//! A message or nested-type module named `echo_server` / `echo_client` in
//! the same package is reported as an [`IdlError`](crate::proto::idl::IdlError).

use std::fmt::Write as _;

use crate::proto::codegen::{doc_comment, MethodContext, ServiceContext, ServiceGenerator};
use crate::proto::MethodKind;

const G: &str = "::ferroly::grpc";

/// Clippy's naming lints, allowed on the generated trait and client: their
/// method names come from the `.proto` (an rpc may be called `New` or
/// `IntoFoo`), so a consumer's `clippy -D warnings` must not reject them.
const LINT_ALLOWS: &str = "#[allow(clippy::new_ret_no_self, clippy::wrong_self_convention, clippy::should_implement_trait)]\n";

/// Generates a server trait, a server adapter and a client per service; see
/// the [module docs](self).
#[derive(Debug, Clone)]
pub struct GrpcGenerator {
    server: bool,
    client: bool,
}

impl Default for GrpcGenerator {
    /// The same as [`GrpcGenerator::new`]: both sides.
    fn default() -> Self {
        Self::new()
    }
}

impl GrpcGenerator {
    /// Generates both the server side and the client.
    pub fn new() -> Self {
        GrpcGenerator {
            server: true,
            client: true,
        }
    }

    /// Whether to generate the server trait and adapter (default on).
    #[must_use]
    pub fn server(mut self, on: bool) -> Self {
        self.server = on;
        self
    }

    /// Whether to generate the client (default on).
    #[must_use]
    pub fn client(mut self, on: bool) -> Self {
        self.client = on;
        self
    }
}

/// A type path relative to the package module, as seen from one of the
/// service's submodules (`echo_server`, `echo_client`): one `super::` more,
/// unless the path is absolute or not a path at all (`()`).
fn from_submodule(path: &str) -> String {
    if path.starts_with("::") || path.starts_with("crate::") || path.starts_with('(') {
        path.to_owned()
    } else {
        format!("super::{path}")
    }
}

fn request_type(m: &MethodContext<'_>) -> String {
    let ty = from_submodule(&m.input_type);
    if m.client_streaming {
        format!("{G}::Streaming<{ty}>")
    } else {
        ty
    }
}

fn response_type(m: &MethodContext<'_>) -> String {
    let ty = from_submodule(&m.output_type);
    if m.server_streaming {
        format!("{G}::Streaming<{ty}>")
    } else {
        ty
    }
}

fn registrar(kind: MethodKind) -> &'static str {
    match kind {
        MethodKind::Unary => "unary",
        MethodKind::ServerStreaming => "server_streaming",
        MethodKind::ClientStreaming => "client_streaming",
        MethodKind::BidiStreaming => "bidi_streaming",
    }
}

fn shape(kind: MethodKind) -> &'static str {
    match kind {
        MethodKind::Unary => "Unary",
        MethodKind::ServerStreaming => "Server-streaming",
        MethodKind::ClientStreaming => "Client-streaming",
        MethodKind::BidiStreaming => "Bidirectional-streaming",
    }
}

fn method_doc(out: &mut String, m: &MethodContext<'_>) {
    let _ = writeln!(out, "/// {} `{}` (`{}`).", shape(m.kind), m.name, m.path);
    let doc = doc_comment(m.doc);
    if !doc.is_empty() {
        out.push_str("///\n");
        out.push_str(&doc);
    }
}

/// The client's own methods: its inherent `new` / `channel`, and the methods
/// of `Clone`, `Debug`, `Into` and `TryInto` an `rpc` would shadow or be
/// hidden by.
const CLIENT_OWN: &[&str] = &[
    "new",
    "channel",
    "clone",
    "clone_from",
    "fmt",
    "into",
    "try_into",
];

/// The client method names of a service's `rpc`s, in order: each `rpc`'s
/// snake-case name, unless it clashes with one of the client's own methods
/// (then with an `_rpc` suffix). A suffixed name that another `rpc` already
/// has (`rpc New` next to `rpc NewRpc`) gets further `_`s until it is free.
fn client_method_names(s: &ServiceContext<'_>) -> Vec<String> {
    let mut taken: std::collections::HashSet<String> =
        s.methods.iter().map(|m| m.rust_name.clone()).collect();
    s.methods
        .iter()
        .map(|m| {
            if !CLIENT_OWN.contains(&m.rust_name.as_str()) {
                return m.rust_name.clone();
            }
            let mut name = format!("{}_rpc", m.rust_name);
            while taken.contains(&name) || CLIENT_OWN.contains(&name.as_str()) {
                name.push('_');
            }
            taken.insert(name.clone());
            name
        })
        .collect()
}

/// The stem of a service's submodules: its snake-case name, without a
/// raw-identifier prefix (`type_server` is not a keyword).
fn stem(s: &ServiceContext<'_>) -> String {
    s.snake_name.trim_start_matches("r#").to_owned()
}

impl GrpcGenerator {
    fn server_side(&self, s: &ServiceContext<'_>, out: &mut String) {
        let name = &s.rust_name;
        let _ = writeln!(
            out,
            "/// The server side of the `{}` service: the `{name}` trait to implement and the `{name}Server` adapter that serves it.",
            s.full_name
        );
        let _ = writeln!(out, "pub mod {}_server {{", stem(s));
        let _ = writeln!(out, "/// The handlers of the `{}` service.", s.full_name);
        let doc = doc_comment(s.doc);
        if !doc.is_empty() {
            out.push_str("///\n");
            out.push_str(&doc);
        }
        let _ = writeln!(out, "///");
        let _ = writeln!(
            out,
            "/// Implement it and register it with [`{name}Server`]; the trait is object-safe, so a `{name}Server<dyn {name}>` works too."
        );
        out.push_str(LINT_ALLOWS);
        let _ = writeln!(
            out,
            "pub trait {name}: ::core::marker::Send + ::core::marker::Sync + 'static {{"
        );
        for m in &s.methods {
            method_doc(out, m);
            let _ = writeln!(
                out,
                "fn {}(&self, request: {G}::Request<{}>) -> {G}::BoxFuture<'_, ::core::result::Result<{G}::Response<{}>, {G}::Status>>;",
                m.rust_name,
                request_type(m),
                response_type(m)
            );
        }
        out.push_str("}\n\n");

        let _ = writeln!(
            out,
            "/// Serves an implementation of [`{name}`]: add it to a server with `GrpcServer::add_service`."
        );
        let _ = writeln!(out, "pub struct {name}Server<T: ?::core::marker::Sized> {{");
        let _ = writeln!(out, "inner: ::std::sync::Arc<T>,");
        out.push_str("}\n\n");
        let _ = writeln!(out, "impl<T: {name}> {name}Server<T> {{");
        let _ = writeln!(out, "/// Wraps an implementation.");
        let _ = writeln!(out, "pub fn new(inner: T) -> Self {{");
        let _ = writeln!(out, "Self::from_arc(::std::sync::Arc::new(inner))");
        out.push_str("}\n}\n\n");
        let _ = writeln!(
            out,
            "impl<T: {name} + ?::core::marker::Sized> {name}Server<T> {{"
        );
        let _ = writeln!(
            out,
            "/// Wraps a shared implementation (an `Arc<dyn {name}>` too)."
        );
        let _ = writeln!(
            out,
            "pub fn from_arc(inner: ::std::sync::Arc<T>) -> Self {{"
        );
        let _ = writeln!(out, "Self {{ inner }}");
        out.push_str("}\n}\n\n");
        let _ = writeln!(
            out,
            "impl<T: ?::core::marker::Sized> ::core::fmt::Debug for {name}Server<T> {{"
        );
        let _ = writeln!(
            out,
            "fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {{"
        );
        let _ = writeln!(out, "f.write_str(\"{name}Server\")");
        out.push_str("}\n}\n\n");
        let _ = writeln!(
            out,
            "impl<T: {name} + ?::core::marker::Sized> {G}::GrpcService for {name}Server<T> {{"
        );
        let _ = writeln!(
            out,
            "fn register(self, server: {G}::GrpcServer) -> {G}::GrpcServer {{"
        );
        if s.methods.is_empty() {
            let _ = writeln!(out, "::core::mem::drop(self.inner);");
        }
        for m in &s.methods {
            let _ = writeln!(out, "let svc = ::std::sync::Arc::clone(&self.inner);");
            let _ = writeln!(
                out,
                "let server = server.{}({:?}, move |request: {G}::Request<{}>| {{",
                registrar(m.kind),
                m.path,
                request_type(m)
            );
            let _ = writeln!(out, "let svc = ::std::sync::Arc::clone(&svc);");
            // Fully qualified, so an rpc named like a method of `Arc` or of
            // a trait in scope (`clone`, `as_ref`, `deref`) reaches the
            // service trait.
            let _ = writeln!(
                out,
                "async move {{ <T as {name}>::{}(&*svc, request).await }}",
                m.rust_name
            );
            out.push_str("});\n");
        }
        let _ = writeln!(out, "server");
        out.push_str("}\n}\n}\n");
    }

    fn client_side(&self, s: &ServiceContext<'_>, out: &mut String) {
        let name = &s.rust_name;
        let _ = writeln!(
            out,
            "/// The client side of the `{}` service: the typed `{name}Client`.",
            s.full_name
        );
        let _ = writeln!(out, "pub mod {}_client {{", stem(s));
        let _ = writeln!(out, "/// A client for the `{}` service.", s.full_name);
        let doc = doc_comment(s.doc);
        if !doc.is_empty() {
            out.push_str("///\n");
            out.push_str(&doc);
        }
        let _ = writeln!(out, "#[derive(Debug, Clone)]");
        let _ = writeln!(out, "pub struct {name}Client {{");
        let _ = writeln!(out, "channel: {G}::Channel,");
        out.push_str("}\n\n");
        out.push_str(LINT_ALLOWS);
        let _ = writeln!(out, "impl {name}Client {{");
        let _ = writeln!(out, "/// A client making its calls over `channel`.");
        let _ = writeln!(out, "pub fn new(channel: {G}::Channel) -> Self {{");
        let _ = writeln!(out, "Self {{ channel }}");
        out.push_str("}\n\n");
        let _ = writeln!(out, "/// The channel the calls go over.");
        let _ = writeln!(out, "pub fn channel(&self) -> &{G}::Channel {{");
        let _ = writeln!(out, "&self.channel");
        out.push('}');
        out.push('\n');
        for (m, method_name) in s.methods.iter().zip(client_method_names(s)) {
            out.push('\n');
            method_doc(out, m);
            let _ = writeln!(
                out,
                "pub async fn {}(&self, request: impl {G}::IntoRequest<{}>) -> ::core::result::Result<{G}::Response<{}>, {G}::Status> {{",
                method_name,
                request_type(m),
                response_type(m)
            );
            let _ = writeln!(
                out,
                "self.channel.{}({:?}, {G}::IntoRequest::into_request(request)).await",
                registrar(m.kind),
                m.path
            );
            out.push_str("}\n");
        }
        out.push_str("}\n}\n");
    }
}

impl ServiceGenerator for GrpcGenerator {
    fn generate(&self, service: &ServiceContext<'_>, out: &mut String) {
        let mut code = String::new();
        if self.server {
            self.server_side(service, &mut code);
        }
        if self.client {
            if self.server {
                code.push('\n');
            }
            self.client_side(service, &mut code);
        }
        out.push_str(&indent(&code));
    }

    /// The service's submodules, `<service>_server` and `<service>_client`
    /// (each only when that side is generated).
    fn reserved_names(&self, service: &ServiceContext<'_>) -> Vec<String> {
        let stem = stem(service);
        let mut names = Vec::new();
        if self.server {
            names.push(format!("{stem}_server"));
        }
        if self.client {
            names.push(format!("{stem}_client"));
        }
        names
    }
}

/// Indents generated items by brace depth (four spaces per level). Doc
/// comments do not count, since `.proto` comments may contain braces; the
/// generated code lines themselves have balanced braces outside strings.
fn indent(code: &str) -> String {
    let mut out = String::with_capacity(code.len() * 2);
    let mut depth = 0usize;
    for line in code.lines() {
        let line = line.trim();
        if line.is_empty() {
            out.push('\n');
            continue;
        }
        let is_doc = line.starts_with("///");
        let leading_close = if is_doc {
            0
        } else {
            line.chars().take_while(|&c| c == '}').count()
        };
        let level = depth.saturating_sub(leading_close);
        for _ in 0..level {
            out.push_str("    ");
        }
        out.push_str(line);
        out.push('\n');
        if !is_doc {
            let opens = line.matches('{').count();
            let closes = line.matches('}').count();
            depth = (depth + opens).saturating_sub(closes);
        }
    }
    out
}
