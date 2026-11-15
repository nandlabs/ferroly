//! The recursive-descent `.proto` parser.

use super::lexer::{tokenize, Tok, Token};
use super::{
    Constant, EnumDef, EnumValueDef, ExtendDef, FieldDef, FieldType, IdlError, Import, ImportKind,
    Label, MessageDef, MethodDef, NamedType, OneofDef, OptionEntry, ProtoFile, Reserved,
    ScalarType, ServiceDef, Span, Syntax,
};

/// The deepest nesting of messages (and of `{ … }` option aggregates) the
/// parser accepts; it recurses once per level.
const MAX_NESTING: usize = 64;

/// Parses one `.proto` file. Type references are left unresolved (use
/// [`FileSet::load`](super::FileSet::load) to resolve them across imports),
/// but each definition is checked on its own: field numbers, reserved
/// numbers and names, duplicate names, enum rules and proto3 restrictions.
///
/// `name` is used for error messages and as [`ProtoFile::name`].
///
/// ```
/// use ferroly::proto::idl::parse;
///
/// let err = parse("bad.proto", "syntax = \"proto3\";\nmessage M {\n  int32 a = 0;\n}").unwrap_err();
/// assert_eq!(err.to_string(), "bad.proto:3:13: field number 0 is not allowed; field numbers start at 1");
/// ```
pub fn parse(name: &str, source: &str) -> Result<ProtoFile, IdlError> {
    let tokens = tokenize(name, source)?;
    let mut p = Parser {
        file: name,
        toks: tokens,
        pos: 0,
        syntax: Syntax::Proto2,
        depth: 0,
    };
    let file = p.file()?;
    super::resolve::validate_file(&file)?;
    Ok(file)
}

struct Parser<'a> {
    file: &'a str,
    toks: Vec<Token>,
    pos: usize,
    syntax: Syntax,
    depth: usize,
}

