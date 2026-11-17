//! Rust source for the messages, enums and services of `.proto` files.
//!
//! The output is plain Rust text that uses the `ferroly::proto` derives —
//! no `protoc`, no runtime reflection. Call it from a `build.rs`, write the
//! text into `OUT_DIR`, and `include!` it:
//!
//! ```no_run
//! // The body of `fn main` in build.rs, with
//! // `ferroly = { version = "0.7", features = ["protobuf"] }` under
//! // [build-dependencies]. `compile_to_file` prints `cargo:rerun-if-changed`
//! // for every `.proto` file it reads.
//! let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("protos.rs");
//! ferroly::proto::codegen::Config::new()
//!     .compile_to_file(&["proto"], &["greet/v1/greet.proto"], &out)
//!     .unwrap_or_else(|e| panic!("{e}"));
//! ```
//!
//! ```ignore
//! // src/lib.rs
//! pub mod protos {
//!     include!(concat!(env!("OUT_DIR"), "/protos.rs"));
//! }
//! use protos::greet::v1::HelloRequest;
//! ```
//!
//! # Mapping
//!
//! | `.proto` | Rust |
//! |---|---|
//! | `package a.b;` | `pub mod a { pub mod b { … } }` |
//! | `message Foo` | `pub struct Foo` deriving `Message`, `Clone`, `PartialEq`, `Debug`, `Default`, with an `unknown_fields` member |
//! | nested `message Bar` in `Foo` | `foo::Bar` |
//! | `enum Kind` | `pub enum Kind` deriving `Enumeration` (`#[repr(i32)]`), values with the enum-name prefix stripped (`KIND_FAST` → `Fast`) |
//! | scalar field (proto3) | `T` |
//! | `optional` field | `Option<T>` |
//! | message field | `Option<M>` (`Option<Box<M>>` when the type contains itself) |
//! | enum field | `EnumValue<E>` (open: unknown numbers are kept) |
//! | `repeated T` | `Vec<T>` |
//! | `map<K, V>` | `BTreeMap<K, V>` (or `HashMap`, see [`MapType`]) |
//! | `oneof kind` | `kind: Option<foo::Kind>`, an enum deriving `Oneof` |
//! | proto2 `required` | `T` with `#[proto(required)]` |
//! | `service Greeter` | `GREETER_DESCRIPTOR: ServiceDescriptor`, plus whatever each [`ServiceGenerator`] adds |
//!
//! Types for every file in the [`FileSet`] are generated — imports
//! included — into one module tree, and references between packages are
//! written as relative `super::` paths, so the output works wherever it is
//! included. Comments above definitions become doc comments, and every module
//! is documented, so the output builds under `#![deny(missing_docs)]`. Struct
//! members follow field-number order, so encodings match protoc's. Two
//! definitions that would produce the same Rust name in a module are an
//! [`IdlError`].
//!
//! # Extending: service generators
//!
//! Each `service` gets its [`ServiceDescriptor`](super::ServiceDescriptor)
//! constant, and then every registered [`ServiceGenerator`] is called with a
//! [`ServiceContext`] — the service and its methods with request paths,
//! streaming flags and the Rust paths of the request and response types —
//! and appends items (client stubs, server traits, …) to the service's
//! package module:
//!
//! ```
//! use ferroly::proto::codegen::{Config, ServiceContext, ServiceGenerator};
//! use ferroly::proto::idl::FileSet;
//!
//! struct MethodList;
//!
//! impl ServiceGenerator for MethodList {
//!     fn generate(&self, service: &ServiceContext<'_>, out: &mut String) {
//!         out.push_str(&format!(
//!             "/// The methods of `{}`.\npub const {}_METHODS: &[&str] = &[{}];\n",
//!             service.full_name,
//!             service.const_prefix,
//!             service
//!                 .methods
//!                 .iter()
//!                 .map(|m| format!("{:?}", m.path))
//!                 .collect::<Vec<_>>()
//!                 .join(", "),
//!         ));
//!     }
//! }
//!
//! let set = FileSet::from_sources(&[("svc.proto", r#"
//!     syntax = "proto3";
//!     package demo;
//!     message Ping {}
//!     service Echo { rpc Call (Ping) returns (Ping); }
//! "#)]).unwrap();
//! let code = Config::new().service_generator(MethodList).generate(&set).unwrap();
//! assert!(code.contains(r#"pub const ECHO_METHODS: &[&str] = &["/demo.Echo/Call"];"#));
//! assert!(code.contains("pub const ECHO_DESCRIPTOR: ::ferroly::proto::ServiceDescriptor"));
//! ```

mod names;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fmt::Write as _;
use std::path::Path;

use super::idl::{
    EnumDef, FieldDef, FieldType, FileSet, FsLoader, IdlError, Label, Loader, MessageDef,
    MethodDef, ProtoFile, ScalarType, ServiceDef, Span, Syntax, TypeKind,
};
use super::service::MethodKind;

pub use names::{ident as rust_ident, pascal as pascal_case, snake as snake_case};

/// The Rust map type generated for `map<K, V>` fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MapType {
    /// `std::collections::BTreeMap` — deterministic encoding order (the
    /// default).
    #[default]
    BTreeMap,
    /// `std::collections::HashMap`.
    HashMap,
}

