//! Loading imports, checking definitions, and resolving type names.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Component, Path, PathBuf};

use super::{
    parse, Constant, EnumDef, FieldDef, FieldType, IdlError, ImportKind, Label, MessageDef,
    NamedType, ProtoFile, ScalarType, ServiceDef, Span, Syntax, TypeKind, TypeTarget,
};

/// The most files one [`FileSet::load`] reads (roots plus imports).
const MAX_FILES: usize = 1024;
/// The deepest import chain followed.
const MAX_IMPORT_DEPTH: usize = 64;
/// The largest file [`FsLoader`] reads.
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Supplies the source text of `.proto` files by import path.
///
/// Implemented by [`FsLoader`] (a search path on disk), [`MemoryLoader`] (an
/// in-memory map) and any `Fn(&str) -> io::Result<String>`.
pub trait Loader {
    /// The source of the file imported as `path` (such as
    /// `google/api/http.proto`). A missing file is
    /// [`io::ErrorKind::NotFound`].
    fn load(&self, path: &str) -> io::Result<String>;
}

impl<F: Fn(&str) -> io::Result<String>> Loader for F {
    fn load(&self, path: &str) -> io::Result<String> {
        self(path)
    }
}

/// Loads files from a list of include directories, like `protoc -I`: an
/// import path is looked up in each directory in order.
///
/// Import paths must be relative and may not contain `..`, so a file cannot
/// reach outside the include directories.
#[derive(Debug, Clone, Default)]
pub struct FsLoader {
    include_paths: Vec<PathBuf>,
}

impl FsLoader {
    /// A loader searching `include_paths` in order.
    pub fn new<P: AsRef<Path>>(include_paths: impl IntoIterator<Item = P>) -> Self {
        FsLoader {
            include_paths: include_paths
                .into_iter()
                .map(|p| p.as_ref().to_path_buf())
                .collect(),
        }
    }

    /// The directories searched, in order.
    pub fn include_paths(&self) -> &[PathBuf] {
        &self.include_paths
    }

    /// The file an import path loads from: the first include directory that
    /// has it. `None` when no directory has it or the path is not a valid
    /// import path.
    pub fn locate(&self, path: &str) -> Option<PathBuf> {
        let rel = checked_import_path(path).ok()?;
        self.include_paths
            .iter()
            .map(|dir| dir.join(rel))
            .find(|full| full.is_file())
    }
}

fn checked_import_path(path: &str) -> io::Result<&Path> {
    let rel = Path::new(path);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "import paths must be relative and stay inside the include paths",
        ));
    }
    Ok(rel)
}

impl Loader for FsLoader {
    fn load(&self, path: &str) -> io::Result<String> {
        let rel = checked_import_path(path)?;
        for dir in &self.include_paths {
            let full = dir.join(rel);
            match std::fs::metadata(&full) {
                Ok(meta) if meta.is_file() => {
                    if meta.len() > MAX_FILE_BYTES {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("file is larger than {MAX_FILE_BYTES} bytes"),
                        ));
                    }
                    return std::fs::read_to_string(&full);
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "not found in any include path",
        ))
    }
}

/// An in-memory set of files keyed by import path (handy for tests and for
/// sources embedded with `include_str!`).
#[derive(Debug, Clone, Default)]
pub struct MemoryLoader {
    files: HashMap<String, String>,
}

impl MemoryLoader {
    /// An empty loader.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds (or replaces) a file.
    pub fn insert(&mut self, path: impl Into<String>, source: impl Into<String>) -> &mut Self {
        self.files.insert(path.into(), source.into());
        self
    }

    /// Adds a file, builder-style.
    #[must_use]
    pub fn with(mut self, path: impl Into<String>, source: impl Into<String>) -> Self {
        self.insert(path, source);
        self
    }
}

impl Loader for MemoryLoader {
    fn load(&self, path: &str) -> io::Result<String> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"))
    }
}

/// A set of parsed files with every import loaded and every type name
/// resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct FileSet {
    /// Dependencies come before the files that import them.
    files: Vec<ProtoFile>,
    /// The roots passed to `load`, in order.
    roots: Vec<String>,
}

impl FileSet {
    /// Parses `roots` and, transitively, every file they import (through
    /// `loader`, falling back to the built-in well-known types), then checks
    /// the whole set and resolves every message, enum and method type.
    pub fn load<L: Loader + ?Sized>(loader: &L, roots: &[&str]) -> Result<FileSet, IdlError> {
        let mut st = LoadState {
            loader,
            files: Vec::new(),
            done: HashSet::new(),
            stack: Vec::new(),
        };
        for root in roots {
            st.load(root, None)?;
        }
        let mut set = FileSet {
            files: st.files,
            roots: roots.iter().map(|r| (*r).to_owned()).collect(),
        };
        set.resolve()?;
        Ok(set)
    }