impl Parser<'_> {
    // ---- token helpers ------------------------------------------------------

    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        let i = (self.pos + n).min(self.toks.len() - 1);
        &self.toks[i].tok
    }

    fn span(&self) -> Span {
        self.toks[self.pos].span
    }

    fn doc(&self) -> String {
        self.toks[self.pos].doc.clone()
    }

    fn next(&mut self) -> Token {
        let t = self.toks[self.pos].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn err<T>(&self, span: Span, msg: impl Into<String>) -> Result<T, IdlError> {
        Err(IdlError::new(self.file, span, msg))
    }

    fn describe(tok: &Tok) -> String {
        match tok {
            Tok::Ident(s) => format!("`{s}`"),
            Tok::Int(i) => format!("number {i}"),
            Tok::Float(f) => format!("number {f}"),
            Tok::Str(s) => format!("string {s:?}"),
            Tok::Sym(c) => format!("`{c}`"),
            Tok::Eof => "end of file".to_owned(),
        }
    }

    fn unexpected<T>(&self, wanted: &str) -> Result<T, IdlError> {
        self.err(
            self.span(),
            format!("expected {wanted}, found {}", Self::describe(self.peek())),
        )
    }

    fn is_sym(&self, c: char) -> bool {
        *self.peek() == Tok::Sym(c)
    }

    fn is_word(&self, w: &str) -> bool {
        matches!(self.peek(), Tok::Ident(s) if s == w)
    }

    fn eat_sym(&mut self, c: char) -> bool {
        if self.is_sym(c) {
            self.next();
            true
        } else {
            false
        }
    }

    fn eat_word(&mut self, w: &str) -> bool {
        if self.is_word(w) {
            self.next();
            true
        } else {
            false
        }
    }

    fn expect_sym(&mut self, c: char) -> Result<(), IdlError> {
        if self.eat_sym(c) {
            Ok(())
        } else {
            self.unexpected(&format!("`{c}`"))
        }
    }

    fn expect_word(&mut self, w: &str) -> Result<(), IdlError> {
        if self.eat_word(w) {
            Ok(())
        } else {
            self.unexpected(&format!("`{w}`"))
        }
    }

    fn ident(&mut self, what: &str) -> Result<String, IdlError> {
        match self.peek().clone() {
            Tok::Ident(s) => {
                self.next();
                Ok(s)
            }
            _ => self.unexpected(what),
        }
    }

    /// `a.b.c` (optionally with a leading dot).
    fn full_ident(&mut self, what: &str) -> Result<String, IdlError> {
        let mut out = String::new();
        if self.eat_sym('.') {
            out.push('.');
        }
        out.push_str(&self.ident(what)?);
        while self.is_sym('.') {
            self.next();
            out.push('.');
            out.push_str(&self.ident("an identifier after `.`")?);
        }
        Ok(out)
    }

    fn string(&mut self, what: &str) -> Result<String, IdlError> {
        match self.peek().clone() {
            Tok::Str(s) => {
                self.next();
                Ok(s)
            }
            _ => self.unexpected(what),
        }
    }

    fn int(&mut self, what: &str) -> Result<(u64, Span), IdlError> {
        let span = self.span();
        match *self.peek() {
            Tok::Int(i) => {
                self.next();
                Ok((i, span))
            }
            _ => self.unexpected(what),
        }
    }

    fn enter(&mut self) -> Result<(), IdlError> {
        self.depth += 1;
        if self.depth > MAX_NESTING {
            return self.err(
                self.span(),
                format!("nesting deeper than {MAX_NESTING} levels"),
            );
        }
        Ok(())
    }

    // ---- file ---------------------------------------------------------------

    fn file(&mut self) -> Result<ProtoFile, IdlError> {
        let mut f = ProtoFile {
            name: self.file.to_owned(),
            syntax: Syntax::Proto2,
            package: String::new(),
            imports: Vec::new(),
            options: Vec::new(),
            messages: Vec::new(),
            enums: Vec::new(),
            services: Vec::new(),
            extensions: Vec::new(),
        };
        if self.is_word("syntax") {
            self.next();
            self.expect_sym('=')?;
            let span = self.span();
            let s = self.string("a syntax string")?;
            f.syntax = match s.as_str() {
                "proto3" => Syntax::Proto3,
                "proto2" => Syntax::Proto2,
                other => {
                    return self.err(
                        span,
                        format!("unknown syntax {other:?}; expected \"proto2\" or \"proto3\""),
                    )
                }
            };
            self.expect_sym(';')?;
        } else if self.is_word("edition") {
            return self.err(
                self.span(),
                "editions are not supported; use syntax = \"proto3\" or \"proto2\"",
            );
        }
        self.syntax = f.syntax;
        let mut package_seen = false;
        loop {
            let span = self.span();
            match self.peek().clone() {
                Tok::Eof => break,
                Tok::Sym(';') => {
                    self.next();
                }
                Tok::Ident(w) => match w.as_str() {
                    "import" => {
                        self.next();
                        let kind = if self.eat_word("public") {
                            ImportKind::Public
                        } else if self.eat_word("weak") {
                            ImportKind::Weak
                        } else {
                            ImportKind::Normal
                        };
                        let path = self.string("an import path string")?;
                        self.expect_sym(';')?;
                        f.imports.push(Import { path, kind, span });
                    }
                    "package" => {
                        self.next();
                        if package_seen {
                            return self.err(span, "a file has at most one package statement");
                        }
                        package_seen = true;
                        let p = self.full_ident("a package name")?;
                        if p.starts_with('.') {
                            return self.err(span, "a package name cannot start with `.`");
                        }
                        f.package = p;
                        self.expect_sym(';')?;
                    }
                    "option" => {
                        self.next();
                        f.options.push(self.option_body(span)?);
                        self.expect_sym(';')?;
                    }
                    "message" => {
                        let prefix = f.package.clone();
                        f.messages.push(self.message(&prefix)?);
                    }
                    "enum" => {
                        let prefix = f.package.clone();
                        f.enums.push(self.enumeration(&prefix)?);
                    }
                    "service" => {
                        let prefix = f.package.clone();
                        f.services.push(self.service(&prefix)?);
                    }
                    "extend" => f.extensions.push(self.extend()?),
                    "syntax" | "edition" => {
                        return self.err(span, format!("`{w}` must be the first statement of the file"))
                    }
                    _ => return self.unexpected("a top-level declaration (`message`, `enum`, `service`, `import`, `package`, `option`)"),
                },
                _ => return self.unexpected("a top-level declaration"),
            }
        }
        Ok(f)
    }

    // ---- options --------------------------------------------------------------

    /// `name = constant` (after `option`, or inside `[...]`).
    fn option_body(&mut self, span: Span) -> Result<OptionEntry, IdlError> {
        let name = self.option_name()?;
        self.expect_sym('=')?;
        let value = self.constant()?;
        Ok(OptionEntry { name, value, span })
    }

    fn option_name(&mut self) -> Result<String, IdlError> {
        let mut out = String::new();
        loop {
            if self.eat_sym('(') {
                out.push('(');
                out.push_str(&self.full_ident("an extension name")?);
                self.expect_sym(')')?;
                out.push(')');
            } else {
                out.push_str(&self.ident("an option name")?);
            }
            if self.eat_sym('.') {
                out.push('.');
            } else {
                break;
            }
        }
        Ok(out)
    }

    fn constant(&mut self) -> Result<Constant, IdlError> {
        let span = self.span();
        match self.peek().clone() {
            Tok::Str(s) => {
                self.next();
                Ok(Constant::Str(s))
            }
            Tok::Int(i) => {
                self.next();
                Ok(Constant::Int(i128::from(i)))
            }
            Tok::Float(x) => {
                self.next();
                Ok(Constant::Float(x))
            }
            Tok::Sym(c @ ('-' | '+')) => {
                self.next();
                let neg = c == '-';
                match self.peek().clone() {
                    Tok::Int(i) => {
                        self.next();
                        let v = i128::from(i);
                        Ok(Constant::Int(if neg { -v } else { v }))
                    }
                    Tok::Float(x) => {
                        self.next();
                        Ok(Constant::Float(if neg { -x } else { x }))
                    }
                    Tok::Ident(w) if w == "inf" || w == "nan" => {
                        self.next();
                        Ok(Constant::Ident(format!("{c}{w}")))
                    }
                    _ => self.err(span, "expected a number after the sign"),
                }
            }
            Tok::Ident(w) if w == "true" || w == "false" => {
                self.next();
                Ok(Constant::Bool(w == "true"))
            }
            Tok::Ident(_) => Ok(Constant::Ident(self.full_ident("a constant")?)),
            Tok::Sym('{') => self.aggregate(),
            _ => self.unexpected("an option value"),
        }
    }

    /// Skips a balanced `{ … }` text-format aggregate, keeping it as text.
    fn aggregate(&mut self) -> Result<Constant, IdlError> {
        let open = self.span();
        self.next();
        let mut depth = 1usize;
        let mut text = String::from("{");
        loop {
            let t = self.next();
            match &t.tok {
                Tok::Eof => return self.err(open, "unterminated `{` in option value"),
                Tok::Sym('{') => {
                    depth += 1;
                    if depth > MAX_NESTING {
                        return self
                            .err(t.span, format!("nesting deeper than {MAX_NESTING} levels"));
                    }
                }
                Tok::Sym('}') => depth -= 1,
                _ => {}
            }
            text.push(' ');
            match &t.tok {
                Tok::Ident(s) => text.push_str(s),
                Tok::Int(i) => text.push_str(&i.to_string()),
                Tok::Float(x) => text.push_str(&x.to_string()),
                Tok::Str(s) => text.push_str(&format!("{s:?}")),
                Tok::Sym(c) => text.push(*c),
                Tok::Eof => {}
            }
            if depth == 0 {
                return Ok(Constant::Aggregate(text));
            }
        }
    }

    /// `[a = 1, b = 2]`, if present.
    fn field_options(&mut self) -> Result<Vec<OptionEntry>, IdlError> {
        let mut out = Vec::new();
        if !self.eat_sym('[') {
            return Ok(out);
        }
        loop {
            let span = self.span();
            out.push(self.option_body(span)?);
            if self.eat_sym(']') {
                return Ok(out);
            }
            self.expect_sym(',')?;
        }
    }

    // ---- messages ---------------------------------------------------------------

    fn message(&mut self, prefix: &str) -> Result<MessageDef, IdlError> {
        let doc = self.doc();
        let span = self.span();
        self.expect_word("message")?;
        let name = self.ident("a message name")?;
        let full_name = join(prefix, &name);
        self.enter()?;
        self.expect_sym('{')?;
        let mut m = MessageDef {
            name,
            full_name,
            fields: Vec::new(),
            oneofs: Vec::new(),
            messages: Vec::new(),
            enums: Vec::new(),
            reserved: Reserved::default(),
            extension_ranges: Vec::new(),
            extensions: Vec::new(),
            options: Vec::new(),
            doc,
            span,
        };
        loop {
            let span = self.span();
            match self.peek().clone() {
                Tok::Sym('}') => {
                    self.next();
                    break;
                }
                Tok::Sym(';') => {
                    self.next();
                }
                Tok::Eof => return self.err(m.span, format!("message `{}` is not closed", m.name)),
                Tok::Ident(w) => match w.as_str() {
                    "message" => {
                        let full = m.full_name.clone();
                        m.messages.push(self.message(&full)?);
                    }
                    "enum" => {
                        let full = m.full_name.clone();
                        m.enums.push(self.enumeration(&full)?);
                    }
                    "option" => {
                        self.next();
                        m.options.push(self.option_body(span)?);
                        self.expect_sym(';')?;
                    }
                    "oneof" => self.oneof(&mut m)?,
                    "reserved" => {
                        self.next();
                        self.reserved(&mut m.reserved, false)?;
                    }
                    "extensions" => {
                        self.next();
                        let mut r = Reserved::default();
                        self.ranges(&mut r, false)?;
                        self.field_options()?;
                        self.expect_sym(';')?;
                        m.extension_ranges
                            .extend(r.ranges.into_iter().map(|(a, b)| (a as u32, b as u32)));
                    }
                    "extend" => m.extensions.push(self.extend()?),
                    "map" if *self.peek_at(1) == Tok::Sym('<') => {
                        let f = self.map_field()?;
                        m.fields.push(f);
                    }
                    _ => {
                        let f = self.field(true)?;
                        m.fields.push(f);
                    }
                },
                // A field whose type is fully qualified: `.pkg.Type name = 1;`.
                Tok::Sym('.') => {
                    let f = self.field(true)?;
                    m.fields.push(f);
                }
                _ => return self.unexpected("a field or declaration"),
            }
        }
        self.depth -= 1;
        Ok(m)
    }

    fn label(&mut self, allow: bool) -> Result<Label, IdlError> {
        let span = self.span();
        let label = if self.is_word("optional") && !matches!(self.peek_at(1), Tok::Sym('=' | '.')) {
            Label::Optional
        } else if self.is_word("required") && !matches!(self.peek_at(1), Tok::Sym('=' | '.')) {
            Label::Required
        } else if self.is_word("repeated") && !matches!(self.peek_at(1), Tok::Sym('=' | '.')) {
            Label::Repeated
        } else {
            return Ok(Label::None);
        };
        if !allow {
            return self.err(span, "a oneof member takes no label");
        }
        if label == Label::Required && self.syntax == Syntax::Proto3 {
            return self.err(span, "`required` is not allowed in proto3");
        }
        self.next();
        Ok(label)
    }

    fn field_type(&mut self) -> Result<FieldType, IdlError> {
        if let Tok::Ident(w) = self.peek() {
            if let Some(s) = ScalarType::from_keyword(w) {
                if !matches!(self.peek_at(1), Tok::Sym('.')) {
                    self.next();
                    return Ok(FieldType::Scalar(s));
                }
            }
        }
        Ok(FieldType::Named(NamedType::new(
            self.full_ident("a field type")?,
        )))
    }

    /// A field, after the caller has ruled out every other declaration.
    fn field(&mut self, labels: bool) -> Result<FieldDef, IdlError> {
        let doc = self.doc();
        let span = self.span();
        let label = self.label(labels)?;
        if self.is_word("group") {
            return self.err(self.span(), "proto2 groups are not supported");
        }
        let ty = self.field_type()?;
        let name = self.ident("a field name")?;
        self.expect_sym('=')?;
        let number = self.field_number()?;
        let options = self.field_options()?;
        self.expect_sym(';')?;
        Ok(FieldDef {
            name,
            number,
            label,
            ty,
            oneof: None,
            options,
            doc,
            span,
        })
    }

    fn field_number(&mut self) -> Result<u32, IdlError> {
        let span = self.span();
        if self.is_sym('-') {
            return self.err(span, "a field number must be positive");
        }
        let (n, span) = self.int("a field number")?;
        if n == 0 {
            return self.err(
                span,
                "field number 0 is not allowed; field numbers start at 1",
            );
        }
        if n > u64::from(super::super::wire::MAX_TAG) {
            return self.err(
                span,
                format!("field number {n} is above the maximum 536870911"),
            );
        }
        if (19_000..=19_999).contains(&n) {
            return self.err(
                span,
                format!("field number {n} is in the range 19000-19999 reserved for the protobuf implementation"),
            );
        }
        Ok(n as u32)
    }

    fn map_field(&mut self) -> Result<FieldDef, IdlError> {
        let doc = self.doc();
        let span = self.span();
        self.expect_word("map")?;
        self.expect_sym('<')?;
        let key_span = self.span();
        let key = match self.field_type()? {
            FieldType::Scalar(s) if s.valid_map_key() => s,
            _ => {
                return self.err(
                    key_span,
                    "a map key must be an integer, bool or string type",
                )
            }
        };
        self.expect_sym(',')?;
        let value_span = self.span();
        let value = self.field_type()?;
        if self.is_word("map") {
            return self.err(value_span, "a map value cannot be another map");
        }
        self.expect_sym('>')?;
        let name = self.ident("a field name")?;
        self.expect_sym('=')?;
        let number = self.field_number()?;
        let options = self.field_options()?;
        self.expect_sym(';')?;
        Ok(FieldDef {
            name,
            number,
            label: Label::None,
            ty: FieldType::Map(key, Box::new(value)),
            oneof: None,
            options,
            doc,
            span,
        })
    }

    fn oneof(&mut self, m: &mut MessageDef) -> Result<(), IdlError> {
        let doc = self.doc();
        let span = self.span();
        self.expect_word("oneof")?;
        let name = self.ident("a oneof name")?;
        self.expect_sym('{')?;
        let index = m.oneofs.len();
        let mut options = Vec::new();
        let mut members = 0;
        loop {
            let at = self.span();
            if self.eat_sym('}') {
                break;
            }
            if self.eat_sym(';') {
                continue;
            }
            if *self.peek() == Tok::Eof {
                return self.err(span, format!("oneof `{name}` is not closed"));
            }
            if self.is_word("option") && !matches!(self.peek_at(1), Tok::Sym('.')) {
                self.next();
                options.push(self.option_body(at)?);
                self.expect_sym(';')?;
                continue;
            }
            if self.is_word("map") && *self.peek_at(1) == Tok::Sym('<') {
                return self.err(at, "a map field cannot be a oneof member");
            }
            let mut f = self.field(false)?;
            f.oneof = Some(index);
            m.fields.push(f);
            members += 1;
        }
        if members == 0 {
            return self.err(span, format!("oneof `{name}` has no members"));
        }
        m.oneofs.push(OneofDef {
            name,
            options,
            doc,
            span,
        });
        Ok(())
    }

    /// `reserved 1, 2 to 5, 9 to max;` or `reserved "a", "b";`.
    fn reserved(&mut self, r: &mut Reserved, enum_values: bool) -> Result<(), IdlError> {
        match self.peek() {
            Tok::Str(_) => loop {
                let s = self.string("a reserved name")?;
                r.names.push(s);
                if !self.eat_sym(',') {
                    break;
                }
            },
            Tok::Ident(_) => loop {
                // Editions spell reserved names as bare identifiers.
                let s = self.ident("a reserved name")?;
                r.names.push(s);
                if !self.eat_sym(',') {
                    break;
                }
            },
            _ => self.ranges(r, enum_values)?,
        }
        self.expect_sym(';')
    }

    fn signed(&mut self, enum_values: bool) -> Result<(i64, Span), IdlError> {
        let span = self.span();
        let neg = if self.is_sym('-') {
            if !enum_values {
                return self.err(span, "a field number must be positive");
            }
            self.next();
            true
        } else {
            false
        };
        let (n, _) = self.int("a number")?;
        let v =
            i64::try_from(n).map_err(|_| IdlError::new(self.file, span, "number out of range"))?;
        Ok((if neg { -v } else { v }, span))
    }

    fn ranges(&mut self, r: &mut Reserved, enum_values: bool) -> Result<(), IdlError> {
        let max = if enum_values {
            i64::from(i32::MAX)
        } else {
            i64::from(super::super::wire::MAX_TAG)
        };
        loop {
            let (start, span) = self.signed(enum_values)?;
            let end = if self.eat_word("to") {
                if self.eat_word("max") {
                    max
                } else {
                    self.signed(enum_values)?.0
                }
            } else {
                start
            };
            if end < start {
                return self.err(span, format!("range {start} to {end} is empty"));
            }
            r.ranges.push((start, end));
            if !self.eat_sym(',') {
                return Ok(());
            }
        }
    }

    // ---- enums --------------------------------------------------------------------

    fn enumeration(&mut self, prefix: &str) -> Result<EnumDef, IdlError> {
        let doc = self.doc();
        let span = self.span();
        self.expect_word("enum")?;
        let name = self.ident("an enum name")?;
        self.expect_sym('{')?;
        let mut e = EnumDef {
            full_name: join(prefix, &name),
            name,
            values: Vec::new(),
            reserved: Reserved::default(),
            options: Vec::new(),
            doc,
            span,
        };
        loop {
            let at = self.span();
            match self.peek().clone() {
                Tok::Sym('}') => {
                    self.next();
                    break;
                }
                Tok::Sym(';') => {
                    self.next();
                }
                Tok::Eof => return self.err(e.span, format!("enum `{}` is not closed", e.name)),
                Tok::Ident(w) if w == "option" && !matches!(self.peek_at(1), Tok::Sym('=')) => {
                    self.next();
                    e.options.push(self.option_body(at)?);
                    self.expect_sym(';')?;
                }
                Tok::Ident(w) if w == "reserved" && !matches!(self.peek_at(1), Tok::Sym('=')) => {
                    self.next();
                    self.reserved(&mut e.reserved, true)?;
                }
                Tok::Ident(_) => {
                    let doc = self.doc();
                    let name = self.ident("an enum value name")?;
                    self.expect_sym('=')?;
                    let (n, nspan) = self.signed(true)?;
                    let number = i32::try_from(n).map_err(|_| {
                        IdlError::new(
                            self.file,
                            nspan,
                            format!("enum value {n} does not fit in int32"),
                        )
                    })?;
                    let options = self.field_options()?;
                    self.expect_sym(';')?;
                    e.values.push(EnumValueDef {
                        name,
                        number,
                        options,
                        doc,
                        span: at,
                    });
                }
                _ => return self.unexpected("an enum value"),
            }
        }
        Ok(e)
    }

    // ---- services ---------------------------------------------------------------

    fn service(&mut self, prefix: &str) -> Result<ServiceDef, IdlError> {
        let doc = self.doc();
        let span = self.span();
        self.expect_word("service")?;
        let name = self.ident("a service name")?;
        self.expect_sym('{')?;
        let mut s = ServiceDef {
            full_name: join(prefix, &name),
            name,
            methods: Vec::new(),
            options: Vec::new(),
            doc,
            span,
        };
        loop {
            let at = self.span();
            match self.peek().clone() {
                Tok::Sym('}') => {
                    self.next();
                    break;
                }
                Tok::Sym(';') => {
                    self.next();
                }
                Tok::Eof => return self.err(s.span, format!("service `{}` is not closed", s.name)),
                Tok::Ident(w) if w == "option" => {
                    self.next();
                    s.options.push(self.option_body(at)?);
                    self.expect_sym(';')?;
                }
                Tok::Ident(w) if w == "rpc" => s.methods.push(self.rpc()?),
                _ => return self.unexpected("`rpc` or `option`"),
            }
        }
        Ok(s)
    }

    fn rpc(&mut self) -> Result<MethodDef, IdlError> {
        let doc = self.doc();
        let span = self.span();
        self.expect_word("rpc")?;
        let name = self.ident("a method name")?;
        let (client_streaming, input) = self.rpc_type()?;
        self.expect_word("returns")?;
        let (server_streaming, output) = self.rpc_type()?;
        let mut options = Vec::new();
        if self.eat_sym('{') {
            loop {
                let at = self.span();
                if self.eat_sym('}') {
                    break;
                }
                if self.eat_sym(';') {
                    continue;
                }
                if self.eat_word("option") {
                    options.push(self.option_body(at)?);
                    self.expect_sym(';')?;
                } else {
                    return self.unexpected("`option` or `}`");
                }
            }
            // A `;` after the body is allowed.
            self.eat_sym(';');
        } else {
            self.expect_sym(';')?;
        }
        Ok(MethodDef {
            name,
            input,
            output,
            client_streaming,
            server_streaming,
            options,
            doc,
            span,
        })
    }

    /// `( [stream] Type )`.
    fn rpc_type(&mut self) -> Result<(bool, NamedType), IdlError> {
        self.expect_sym('(')?;
        // `stream` is a modifier unless it is the type name itself (`(stream)`)
        // or its first component (`(stream.Msg)`, with no space before the
        // dot — `(stream .pkg.Msg)` streams a fully qualified type).
        let streaming = self.is_word("stream")
            && match self.peek_at(1) {
                Tok::Sym(')') => false,
                Tok::Sym('.') => {
                    let here = self.span();
                    let dot = self.toks[(self.pos + 1).min(self.toks.len() - 1)].span;
                    !(dot.line == here.line && dot.column == here.column + 6)
                }
                _ => true,
            };
        if streaming {
            self.next();
        }
        let span = self.span();
        let name = self.full_ident("a message type")?;
        if ScalarType::from_keyword(&name).is_some() {
            return self.err(
                span,
                format!("an rpc takes a message type, not the scalar `{name}`"),
            );
        }
        self.expect_sym(')')?;
        Ok((streaming, NamedType::new(name)))
    }

    fn extend(&mut self) -> Result<ExtendDef, IdlError> {
        let span = self.span();
        self.expect_word("extend")?;
        let extendee = self.full_ident("the extended message")?;
        self.expect_sym('{')?;
        let mut fields = Vec::new();
        loop {
            if self.eat_sym('}') {
                break;
            }
            if self.eat_sym(';') {
                continue;
            }
            if *self.peek() == Tok::Eof {
                return self.err(span, "extend block is not closed");
            }
            fields.push(self.field(true)?);
        }
        Ok(ExtendDef {
            extendee,
            fields,
            span,
        })
    }
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}.{name}")
    }
}