/// Adds Rust items for each `service` of the generated files.
///
/// Register implementations with [`Config::service_generator`]; they run in
/// registration order after the service's descriptor constant is written.
/// See the [module docs](self) for an example.
pub trait ServiceGenerator {
    /// Appends items for `service` to `out`, which becomes part of the body of
    /// the service's package module. Paths in `service` are relative to that
    /// module.
    ///
    /// Write items at column 0: the text is indented line by line to its
    /// module depth afterwards, so avoid multi-line string literals.
    fn generate(&self, service: &ServiceContext<'_>, out: &mut String);

    /// Rust item names (types, modules, consts) this generator adds to the
    /// service's package module, so codegen can report a clash as an
    /// [`IdlError`] instead of emitting code that does not compile. The
    /// default declares none.
    fn reserved_names(&self, service: &ServiceContext<'_>) -> Vec<String> {
        let _ = service;
        Vec::new()
    }
}

/// What a [`ServiceGenerator`] knows about one service.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ServiceContext<'a> {
    /// The file declaring the service.
    pub file: &'a ProtoFile,
    /// The parsed service (options, spans, raw method definitions).
    pub service: &'a ServiceDef,
    /// The package (empty without one).
    pub package: &'a str,
    /// The fully qualified service name (`greet.v1.Greeter`).
    pub full_name: &'a str,
    /// The service name in `PascalCase` (`Greeter`), for naming types such as
    /// `GreeterClient`.
    pub rust_name: String,
    /// The service name in `snake_case` (`greeter`), for naming modules.
    pub snake_name: String,
    /// The service name in `SCREAMING_SNAKE_CASE` (`GREETER`), for naming
    /// constants.
    pub const_prefix: String,
    /// The name of the generated descriptor constant (`GREETER_DESCRIPTOR`),
    /// or `None` when [`Config::service_descriptors`] is off.
    pub descriptor: Option<String>,
    /// The comment above the service, as plain text.
    pub doc: &'a str,
    /// The methods, in declaration order.
    pub methods: Vec<MethodContext<'a>>,
}

/// What a [`ServiceGenerator`] knows about one method.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct MethodContext<'a> {
    /// The parsed method.
    pub method: &'a MethodDef,
    /// The method name as declared (`SayHello`).
    pub name: &'a str,
    /// The method name in `snake_case`, safe as a Rust identifier
    /// (`say_hello`; `r#type` for a method named `type`).
    pub rust_name: String,
    /// The request path: `/greet.v1.Greeter/SayHello`.
    pub path: String,
    /// The fully qualified request message (`greet.v1.HelloRequest`).
    pub input_proto: &'a str,
    /// The fully qualified response message.
    pub output_proto: &'a str,
    /// The Rust path of the request type, relative to the service's package
    /// module (`HelloRequest`, `super::common::Reply`).
    pub input_type: String,
    /// The Rust path of the response type, relative to the package module.
    pub output_type: String,
    /// Whether the client streams requests.
    pub client_streaming: bool,
    /// Whether the server streams responses.
    pub server_streaming: bool,
    /// The RPC shape.
    pub kind: MethodKind,
    /// The comment above the method, as plain text.
    pub doc: &'a str,
}

/// Code-generation options.
pub struct Config {
    map_type: MapType,
    descriptors: bool,
    generators: Vec<Box<dyn ServiceGenerator>>,
    type_attributes: Vec<(String, String)>,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("map_type", &self.map_type)
            .field("descriptors", &self.descriptors)
            .field("generators", &self.generators.len())
            .field("type_attributes", &self.type_attributes)
            .finish()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

impl Config {
    /// The defaults: `BTreeMap` maps, service descriptors on, no service
    /// generators.
    pub fn new() -> Self {
        Config {
            map_type: MapType::BTreeMap,
            descriptors: true,
            generators: Vec::new(),
            type_attributes: Vec::new(),
        }
    }

    /// The Rust type for `map<K, V>` fields.
    #[must_use]
    pub fn map_type(mut self, map_type: MapType) -> Self {
        self.map_type = map_type;
        self
    }

    /// Whether to emit a `ServiceDescriptor` constant per service (default
    /// on).
    #[must_use]
    pub fn service_descriptors(mut self, on: bool) -> Self {
        self.descriptors = on;
        self
    }

    /// Adds a generator run for every service.
    #[must_use]
    pub fn service_generator(mut self, generator: impl ServiceGenerator + 'static) -> Self {
        self.generators.push(Box::new(generator));
        self
    }

    /// Adds an attribute (such as `#[derive(Eq, Hash)]`) to every message,
    /// enum and oneof enum whose fully qualified name is `path` or lies under
    /// it. `path` starts with a dot: `.` matches everything, `.pkg` a package,
    /// `.pkg.Msg` one type and its nested types and oneofs, `.pkg.Msg.kind`
    /// the enum of oneof `kind`.
    #[must_use]
    pub fn type_attribute(mut self, path: impl Into<String>, attribute: impl Into<String>) -> Self {
        self.type_attributes.push((path.into(), attribute.into()));
        self
    }

    /// Generates Rust source for every file in `set`.
    pub fn generate(&self, set: &FileSet) -> Result<String, IdlError> {
        Generator::new(self, set).run()
    }

    /// Loads `roots` (and their imports) through `loader` and generates their
    /// Rust source.
    pub fn compile<L: Loader + ?Sized>(
        &self,
        loader: &L,
        roots: &[&str],
    ) -> Result<String, IdlError> {
        self.generate(&FileSet::load(loader, roots)?)
    }