    /// Parses and resolves in-memory sources (each `(path, source)`; every
    /// one is a root).
    pub fn from_sources(sources: &[(&str, &str)]) -> Result<FileSet, IdlError> {
        let mut loader = MemoryLoader::new();
        for (path, src) in sources {
            loader.insert(*path, *src);
        }
        let roots: Vec<&str> = sources.iter().map(|(p, _)| *p).collect();
        Self::load(&loader, &roots)
    }

    /// Every file, dependencies before dependents.
    pub fn files(&self) -> &[ProtoFile] {
        &self.files
    }

    /// The root files, as passed to [`load`](Self::load).
    pub fn roots(&self) -> &[String] {
        &self.roots
    }

    /// The file loaded under `name`.
    pub fn file(&self, name: &str) -> Option<&ProtoFile> {
        self.files.iter().find(|f| f.name == name)
    }

    /// The message with the fully qualified name `full_name` (no leading dot).
    pub fn message(&self, full_name: &str) -> Option<&MessageDef> {
        fn find<'a>(ms: &'a [MessageDef], name: &str) -> Option<&'a MessageDef> {
            for m in ms {
                if m.full_name == name {
                    return Some(m);
                }
                if name.starts_with(&m.full_name) {
                    if let Some(found) = find(&m.messages, name) {
                        return Some(found);
                    }
                }
            }
            None
        }
        let name = full_name.strip_prefix('.').unwrap_or(full_name);
        self.files.iter().find_map(|f| find(&f.messages, name))
    }

    /// The enum with the fully qualified name `full_name` (no leading dot).
    pub fn enumeration(&self, full_name: &str) -> Option<&EnumDef> {
        fn find<'a>(ms: &'a [MessageDef], es: &'a [EnumDef], name: &str) -> Option<&'a EnumDef> {
            if let Some(e) = es.iter().find(|e| e.full_name == name) {
                return Some(e);
            }
            ms.iter().find_map(|m| find(&m.messages, &m.enums, name))
        }
        let name = full_name.strip_prefix('.').unwrap_or(full_name);
        self.files
            .iter()
            .find_map(|f| find(&f.messages, &f.enums, name))
    }

    /// Every service, with the file that declares it.
    pub fn services(&self) -> impl Iterator<Item = (&ProtoFile, &ServiceDef)> {
        self.files
            .iter()
            .flat_map(|f| f.services.iter().map(move |s| (f, s)))
    }

    fn resolve(&mut self) -> Result<(), IdlError> {
        // Symbol table: every message and enum, by full name.
        let mut symbols: HashMap<String, (TypeKind, String)> = HashMap::new();
        for f in &self.files {
            let mut out = Vec::new();
            collect(&f.messages, &f.enums, &mut out);
            for (name, kind, span) in out {
                if let Some((_, other)) = symbols.get(&name) {
                    return Err(IdlError::new(
                        &f.name,
                        span,
                        format!("`{name}` is already defined in {other}"),
                    ));
                }
                symbols.insert(name, (kind, f.name.clone()));
            }
        }
        // Enums declared in proto2 files are closed; proto3 messages may not
        // use them.
        let syntax_of: HashMap<&str, Syntax> = self
            .files
            .iter()
            .map(|f| (f.name.as_str(), f.syntax))
            .collect();
        let closed_enums: HashSet<String> = symbols
            .iter()
            .filter(|(_, (kind, file))| {
                *kind == TypeKind::Enum && syntax_of.get(file.as_str()) == Some(&Syntax::Proto2)
            })
            .map(|(name, _)| name.clone())
            .collect();
        // Packages and their prefixes are scopes a name's first part can bind to.
        let mut packages = HashSet::new();
        for f in &self.files {
            let mut prefix = f.package.as_str();
            while !prefix.is_empty() {
                packages.insert(prefix.to_owned());
                prefix = &prefix[..prefix.rfind('.').unwrap_or(0)];
            }
        }
        let symbols = Symbols {
            types: symbols
                .into_iter()
                .map(|(k, (kind, _))| (k, kind))
                .collect(),
            packages,
            closed_enums,
        };
        let symbols = &symbols;
        for f in &mut self.files {
            let file = f.name.clone();
            let proto3 = f.syntax == Syntax::Proto3;
            for m in &mut f.messages {
                resolve_message(&file, proto3, m, symbols)?;
            }
            for s in &mut f.services {
                let scope = s.full_name.clone();
                for method in &mut s.methods {
                    for ty in [&mut method.input, &mut method.output] {
                        resolve_name(&file, ty, &scope, symbols, method.span)?;
                        if ty.target.as_ref().is_some_and(|t| t.kind == TypeKind::Enum) {
                            return Err(IdlError::new(
                                &file,
                                method.span,
                                format!(
                                    "rpc `{}` uses the enum `{}`; methods take messages",
                                    method.name, ty.name
                                ),
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn collect(ms: &[MessageDef], es: &[EnumDef], out: &mut Vec<(String, TypeKind, Span)>) {
    for e in es {
        out.push((e.full_name.clone(), TypeKind::Enum, e.span));
    }
    for m in ms {
        out.push((m.full_name.clone(), TypeKind::Message, m.span));
        collect(&m.messages, &m.enums, out);
    }
}

/// What names resolve against.
struct Symbols {
    /// Every message and enum, by full name.
    types: HashMap<String, TypeKind>,
    /// Every package and package prefix (`a.b` gives `a` and `a.b`).
    packages: HashSet<String>,
    /// Enums declared in proto2 files.
    closed_enums: HashSet<String>,
}

fn resolve_message(
    file: &str,
    proto3: bool,
    m: &mut MessageDef,
    symbols: &Symbols,
) -> Result<(), IdlError> {
    let scope = m.full_name.clone();
    for f in &mut m.fields {
        let span = f.span;
        let ty = match &mut f.ty {
            FieldType::Named(n) => Some(n),
            FieldType::Map(_, v) => match v.as_mut() {
                FieldType::Named(n) => Some(n),
                _ => None,
            },
            _ => None,
        };
        if let Some(n) = ty {
            resolve_name(file, n, &scope, symbols, span)?;
            if let Some(t) = n.target.as_ref() {
                if proto3 && symbols.closed_enums.contains(&t.full_name) {
                    return Err(IdlError::new(
                        file,
                        span,
                        format!(
                            "field `{}` uses `{}`, a proto2 (closed) enum; proto3 messages can use only proto3 enums",
                            f.name, t.full_name
                        ),
                    ));
                }
            }
        }
        if f.packed() == Some(true) {
            let packable = match &f.ty {
                FieldType::Scalar(s) => s.packable(),
                FieldType::Named(n) => n.target.as_ref().is_some_and(|t| t.kind == TypeKind::Enum),
                _ => false,
            };
            if !packable {
                return Err(IdlError::new(
                    file,
                    span,
                    format!("field `{}`: [packed = true] applies only to repeated scalar numeric, bool and enum fields", f.name),
                ));
            }
        }
    }
    for nested in &mut m.messages {
        resolve_message(file, proto3, nested, symbols)?;
    }
    Ok(())
}

/// Resolves a type name as protoc does: an absolute `.a.B` directly; a
/// relative name from the innermost scope outwards. For a dotted name `a.B`,
/// the first scope where `a` names a message, enum or package decides: `B`
/// must then exist inside it, and outer scopes are not tried (so an inner
/// `a` shadows an outer package `a`).
fn resolve_name(
    file: &str,
    ty: &mut NamedType,
    scope: &str,
    symbols: &Symbols,
    span: Span,
) -> Result<(), IdlError> {
    let unknown = |msg: String| Err(IdlError::new(file, span, msg));
    let found = if let Some(abs) = ty.name.strip_prefix('.') {
        symbols.types.get(abs).map(|k| (abs.to_owned(), *k))
    } else {
        let first = ty.name.split('.').next().unwrap_or("");
        let dotted = first.len() < ty.name.len();
        let mut scope = scope.to_owned();
        loop {
            let join = |name: &str| {
                if scope.is_empty() {
                    name.to_owned()
                } else {
                    format!("{scope}.{name}")
                }
            };
            if dotted {
                let head = join(first);
                if symbols.types.contains_key(&head) || symbols.packages.contains(&head) {
                    let candidate = join(&ty.name);
                    match symbols.types.get(&candidate) {
                        Some(k) => break Some((candidate, *k)),
                        None => {
                            return unknown(format!(
                            "unknown type `{}`: `{first}` refers to `{head}`, which has no `{}`",
                            ty.name,
                            &ty.name[first.len() + 1..]
                        ))
                        }
                    }
                }
            } else {
                let candidate = join(&ty.name);
                if let Some(k) = symbols.types.get(&candidate) {
                    break Some((candidate, *k));
                }
            }
            if scope.is_empty() {
                break None;
            }
            scope.truncate(scope.rfind('.').unwrap_or(0));
        }
    };
    match found {
        Some((full_name, kind)) => {
            ty.target = Some(TypeTarget { full_name, kind });
            Ok(())
        }
        None => unknown(format!("unknown type `{}`", ty.name)),
    }
}

struct LoadState<'a, L: Loader + ?Sized> {
    loader: &'a L,
    files: Vec<ProtoFile>,
    done: HashSet<String>,
    stack: Vec<String>,
}

impl<L: Loader + ?Sized> LoadState<'_, L> {
    /// Loads `path` and its imports (depth first, dependencies first).
    /// `from` is the importing file and the import's position.
    fn load(&mut self, path: &str, from: Option<(&str, Span, ImportKind)>) -> Result<(), IdlError> {
        if self.done.contains(path) {
            return Ok(());
        }
        let at = |msg: String| match from {
            Some((file, span, _)) => IdlError::new(file, span, msg),
            None => IdlError::new(path, Span::default(), msg),
        };
        if self.stack.iter().any(|p| p == path) {
            let mut chain = self.stack.clone();
            chain.push(path.to_owned());
            return Err(at(format!("import cycle: {}", chain.join(" -> "))));
        }
        if self.stack.len() >= MAX_IMPORT_DEPTH {
            return Err(at(format!(
                "imports nested deeper than {MAX_IMPORT_DEPTH} files"
            )));
        }
        if self.done.len() >= MAX_FILES {
            return Err(at(format!("more than {MAX_FILES} files")));
        }
        let source = match self.loader.load(path) {
            Ok(s) => s,
            Err(e) => match super::wkt::source(path) {
                Some(builtin) if e.kind() == io::ErrorKind::NotFound => builtin.to_owned(),
                _ => {
                    if matches!(from, Some((_, _, ImportKind::Weak)))
                        && e.kind() == io::ErrorKind::NotFound
                    {
                        return Ok(());
                    }
                    return Err(at(format!("cannot load {path:?}: {e}")));
                }
            },
        };
        let file = parse(path, &source)?;
        self.stack.push(path.to_owned());
        for import in &file.imports {
            self.load(&import.path, Some((path, import.span, import.kind)))?;
        }
        self.stack.pop();
        self.done.insert(path.to_owned());
        self.files.push(file);
        Ok(())
    }
}

// ---- per-file checks --------------------------------------------------------

/// Checks the definitions of one file that do not need other files.
pub(super) fn validate_file(f: &ProtoFile) -> Result<(), IdlError> {
    let mut top = HashMap::new();
    for m in &f.messages {
        unique(&f.name, &mut top, &m.name, m.span)?;
        validate_message(f, m)?;
    }
    for e in &f.enums {
        unique(&f.name, &mut top, &e.name, e.span)?;
        validate_enum(f, e)?;
    }
    // Enum values are scoped like C++ enumerators: as siblings of their enum.
    for e in &f.enums {
        for v in &e.values {
            unique(&f.name, &mut top, &v.name, v.span)?;
        }
    }
    for s in &f.services {
        unique(&f.name, &mut top, &s.name, s.span)?;
        let mut methods = HashMap::new();
        for m in &s.methods {
            unique(&f.name, &mut methods, &m.name, m.span)?;
        }
    }
    Ok(())
}

fn unique<'a>(
    file: &str,
    seen: &mut HashMap<&'a str, Span>,
    name: &'a str,
    span: Span,
) -> Result<(), IdlError> {
    if let Some(prev) = seen.insert(name, span) {
        return Err(IdlError::new(
            file,
            span,
            format!("`{name}` is already defined on line {}", prev.line),
        ));
    }
    Ok(())
}

fn validate_message(f: &ProtoFile, m: &MessageDef) -> Result<(), IdlError> {
    let file = &f.name;
    let mut names = HashMap::new();
    let mut numbers: HashMap<u32, &str> = HashMap::new();
    for field in &m.fields {
        unique(file, &mut names, &field.name, field.span)?;
        if let Some(prev) = numbers.insert(field.number, &field.name) {
            return Err(IdlError::new(
                file,
                field.span,
                format!("field number {} is already used by `{prev}`", field.number),
            ));
        }
        if m.reserved.contains_number(i64::from(field.number)) {
            return Err(IdlError::new(
                file,
                field.span,
                format!(
                    "field `{}` uses the reserved number {}",
                    field.name, field.number
                ),
            ));
        }
        if m.reserved.contains_name(&field.name) {
            return Err(IdlError::new(
                file,
                field.span,
                format!("field name `{}` is reserved", field.name),
            ));
        }
        if m.extension_ranges
            .iter()
            .any(|&(a, b)| a <= field.number && field.number <= b)
        {
            return Err(IdlError::new(
                file,
                field.span,
                format!("field number {} is in an extension range", field.number),
            ));
        }
        validate_field(f, field)?;
    }
    for o in &m.oneofs {
        unique(file, &mut names, &o.name, o.span)?;
    }
    for nested in &m.messages {
        unique(file, &mut names, &nested.name, nested.span)?;
        validate_message(f, nested)?;
    }
    for e in &m.enums {
        unique(file, &mut names, &e.name, e.span)?;
        validate_enum(f, e)?;
    }
    // Enum values are scoped like C++ enumerators: as siblings of their enum.
    for e in &m.enums {
        for v in &e.values {
            unique(file, &mut names, &v.name, v.span)?;
        }
    }
    Ok(())
}

fn validate_field(f: &ProtoFile, field: &FieldDef) -> Result<(), IdlError> {
    let file = &f.name;
    let is_map = matches!(field.ty, FieldType::Map(..));
    if f.syntax == Syntax::Proto2 && field.label == Label::None && field.oneof.is_none() && !is_map
    {
        return Err(IdlError::new(
            file,
            field.span,
            format!(
                "proto2 field `{}` needs a label (`optional`, `required` or `repeated`)",
                field.name
            ),
        ));
    }
    if f.syntax == Syntax::Proto3 && field.options.iter().any(|o| o.name == "default") {
        return Err(IdlError::new(
            file,
            field.span,
            "default values are not allowed in proto3",
        ));
    }
    if let Some(o) = field.options.iter().find(|o| o.name == "packed") {
        if !matches!(o.value, Constant::Bool(_)) {
            return Err(IdlError::new(
                file,
                o.span,
                format!("field `{}`: `packed` takes `true` or `false`", field.name),
            ));
        }
    }
    if field.packed().is_some() && field.label != Label::Repeated {
        return Err(IdlError::new(
            file,
            field.span,
            format!(
                "field `{}`: `packed` applies only to repeated fields",
                field.name
            ),
        ));
    }
    if let FieldType::Scalar(s @ (ScalarType::String | ScalarType::Bytes)) = field.ty {
        if field.packed() == Some(true) {
            return Err(IdlError::new(
                file,
                field.span,
                format!(
                    "field `{}`: a repeated {} cannot be packed",
                    field.name,
                    s.keyword()
                ),
            ));
        }
    }
    Ok(())
}

fn validate_enum(f: &ProtoFile, e: &EnumDef) -> Result<(), IdlError> {
    let file = &f.name;
    let Some(first) = e.values.first() else {
        return Err(IdlError::new(
            file,
            e.span,
            format!("enum `{}` has no values", e.name),
        ));
    };
    if f.syntax == Syntax::Proto3 && first.number != 0 {
        return Err(IdlError::new(
            file,
            first.span,
            format!("the first value of proto3 enum `{}` must be 0", e.name),
        ));
    }
    let mut names = HashMap::new();
    let mut numbers: HashMap<i32, &str> = HashMap::new();
    let mut aliased = false;
    for v in &e.values {
        unique(file, &mut names, &v.name, v.span)?;
        if let Some(prev) = numbers.insert(v.number, &v.name) {
            aliased = true;
            if !e.allow_alias() {
                return Err(IdlError::new(
                    file,
                    v.span,
                    format!(
                        "`{}` reuses the number {} of `{prev}`; set `option allow_alias = true;` to allow aliases",
                        v.name, v.number
                    ),
                ));
            }
        }
        if e.reserved.contains_number(i64::from(v.number)) {
            return Err(IdlError::new(
                file,
                v.span,
                format!(
                    "enum value `{}` uses the reserved number {}",
                    v.name, v.number
                ),
            ));
        }
        if e.reserved.contains_name(&v.name) {
            return Err(IdlError::new(
                file,
                v.span,
                format!("enum value name `{}` is reserved", v.name),
            ));
        }
    }
    if e.allow_alias() && !aliased {
        return Err(IdlError::new(
            file,
            e.span,
            format!(
                "enum `{}` sets `option allow_alias = true;` but has no aliases",
                e.name
            ),
        ));
    }
    Ok(())
}
