//! The `.proto` tokenizer.

use super::{IdlError, Span};

/// One token.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Tok {
    Ident(String),
    /// An integer literal, already decoded (decimal, hex or octal).
    Int(u64),
    Float(f64),
    /// A string literal (adjacent literals are concatenated).
    Str(String),
    Sym(char),
    Eof,
}

/// A token with its position and the comment block directly above it.
#[derive(Debug, Clone)]
pub(super) struct Token {
    pub tok: Tok,
    pub span: Span,
    /// The leading comment: comments ending on the line just before the token,
    /// with no blank line between.
    pub doc: String,
}

/// The longest token or comment the lexer accepts, so a hostile file cannot
/// build huge strings one token at a time beyond the file size itself.
const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;

pub(super) fn tokenize(file: &str, src: &str) -> Result<Vec<Token>, IdlError> {
    if src.len() > MAX_FILE_BYTES {
        return Err(IdlError::new(
            file,
            Span::default(),
            format!(
                "file is {} bytes, over the limit of {MAX_FILE_BYTES}",
                src.len()
            ),
        ));
    }
    let mut lx = Lexer {
        file,
        bytes: src.as_bytes(),
        pos: 0,
        line: 1,
        col: 1,
    };
    let mut out: Vec<Token> = Vec::new();
    let mut doc_lines: Vec<String> = Vec::new();
    // Line on which the most recent comment ended (to detect blank lines).
    let mut doc_end_line = 0u32;
    loop {
        // Whitespace and comments.
        loop {
            match lx.peek() {
                Some(b' ' | b'\t' | b'\r' | b'\n' | 0x0c | 0x0b) => lx.bump(),
                Some(b'/') if lx.peek_at(1) == Some(b'/') => {
                    let start_line = lx.line;
                    if doc_end_line != 0 && start_line > doc_end_line + 1 {
                        doc_lines.clear();
                    }
                    let attached = out.last().map_or(true, |t| t.span.line < start_line);
                    lx.bump();
                    lx.bump();
                    let begin = lx.pos;
                    while let Some(b) = lx.peek() {
                        if b == b'\n' {
                            break;
                        }
                        lx.bump();
                    }
                    if attached {
                        let text = String::from_utf8_lossy(&lx.bytes[begin..lx.pos]);
                        // A CRLF file leaves the `\r` before the newline.
                        let text = text.strip_suffix('\r').unwrap_or(&text);
                        doc_lines.push(text.strip_prefix(' ').unwrap_or(text).to_owned());
                        doc_end_line = start_line;
                    }
                }
                Some(b'/') if lx.peek_at(1) == Some(b'*') => {
                    let start = lx.here();
                    let start_line = lx.line;
                    if doc_end_line != 0 && start_line > doc_end_line + 1 {
                        doc_lines.clear();
                    }
                    let attached = out.last().map_or(true, |t| t.span.line < start_line);
                    lx.bump();
                    lx.bump();
                    let begin = lx.pos;
                    loop {
                        match lx.peek() {
                            None => return Err(lx.error(start, "unterminated block comment")),
                            Some(b'*') if lx.peek_at(1) == Some(b'/') => break,
                            Some(_) => lx.bump(),
                        }
                    }
                    let text = String::from_utf8_lossy(&lx.bytes[begin..lx.pos]).into_owned();
                    lx.bump();
                    lx.bump();
                    if attached {
                        for l in text.lines() {
                            let l = l.trim();
                            let l = l.strip_prefix('*').map_or(l, str::trim_start);
                            doc_lines.push(l.to_owned());
                        }
                        while doc_lines.last().is_some_and(String::is_empty) {
                            doc_lines.pop();
                        }
                        // One drain, not a `remove(0)` per blank line (which
                        // is quadratic in the number of blank lines).
                        let blank = doc_lines.iter().take_while(|l| l.is_empty()).count();
                        doc_lines.drain(..blank);
                        doc_end_line = lx.line;
                    }
                }
                _ => break,
            }
        }
        let span = lx.here();
        let doc = if doc_end_line != 0 && span.line <= doc_end_line + 1 {
            doc_lines.join("\n")
        } else {
            String::new()
        };
        doc_lines.clear();
        doc_end_line = 0;
        let Some(b) = lx.peek() else {
            out.push(Token {
                tok: Tok::Eof,
                span,
                doc: String::new(),
            });
            return Ok(out);
        };
        let tok = if b.is_ascii_alphabetic() || b == b'_' {
            let begin = lx.pos;
            while lx
                .peek()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
            {
                lx.bump();
            }
            Tok::Ident(String::from_utf8_lossy(&lx.bytes[begin..lx.pos]).into_owned())
        } else if b.is_ascii_digit()
            || (b == b'.' && lx.peek_at(1).is_some_and(|c| c.is_ascii_digit()))
        {
            lx.number(span)?
        } else if b == b'"' || b == b'\'' {
            let mut s = lx.string(span)?;
            // Adjacent literals concatenate: "a" "b" is "ab", with any
            // whitespace or comments between them (as protoc allows).
            loop {
                let save = (lx.pos, lx.line, lx.col);
                lx.skip_trivia();
                match lx.peek() {
                    Some(b'"' | b'\'') => {
                        let at = lx.here();
                        s.push_str(&lx.string(at)?);
                    }
                    _ => {
                        (lx.pos, lx.line, lx.col) = save;
                        break;
                    }
                }
            }
            Tok::Str(s)
        } else if b"{}[]()<>;,=.-+:/".contains(&b) {
            lx.bump();
            Tok::Sym(b as char)
        } else {
            let c = src[lx.pos..].chars().next().unwrap_or('?');
            return Err(lx.error(span, format!("unexpected character {c:?}")));
        };
        out.push(Token { tok, span, doc });
    }
}