    /// For `build.rs`: loads `roots` from the include directories (as
    /// `protoc -I` would), generates their Rust source and writes it to `out`
    /// (only when it changed, so dependents are not rebuilt needlessly).
    ///
    /// Run from a build script (`OUT_DIR` is set), it also prints
    /// `cargo:rerun-if-changed=` for every `.proto` file it read, so the
    /// build script reruns when one of them changes, and only then.
    pub fn compile_to_file<P: AsRef<Path>>(
        &self,
        include_paths: &[P],
        roots: &[&str],
        out: impl AsRef<Path>,
    ) -> Result<(), IdlError> {
        let loader = FsLoader::new(include_paths);
        let set = FileSet::load(&loader, roots)?;
        if std::env::var_os("OUT_DIR").is_some() {
            for f in set.files() {
                if let Some(path) = loader.locate(&f.name) {
                    println!("cargo:rerun-if-changed={}", path.display());
                }
            }
        }
        let code = self.generate(&set)?;
        let out = out.as_ref();
        if std::fs::read_to_string(out).ok().as_deref() == Some(code.as_str()) {
            return Ok(());
        }
        std::fs::write(out, code).map_err(|e| {
            IdlError::new(
                out.display().to_string(),
                Span::default(),
                format!("cannot write: {e}"),
            )
        })
    }
}

/// Generates Rust source for `set` with the default [`Config`].
pub fn generate(set: &FileSet) -> Result<String, IdlError> {
    Config::new().generate(set)
}

/// Formats `text` as `///` doc-comment lines.
///
/// Each line is trimmed and Markdown that rustdoc would misread in
/// free-form `.proto` comments (link brackets, HTML-like `<…>`, code fences)
/// is escaped, so generated docs never produce rustdoc warnings or doctests.
/// Bare `http://` / `https://` URLs become `<…>` autolinks (rustdoc's
/// `bare_urls`), tabs become spaces (clippy's `tabs_in_doc_comments`), and
/// lines continuing a list item are indented under it (clippy's
/// `doc_lazy_continuation`). Returns an empty string for empty text.
pub fn doc_comment(text: &str) -> String {
    let mut out = String::new();
    // The indent continuation lines need while inside a list item.
    let mut list_indent: Option<usize> = None;
    for line in text.lines() {
        let line = line.replace('\t', "    ");
        let line = line.trim();
        if line.is_empty() {
            list_indent = None;
            out.push_str("///\n");
            continue;
        }
        let line = line.replace("```", "'''");
        let marker = list_marker(&line);
        let mut escaped = String::with_capacity(line.len() + 8);
        if let (None, Some(indent)) = (marker, list_indent) {
            escaped.extend(std::iter::repeat(' ').take(indent));
        }
        if marker.is_some() {
            list_indent = marker;
        }
        let mut in_code = false;
        let mut rest = line.as_str();
        while let Some(c) = rest.chars().next() {
            if !in_code && (rest.starts_with("https://") || rest.starts_with("http://")) {
                let url = url_prefix(rest);
                escaped.push('<');
                escaped.push_str(url);
                escaped.push('>');
                rest = &rest[url.len()..];
                continue;
            }
            match c {
                '`' => {
                    in_code = !in_code;
                    escaped.push(c);
                }
                '[' | ']' | '<' | '>' | '*' | '_' | '#' | '\\' if !in_code => {
                    escaped.push('\\');
                    escaped.push(c);
                }
                _ => escaped.push(c),
            }
            rest = &rest[c.len_utf8()..];
        }
        if in_code {
            // An unbalanced backtick would open a code span to the end.
            escaped = escaped.replace('`', "'");
        }
        out.push_str("/// ");
        out.push_str(&escaped);
        out.push('\n');
    }
    // Trailing empty doc lines add nothing.
    while out.ends_with("///\n") {
        out.truncate(out.len() - 4);
    }
    out
}

/// The width of a Markdown list marker starting `line` (`- `, `+ `, `1. `,
/// `2) `), which continuation lines are indented by. `*` lists cannot occur:
/// `*` is escaped.
fn list_marker(line: &str) -> Option<usize> {
    if line.starts_with("- ") || line.starts_with("+ ") {
        return Some(2);
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    let rest = &line[digits..];
    (digits > 0 && digits <= 9 && (rest.starts_with(". ") || rest.starts_with(") ")))
        .then_some(digits + 2)
}

/// The URL at the start of `text`: up to whitespace or a character that
/// cannot be in an autolink, without trailing punctuation.
fn url_prefix(text: &str) -> &str {
    let end = text
        .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '`' | '"'))
        .unwrap_or(text.len());
    text[..end].trim_end_matches(['.', ',', ';', ':', ')', '!', '?', '\''])
}

// ---- the generator ------------------------------------------------------------

/// One Rust module of the output.
#[derive(Default)]
struct Module {
    doc: String,
    items: String,
    children: BTreeMap<String, Module>,
}

impl Module {
    fn child(&mut self, path: &[String]) -> &mut Module {
        let mut m = self;
        for p in path {
            m = m.children.entry(p.clone()).or_default();
        }
        m
    }

    fn render(&self, depth: usize, out: &mut String) {
        let pad = "    ".repeat(depth);
        for line in self.items.trim_start_matches('\n').lines() {
            if line.is_empty() {
                out.push('\n');
            } else {
                out.push_str(&pad);
                out.push_str(line);
                out.push('\n');
            }
        }
        for (name, child) in &self.children {
            if !out.is_empty() && !out.ends_with("\n\n") {
                out.push('\n');
            }
            if child.doc.is_empty() {
                // Every module is documented, so the output compiles under a
                // consumer's `#![deny(missing_docs)]`.
                let _ = writeln!(out, "{pad}/// Module `{name}`.");
            }
            for line in child.doc.lines() {
                out.push_str(&pad);
                out.push_str(line);
                out.push('\n');
            }
            let _ = writeln!(out, "{pad}pub mod {name} {{");
            child.render(depth + 1, out);
            let _ = writeln!(out, "{pad}}}");
        }
    }
}

