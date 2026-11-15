//! Turns template source into tokens, applying Jinja2's whitespace rules
//! (`trim_blocks`, `lstrip_blocks`, `-` / `+` markers, the final newline) and
//! newline normalisation as it goes, the way Jinja2's own lexer does.

use super::error::{Fail, TemplateErrorKind, R};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    /// Literal template text, whitespace control already applied.
    Text(String),
    VarBegin,
    VarEnd,
    BlockBegin,
    BlockEnd,
    Name(String),
    Str(String),
    Int(i128),
    Float(f64),
    Op(&'static str),
    Eof,
}

#[derive(Debug, Clone)]
pub(crate) struct Token {
    pub(crate) tok: Tok,
    pub(crate) pos: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct LexOptions {
    pub(crate) trim_blocks: bool,
    pub(crate) lstrip_blocks: bool,
    pub(crate) keep_trailing_newline: bool,
}

/// Python's `str.isspace` (Rust's `char::is_whitespace` minus nothing, plus
/// the four ASCII separator controls).
pub(crate) fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

fn is_name_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic() || (!c.is_ascii() && c.is_alphabetic())
}

fn is_name_char(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric() || (!c.is_ascii() && c.is_alphanumeric())
}

/// Replaces `\r\n` and lone `\r` with `\n`, as Jinja2 does for template text.
fn normalize_newlines(s: &str) -> String {
    if !s.contains('\r') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

const OPERATORS: [&str; 26] = [
    "//", "**", "==", "!=", "<=", ">=", "+", "-", "*", "/", "%", "~", "(", ")", "[", "]", "{", "}",
    ",", ".", ":", "|", "=", "<", ">", ";",
];

#[derive(Clone, Copy, PartialEq)]
enum TagKind {
    Var,
    Block,
}

struct Lexer<'s> {
    src: &'s str,
    pos: usize,
    end: usize,
    opts: LexOptions,
    toks: Vec<Token>,
    /// Jinja2's `line_starting`: the previous match ended with a newline (or
    /// this is the start of the template).
    line_starting: bool,
}

pub(crate) fn tokenize(src: &str, opts: LexOptions) -> R<Vec<Token>> {
    let mut end = src.len();
    if !opts.keep_trailing_newline {
        if src.ends_with("\r\n") {
            end -= 2;
        } else if src.ends_with('\n') || src.ends_with('\r') {
            end -= 1;
        }
    }
    let mut lx = Lexer {
        src,
        pos: 0,
        end,
        opts,
        toks: Vec::new(),
        line_starting: true,
    };
    lx.root()?;
    lx.toks.push(Token {
        tok: Tok::Eof,
        pos: end,
    });
    Ok(lx.toks)
}