struct Lexer<'a> {
    file: &'a str,
    bytes: &'a [u8],
    pos: usize,
    line: u32,
    col: u32,
}

impl Lexer<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_at(&self, n: usize) -> Option<u8> {
        self.bytes.get(self.pos + n).copied()
    }

    fn bump(&mut self) {
        if let Some(b) = self.peek() {
            self.pos += 1;
            if b == b'\n' {
                self.line += 1;
                self.col = 1;
            } else if b & 0xc0 != 0x80 {
                // Count characters, not UTF-8 continuation bytes.
                self.col += 1;
            }
        }
    }

    /// Skips whitespace and comments (an unterminated block comment is left
    /// for the main loop to report).
    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r' | b'\n') => self.bump(),
                Some(b'/') if self.peek_at(1) == Some(b'/') => {
                    while self.peek().is_some_and(|b| b != b'\n') {
                        self.bump();
                    }
                }
                Some(b'/') if self.peek_at(1) == Some(b'*') => {
                    let close = self.bytes[self.pos + 2..]
                        .windows(2)
                        .position(|w| w == b"*/");
                    let Some(close) = close else { return };
                    for _ in 0..close + 4 {
                        self.bump();
                    }
                }
                _ => return,
            }
        }
    }

    fn here(&self) -> Span {
        Span {
            line: self.line,
            column: self.col,
        }
    }

    fn error(&self, span: Span, msg: impl Into<String>) -> IdlError {
        IdlError::new(self.file, span, msg)
    }

    fn number(&mut self, span: Span) -> Result<Tok, IdlError> {
        let begin = self.pos;
        let hex = self.peek() == Some(b'0') && matches!(self.peek_at(1), Some(b'x' | b'X'));
        if hex {
            self.bump();
            self.bump();
            while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                self.bump();
            }
            let digits = std::str::from_utf8(&self.bytes[begin + 2..self.pos]).unwrap_or("");
            return u64::from_str_radix(digits, 16)
                .map(Tok::Int)
                .map_err(|_| self.error(span, "invalid hexadecimal integer"));
        }
        let mut float = false;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.bump();
            } else if c == b'.' {
                float = true;
                self.bump();
            } else if c == b'e' || c == b'E' {
                float = true;
                self.bump();
                if matches!(self.peek(), Some(b'+' | b'-')) {
                    self.bump();
                }
            } else {
                break;
            }
        }
        if self
            .peek()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        {
            return Err(self.error(span, "invalid number: a letter follows the digits"));
        }
        let text = std::str::from_utf8(&self.bytes[begin..self.pos]).unwrap_or("");
        if float {
            return text
                .parse::<f64>()
                .map(Tok::Float)
                .map_err(|_| self.error(span, format!("invalid number {text:?}")));
        }
        if text.len() > 1 && text.starts_with('0') {
            return u64::from_str_radix(&text[1..], 8)
                .map(Tok::Int)
                .map_err(|_| self.error(span, format!("invalid octal integer {text:?}")));
        }
        text.parse::<u64>()
            .map(Tok::Int)
            .map_err(|_| self.error(span, format!("integer {text} does not fit in 64 bits")))
    }

    fn string(&mut self, span: Span) -> Result<String, IdlError> {
        let quote = self.peek().unwrap_or(b'"');
        self.bump();
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(b) = self.peek() else {
                return Err(self.error(span, "unterminated string"));
            };
            match b {
                b'\n' => return Err(self.error(span, "unterminated string (newline in string)")),
                b if b == quote => {
                    self.bump();
                    break;
                }
                b'\\' => {
                    self.bump();
                    let esc_span = self.here();
                    let Some(e) = self.peek() else {
                        return Err(self.error(span, "unterminated string"));
                    };
                    self.bump();
                    match e {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'a' => out.push(0x07),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'v' => out.push(0x0b),
                        b'\\' | b'\'' | b'"' | b'?' => out.push(e),
                        b'x' | b'X' => {
                            let mut v: u32 = 0;
                            let mut n = 0;
                            while n < 2 && self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                                v = v * 16
                                    + (self.peek().unwrap_or(b'0') as char)
                                        .to_digit(16)
                                        .unwrap_or(0);
                                self.bump();
                                n += 1;
                            }
                            if n == 0 {
                                return Err(self.error(esc_span, "\\x needs a hexadecimal digit"));
                            }
                            out.push(v as u8);
                        }
                        b'0'..=b'7' => {
                            let mut v: u32 = u32::from(e - b'0');
                            let mut n = 1;
                            while n < 3 && matches!(self.peek(), Some(b'0'..=b'7')) {
                                v = v * 8 + u32::from(self.peek().unwrap_or(b'0') - b'0');
                                self.bump();
                                n += 1;
                            }
                            let byte = u8::try_from(v).map_err(|_| {
                                self.error(esc_span, "octal escape is above \\377 (one byte)")
                            })?;
                            out.push(byte);
                        }
                        b'u' | b'U' => {
                            let digits = if e == b'u' { 4 } else { 8 };
                            let mut v: u32 = 0;
                            for _ in 0..digits {
                                let Some(d) = self.peek().and_then(|c| (c as char).to_digit(16))
                                else {
                                    return Err(self.error(esc_span, "invalid unicode escape"));
                                };
                                v = v
                                    .checked_mul(16)
                                    .and_then(|x| x.checked_add(d))
                                    .unwrap_or(u32::MAX);
                                self.bump();
                            }
                            let c = char::from_u32(v)
                                .ok_or_else(|| self.error(esc_span, "invalid unicode escape"))?;
                            let mut b = [0u8; 4];
                            out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
                        }
                        _ => {
                            return Err(
                                self.error(esc_span, format!("invalid escape \\{}", e as char))
                            )
                        }
                    }
                }
                _ => {
                    out.push(b);
                    self.bump();
                }
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> {
        tokenize("t.proto", src)
            .unwrap()
            .into_iter()
            .map(|t| t.tok)
            .collect()
    }

    #[test]
    fn numbers_strings_and_symbols() {
        assert_eq!(
            toks("a = 0x1F 017 42 1.5 -2e3;"),
            [
                Tok::Ident("a".into()),
                Tok::Sym('='),
                Tok::Int(31),
                Tok::Int(15),
                Tok::Int(42),
                Tok::Float(1.5),
                Tok::Sym('-'),
                Tok::Float(2000.0),
                Tok::Sym(';'),
                Tok::Eof
            ]
        );
        assert_eq!(
            toks(r#""a\n\x41\101é" 'b'"#),
            [Tok::Str("a\nAAéb".into()), Tok::Eof]
        );
    }

    #[test]
    fn comments_become_docs() {
        let t = tokenize(
            "t.proto",
            "// detached\n\n// one\n// two\nmessage /* inline */ M {}\n/* block\n * doc\n */\nenum",
        )
        .unwrap();
        assert_eq!(t[0].doc, "one\ntwo");
        assert_eq!(t[1].doc, "");
        assert_eq!(t[4].doc, "block\ndoc");

        // CRLF line endings.
        let t = tokenize("t.proto", "// one\r\n// two\r\nmessage\r\n  M").unwrap();
        assert_eq!(t[0].doc, "one\ntwo");
        assert_eq!((t[1].span.line, t[1].span.column), (4, 3));
    }

    #[test]
    fn errors_have_positions() {
        let e = tokenize("t.proto", "message\n  \"abc").unwrap_err();
        assert_eq!((e.line, e.column), (2, 3));
        assert!(e.message.contains("unterminated string"));
        let e = tokenize("t.proto", "a /* x").unwrap_err();
        assert_eq!((e.line, e.column), (1, 3));
        let e = tokenize("t.proto", "a\n  # b").unwrap_err();
        assert_eq!(e.to_string(), "t.proto:2:3: unexpected character '#'");
        assert!(tokenize("t.proto", "99999999999999999999").is_err());
        assert!(tokenize("t.proto", "12ab").is_err());
        assert!(tokenize("t.proto", r#""\q""#).is_err());
    }
}