/// Where a message or enum lands in the Rust output.
#[derive(Clone)]
struct Location {
    /// Module path from the root (`["greet", "v1", "outer"]`).
    module: Vec<String>,
    /// The item name (`Inner`).
    name: String,
}

struct Generator<'a> {
    cfg: &'a Config,
    set: &'a FileSet,
    locations: HashMap<String, Location>,
    /// Singular message edges for recursion detection.
    edges: HashMap<String, Vec<String>>,
    root: Module,
    /// The Rust names claimed in each module, to report clashes.
    claimed: HashMap<(Vec<String>, Namespace, String), Claim>,
}

/// Which Rust namespace a generated item occupies.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Namespace {
    /// Structs, enums, modules, traits.
    Type,
    /// Constants.
    Value,
}

/// What produced a Rust name.
#[derive(Clone)]
struct Claim {
    /// A description such as "message `a.Foo`"; two claims with the same
    /// origin (one package spread over files) do not clash.
    origin: String,
    file: String,
    span: Span,
}

const P: &str = "::ferroly::proto";

impl<'a> Generator<'a> {
    fn new(cfg: &'a Config, set: &'a FileSet) -> Self {
        Generator {
            cfg,
            set,
            locations: HashMap::new(),
            edges: HashMap::new(),
            root: Module::default(),
            claimed: HashMap::new(),
        }
    }

    /// Claims the Rust name `name` in `module`, failing when another
    /// definition already produced it.
    fn claim_item(
        &mut self,
        module: &[String],
        ns: Namespace,
        name: &str,
        claim: Claim,
    ) -> Result<(), IdlError> {
        let key = (
            module.to_vec(),
            ns,
            name.trim_start_matches("r#").to_owned(),
        );
        if let Some(prev) = self.claimed.get(&key) {
            if prev.origin == claim.origin {
                return Ok(());
            }
            let place = if module.is_empty() {
                "the top-level module".to_owned()
            } else {
                format!("module `{}`", module.join("::"))
            };
            let line = if prev.file == claim.file {
                format!("line {}", prev.span.line)
            } else {
                format!("{} line {}", prev.file, prev.span.line)
            };
            return Err(IdlError::new(
                &claim.file,
                claim.span,
                format!(
                    "{} becomes the Rust name `{name}` in {place}, which {} ({line}) also becomes; rename one",
                    claim.origin, prev.origin
                ),
            ));
        }
        self.claimed.insert(key, claim);
        Ok(())
    }

    /// Claims, before any code is written, the names of every package
    /// module, message, nested-type module, oneof enum and enum, so a clash
    /// is an error naming both definitions rather than Rust that does not
    /// compile.
    fn reserve_type_names(&mut self) -> Result<(), IdlError> {
        let set = self.set;
        for f in set.files() {
            let pkg = package_module(&f.package);
            let parts: Vec<&str> = f.package.split('.').collect();
            for depth in 1..=pkg.len() {
                let claim = Claim {
                    origin: format!("package `{}`", parts[..depth].join(".")),
                    file: f.name.clone(),
                    span: Span::default(),
                };
                self.claim_item(&pkg[..depth - 1], Namespace::Type, &pkg[depth - 1], claim)?;
            }
            for m in &f.messages {
                self.reserve_message(&f.name, m, &pkg)?;
            }
            for e in &f.enums {
                let claim = Claim {
                    origin: format!("enum `{}`", e.full_name),
                    file: f.name.clone(),
                    span: e.span,
                };
                let name = self.locations[&e.full_name].name.clone();
                self.claim_item(&pkg, Namespace::Type, &name, claim)?;
            }
        }
        Ok(())
    }

    fn reserve_message(
        &mut self,
        file: &str,
        m: &MessageDef,
        module: &[String],
    ) -> Result<(), IdlError> {
        let claim = |what: &str, span: Span| Claim {
            origin: format!("{what} `{}`", m.full_name),
            file: file.to_owned(),
            span,
        };
        let name = self.locations[&m.full_name].name.clone();
        self.claim_item(module, Namespace::Type, &name, claim("message", m.span))?;
        if !has_nested_module(m) {
            return Ok(());
        }
        let inner = nested_module(module, &m.name);
        let module_name = inner.last().cloned().unwrap_or_default();
        self.claim_item(
            module,
            Namespace::Type,
            &module_name,
            claim("the nested types of message", m.span),
        )?;
        for (idx, o) in m.oneofs.iter().enumerate() {
            let enum_name = oneof_enum_name(m, idx);
            let c = Claim {
                origin: format!("oneof `{}` of `{}`", o.name, m.full_name),
                file: file.to_owned(),
                span: o.span,
            };
            self.claim_item(&inner, Namespace::Type, &enum_name, c)?;
        }
        for nested in &m.messages {
            self.reserve_message(file, nested, &inner)?;
        }
        for e in &m.enums {
            let c = Claim {
                origin: format!("enum `{}`", e.full_name),
                file: file.to_owned(),
                span: e.span,
            };
            let name = self.locations[&e.full_name].name.clone();
            self.claim_item(&inner, Namespace::Type, &name, c)?;
        }
        Ok(())
    }