impl<'s> Lexer<'s> {
    fn bytes(&self) -> &'s [u8] {
        &self.src.as_bytes()[..self.end]
    }

    fn rest(&self) -> &'s str {
        self.src.get(self.pos..self.end).unwrap_or("")
    }

    fn at(&self, i: usize) -> Option<u8> {
        self.bytes().get(i).copied()
    }

    fn push(&mut self, tok: Tok, pos: usize) {
        self.toks.push(Token { tok, pos });
    }

    /// Next `{{`, `{%` or `{#` at or after `from`.
    fn find_open(&self, from: usize) -> Option<usize> {
        let b = self.bytes();
        let mut i = from;
        while i + 1 < b.len() {
            if b[i] == b'{' && matches!(b[i + 1], b'{' | b'%' | b'#') {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// Applies the strip rules to text preceding a tag and emits it.
    fn emit_text(&mut self, start: usize, end: usize, modifier: Option<u8>, lstrip_ok: bool) {
        let text = self.src.get(start..end).unwrap_or("");
        let text = if modifier == Some(b'-') {
            text.trim_end_matches(is_py_space)
        } else if modifier != Some(b'+') && lstrip_ok && self.opts.lstrip_blocks {
            let l_pos = text.rfind(['\n', '\r']).map_or(0, |p| p + 1);
            let tail = &text[l_pos..];
            if (l_pos > 0 || self.line_starting)
                && !tail.is_empty()
                && tail.chars().all(is_py_space)
            {
                &text[..l_pos]
            } else {
                text
            }
        } else {
            text
        };
        if !text.is_empty() {
            self.push(Tok::Text(normalize_newlines(text)), start);
        }
    }

    fn skip_ws(&mut self) {
        let rest = self.rest();
        let trimmed = rest.trim_start_matches(is_py_space);
        self.pos += rest.len() - trimmed.len();
    }

    /// After a tag end: strip all whitespace (`-`), or drop one newline
    /// (`trim_blocks`), and record whether we ended on a newline.
    fn after_end(&mut self, strip: bool, trim: bool) {
        let start = self.pos;
        if strip {
            self.skip_ws();
        } else if trim {
            let rest = self.rest();
            if rest.starts_with("\r\n") {
                self.pos += 2;
            } else if rest.starts_with('\n') || rest.starts_with('\r') {
                self.pos += 1;
            }
        }
        let consumed = self.src.get(start..self.pos).unwrap_or("");
        self.line_starting = consumed.ends_with(['\n', '\r']);
    }

    fn root(&mut self) -> R<()> {
        loop {
            let start = self.pos;
            let Some(open) = self.find_open(start) else {
                self.emit_text(start, self.end, None, false);
                self.pos = self.end;
                return Ok(());
            };
            let kind = self.at(open + 1).unwrap_or(0);
            let modifier = match self.at(open + 2) {
                Some(m @ (b'-' | b'+')) => Some(m),
                _ => None,
            };
            let after = open + 2 + usize::from(modifier.is_some());
            match kind {
                b'#' => {
                    self.emit_text(start, open, modifier, true);
                    self.comment(open, after)?;
                }
                b'%' => {
                    self.emit_text(start, open, modifier, true);
                    if let Some(body) = self.raw_begin(after) {
                        self.raw(open, body)?;
                    } else {
                        self.push(Tok::BlockBegin, open);
                        self.pos = after;
                        self.tag(TagKind::Block, open)?;
                    }
                }
                _ => {
                    self.emit_text(start, open, modifier, false);
                    self.push(Tok::VarBegin, open);
                    self.pos = after;
                    self.tag(TagKind::Var, open)?;
                }
            }
        }
    }

    fn comment(&mut self, open: usize, after: usize) -> R<()> {
        let rest = self.src.get(after..self.end).unwrap_or("");
        let Some(rel) = rest.find("#}") else {
            return Err(Fail::syntax("unterminated comment: missing '#}'", open));
        };
        let close = after + rel;
        let marker = if close > after {
            self.at(close - 1)
        } else {
            None
        };
        self.pos = close + 2;
        match marker {
            Some(b'-') => self.after_end(true, false),
            Some(b'+') => self.after_end(false, false),
            _ => self.after_end(false, self.opts.trim_blocks),
        }
        Ok(())
    }

    /// If the block tag starting after `{%` / `{%-` is `raw`, returns the
    /// position just past its closing `%}` (and applies `-%}` stripping).
    fn raw_begin(&mut self, after: usize) -> Option<usize> {
        let s = self.src.get(after..self.end)?;
        let t = s.trim_start_matches(is_py_space);
        let t2 = t.strip_prefix("raw")?;
        if t2.chars().next().is_some_and(is_name_char) {
            return None;
        }
        let t3 = t2.trim_start_matches(is_py_space);
        let consumed = s.len() - t3.len();
        if t3.starts_with("-%}") {
            self.pos = after + consumed + 3;
            self.after_end(true, false);
            Some(self.pos)
        } else if t3.starts_with("%}") {
            self.pos = after + consumed + 2;
            self.line_starting = false;
            Some(self.pos)
        } else {
            None
        }
    }

    fn raw(&mut self, open: usize, body: usize) -> R<()> {
        let mut search = body;
        loop {
            let Some(at) = self.find_open(search) else {
                return Err(Fail::syntax(
                    "unterminated raw block: missing '{% endraw %}'",
                    open,
                ));
            };
            search = at + 1;
            if self.at(at + 1) != Some(b'%') {
                continue;
            }
            let modifier = match self.at(at + 2) {
                Some(m @ (b'-' | b'+')) => Some(m),
                _ => None,
            };
            let after = at + 2 + usize::from(modifier.is_some());
            let s = self.src.get(after..self.end).unwrap_or("");
            let t = s.trim_start_matches(is_py_space);
            let Some(t2) = t.strip_prefix("endraw") else {
                continue;
            };
            let t3 = t2.trim_start_matches(is_py_space);
            let consumed = s.len() - t3.len();
            let (len, strip, trim) = if t3.starts_with("+%}") {
                (3, false, false)
            } else if t3.starts_with("-%}") {
                (3, true, false)
            } else if t3.starts_with("%}") {
                (2, false, self.opts.trim_blocks)
            } else {
                continue;
            };
            self.emit_text(body, at, modifier, true);
            self.pos = after + consumed + len;
            self.after_end(strip, trim);
            return Ok(());
        }
    }

    /// Tokens inside `{{ }}` or `{% %}`, up to and including the closing tag.
    fn tag(&mut self, kind: TagKind, open: usize) -> R<()> {
        let mut brackets: Vec<(u8, usize)> = Vec::new();
        loop {
            self.skip_ws();
            let rest = self.rest();
            if rest.is_empty() {
                let what = if kind == TagKind::Var { "}}" } else { "%}" };
                return Err(Fail::syntax(
                    format!("unclosed tag: missing '{what}'"),
                    open,
                ));
            }
            let pos = self.pos;
            if brackets.is_empty() {
                match kind {
                    TagKind::Var => {
                        if rest.starts_with("-}}") {
                            self.push(Tok::VarEnd, pos);
                            self.pos += 3;
                            self.after_end(true, false);
                            return Ok(());
                        }
                        if rest.starts_with("}}") {
                            self.push(Tok::VarEnd, pos);
                            self.pos += 2;
                            self.line_starting = false;
                            return Ok(());
                        }
                    }
                    TagKind::Block => {
                        for (end, strip, trim) in [
                            ("+%}", false, false),
                            ("-%}", true, false),
                            ("%}", false, self.opts.trim_blocks),
                        ] {
                            if rest.starts_with(end) {
                                self.push(Tok::BlockEnd, pos);
                                self.pos += end.len();
                                self.after_end(strip, trim);
                                return Ok(());
                            }
                        }
                    }
                }
            }
            let c = rest.chars().next().unwrap_or(' ');
            if c.is_ascii_digit() {
                self.number()?;
            } else if is_name_start(c) {
                let len = rest
                    .find(|ch: char| !is_name_char(ch))
                    .unwrap_or(rest.len());
                self.push(Tok::Name(rest[..len].to_string()), pos);
                self.pos += len;
            } else if c == '\'' || c == '"' {
                self.string(c)?;
            } else {
                let Some(op) = OPERATORS.iter().find(|op| rest.starts_with(**op)).copied() else {
                    return Err(Fail::syntax(format!("unexpected character '{c}'"), pos));
                };
                match op {
                    "(" | "[" | "{" => brackets.push((op.as_bytes()[0], pos)),
                    ")" | "]" | "}" => {
                        let want = match op {
                            ")" => b'(',
                            "]" => b'[',
                            _ => b'{',
                        };
                        match brackets.pop() {
                            Some((b, _)) if b == want => {}
                            _ => {
                                return Err(Fail::syntax(format!("unexpected '{op}'"), pos));
                            }
                        }
                    }
                    _ => {}
                }
                self.push(Tok::Op(op), pos);
                self.pos += op.len();
            }
        }
    }

    /// A number literal, matched as Jinja2's lexer matches it: a float
    /// first, then an integer (`0x1f`, `0o7`, `0b1`, `1_000`, `0`, `00`).
    /// Whatever the match leaves (the `1` of `01`, the `__0` of `1__0`)
    /// becomes the next token, so the parser reports it.
    fn number(&mut self) -> R<()> {
        let pos = self.pos;
        let rest = self.rest();
        let b = rest.as_bytes();
        // Jinja2's float pattern does not match right after a `.`, so
        // `x.0.1` is two subscripts, not `x` followed by `0.1`.
        let after_dot = pos > 0 && self.src.as_bytes().get(pos - 1) == Some(&b'.');
        if !after_dot {
            if let Some(len) = float_len(b) {
                let text: String = rest[..len].chars().filter(|c| *c != '_').collect();
                let v: f64 = text
                    .parse()
                    .map_err(|_| Fail::syntax("invalid float literal", pos))?;
                self.push(Tok::Float(v), pos);
                self.pos += len;
                return Ok(());
            }
        }
        let (len, radix, prefix) = int_len(b);
        let digits: String = rest[prefix..len].chars().filter(|c| *c != '_').collect();
        let v = i128::from_str_radix(&digits, radix)
            .map_err(|_| Fail::syntax("integer literal too large", pos))?;
        self.push(Tok::Int(v), pos);
        self.pos += len;
        Ok(())
    }

    fn string(&mut self, quote: char) -> R<()> {
        let pos = self.pos;
        let rest = self.rest();
        let mut chars = rest.char_indices().skip(1);
        let mut out = String::new();
        loop {
            let Some((i, c)) = chars.next() else {
                return Err(Fail::syntax("unterminated string literal", pos));
            };
            if c == quote {
                self.push(Tok::Str(out), pos);
                self.pos += i + 1;
                return Ok(());
            }
            match c {
                '\\' => {
                    let Some((_, e)) = chars.next() else {
                        return Err(Fail::syntax("unterminated string literal", pos));
                    };
                    match e {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        '\\' => out.push('\\'),
                        '\'' => out.push('\''),
                        '"' => out.push('"'),
                        'a' => out.push('\u{7}'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'v' => out.push('\u{b}'),
                        '\n' => {}
                        '\r' => {
                            if rest[pos_after(&chars, rest)..].starts_with('\n') {
                                chars.next();
                            }
                        }
                        'x' | 'u' | 'U' => {
                            let n = match e {
                                'x' => 2,
                                'u' => 4,
                                _ => 8,
                            };
                            let mut v: u32 = 0;
                            for _ in 0..n {
                                let d = chars.next().and_then(|(_, h)| h.to_digit(16)).ok_or_else(
                                    || Fail::syntax(format!("truncated \\{e} escape"), pos),
                                )?;
                                v = v.wrapping_mul(16).wrapping_add(d);
                            }
                            out.push(char::from_u32(v).unwrap_or('\u{fffd}'));
                        }
                        '0'..='7' => {
                            let mut v = e.to_digit(8).unwrap_or(0);
                            for _ in 0..2 {
                                let next = rest[pos_after(&chars, rest)..].chars().next();
                                match next.and_then(|d| d.to_digit(8)) {
                                    Some(d) => {
                                        chars.next();
                                        v = v * 8 + d;
                                    }
                                    None => break,
                                }
                            }
                            out.push(char::from_u32(v).unwrap_or('\u{fffd}'));
                        }
                        'N' => {
                            // Python's named escape needs the Unicode name
                            // table; refuse it rather than print it wrongly.
                            return Err(Fail::new(
                                TemplateErrorKind::Unsupported(
                                    "the \\N{...} named character escape".into(),
                                ),
                                pos,
                            ));
                        }
                        other => {
                            out.push('\\');
                            out.push(other);
                        }
                    }
                }
                '\r' => {
                    if rest[pos_after(&chars, rest)..].starts_with('\n') {
                        chars.next();
                    }
                    out.push('\n');
                }
                _ => out.push(c),
            }
        }
    }
}

/// `(_?D)*` from `i`, where `D` is a digit in `radix`: the end of the match.
fn separated_digits(b: &[u8], mut i: usize, radix: u32) -> usize {
    loop {
        let j = if b.get(i) == Some(&b'_') { i + 1 } else { i };
        match b.get(j) {
            Some(c) if char::from(*c).is_digit(radix) => i = j + 1,
            _ => return i,
        }
    }
}

/// `(\d+_)*\d+` from `i`: the end of the match, if there is a digit at `i`.
fn digit_groups(b: &[u8], i: usize) -> Option<usize> {
    if !b.get(i).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    Some(separated_digits(b, i + 1, 10))
}

/// The length of Jinja2's float literal at the start of `b`, if there is
/// one: `(\d+_)*\d+((\.(\d+_)*\d+)?e[+-]?(\d+_)*\d+|\.(\d+_)*\d+)`.
fn float_len(b: &[u8]) -> Option<usize> {
    let whole = digit_groups(b, 0)?;
    let exponent = |at: usize| {
        if !matches!(b.get(at), Some(b'e' | b'E')) {
            return None;
        }
        let sign = usize::from(matches!(b.get(at + 1), Some(b'+' | b'-')));
        digit_groups(b, at + 1 + sign)
    };
    let fraction = if b.get(whole) == Some(&b'.') {
        digit_groups(b, whole + 1)
    } else {
        None
    };
    fraction
        .and_then(exponent)
        .or_else(|| exponent(whole))
        .or(fraction)
}

/// Jinja2's integer literal at the start of `b` (which starts with a digit):
/// `0b(_?[01])+`, `0o(_?[0-7])+`, `0x(_?[0-9a-f])+`, `[1-9](_?\d)*` or
/// `0(_?0)*`. Returns its length, radix and prefix length.
fn int_len(b: &[u8]) -> (usize, u32, usize) {
    if b.first() == Some(&b'0') {
        let radix = match b.get(1).map(|c| c | 0x20) {
            Some(b'x') => 16,
            Some(b'o') => 8,
            Some(b'b') => 2,
            _ => 0,
        };
        if radix != 0 {
            let end = separated_digits(b, 2, radix);
            if end > 2 {
                return (end, radix, 2);
            }
        }
        // `0(_?0)*`: leading zeros only for zero itself.
        return (zeros_end(b), 10, 0);
    }
    (separated_digits(b, 1, 10), 10, 0)
}

/// The end of `0(_?0)*` at the start of `b`.
fn zeros_end(b: &[u8]) -> usize {
    let mut i = 1;
    loop {
        let j = if b.get(i) == Some(&b'_') { i + 1 } else { i };
        if b.get(j) == Some(&b'0') {
            i = j + 1;
        } else {
            return i;
        }
    }
}

/// The byte index the char iterator will read next.
fn pos_after(chars: &std::iter::Skip<std::str::CharIndices<'_>>, rest: &str) -> usize {
    chars.clone().next().map_or(rest.len(), |(i, _)| i)
}
