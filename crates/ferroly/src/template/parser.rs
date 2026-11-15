//! Recursive-descent parser with Jinja2's operator precedence. Filter and
//! test names are resolved here, function calls are checked against the known
//! globals and the template's own names, and nesting depth is bounded, so a
//! template that parses has no unknown names and a bounded evaluation depth.

use std::collections::HashSet;
use std::sync::Arc;

use super::ast::{Args, BinOp, CmpOp, Const, Expr, ExprKind, ForLoop, MacroDef, Stmt, Target};
use super::builtins::{Filter, Test};
use super::error::{Fail, TemplateErrorKind, R};
use super::lexer::{Tok, Token};

/// Statements Jinja2 has that this engine rejects with `Unsupported`.
const UNSUPPORTED: &[&str] = &[
    "autoescape",
    "block",
    "call",
    "do",
    "extends",
    "filter",
    "from",
    "import",
    "include",
    "pluralize",
    "trans",
    "with",
];

/// Words that end an open block, reported as "unexpected" when nothing is
/// open.
const END_WORDS: &[&str] = &[
    "elif",
    "else",
    "endblock",
    "endcall",
    "endfilter",
    "endfor",
    "endif",
    "endmacro",
    "endraw",
    "endset",
    "endwith",
];

/// Reserved words that cannot start an expression.
const RESERVED: &[&str] = &["and", "or", "not", "in", "is", "if", "else"];

pub(crate) struct Parser<'a> {
    toks: Vec<Token>,
    i: usize,
    max_depth: usize,
    block_depth: usize,
    recursion: usize,
    loop_depth: usize,
    calls: Vec<(Arc<str>, usize)>,
    defined: HashSet<Arc<str>>,
    is_global: &'a dyn Fn(&str) -> bool,
}

impl<'a> Parser<'a> {
    pub(crate) fn new(
        toks: Vec<Token>,
        max_depth: usize,
        is_global: &'a dyn Fn(&str) -> bool,
    ) -> Self {
        Parser {
            toks,
            i: 0,
            max_depth,
            block_depth: 0,
            recursion: 0,
            loop_depth: 0,
            calls: Vec::new(),
            defined: HashSet::new(),
            is_global,
        }
    }

    pub(crate) fn parse(mut self) -> R<Vec<Stmt>> {
        let (body, _) = self.body(&[], ("", 0))?;
        for (name, pos) in &self.calls {
            if !(self.is_global)(name) && !self.defined.contains(name) {
                return Err(Fail::new(
                    TemplateErrorKind::UnknownFunction(name.to_string()),
                    *pos,
                ));
            }
        }
        Ok(body)
    }

    // ---- token helpers -------------------------------------------------

    fn cur(&self) -> &Token {
        // The lexer always ends the stream with `Eof`; never step past it.
        let last = self.toks.len().saturating_sub(1);
        &self.toks[self.i.min(last)]
    }

    fn peek(&self, n: usize) -> &Tok {
        let last = self.toks.len().saturating_sub(1);
        &self.toks[(self.i + n).min(last)].tok
    }

    fn pos(&self) -> usize {
        self.cur().pos
    }

    fn advance(&mut self) -> Token {
        let t = self.cur().clone();
        if self.i + 1 < self.toks.len() {
            self.i += 1;
        }
        t
    }

    fn is_op(&self, op: &str) -> bool {
        matches!(&self.cur().tok, Tok::Op(o) if *o == op)
    }