    fn run(mut self) -> Result<String, IdlError> {
        let set = self.set;
        for f in set.files() {
            let pkg = package_module(&f.package);
            for m in &f.messages {
                self.locate_message(m, &pkg);
            }
            for e in &f.enums {
                self.locations.insert(
                    e.full_name.clone(),
                    Location {
                        module: pkg.clone(),
                        name: names::ident(&names::pascal(&e.name)),
                    },
                );
            }
        }
        self.reserve_type_names()?;
        for f in set.files() {
            let pkg = package_module(&f.package);
            // Each module of the package path is documented: `a.b` gives
            // `a` (package prefix `a`) and `a::b` (package `a.b`).
            let parts: Vec<&str> = f.package.split('.').collect();
            for depth in 1..=pkg.len() {
                let node = self.root.child(&pkg[..depth]);
                if node.doc.is_empty() {
                    node.doc = format!("/// Package `{}`.\n", parts[..depth].join("."));
                }
            }
            for m in &f.messages {
                self.message(f, m, &pkg)?;
            }
            for e in &f.enums {
                let code = self.enumeration(e);
                self.root.child(&pkg).items.push_str(&code);
            }
            for s in &f.services {
                let code = self.service(f, s, &pkg)?;
                self.root.child(&pkg).items.push_str(&code);
            }
        }
        let mut out = String::new();
        out.push_str("// @generated by ferroly::proto::codegen. Do not edit.\n");
        let _ = writeln!(out, "// Sources: {}", self.set.roots().join(", "));
        out.push('\n');
        self.root.render(0, &mut out);
        while out.ends_with("\n\n") {
            out.pop();
        }
        Ok(out)
    }

    fn locate_message(&mut self, m: &MessageDef, module: &[String]) {
        self.locations.insert(
            m.full_name.clone(),
            Location {
                module: module.to_vec(),
                name: names::ident(&names::pascal(&m.name)),
            },
        );
        let mut edges = Vec::new();
        for f in &m.fields {
            if f.label == Label::Repeated {
                continue;
            }
            if let FieldType::Named(n) = &f.ty {
                if let Some(t) = n.target.as_ref().filter(|t| t.kind == TypeKind::Message) {
                    edges.push(t.full_name.clone());
                }
            }
        }
        self.edges.insert(m.full_name.clone(), edges);
        let inner = nested_module(module, &m.name);
        for nested in &m.messages {
            self.locate_message(nested, &inner);
        }
        for e in &m.enums {
            self.locations.insert(
                e.full_name.clone(),
                Location {
                    module: inner.clone(),
                    name: names::ident(&names::pascal(&e.name)),
                },
            );
        }
    }

    /// Whether `from` reaches `to` through singular message fields.
    fn reaches(&self, from: &str, to: &str) -> bool {
        let mut seen = HashSet::new();
        let mut stack = vec![from.to_owned()];
        while let Some(cur) = stack.pop() {
            if cur == to {
                return true;
            }
            if !seen.insert(cur.clone()) {
                continue;
            }
            if let Some(next) = self.edges.get(&cur) {
                stack.extend(next.iter().cloned());
            }
        }
        false
    }

    /// The Rust path of the type `full_name`, as seen from `from`.
    fn path_to(&self, from: &[String], full_name: &str) -> String {
        let Some(loc) = self.locations.get(full_name) else {
            return "()".to_owned();
        };
        let common = from
            .iter()
            .zip(&loc.module)
            .take_while(|(a, b)| a == b)
            .count();
        let mut parts: Vec<String> = Vec::new();
        for _ in common..from.len() {
            parts.push("super".to_owned());
        }
        parts.extend(loc.module[common..].iter().cloned());
        parts.push(loc.name.clone());
        parts.join("::")
    }

    fn attributes_for(&self, full_name: &str) -> String {
        let dotted = format!(".{full_name}");
        let mut out = String::new();
        for (path, attr) in &self.cfg.type_attributes {
            let matches = path == "."
                || dotted == *path
                || dotted.starts_with(&format!("{}.", path.trim_end_matches('.')));
            if matches {
                out.push_str(attr);
                out.push('\n');
            }
        }
        out
    }

    // ---- messages -------------------------------------------------------------

