//! Plain-data descriptors of `.proto` services, emitted by
//! [`codegen`](super::codegen) for every `service` block.
//!
//! A descriptor carries everything an RPC transport needs to route a call —
//! the HTTP/2 path `/package.Service/Method`, the request and response message
//! names, and whether each side streams — without tying the generated code to
//! one transport. Code generated for a service looks like:
//!
//! ```
//! use ferroly::proto::{MethodDescriptor, MethodKind, ServiceDescriptor};
//!
//! pub const GREETER_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor::new(
//!     "helloworld",
//!     "Greeter",
//!     "helloworld.Greeter",
//!     &[
//!         MethodDescriptor::new(
//!             "SayHello",
//!             "/helloworld.Greeter/SayHello",
//!             "helloworld.HelloRequest",
//!             "helloworld.HelloReply",
//!             false,
//!             false,
//!         ),
//!         MethodDescriptor::new(
//!             "Chat",
//!             "/helloworld.Greeter/Chat",
//!             "helloworld.HelloRequest",
//!             "helloworld.HelloReply",
//!             true,
//!             true,
//!         ),
//!     ],
//! );
//!
//! let chat = GREETER_DESCRIPTOR.method("Chat").unwrap();
//! assert_eq!(chat.kind(), MethodKind::BidiStreaming);
//! assert_eq!(
//!     GREETER_DESCRIPTOR.method_by_path("/helloworld.Greeter/SayHello").unwrap().kind(),
//!     MethodKind::Unary
//! );
//! ```

/// The four RPC shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MethodKind {
    /// One request, one response.
    Unary,
    /// One request, a stream of responses (`returns (stream T)`).
    ServerStreaming,
    /// A stream of requests (`rpc M(stream T)`), one response.
    ClientStreaming,
    /// Streams in both directions.
    BidiStreaming,
}

impl MethodKind {
    /// The shape for the two `stream` flags.
    pub const fn from_streaming(client_streaming: bool, server_streaming: bool) -> Self {
        match (client_streaming, server_streaming) {
            (false, false) => MethodKind::Unary,
            (false, true) => MethodKind::ServerStreaming,
            (true, false) => MethodKind::ClientStreaming,
            (true, true) => MethodKind::BidiStreaming,
        }
    }
}

/// One `rpc` of a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct MethodDescriptor {
    /// The method name as declared (`SayHello`).
    pub name: &'static str,
    /// The request path: `/package.Service/Method` (`/Service/Method` without
    /// a package).
    pub path: &'static str,
    /// The fully qualified request message name, without a leading dot
    /// (`helloworld.HelloRequest`).
    pub input_type: &'static str,
    /// The fully qualified response message name, without a leading dot.
    pub output_type: &'static str,
    /// Whether the client sends a stream of requests.
    pub client_streaming: bool,
    /// Whether the server sends a stream of responses.
    pub server_streaming: bool,
}

impl MethodDescriptor {
    /// A descriptor (used by generated code).
    pub const fn new(
        name: &'static str,
        path: &'static str,
        input_type: &'static str,
        output_type: &'static str,
        client_streaming: bool,
        server_streaming: bool,
    ) -> Self {
        MethodDescriptor {
            name,
            path,
            input_type,
            output_type,
            client_streaming,
            server_streaming,
        }
    }

    /// The RPC shape.
    pub const fn kind(&self) -> MethodKind {
        MethodKind::from_streaming(self.client_streaming, self.server_streaming)
    }
}

/// One `service` of a `.proto` file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ServiceDescriptor {
    /// The package (`helloworld`; empty without a package).
    pub package: &'static str,
    /// The service name as declared (`Greeter`).
    pub name: &'static str,
    /// The fully qualified name (`helloworld.Greeter`), as used in request
    /// paths.
    pub full_name: &'static str,
    /// The methods, in declaration order.
    pub methods: &'static [MethodDescriptor],
}

impl ServiceDescriptor {
    /// A descriptor (used by generated code).
    pub const fn new(
        package: &'static str,
        name: &'static str,
        full_name: &'static str,
        methods: &'static [MethodDescriptor],
    ) -> Self {
        ServiceDescriptor {
            package,
            name,
            full_name,
            methods,
        }
    }

    /// The method named `name`.
    pub fn method(&self, name: &str) -> Option<&'static MethodDescriptor> {
        self.methods.iter().find(|m| m.name == name)
    }

    /// The method a request path (`/package.Service/Method`) addresses.
    pub fn method_by_path(&self, path: &str) -> Option<&'static MethodDescriptor> {
        self.methods.iter().find(|m| m.path == path)
    }
}