    fn eat_op(&mut self, op: &str) -> bool {
        if self.is_op(op) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn is_name(&self, name: &str) -> bool {
        matches!(&self.cur().tok, Tok::Name(n) if n == name)
    }

    fn eat_name(&mut self, name: &str) -> bool {
        if self.is_name(name) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_op(&mut self, op: &str) -> R<()> {
        if self.eat_op(op) {
            Ok(())
        } else {
            Err(self.unexpected(&format!("'{op}'")))
        }
    }

    fn expect_name(&mut self, what: &str) -> R<(Arc<str>, usize)> {
        let pos = self.pos();
        match &self.cur().tok {
            Tok::Name(n) => {
                let n: Arc<str> = Arc::from(n.as_str());
                self.advance();
                Ok((n, pos))
            }
            _ => Err(self.unexpected(what)),
        }
    }

    fn expect_block_end(&mut self) -> R<()> {
        if matches!(self.cur().tok, Tok::BlockEnd) {
            self.advance();
            Ok(())
        } else {
            Err(self.unexpected("'%}'"))
        }
    }

    fn unexpected(&self, expected: &str) -> Fail {
        let got = match &self.cur().tok {
            Tok::Text(_) => "template text".to_string(),
            Tok::VarBegin => "'{{'".to_string(),
            Tok::VarEnd => "'}}'".to_string(),
            Tok::BlockBegin => "'{%'".to_string(),
            Tok::BlockEnd => "'%}'".to_string(),
            Tok::Name(n) => format!("'{n}'"),
            Tok::Str(_) => "a string".to_string(),
            Tok::Int(_) | Tok::Float(_) => "a number".to_string(),
            Tok::Op(o) => format!("'{o}'"),
            Tok::Eof => "end of template".to_string(),
        };
        Fail::syntax(format!("expected {expected}, found {got}"), self.pos())
    }

    fn enter_block(&mut self, pos: usize) -> R<()> {
        self.block_depth += 1;
        if self.block_depth > self.max_depth {
            return Err(Fail::limit("max_depth", pos));
        }
        Ok(())
    }

    fn leave_block(&mut self) {
        self.block_depth -= 1;
    }

    fn enter_rec(&mut self) -> R<()> {
        self.recursion += 1;
        if self.recursion > self.max_depth {
            return Err(Fail::limit("max_depth", self.pos()));
        }
        Ok(())
    }

    fn leave_rec(&mut self) {
        self.recursion -= 1;
    }

    fn mk(&self, kind: ExprKind, pos: usize) -> R<Expr> {
        let depth = kind.child_depth() + 1;
        if depth + self.block_depth > self.max_depth {
            return Err(Fail::limit("max_depth", pos));
        }
        Ok(Expr { kind, pos, depth })
    }

    // ---- statements ----------------------------------------------------

    /// Parses statements until one of `ends` (a statement name) or the end
    /// of the template. Returns the body and the end word found; the rest of
    /// that end tag is left for the caller.
    fn body(
        &mut self,
        ends: &[&'static str],
        opener: (&str, usize),
    ) -> R<(Vec<Stmt>, &'static str)> {
        let mut out = Vec::new();
        loop {
            let t = self.advance();
            match t.tok {
                Tok::Text(s) => out.push(Stmt::Text(s, t.pos)),
                Tok::VarBegin => {
                    if matches!(self.cur().tok, Tok::VarEnd) {
                        return Err(Fail::syntax("expected an expression in '{{ }}'", t.pos));
                    }
                    let e = self.tuple(true, &[])?;
                    if !matches!(self.cur().tok, Tok::VarEnd) {
                        return Err(self.unexpected("'}}'"));
                    }
                    self.advance();
                    out.push(Stmt::Output(e));
                }
                Tok::BlockBegin => {
                    let (name, npos) = self.expect_name("a statement name")?;
                    if let Some(end) = ends.iter().find(|e| **e == &*name) {
                        return Ok((out, *end));
                    }
                    out.push(self.statement(&name, npos)?);
                }
                Tok::Eof => {
                    if ends.is_empty() {
                        return Ok((out, ""));
                    }
                    let want = ends.last().copied().unwrap_or("end");
                    return Err(Fail::syntax(
                        format!(
                            "unexpected end of template: '{}' is never closed (expected '{want}')",
                            opener.0
                        ),
                        opener.1,
                    ));
                }
                _ => return Err(Fail::syntax("unexpected token outside a tag", t.pos)),
            }
        }
    }

    fn statement(&mut self, name: &str, pos: usize) -> R<Stmt> {
        match name {
            "if" => self.if_stmt(pos),
            "for" => self.for_stmt(pos),
            "set" => self.set_stmt(pos),
            "macro" => self.macro_stmt(pos),
            "break" | "continue" => {
                if self.loop_depth == 0 {
                    return Err(Fail::syntax(format!("'{name}' outside of a loop"), pos));
                }
                self.expect_block_end()?;
                Ok(if name == "break" {
                    Stmt::Break
                } else {
                    Stmt::Continue
                })
            }
            _ if UNSUPPORTED.contains(&name) => Err(Fail::new(
                TemplateErrorKind::Unsupported(format!("the '{name}' statement")),
                pos,
            )),
            _ if END_WORDS.contains(&name) => Err(Fail::syntax(
                format!("unexpected '{name}': no open block it could close"),
                pos,
            )),
            _ => Err(Fail::syntax(format!("unknown statement '{name}'"), pos)),
        }
    }

    fn if_stmt(&mut self, pos: usize) -> R<Stmt> {
        let mut cond = self.tuple(false, &[])?;
        self.expect_block_end()?;
        self.enter_block(pos)?;
        let mut branches = Vec::new();
        let mut otherwise = Vec::new();
        loop {
            let (body, end) = self.body(&["elif", "else", "endif"], ("if", pos))?;
            branches.push((cond, body));
            match end {
                "elif" => {
                    cond = self.tuple(false, &[])?;
                    self.expect_block_end()?;
                }
                "else" => {
                    self.expect_block_end()?;
                    otherwise = self.body(&["endif"], ("if", pos))?.0;
                    self.expect_block_end()?;
                    break;
                }
                _ => {
                    self.expect_block_end()?;
                    break;
                }
            }
        }
        self.leave_block();
        Ok(Stmt::If {
            branches,
            otherwise,
        })
    }

    /// `name`, `a, b` or `(a, b)`.
    fn names_target(&mut self) -> R<Target> {
        let paren = self.eat_op("(");
        let (first, _) = self.expect_name("a variable name")?;
        let mut names = vec![first];
        let mut tuple = paren;
        while self.eat_op(",") {
            tuple = true;
            if !matches!(self.cur().tok, Tok::Name(_)) {
                break;
            }
            names.push(self.expect_name("a variable name")?.0);
        }
        if paren {
            self.expect_op(")")?;
        }
        for n in &names {
            self.defined.insert(n.clone());
        }
        if tuple {
            Ok(Target::Tuple(names))
        } else {
            Ok(Target::Name(names.remove(0)))
        }
    }

    fn for_stmt(&mut self, pos: usize) -> R<Stmt> {
        let target = self.names_target()?;
        if !self.eat_name("in") {
            return Err(self.unexpected("'in'"));
        }
        let iter = self.tuple(false, &["if", "recursive"])?;
        let filter = if self.eat_name("if") {
            Some(self.expression(true)?)
        } else {
            None
        };
        if self.is_name("recursive") {
            return Err(Fail::new(
                TemplateErrorKind::Unsupported("recursive loops".into()),
                self.pos(),
            ));
        }
        self.expect_block_end()?;
        self.enter_block(pos)?;
        self.loop_depth += 1;
        let (body, end) = self.body(&["else", "endfor"], ("for", pos))?;
        self.loop_depth -= 1;
        self.expect_block_end()?;
        let otherwise = if end == "else" {
            let (b, _) = self.body(&["endfor"], ("for", pos))?;
            self.expect_block_end()?;
            b
        } else {
            Vec::new()
        };
        self.leave_block();
        Ok(Stmt::For(Box::new(ForLoop {
            target,
            iter,
            filter,
            body,
            otherwise,
            pos,
        })))
    }

    fn set_stmt(&mut self, pos: usize) -> R<Stmt> {
        let target = if matches!(self.peek(1), Tok::Op(".")) {
            let (ns, _) = self.expect_name("a variable name")?;
            self.advance();
            let (attr, _) = self.expect_name("an attribute name")?;
            Target::Attr(ns, attr)
        } else {
            self.names_target()?
        };
        if self.eat_op("=") {
            let value = self.tuple(true, &[])?;
            self.expect_block_end()?;
            return Ok(Stmt::Set { target, value, pos });
        }
        match (&target, &self.cur().tok) {
            (Target::Name(name), Tok::BlockEnd) => {
                let name = name.clone();
                self.advance();
                self.enter_block(pos)?;
                let (body, _) = self.body(&["endset"], ("set", pos))?;
                self.expect_block_end()?;
                self.leave_block();
                Ok(Stmt::SetBlock { name, body })
            }
            _ => Err(self.unexpected("'='")),
        }
    }

    fn macro_stmt(&mut self, pos: usize) -> R<Stmt> {
        let (name, _) = self.expect_name("a macro name")?;
        self.defined.insert(name.clone());
        self.expect_op("(")?;
        let mut params: Vec<(Arc<str>, Option<Expr>)> = Vec::new();
        let mut seen: HashSet<Arc<str>> = HashSet::new();
        let mut any_default = false;
        while !self.eat_op(")") {
            if !params.is_empty() {
                self.expect_op(",")?;
                if self.eat_op(")") {
                    break;
                }
            }
            let (p, ppos) = self.expect_name("a parameter name")?;
            if !seen.insert(p.clone()) {
                return Err(Fail::syntax(format!("duplicate parameter '{p}'"), ppos));
            }
            let default = if self.eat_op("=") {
                any_default = true;
                Some(self.expression(true)?)
            } else if any_default {
                return Err(Fail::syntax(
                    "a parameter without a default follows one with a default",
                    ppos,
                ));
            } else {
                None
            };
            self.defined.insert(p.clone());
            params.push((p, default));
        }
        self.expect_block_end()?;
        self.enter_block(pos)?;
        let saved_loop = std::mem::replace(&mut self.loop_depth, 0);
        let (body, _) = self.body(&["endmacro"], ("macro", pos))?;
        self.loop_depth = saved_loop;
        if matches!(&self.cur().tok, Tok::Name(n) if **n == *name) {
            self.advance();
        }
        self.expect_block_end()?;
        self.leave_block();
        Ok(Stmt::Macro(Arc::new(MacroDef::new(name, params, body))))
    }

    // ---- expressions ---------------------------------------------------

    fn is_tuple_end(&self, extra: &[&str]) -> bool {
        match &self.cur().tok {
            Tok::VarEnd | Tok::BlockEnd | Tok::Eof => true,
            Tok::Op(")") => true,
            Tok::Name(n) => extra.contains(&n.as_str()),
            _ => false,
        }
    }

    /// An expression, or a comma-separated tuple of them (evaluated as a list).
    fn tuple(&mut self, with_cond: bool, extra_end: &[&str]) -> R<Expr> {
        let pos = self.pos();
        let mut items = Vec::new();
        let mut is_tuple = false;
        loop {
            if !items.is_empty() {
                if !self.eat_op(",") {
                    break;
                }
                is_tuple = true;
                if self.is_tuple_end(extra_end) {
                    break;
                }
            }
            items.push(self.expression(with_cond)?);
        }
        if !is_tuple {
            if let Some(e) = items.pop() {
                return Ok(e);
            }
        }
        self.mk(ExprKind::List(items), pos)
    }

    /// A full expression; `with_cond` also accepts `x if c else y`.
    fn expression(&mut self, with_cond: bool) -> R<Expr> {
        self.enter_rec()?;
        let r = self.expression_inner(with_cond);
        self.leave_rec();
        r
    }

    fn expression_inner(&mut self, with_cond: bool) -> R<Expr> {
        let mut e = self.climb(1)?;
        while with_cond && self.is_name("if") {
            let pos = self.pos();
            self.advance();
            let cond = self.climb(1)?;
            let otherwise = if self.eat_name("else") {
                Some(Box::new(self.expression(true)?))
            } else {
                None
            };
            e = self.mk(
                ExprKind::Cond {
                    cond: Box::new(cond),
                    then: Box::new(e),
                    otherwise,
                },
                pos,
            )?;
        }
        Ok(e)
    }

    /// The binary operator at the cursor, with its precedence level (Jinja2's
    /// order, lowest first): `or` 1, `and` 2, (`not` 3, prefix), comparisons
    /// 4, `+ -` 5, `~` 6, `* / // %` 7, `**` 8.
    fn binop_level(&self) -> Option<u8> {
        Some(match &self.cur().tok {
            Tok::Name(n) if n == "or" => 1,
            Tok::Name(n) if n == "and" => 2,
            Tok::Name(n) if n == "in" => 4,
            Tok::Name(n) if n == "not" && matches!(self.peek(1), Tok::Name(m) if m == "in") => 4,
            Tok::Op("==" | "!=" | "<" | "<=" | ">" | ">=") => 4,
            Tok::Op("+" | "-") => 5,
            Tok::Op("~") => 6,
            Tok::Op("*" | "/" | "//" | "%") => 7,
            Tok::Op("**") => 8,
            _ => return None,
        })
    }

    /// Precedence climbing over the binary operators. Runs of one level are
    /// kept flat (`Or`, `And`, `Compare`, `Concat`, `Chain`), so neither
    /// parsing nor evaluating a long run recurses per operator.
    fn climb(&mut self, min: u8) -> R<Expr> {
        let pos = self.pos();
        let mut left = if min <= 3 && self.is_name("not") {
            self.advance();
            self.enter_rec()?;
            let inner = self.climb(3);
            self.leave_rec();
            self.mk(ExprKind::Not(Box::new(inner?)), pos)?
        } else {
            self.unary(true)?
        };
        while let Some(level) = self.binop_level() {
            if level < min {
                break;
            }
            left = match level {
                1 | 2 => self.bool_run(left, level, pos)?,
                4 => self.compare_run(left, pos)?,
                6 => self.concat_run(left, pos)?,
                _ => self.arith_run(left, level, pos)?,
            };
        }
        Ok(left)
    }

    fn bool_run(&mut self, first: Expr, level: u8, pos: usize) -> R<Expr> {
        let word = if level == 1 { "or" } else { "and" };
        let mut items = vec![first];
        while self.eat_name(word) {
            items.push(self.climb(level + 1)?);
        }
        let kind = if level == 1 {
            ExprKind::Or(items)
        } else {
            ExprKind::And(items)
        };
        self.mk(kind, pos)
    }

    fn compare_run(&mut self, first: Expr, pos: usize) -> R<Expr> {
        let mut ops = Vec::new();
        loop {
            let op = match &self.cur().tok {
                Tok::Op("==") => CmpOp::Eq,
                Tok::Op("!=") => CmpOp::Ne,
                Tok::Op("<") => CmpOp::Lt,
                Tok::Op("<=") => CmpOp::Le,
                Tok::Op(">") => CmpOp::Gt,
                Tok::Op(">=") => CmpOp::Ge,
                Tok::Name(n) if n == "in" => CmpOp::In,
                Tok::Name(n) if n == "not" && matches!(self.peek(1), Tok::Name(m) if m == "in") => {
                    self.advance();
                    CmpOp::NotIn
                }
                _ => break,
            };
            self.advance();
            ops.push((op, self.climb(5)?));
        }
        self.mk(ExprKind::Compare(Box::new(first), ops), pos)
    }

    fn concat_run(&mut self, first: Expr, pos: usize) -> R<Expr> {
        let mut parts = vec![first];
        while self.eat_op("~") {
            parts.push(self.climb(7)?);
        }
        self.mk(ExprKind::Concat(parts), pos)
    }

    fn arith_run(&mut self, first: Expr, level: u8, pos: usize) -> R<Expr> {
        let mut rest = Vec::new();
        loop {
            let op = match (level, &self.cur().tok) {
                (5, Tok::Op("+")) => BinOp::Add,
                (5, Tok::Op("-")) => BinOp::Sub,
                (7, Tok::Op("*")) => BinOp::Mul,
                (7, Tok::Op("/")) => BinOp::Div,
                (7, Tok::Op("//")) => BinOp::FloorDiv,
                (7, Tok::Op("%")) => BinOp::Mod,
                (8, Tok::Op("**")) => BinOp::Pow,
                _ => break,
            };
            let op_pos = self.pos();
            self.advance();
            rest.push((op, op_pos, self.climb(level + 1)?));
        }
        self.mk(ExprKind::Chain(Box::new(first), rest), pos)
    }

    fn unary(&mut self, with_filter: bool) -> R<Expr> {
        let pos = self.pos();
        let node = if self.is_op("-") || self.is_op("+") {
            let neg = self.is_op("-");
            self.advance();
            self.enter_rec()?;
            let inner = self.unary(false);
            self.leave_rec();
            let inner = Box::new(inner?);
            self.mk(
                if neg {
                    ExprKind::Neg(inner)
                } else {
                    ExprKind::Pos(inner)
                },
                pos,
            )?
        } else {
            self.primary()?
        };
        let node = self.postfix(node)?;
        if with_filter {
            self.filter_expr(node)
        } else {
            Ok(node)
        }
    }

    fn primary(&mut self) -> R<Expr> {
        let pos = self.pos();
        let t = self.cur().tok.clone();
        match t {
            Tok::Name(n) => {
                let kind = match n.as_str() {
                    "true" | "True" => ExprKind::Const(Const::Bool(true)),
                    "false" | "False" => ExprKind::Const(Const::Bool(false)),
                    "none" | "None" => ExprKind::Const(Const::None),
                    r if RESERVED.contains(&r) => return Err(self.unexpected("an expression")),
                    _ => ExprKind::Name(Arc::from(n.as_str())),
                };
                self.advance();
                self.mk(kind, pos)
            }
            Tok::Str(s) => {
                self.advance();
                let mut s = s;
                while let Tok::Str(more) = &self.cur().tok {
                    s.push_str(more);
                    self.advance();
                }
                self.mk(ExprKind::Const(Const::Str(Arc::from(s))), pos)
            }
            Tok::Int(v) => {
                self.advance();
                self.mk(ExprKind::Const(Const::Int(v)), pos)
            }
            Tok::Float(v) => {
                self.advance();
                self.mk(ExprKind::Const(Const::Float(v)), pos)
            }
            Tok::Op("(") => {
                self.advance();
                if self.eat_op(")") {
                    return self.mk(ExprKind::List(Vec::new()), pos);
                }
                let e = self.tuple(true, &[])?;
                self.expect_op(")")?;
                Ok(e)
            }
            Tok::Op("[") => {
                self.advance();
                let mut items = Vec::new();
                while !self.eat_op("]") {
                    if !items.is_empty() {
                        self.expect_op(",")?;
                        if self.eat_op("]") {
                            break;
                        }
                    }
                    items.push(self.expression(true)?);
                }
                self.mk(ExprKind::List(items), pos)
            }
            Tok::Op("{") => {
                self.advance();
                let mut pairs = Vec::new();
                while !self.eat_op("}") {
                    if !pairs.is_empty() {
                        self.expect_op(",")?;
                        if self.eat_op("}") {
                            break;
                        }
                    }
                    let k = self.expression(true)?;
                    self.expect_op(":")?;
                    let v = self.expression(true)?;
                    pairs.push((k, v));
                }
                self.mk(ExprKind::Map(pairs), pos)
            }
            _ => Err(self.unexpected("an expression")),
        }
    }

    fn postfix(&mut self, mut node: Expr) -> R<Expr> {
        loop {
            let pos = self.pos();
            if self.eat_op(".") {
                let t = self.cur().tok.clone();
                node = match t {
                    Tok::Name(n) => {
                        self.advance();
                        self.mk(ExprKind::Attr(Box::new(node), Arc::from(n.as_str())), pos)?
                    }
                    Tok::Int(v) => {
                        self.advance();
                        let idx = self.mk(ExprKind::Const(Const::Int(v)), pos)?;
                        self.mk(ExprKind::Item(Box::new(node), Box::new(idx)), pos)?
                    }
                    _ => return Err(self.unexpected("an attribute name")),
                };
            } else if self.eat_op("[") {
                node = self.subscript(node, pos)?;
            } else if self.is_op("(") {
                node = self.call(node)?;
            } else {
                return Ok(node);
            }
        }
    }

    fn subscript(&mut self, node: Expr, pos: usize) -> R<Expr> {
        let start = if self.is_op(":") {
            None
        } else {
            Some(Box::new(self.expression(true)?))
        };
        if !self.is_op(":") {
            self.expect_op("]")?;
            let Some(index) = start else {
                return Err(self.unexpected("an index"));
            };
            return self.mk(ExprKind::Item(Box::new(node), index), pos);
        }
        self.advance();
        let stop = if self.is_op("]") || self.is_op(":") {
            None
        } else {
            Some(Box::new(self.expression(true)?))
        };
        let step = if self.eat_op(":") && !self.is_op("]") {
            Some(Box::new(self.expression(true)?))
        } else {
            None
        };
        self.expect_op("]")?;
        self.mk(
            ExprKind::Slice {
                obj: Box::new(node),
                start,
                stop,
                step,
            },
            pos,
        )
    }

    fn call_args(&mut self) -> R<Args> {
        self.expect_op("(")?;
        let mut args = Args::default();
        let mut keywords: HashSet<Arc<str>> = HashSet::new();
        let mut first = true;
        loop {
            if self.eat_op(")") {
                return Ok(args);
            }
            if !first {
                self.expect_op(",")?;
                if self.eat_op(")") {
                    return Ok(args);
                }
            }
            first = false;
            if self.is_op("*") || self.is_op("**") {
                return Err(Fail::new(
                    TemplateErrorKind::Unsupported("'*' / '**' argument unpacking".into()),
                    self.pos(),
                ));
            }
            if matches!(self.cur().tok, Tok::Name(_)) && matches!(self.peek(1), Tok::Op("=")) {
                let (name, npos) = self.expect_name("an argument name")?;
                self.advance();
                if !keywords.insert(name.clone()) {
                    return Err(Fail::syntax(
                        format!("duplicate keyword argument '{name}'"),
                        npos,
                    ));
                }
                let v = self.expression(true)?;
                args.kw.push((name, v));
            } else {
                if !args.kw.is_empty() {
                    return Err(Fail::syntax(
                        "positional argument after a keyword argument",
                        self.pos(),
                    ));
                }
                args.pos.push(self.expression(true)?);
            }
        }
    }

    fn call(&mut self, callee: Expr) -> R<Expr> {
        let pos = callee.pos;
        if let ExprKind::Name(n) = &callee.kind {
            self.calls.push((n.clone(), callee.pos));
        }
        let args = self.call_args()?;
        self.mk(
            ExprKind::Call {
                callee: Box::new(callee),
                args,
            },
            pos,
        )
    }

    fn dotted_name(&mut self, what: &str) -> R<(String, usize)> {
        let (first, pos) = self.expect_name(what)?;
        let mut name = first.to_string();
        while matches!(self.cur().tok, Tok::Op(".")) && matches!(self.peek(1), Tok::Name(_)) {
            self.advance();
            let (part, _) = self.expect_name(what)?;
            name.push('.');
            name.push_str(&part);
        }
        Ok((name, pos))
    }

    fn filter_expr(&mut self, mut node: Expr) -> R<Expr> {
        loop {
            if self.is_op("|") {
                let pos = self.pos();
                self.advance();
                let (name, npos) = self.dotted_name("a filter name")?;
                let filter = Filter::from_name(&name).ok_or_else(|| {
                    Fail::new(TemplateErrorKind::UnknownFilter(name.clone()), npos)
                })?;
                let args = if self.is_op("(") {
                    self.call_args()?
                } else {
                    Args::default()
                };
                self.check_name_args(filter, &args)?;
                node = self.mk(
                    ExprKind::Filter {
                        value: Box::new(node),
                        filter,
                        args,
                    },
                    pos,
                )?;
            } else if self.is_name("is") {
                let pos = self.pos();
                self.advance();
                let negated = self.eat_name("not");
                let (name, npos) = self.dotted_name("a test name")?;
                let test = Test::from_name(&name)
                    .ok_or_else(|| Fail::new(TemplateErrorKind::UnknownTest(name.clone()), npos))?;
                let args = if self.is_op("(") {
                    self.call_args()?
                } else if test.takes_arg() && self.starts_test_arg() {
                    let arg = self.primary()?;
                    let arg = self.postfix(arg)?;
                    Args {
                        pos: vec![arg],
                        kw: Vec::new(),
                    }
                } else {
                    Args::default()
                };
                node = self.mk(
                    ExprKind::Test {
                        value: Box::new(node),
                        test,
                        negated,
                        args,
                    },
                    pos,
                )?;
            } else if self.is_op("(") {
                node = self.call(node)?;
            } else {
                return Ok(node);
            }
        }
    }

    fn starts_test_arg(&self) -> bool {
        match &self.cur().tok {
            Tok::Name(n) => !RESERVED.contains(&n.as_str()),
            Tok::Str(_) | Tok::Int(_) | Tok::Float(_) => true,
            Tok::Op(o) => matches!(*o, "(" | "[" | "{"),
            _ => false,
        }
    }

    /// `select('odd')`, `map('upper')` and friends name a test or filter in a
    /// string; check it here when it is a literal.
    fn check_name_args(&self, filter: Filter, args: &Args) -> R<()> {
        let (index, is_test) = match filter {
            Filter::Select | Filter::Reject => (0, true),
            Filter::Selectattr | Filter::Rejectattr => (1, true),
            Filter::Map if !args.kw.iter().any(|(k, _)| &**k == "attribute") => (0, false),
            _ => return Ok(()),
        };
        if let Some(Expr {
            kind: ExprKind::Const(Const::Str(name)),
            pos,
            ..
        }) = args.pos.get(index)
        {
            if is_test && Test::from_name(name).is_none() {
                return Err(Fail::new(
                    TemplateErrorKind::UnknownTest(name.to_string()),
                    *pos,
                ));
            }
            if !is_test && Filter::from_name(name).is_none() {
                return Err(Fail::new(
                    TemplateErrorKind::UnknownFilter(name.to_string()),
                    *pos,
                ));
            }
        }
        Ok(())
    }
}