    fn message(
        &mut self,
        file: &ProtoFile,
        m: &MessageDef,
        module: &[String],
    ) -> Result<(), IdlError> {
        let loc = self.locations[&m.full_name].clone();
        let inner = nested_module(module, &m.name);
        let mut code = String::new();
        code.push('\n');
        code.push_str(&item_doc(
            &m.doc,
            &format!("Message `{}`.", m.full_name),
            m.deprecated(),
        ));
        code.push_str(&self.attributes_for(&m.full_name));
        let _ = writeln!(
            code,
            "#[derive(Clone, PartialEq, Debug, Default, {P}::Message)]"
        );
        let _ = writeln!(code, "pub struct {} {{", loc.name);

        let mut used: HashMap<String, Span> = HashMap::new();
        let mut oneofs_done = HashSet::new();
        let mut oneof_code = String::new();

        // Members in field-number order (a oneof at its lowest member's
        // number), so the derive writes fields in the order protoc and the
        // other runtimes do, and re-encoded bytes match theirs.
        let mut order: Vec<&FieldDef> = m.fields.iter().collect();
        let oneof_first = |idx: usize| {
            m.fields
                .iter()
                .filter(|f| f.oneof == Some(idx))
                .map(|f| f.number)
                .min()
                .unwrap_or(0)
        };
        order.sort_by_key(|f| f.oneof.map_or(f.number, oneof_first));

        for f in order {
            if let Some(idx) = f.oneof {
                if !oneofs_done.insert(idx) {
                    continue;
                }
                let o = &m.oneofs[idx];
                let field_name = names::ident(&names::snake(&o.name));
                claim(&file.name, &mut used, &field_name, o.span)?;
                let enum_name = oneof_enum_name(m, idx);
                let enum_path = format!(
                    "{}::{enum_name}",
                    inner.last().map_or(String::new(), Clone::clone)
                );
                code.push_str(&indent(&item_doc(
                    &o.doc,
                    &format!("Oneof `{}`.", o.name),
                    false,
                )));
                let _ = writeln!(code, "    #[proto(oneof)]");
                let _ = writeln!(
                    code,
                    "    pub {field_name}: ::core::option::Option<{enum_path}>,"
                );
                oneof_code.push_str(&self.oneof(file, m, idx, &enum_name, &inner)?);
                continue;
            }
            let field_name = names::ident(&names::snake(&f.name));
            claim(&file.name, &mut used, &field_name, f.span)?;
            let (ty, attrs) = self.field(file, m, f, module);
            code.push_str(&indent(&item_doc(
                &f.doc,
                &format!("Field `{}` = {}.", f.name, f.number),
                f.deprecated(),
            )));
            let _ = writeln!(code, "    #[proto({attrs})]");
            let _ = writeln!(code, "    pub {field_name}: {ty},");
        }
        let mut unknown = "unknown_fields".to_owned();
        while used.contains_key(&unknown) {
            unknown.push('_');
        }
        let _ = writeln!(
            code,
            "    /// Fields this program does not know, kept so they are written back."
        );
        let _ = writeln!(code, "    #[proto(unknown)]");
        let _ = writeln!(code, "    pub {unknown}: {P}::UnknownFields,");
        code.push_str("}\n");
        self.root.child(module).items.push_str(&code);

        if has_nested_module(m) {
            let node = self.root.child(&inner);
            if node.doc.is_empty() {
                node.doc = format!("/// Types nested in `{}`.\n", m.full_name);
            }
            node.items.push_str(&oneof_code);
            for nested in &m.messages {
                self.message(file, nested, &inner)?;
            }
            for e in &m.enums {
                let code = self.enumeration(e);
                self.root.child(&inner).items.push_str(&code);
            }
        }
        Ok(())
    }

    /// The Rust type and `#[proto(...)]` contents of a regular field.
    fn field(
        &self,
        file: &ProtoFile,
        m: &MessageDef,
        f: &FieldDef,
        module: &[String],
    ) -> (String, String) {
        let tag = f.number;
        match &f.ty {
            FieldType::Map(k, v) => {
                let map = match self.cfg.map_type {
                    MapType::BTreeMap => "::std::collections::BTreeMap",
                    MapType::HashMap => "::std::collections::HashMap",
                };
                let (vty, vkind) = self.element(v, module, false);
                (
                    format!("{map}<{}, {vty}>", scalar_rust(*k)),
                    format!(
                        "tag = {tag}, map(key = \"{}\", value = \"{vkind}\")",
                        k.keyword()
                    ),
                )
            }
            ty => {
                let boxed = match ty {
                    FieldType::Named(n) => n.target.as_ref().is_some_and(|t| {
                        t.kind == TypeKind::Message
                            && f.label != Label::Repeated
                            && self.reaches(&t.full_name, &m.full_name)
                    }),
                    _ => false,
                };
                let (elem, kind) = self.element(ty, module, boxed);
                let is_message = matches!(ty, FieldType::Named(n) if n.target.as_ref().is_some_and(|t| t.kind == TypeKind::Message));
                let packable = match ty {
                    FieldType::Scalar(s) => s.packable(),
                    FieldType::Named(_) => !is_message,
                    FieldType::Map(..) => false,
                };
                let mut attrs = format!("tag = {tag}, {kind}");
                let rust = match f.label {
                    Label::Repeated => {
                        let packed = match (f.packed(), file.syntax) {
                            (Some(p), _) => p,
                            (None, Syntax::Proto3) => true,
                            (None, Syntax::Proto2) => false,
                        };
                        if packable && !packed {
                            attrs.push_str(", packed = false");
                        }
                        format!("::std::vec::Vec<{elem}>")
                    }
                    Label::Required => {
                        attrs.push_str(", required");
                        elem
                    }
                    Label::Optional => format!("::core::option::Option<{elem}>"),
                    Label::None if is_message || file.syntax == Syntax::Proto2 => {
                        format!("::core::option::Option<{elem}>")
                    }
                    Label::None => elem,
                };
                (rust, attrs)
            }
        }
    }

    /// The Rust type of one value and its kind keyword.
    fn element(&self, ty: &FieldType, module: &[String], boxed: bool) -> (String, &'static str) {
        match ty {
            FieldType::Scalar(s) => (scalar_rust(*s).to_owned(), s.keyword()),
            FieldType::Named(n) => match &n.target {
                Some(t) if t.kind == TypeKind::Enum => (
                    format!("{P}::EnumValue<{}>", self.path_to(module, &t.full_name)),
                    "enumeration",
                ),
                Some(t) => {
                    let path = self.path_to(module, &t.full_name);
                    if boxed {
                        (format!("::std::boxed::Box<{path}>"), "message")
                    } else {
                        (path, "message")
                    }
                }
                None => ("()".to_owned(), "message"),
            },
            FieldType::Map(..) => ("()".to_owned(), "message"),
        }
    }

    fn oneof(
        &self,
        file: &ProtoFile,
        m: &MessageDef,
        idx: usize,
        enum_name: &str,
        module: &[String],
    ) -> Result<String, IdlError> {
        let o = &m.oneofs[idx];
        let mut code = String::new();
        code.push('\n');
        code.push_str(&item_doc(
            &o.doc,
            &format!("Oneof `{}` of `{}`.", o.name, m.full_name),
            false,
        ));
        code.push_str(&self.attributes_for(&format!("{}.{}", m.full_name, o.name)));
        let _ = writeln!(code, "#[derive(Clone, PartialEq, Debug, {P}::Oneof)]");
        let _ = writeln!(code, "pub enum {enum_name} {{");
        let mut used: HashMap<String, Span> = HashMap::new();
        let mut members: Vec<&FieldDef> =
            m.fields.iter().filter(|f| f.oneof == Some(idx)).collect();
        members.sort_by_key(|f| f.number);
        for f in members {
            let variant = names::ident(&names::pascal(&f.name));
            claim(&file.name, &mut used, &variant, f.span)?;
            let boxed = match &f.ty {
                FieldType::Named(n) => n.target.as_ref().is_some_and(|t| {
                    t.kind == TypeKind::Message && self.reaches(&t.full_name, &m.full_name)
                }),
                _ => false,
            };
            let (ty, kind) = self.element(&f.ty, module, boxed);
            code.push_str(&indent(&item_doc(
                &f.doc,
                &format!("Field `{}` = {}.", f.name, f.number),
                f.deprecated(),
            )));
            let _ = writeln!(code, "    #[proto(tag = {}, {kind})]", f.number);
            let _ = writeln!(code, "    {variant}({ty}),");
        }
        code.push_str("}\n");
        Ok(code)
    }

    // ---- enums ----------------------------------------------------------------

    fn enumeration(&self, e: &EnumDef) -> String {
        let loc = &self.locations[&e.full_name];
        let mut code = String::new();
        code.push('\n');
        code.push_str(&item_doc(
            &e.doc,
            &format!("Enum `{}`.", e.full_name),
            e.deprecated(),
        ));
        code.push_str(&self.attributes_for(&e.full_name));
        let _ = writeln!(
            code,
            "#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, {P}::Enumeration)]"
        );
        let _ = writeln!(code, "#[repr(i32)]");
        let _ = writeln!(code, "pub enum {} {{", loc.name);
        let variants = variant_names(e);
        let mut numbers = HashSet::new();
        let mut aliases = Vec::new();
        for (v, rust) in e.values.iter().zip(&variants) {
            if !numbers.insert(v.number) {
                aliases.push(v);
                continue;
            }
            code.push_str(&indent(&item_doc(
                &v.doc,
                &format!("`{}` = {}.", v.name, v.number),
                v.deprecated(),
            )));
            let _ = writeln!(code, "    #[proto(name = {:?})]", v.name);
            let _ = writeln!(code, "    {rust} = {},", v.number);
        }
        code.push_str("}\n");
        if !aliases.is_empty() {
            let _ = writeln!(code, "\nimpl {} {{", loc.name);
            for v in aliases {
                let target = e
                    .values
                    .iter()
                    .zip(&variants)
                    .find(|(o, _)| o.number == v.number)
                    .map(|(_, r)| r.clone())
                    .unwrap_or_default();
                let _ = writeln!(code, "    /// `{}`: an alias of `{target}`.", v.name);
                let _ = writeln!(
                    code,
                    "    pub const {}: {} = {}::{target};",
                    names::ident(&names::screaming(&v.name)),
                    loc.name,
                    loc.name
                );
            }
            code.push_str("}\n");
        }
        code
    }

    // ---- services ---------------------------------------------------------------

    fn service(
        &mut self,
        file: &'a ProtoFile,
        s: &'a ServiceDef,
        module: &[String],
    ) -> Result<String, IdlError> {
        let const_prefix = names::screaming(&s.name);
        let descriptor = self
            .cfg
            .descriptors
            .then(|| format!("{const_prefix}_DESCRIPTOR"));
        let methods: Vec<MethodContext<'a>> = s
            .methods
            .iter()
            .map(|m| {
                let input_proto = m
                    .input
                    .target
                    .as_ref()
                    .map_or(m.input.name.as_str(), |t| t.full_name.as_str());
                let output_proto = m
                    .output
                    .target
                    .as_ref()
                    .map_or(m.output.name.as_str(), |t| t.full_name.as_str());
                MethodContext {
                    method: m,
                    name: &m.name,
                    rust_name: names::ident(&names::snake(&m.name)),
                    path: format!("/{}/{}", s.full_name, m.name),
                    input_proto,
                    output_proto,
                    input_type: self.path_to(module, input_proto),
                    output_type: self.path_to(module, output_proto),
                    client_streaming: m.client_streaming,
                    server_streaming: m.server_streaming,
                    kind: MethodKind::from_streaming(m.client_streaming, m.server_streaming),
                    doc: &m.doc,
                }
            })
            .collect();
        let ctx = ServiceContext {
            file,
            service: s,
            package: &file.package,
            full_name: &s.full_name,
            rust_name: names::pascal(&s.name),
            snake_name: names::snake(&s.name),
            const_prefix,
            descriptor,
            doc: &s.doc,
            methods,
        };
        let claim = |origin: String| Claim {
            origin,
            file: file.name.clone(),
            span: s.span,
        };
        if let Some(name) = &ctx.descriptor {
            let c = claim(format!("the descriptor of service `{}`", s.full_name));
            self.claim_item(module, Namespace::Value, name, c)?;
        }
        let generators = &self.cfg.generators;
        for g in generators {
            for name in g.reserved_names(&ctx) {
                let origin = format!("an item generated for service `{}`", s.full_name);
                // A generator's name may be a type or a value: it clashes
                // with either.
                self.claim_item(module, Namespace::Type, &name, claim(origin.clone()))?;
                self.claim_item(module, Namespace::Value, &name, claim(origin))?;
            }
        }
        let mut code = String::new();
        if let Some(name) = &ctx.descriptor {
            code.push('\n');
            let _ = writeln!(code, "/// Descriptor of the `{}` service.", s.full_name);
            let doc = doc_comment(&s.doc);
            if !doc.is_empty() {
                code.push_str("///\n");
                code.push_str(&doc);
            }
            let _ = writeln!(
                code,
                "pub const {name}: {P}::ServiceDescriptor = {P}::ServiceDescriptor::new("
            );
            let _ = writeln!(code, "    {:?},", file.package);
            let _ = writeln!(code, "    {:?},", s.name);
            let _ = writeln!(code, "    {:?},", s.full_name);
            let _ = writeln!(code, "    &[");
            for m in &ctx.methods {
                let _ = writeln!(code, "        {P}::MethodDescriptor::new(");
                let _ = writeln!(code, "            {:?},", m.name);
                let _ = writeln!(code, "            {:?},", m.path);
                let _ = writeln!(code, "            {:?},", m.input_proto);
                let _ = writeln!(code, "            {:?},", m.output_proto);
                let _ = writeln!(code, "            {},", m.client_streaming);
                let _ = writeln!(code, "            {},", m.server_streaming);
                let _ = writeln!(code, "        ),");
            }
            let _ = writeln!(code, "    ],");
            let _ = writeln!(code, ");");
        }
        for g in &self.cfg.generators {
            let mut extra = String::new();
            g.generate(&ctx, &mut extra);
            if !extra.is_empty() {
                code.push('\n');
                code.push_str(&extra);
                if !extra.ends_with('\n') {
                    code.push('\n');
                }
            }
        }
        Ok(code)
    }
}

fn package_module(package: &str) -> Vec<String> {
    if package.is_empty() {
        return Vec::new();
    }
    package
        .split('.')
        .map(|p| names::ident(&names::snake(p)))
        .collect()
}

/// Whether `m` gets a module for its nested messages, enums and oneofs.
fn has_nested_module(m: &MessageDef) -> bool {
    !m.messages.is_empty() || !m.enums.is_empty() || !m.oneofs.is_empty()
}

/// The Rust name of the enum generated for oneof `idx` of `m`: its name in
/// `PascalCase`, with `Oneof` appended when a nested type already has it.
fn oneof_enum_name(m: &MessageDef, idx: usize) -> String {
    let mut name = names::pascal(&m.oneofs[idx].name);
    let taken = m
        .messages
        .iter()
        .map(|n| names::pascal(&n.name))
        .chain(m.enums.iter().map(|e| names::pascal(&e.name)))
        .any(|n| n == name);
    if taken {
        name.push_str("Oneof");
    }
    names::ident(&name)
}

fn nested_module(module: &[String], message: &str) -> Vec<String> {
    let mut m = module.to_vec();
    m.push(names::ident(&names::snake(message)));
    m
}

fn claim(
    file: &str,
    used: &mut HashMap<String, Span>,
    name: &str,
    span: Span,
) -> Result<(), IdlError> {
    if let Some(prev) = used.insert(name.to_owned(), span) {
        return Err(IdlError::new(
            file,
            span,
            format!(
                "the Rust name `{name}` is also produced by the definition on line {}; rename one",
                prev.line
            ),
        ));
    }
    Ok(())
}

fn scalar_rust(s: ScalarType) -> &'static str {
    match s {
        ScalarType::Double => "f64",
        ScalarType::Float => "f32",
        ScalarType::Int32 | ScalarType::SInt32 | ScalarType::SFixed32 => "i32",
        ScalarType::Int64 | ScalarType::SInt64 | ScalarType::SFixed64 => "i64",
        ScalarType::UInt32 | ScalarType::Fixed32 => "u32",
        ScalarType::UInt64 | ScalarType::Fixed64 => "u64",
        ScalarType::Bool => "bool",
        ScalarType::String => "::std::string::String",
        ScalarType::Bytes => "::std::vec::Vec<u8>",
    }
}

/// Doc lines for an item: its `.proto` comment, or `fallback` without one.
fn item_doc(doc: &str, fallback: &str, deprecated: bool) -> String {
    let mut out = doc_comment(doc);
    if out.is_empty() {
        let _ = writeln!(out, "/// {fallback}");
    }
    if deprecated {
        out.push_str("///\n/// Deprecated in the `.proto` definition.\n");
    }
    out
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|l| {
            if l.is_empty() {
                "\n".to_owned()
            } else {
                format!("    {l}\n")
            }
        })
        .collect()
}

/// Rust variant names for an enum: the shared `ENUM_NAME_` prefix stripped
/// when every value has it, then `PascalCase`; the full names when stripping
/// would collide or start with a digit.
fn variant_names(e: &EnumDef) -> Vec<String> {
    let prefix = format!("{}_", names::screaming(&e.name));
    let stripped: Option<Vec<String>> = e
        .values
        .iter()
        .map(|v| {
            v.name
                .strip_prefix(&prefix)
                .filter(|rest| !rest.is_empty() && !rest.starts_with(|c: char| c.is_ascii_digit()))
                .map(names::pascal)
        })
        .collect();
    let distinct = |names: &[String]| {
        let mut seen = HashSet::new();
        names.iter().all(|n| seen.insert(n.clone()))
    };
    let chosen = match stripped {
        Some(s) if distinct(&s) => s,
        _ => e.values.iter().map(|v| names::pascal(&v.name)).collect(),
    };
    let chosen = if distinct(&chosen) {
        chosen
    } else {
        e.values.iter().map(|v| v.name.clone()).collect()
    };
    chosen.into_iter().map(|n| names::ident(&n)).collect()
}
