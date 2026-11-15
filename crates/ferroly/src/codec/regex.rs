//! A small, linear-time regular-expression engine for the schema `pattern`
//! keyword.
//!
//! Crate-private: the only consumer is [`schema`](super::schema), which
//! compiles each `pattern` once when a [`Schema`](super::schema::Schema) is
//! built and runs it against string values.
//!
//! # Syntax
//!
//! The ECMA-262 subset that JSON Schema recommends for interoperable patterns,
//! with the strict escape rules of the `u` (Unicode) flag, which JSON Schema
//! says patterns should be built with: literals; the escapes `\n \t \r \f \v
//! \0 \cX \xHH \uHHHH \u{H…}` and an escaped syntax character
//! (`^ $ \ . * + ? ( ) [ ] { } | /`); `.` (any character except a line
//! terminator); the anchors `^` and `$` (start and end of the whole input); the
//! word boundaries `\b` and `\B`; the classes `\d \D \w \W \s \S` (ECMA
//! definitions); bracket classes `[...]` / `[^...]` with ranges; groups `(...)`,
//! `(?:...)` and `(?<name>...)`; alternation `|`; and the quantifiers
//! `* + ? {n} {n,} {n,m}`, each optionally lazy (`*?` …). Laziness does not
//! change whether a string matches, so it is parsed and then ignored.
//!
//! Matching is over Unicode scalar values (Rust `char`s) and is an
//! **unanchored search**, as JSON Schema defines `pattern`: the pattern matches
//! if it matches anywhere in the string.
//!
//! Everything else — backreferences, lookahead and lookbehind, inline flags,
//! Unicode property escapes, octal escapes, unknown escapes, an unescaped `{`,
//! `}` or `]` — is a compile error, never a silent literal.
//!
//! # Cost
//!
//! The pattern is parsed to an AST, compiled to a Thompson NFA program, and run
//! by a Pike VM: every live thread advances in lock-step, one input character
//! at a time, and each program position is held at most once per step. There
//! is no backtracking, so matching costs at most O(program size × input
//! characters) whatever the pattern — `(a+)+$` cannot blow up. The pattern
//! length, group nesting depth and compiled program size are capped (see the
//! `MAX_*` constants), and the program size is computed *before* counted
//! repetition is expanded, so `(?:a{1000}){1000}` is rejected rather than
//! expanded.
//!
//! Counted repetition is compiled so that it does not multiply the live
//! threads. The optional tail of `X{n,m}` is nested — `(?:X(?:X(?:X)?)?)?` —
//! with every skip jumping straight past it, so an anchored match keeps O(1)
//! threads instead of one per remaining copy. When `X` is a single character
//! or class (`[a-z0-9]{1,255}`, `.{0,4900}`), the tail is one counting
//! instruction instead: its threads differ only in how many copies they have
//! used, and the one that has used the fewest can do everything the others
//! can, so only that one is kept. The thread lists live in a per-thread
//! scratch buffer reused across calls.
//!
//! A lone surrogate escape (`\uD800`, or a class range reaching into
//! U+D800–U+DFFF) can never match a Unicode scalar value, so it matches
//! nothing — as with the ECMA `u` flag on well-formed input — rather than
//! being a compile error. Group names may repeat across alternatives
//! (`(?<x>a)|(?<x>b)`, ES2025); a repeat within one alternative is an error.

use std::fmt;
use std::sync::Arc;

/// Longest accepted pattern, in bytes.
pub(crate) const MAX_PATTERN_BYTES: usize = 4096;
/// Deepest accepted group nesting.
pub(crate) const MAX_DEPTH: usize = 64;
/// Largest accepted compiled program, in instructions.
pub(crate) const MAX_PROGRAM: usize = 10_000;

/// Why a pattern failed to compile, with the byte offset in the pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegexError {
    offset: usize,
    message: String,
}

impl fmt::Display for RegexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at byte {})", self.message, self.offset)
    }
}

/// A compiled pattern. Cheap to clone (the program's character classes are
/// shared) and `Send + Sync`.
#[derive(Debug, Clone)]
pub(crate) struct Regex {
    prog: Vec<Inst>,
    /// Every match must start at input position 0 (the pattern begins with
    /// `^`), so the search need not restart at later positions.
    anchored: bool,
}

/// A sorted, non-overlapping list of inclusive character ranges.
type Ranges = Arc<[(char, char)]>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Look {
    Start,
    End,
    WordBoundary,
    NotWordBoundary,
}

#[derive(Debug, Clone)]
enum Ast {
    Empty,
    Char(char),
    Class(Ranges),
    Look(Look),
    Concat(Vec<Ast>),
    Alt(Vec<Ast>),
    Repeat {
        sub: Box<Ast>,
        min: u32,
        max: Option<u32>,
    },
}

#[derive(Debug, Clone)]
enum Inst {
    Char(char),
    Class(Ranges),
    Look(Look),
    Split(usize, usize),
    Jmp(usize),
    /// The optional tail of `X{n,m}` for a one-character `X`: consume up to
    /// `max` more characters in `ranges`, or continue at the next instruction.
    /// A thread here carries how many it has consumed; only the smallest
    /// count is kept per step, since it can do everything a larger one can.
    Count {
        ranges: Ranges,
        max: u32,
    },
    Match,
}

/// The reusable thread lists of [`Regex::is_match`].
struct Scratch {
    clist: Threads,
    nlist: Threads,
    stack: Vec<(usize, u32)>,
}

impl Scratch {
    fn new() -> Scratch {
        Scratch {
            clist: Threads::new(0),
            nlist: Threads::new(0),
            stack: Vec::new(),
        }
    }
}

thread_local! {
    static SCRATCH: std::cell::RefCell<Scratch> = std::cell::RefCell::new(Scratch::new());
}

impl Regex {
    /// Compiles `pattern`, or reports why it is unsupported or malformed.
    pub(crate) fn new(pattern: &str) -> Result<Regex, RegexError> {
        if pattern.len() > MAX_PATTERN_BYTES {
            return Err(RegexError {
                offset: 0,
                message: format!(
                    "pattern is {} bytes, longer than the {MAX_PATTERN_BYTES}-byte limit",
                    pattern.len()
                ),
            });
        }
        let mut parser = Parser {
            src: pattern,
            pos: 0,
            depth: 0,
            names: Vec::new(),
        };
        let ast = parser.parse_alt()?;
        if parser.pos < pattern.len() {
            // `parse_alt` stops only at the end or at a `)` it did not open.
            return Err(parser.error(parser.pos, "unmatched ')'"));
        }
        let size = size(&ast);
        if size >= MAX_PROGRAM as u64 {
            return Err(RegexError {
                offset: 0,
                message: format!(
                    "pattern compiles to more than {MAX_PROGRAM} instructions \
                     (reduce counted repetition such as {{n,m}})"
                ),
            });
        }
        let mut prog = Vec::with_capacity(size as usize + 1);
        emit(&ast, &mut prog);
        prog.push(Inst::Match);
        Ok(Regex {
            prog,
            anchored: anchored(&ast),
        })
    }

    /// Whether the pattern matches anywhere in `text`.
    pub(crate) fn is_match(&self, text: &str) -> bool {
        self.run(text).0
    }

    /// [`is_match`](Self::is_match), plus the number of thread insertions it
    /// made: the work the match cost, for tests that bound it.
    fn run(&self, text: &str) -> (bool, u64) {
        SCRATCH.with(|cell| match cell.try_borrow_mut() {
            Ok(mut scratch) => self.run_in(&mut scratch, text),
            // Never re-entered in practice; fall back to fresh lists if it is.
            Err(_) => self.run_in(&mut Scratch::new(), text),
        })
    }

    fn run_in(&self, scratch: &mut Scratch, text: &str) -> (bool, u64) {
        let Scratch {
            clist,
            nlist,
            stack,
        } = scratch;
        clist.reset(self.prog.len());
        nlist.reset(self.prog.len());
        let mut work = 0u64;
        let mut prev: Option<char> = None;
        let mut chars = text.chars();
        let mut cur = chars.next();
        loop {
            // Unanchored search: a new thread starts at every position.
            if (!self.anchored || prev.is_none())
                && self.add(clist, stack, (0, 0), prev, cur, &mut work)
            {
                return (true, work);
            }
            let Some(ch) = cur else {
                return (false, work);
            };
            if clist.len == 0 && self.anchored {
                return (false, work);
            }
            let next = chars.next();
            nlist.clear();
            for i in 0..clist.len {
                let pc = clist.dense[i];
                let (consumed, thread) = match &self.prog[pc] {
                    Inst::Char(c) => (*c == ch, (pc + 1, 0)),
                    Inst::Class(ranges) => (class_contains(ranges, ch), (pc + 1, 0)),
                    Inst::Count { ranges, max } => {
                        let used = clist.count[pc];
                        (used < *max && class_contains(ranges, ch), (pc, used + 1))
                    }
                    _ => (false, (0, 0)),
                };
                if consumed && self.add(nlist, stack, thread, Some(ch), next, &mut work) {
                    return (true, work);
                }
            }
            std::mem::swap(clist, nlist);
            prev = Some(ch);
            cur = next;
        }
    }

    /// Adds the thread `(pc, used)` (`used` counts the copies a thread at an
    /// [`Inst::Count`] has consumed) and everything reachable from it without
    /// consuming input (between `prev` and `next`). Returns whether a `Match`
    /// is reachable. Iterative, so long `Split` chains cannot overflow the
    /// stack; each position is visited at most once per list.
    fn add(
        &self,
        list: &mut Threads,
        stack: &mut Vec<(usize, u32)>,
        thread: (usize, u32),
        prev: Option<char>,
        next: Option<char>,
        work: &mut u64,
    ) -> bool {
        stack.clear();
        stack.push(thread);
        while let Some((pc, used)) = stack.pop() {
            *work += 1;
            if !list.insert(pc) {
                // Already live. A counter keeps the smaller count: what is
                // reachable from it without input does not depend on it.
                if matches!(self.prog[pc], Inst::Count { .. }) && used < list.count[pc] {
                    list.count[pc] = used;
                }
                continue;
            }
            match &self.prog[pc] {
                Inst::Jmp(to) => stack.push((*to, 0)),
                Inst::Split(a, b) => {
                    stack.push((*b, 0));
                    stack.push((*a, 0));
                }
                Inst::Look(look) => {
                    if look_holds(*look, prev, next) {
                        stack.push((pc + 1, 0));
                    }
                }
                Inst::Count { .. } => {
                    list.count[pc] = used;
                    stack.push((pc + 1, 0));
                }
                Inst::Match => return true,
                Inst::Char(_) | Inst::Class(_) => {}
            }
        }
        false
    }
}

/// A sparse set of program positions (the classic Pike-VM thread list):
/// O(1) insert, membership and clear. Stale entries from an earlier use are
/// harmless, so a list is reused across calls without being zeroed.
struct Threads {
    dense: Vec<usize>,
    sparse: Vec<usize>,
    /// Per position: the copies used by the thread at an [`Inst::Count`].
    count: Vec<u32>,
    len: usize,
}

impl Threads {
    fn new(n: usize) -> Threads {
        Threads {
            dense: vec![0; n],
            sparse: vec![0; n],
            count: vec![0; n],
            len: 0,
        }
    }

    /// Empties the list and makes it hold positions below `n`.
    fn reset(&mut self, n: usize) {
        if self.sparse.len() < n {
            self.dense.resize(n, 0);
            self.sparse.resize(n, 0);
            self.count.resize(n, 0);
        }
        self.len = 0;
    }

    /// Inserts `pc`; false if it was already present.
    fn insert(&mut self, pc: usize) -> bool {
        let i = self.sparse[pc];
        if i < self.len && self.dense[i] == pc {
            return false;
        }
        self.dense[self.len] = pc;
        self.sparse[pc] = self.len;
        self.len += 1;
        true
    }

    fn clear(&mut self) {
        self.len = 0;
    }
}

fn is_word(c: Option<char>) -> bool {
    matches!(c, Some('a'..='z' | 'A'..='Z' | '0'..='9' | '_'))
}

fn look_holds(look: Look, prev: Option<char>, next: Option<char>) -> bool {
    match look {
        Look::Start => prev.is_none(),
        Look::End => next.is_none(),
        Look::WordBoundary => is_word(prev) != is_word(next),
        Look::NotWordBoundary => is_word(prev) == is_word(next),
    }
}

fn class_contains(ranges: &[(char, char)], c: char) -> bool {
    ranges
        .binary_search_by(|&(lo, hi)| {
            if hi < c {
                std::cmp::Ordering::Less
            } else if lo > c {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

// ---------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------

/// The number of instructions `emit` produces for `ast` (excluding the final
/// `Match`), saturating so that counted repetition is sized before expansion.
fn size(ast: &Ast) -> u64 {
    match ast {
        Ast::Empty => 0,
        Ast::Char(_) | Ast::Class(_) | Ast::Look(_) => 1,
        Ast::Concat(items) => items
            .iter()
            .fold(0u64, |acc, a| acc.saturating_add(size(a))),
        Ast::Alt(alts) => alts
            .iter()
            .fold(0u64, |acc, a| acc.saturating_add(size(a)))
            .saturating_add(2 * (alts.len() as u64 - 1)),
        Ast::Repeat { sub, min, max } => {
            let s = size(sub);
            if s == 0 {
                return 0;
            }
            let min = u64::from(*min);
            match max {
                None if min == 0 => s.saturating_add(2),
                None => min.saturating_mul(s).saturating_add(1),
                Some(max) => min
                    .saturating_mul(s)
                    .saturating_add((u64::from(*max) - min).saturating_mul(s.saturating_add(1))),
            }
        }
    }
}

fn emit(ast: &Ast, prog: &mut Vec<Inst>) {
    match ast {
        Ast::Empty => {}
        Ast::Char(c) => prog.push(Inst::Char(*c)),
        Ast::Class(ranges) => prog.push(Inst::Class(Arc::clone(ranges))),
        Ast::Look(look) => prog.push(Inst::Look(*look)),
        Ast::Concat(items) => items.iter().for_each(|a| emit(a, prog)),
        Ast::Alt(alts) => {
            let mut jumps = Vec::with_capacity(alts.len());
            for (i, alt) in alts.iter().enumerate() {
                if i + 1 == alts.len() {
                    emit(alt, prog);
                } else {
                    let split = prog.len();
                    prog.push(Inst::Split(split + 1, 0));
                    emit(alt, prog);
                    jumps.push(prog.len());
                    prog.push(Inst::Jmp(0));
                    prog[split] = Inst::Split(split + 1, prog.len());
                }
            }
            let end = prog.len();
            for j in jumps {
                prog[j] = Inst::Jmp(end);
            }
        }
        Ast::Repeat { sub, min, max } => {
            // A sub-pattern that only ever matches the empty string adds
            // nothing, however often it repeats (and must not be looped).
            if size(sub) == 0 {
                return;
            }
            let mut last_start = prog.len();
            for _ in 0..*min {
                last_start = prog.len();
                emit(sub, prog);
            }
            match max {
                None if *min == 0 => {
                    // L: split L+1, END; sub; jmp L; END:
                    let split = prog.len();
                    prog.push(Inst::Split(split + 1, 0));
                    emit(sub, prog);
                    prog.push(Inst::Jmp(split));
                    prog[split] = Inst::Split(split + 1, prog.len());
                }
                None => {
                    // Loop back over the last mandatory copy.
                    let here = prog.len();
                    prog.push(Inst::Split(last_start, here + 1));
                }
                Some(max) if max > min => {
                    let optional = *max - *min;
                    if let Some(ranges) = single_char(sub) {
                        // One counting instruction, holding one thread.
                        prog.push(Inst::Count {
                            ranges,
                            max: optional,
                        });
                        return;
                    }
                    // Nested, `(?:X(?:X(?:X)?)?)?`: every skip jumps to the
                    // end, so the copies not yet entered hold no threads.
                    let mut splits = Vec::with_capacity(optional as usize);
                    for _ in 0..optional {
                        splits.push(prog.len());
                        prog.push(Inst::Split(0, 0));
                        emit(sub, prog);
                    }
                    let end = prog.len();
                    for split in splits {
                        prog[split] = Inst::Split(split + 1, end);
                    }
                }
                Some(_) => {}
            }
        }
    }
}

/// The character set of a one-character sub-pattern (a literal or a class).
fn single_char(ast: &Ast) -> Option<Ranges> {
    match ast {
        Ast::Char(c) => Some(vec![(*c, *c)].into()),
        Ast::Class(ranges) => Some(Arc::clone(ranges)),
        _ => None,
    }
}

/// Whether every match of `ast` must begin at input position 0.
fn anchored(ast: &Ast) -> bool {
    match ast {
        Ast::Look(Look::Start) => true,
        Ast::Concat(items) => items.first().is_some_and(anchored),
        Ast::Alt(alts) => alts.iter().all(anchored),
        Ast::Repeat { sub, min, .. } => *min >= 1 && anchored(sub),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

struct Parser<'a> {
    src: &'a str,
    /// Byte offset of the next unread character.
    pos: usize,
    /// Current group nesting depth.
    depth: usize,
    /// Names of the named groups that can take part in the same match as the
    /// next one: a repeat among them is an error. Names in a sibling
    /// alternative are set aside while the current one is parsed.
    names: Vec<String>,
}

/// One member of a bracket class: a single code point (a possible range
/// endpoint, which may be a lone surrogate) or a whole class escape such as
/// `\d`.
enum ClassItem {
    Char(u32),
    Set(Vec<(char, char)>),
}

impl Parser<'_> {
    fn error(&self, offset: usize, message: impl Into<String>) -> RegexError {
        RegexError {
            offset,
            message: message.into(),
        }
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn peek2(&self) -> Option<char> {
        let mut it = self.src[self.pos..].chars();
        it.next();
        it.next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += c.len_utf8();
            true
        } else {
            false
        }
    }

    fn parse_alt(&mut self) -> Result<Ast, RegexError> {
        // Names in different alternatives never take part in the same match,
        // so they may repeat (ES2025); after the disjunction all are visible.
        let base = self.names.len();
        let mut set_aside = Vec::new();
        let mut alts = vec![self.parse_concat()?];
        while self.eat('|') {
            set_aside.extend(self.names.drain(base..));
            alts.push(self.parse_concat()?);
        }
        self.names.extend(set_aside);
        Ok(if alts.len() == 1 {
            alts.pop().unwrap_or(Ast::Empty)
        } else {
            Ast::Alt(alts)
        })
    }

    fn parse_concat(&mut self) -> Result<Ast, RegexError> {
        let mut items = Vec::new();
        while !matches!(self.peek(), None | Some('|' | ')')) {
            let start = self.pos;
            let (atom, quantifiable) = self.parse_atom()?;
            items.push(self.parse_quantifier(atom, quantifiable, start)?);
        }
        Ok(match items.len() {
            0 => Ast::Empty,
            1 => items.pop().unwrap_or(Ast::Empty),
            _ => Ast::Concat(items),
        })
    }

    /// Parses one atom; the flag says whether a quantifier may follow it
    /// (assertions cannot be quantified).
    fn parse_atom(&mut self) -> Result<(Ast, bool), RegexError> {
        let start = self.pos;
        let Some(c) = self.bump() else {
            return Err(self.error(start, "unexpected end of pattern"));
        };
        Ok(match c {
            '^' => (Ast::Look(Look::Start), false),
            '$' => (Ast::Look(Look::End), false),
            '.' => (
                Ast::Class(complement(vec![
                    ('\n', '\n'),
                    ('\r', '\r'),
                    ('\u{2028}', '\u{2029}'),
                ])),
                true,
            ),
            '(' => (self.parse_group(start)?, true),
            '[' => (Ast::Class(self.parse_class(start)?), true),
            '\\' => self.parse_escape(start)?,
            '*' | '+' | '?' => {
                return Err(self.error(start, format!("nothing to repeat before '{c}'")));
            }
            '{' => {
                return Err(self.error(
                    start,
                    "unescaped '{' (nothing to repeat; write a literal brace as \\{)",
                ));
            }
            '}' => {
                return Err(self.error(start, "unescaped '}' (write a literal brace as \\})"));
            }
            ']' => {
                return Err(self.error(start, "unescaped ']' (write a literal bracket as \\])"));
            }
            c => (Ast::Char(c), true),
        })
    }

    fn parse_quantifier(
        &mut self,
        atom: Ast,
        quantifiable: bool,
        atom_start: usize,
    ) -> Result<Ast, RegexError> {
        let qstart = self.pos;
        let (min, max) = match self.peek() {
            Some('*') => {
                self.bump();
                (0, None)
            }
            Some('+') => {
                self.bump();
                (1, None)
            }
            Some('?') => {
                self.bump();
                (0, Some(1))
            }
            Some('{') => self.parse_braces()?,
            _ => return Ok(atom),
        };
        if !quantifiable {
            return Err(self.error(
                atom_start,
                "an assertion (^, $, \\b, \\B) cannot be quantified",
            ));
        }
        // A lazy quantifier matches the same set of strings.
        self.eat('?');
        if let Some(max) = max {
            if min > max {
                return Err(self.error(qstart, "numbers out of order in {n,m} quantifier"));
            }
        }
        Ok(Ast::Repeat {
            sub: Box::new(atom),
            min,
            max,
        })
    }

    /// Parses `{n}`, `{n,}` or `{n,m}`; anything else starting with `{` is an
    /// error (an unescaped `{` is not a literal in Unicode mode).
    fn parse_braces(&mut self) -> Result<(u32, Option<u32>), RegexError> {
        let start = self.pos;
        let malformed = |p: &Self| {
            p.error(
                start,
                "malformed {n,m} quantifier (write a literal brace as \\{)",
            )
        };
        self.bump(); // '{'
        let min = self.parse_number().ok_or_else(|| malformed(self))?;
        let max = if self.eat(',') {
            if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                Some(self.parse_number().ok_or_else(|| malformed(self))?)
            } else {
                None
            }
        } else {
            Some(min)
        };
        if !self.eat('}') {
            return Err(malformed(self));
        }
        Ok((min, max))
    }

    /// A decimal number, saturating at `u32::MAX` (the program-size cap
    /// rejects any count that large).
    fn parse_number(&mut self) -> Option<u32> {
        let mut value: Option<u32> = None;
        while let Some(d) = self.peek().and_then(|c| c.to_digit(10)) {
            self.bump();
            value = Some(value.unwrap_or(0).saturating_mul(10).saturating_add(d));
        }
        value
    }

    fn parse_group(&mut self, start: usize) -> Result<Ast, RegexError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error(
                start,
                format!("groups nested deeper than {MAX_DEPTH} levels"),
            ));
        }
        if self.eat('?') {
            match self.peek() {
                Some(':') => {
                    self.bump();
                }
                Some('=' | '!') => {
                    return Err(self.error(start, "lookahead assertions are not supported"));
                }
                Some('<') if matches!(self.peek2(), Some('=' | '!')) => {
                    return Err(self.error(start, "lookbehind assertions are not supported"));
                }
                Some('<') => {
                    self.bump();
                    self.parse_group_name(start)?;
                }
                _ => {
                    return Err(self.error(
                        start,
                        "inline flags and group modifiers ((?...)) are not supported",
                    ));
                }
            }
        }
        let inner = self.parse_alt()?;
        if !self.eat(')') {
            return Err(self.error(start, "unclosed group '('"));
        }
        self.depth -= 1;
        Ok(inner)
    }

    /// Parses `name>` after `(?<`.
    fn parse_group_name(&mut self, start: usize) -> Result<(), RegexError> {
        let name_start = self.pos;
        let mut first = true;
        loop {
            match self.bump() {
                Some('>') if !first => break,
                Some(c)
                    if c == '$'
                        || c == '_'
                        || c.is_alphabetic()
                        || (!first && c.is_alphanumeric()) =>
                {
                    first = false;
                }
                _ => return Err(self.error(start, "invalid group name in (?<name>...)")),
            }
        }
        let name = &self.src[name_start..self.pos - 1];
        if self.names.iter().any(|n| n == name) {
            return Err(self.error(start, format!("duplicate group name '{name}'")));
        }
        self.names.push(name.to_string());
        Ok(())
    }

    /// Parses an escape outside a bracket class (the `\` is consumed).
    fn parse_escape(&mut self, start: usize) -> Result<(Ast, bool), RegexError> {
        let Some(c) = self.bump() else {
            return Err(self.error(start, "pattern ends with a lone backslash"));
        };
        Ok(match c {
            'b' => (Ast::Look(Look::WordBoundary), false),
            'B' => (Ast::Look(Look::NotWordBoundary), false),
            'd' | 'D' | 'w' | 'W' | 's' | 'S' => (Ast::Class(class_escape(c)), true),
            _ => match char::from_u32(self.char_escape(c, start, false)?) {
                Some(ch) => (Ast::Char(ch), true),
                // A lone surrogate: no scalar value can match it.
                None => (Ast::Class(Vec::new().into()), true),
            },
        })
    }

    /// The code point an escape `\c…` denotes, shared by both contexts; class
    /// escapes and `\b` are handled by the callers. It may be a lone
    /// surrogate (from `\uD800`), which the callers make match nothing.
    fn char_escape(&mut self, c: char, start: usize, in_class: bool) -> Result<u32, RegexError> {
        let ch = match c {
            'n' => '\n',
            't' => '\t',
            'r' => '\r',
            'f' => '\u{000C}',
            'v' => '\u{000B}',
            '0' => {
                if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    return Err(self.error(start, "octal escapes are not supported"));
                }
                '\0'
            }
            '1'..='9' => {
                return Err(self.error(start, "backreferences (\\1 ...) are not supported"));
            }
            'k' => {
                return Err(self.error(start, "named backreferences (\\k<name>) are not supported"));
            }
            'p' | 'P' => {
                return Err(self.error(
                    start,
                    "Unicode property escapes (\\p{...}) are not supported",
                ));
            }
            'c' => match self.peek() {
                Some(l) if l.is_ascii_alphabetic() => {
                    self.bump();
                    char::from(l as u8 % 32)
                }
                _ => return Err(self.error(start, "\\c must be followed by an ASCII letter")),
            },
            'x' => {
                let v = self
                    .hex_digits(2)
                    .ok_or_else(|| self.error(start, "\\x must be followed by two hex digits"))?;
                char::from_u32(v).unwrap_or('\0')
            }
            'u' => return self.unicode_escape(start),
            '^' | '$' | '\\' | '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
            | '/' => c,
            '-' if in_class => '-',
            c => {
                return Err(self.error(start, format!("unknown escape '\\{c}'")));
            }
        };
        Ok(u32::from(ch))
    }

    /// Exactly `n` hex digits.
    fn hex_digits(&mut self, n: usize) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..n {
            let d = self.peek()?.to_digit(16)?;
            self.bump();
            value = value * 16 + d;
        }
        Some(value)
    }

    /// `\uHHHH` (a surrogate pair written as two escapes combines into one
    /// character) or `\u{H...}`; the `\u` is consumed. The result is a code
    /// point, possibly a lone surrogate.
    fn unicode_escape(&mut self, start: usize) -> Result<u32, RegexError> {
        let value = if self.eat('{') {
            let mut value = 0u32;
            let mut digits = 0;
            while let Some(d) = self.peek().and_then(|c| c.to_digit(16)) {
                self.bump();
                value = value.saturating_mul(16).saturating_add(d);
                digits += 1;
            }
            if digits == 0 || !self.eat('}') {
                return Err(self.error(start, "malformed \\u{...} escape"));
            }
            if value > 0x10FFFF {
                return Err(self.error(start, "\\u{...} escape beyond U+10FFFF"));
            }
            value
        } else {
            let hi = self
                .hex_digits(4)
                .ok_or_else(|| self.error(start, "\\u must be followed by four hex digits"))?;
            if (0xD800..0xDC00).contains(&hi) && self.src[self.pos..].starts_with("\\u") {
                let save = self.pos;
                self.pos += 2;
                match self.hex_digits(4) {
                    Some(lo) if (0xDC00..0xE000).contains(&lo) => {
                        0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                    }
                    _ => {
                        self.pos = save;
                        hi
                    }
                }
            } else {
                hi
            }
        };
        Ok(value)
    }

    /// Parses a bracket class after its `[`; returns the normalized ranges.
    fn parse_class(&mut self, start: usize) -> Result<Ranges, RegexError> {
        let negated = self.eat('^');
        let mut ranges = Vec::new();
        loop {
            let item_start = self.pos;
            let Some(c) = self.bump() else {
                return Err(self.error(start, "unclosed character class '['"));
            };
            if c == ']' {
                break;
            }
            let lo = self.class_item(c, item_start)?;
            let is_range = self.peek() == Some('-') && !matches!(self.peek2(), None | Some(']'));
            if !is_range {
                match lo {
                    ClassItem::Char(c) => ranges.push((c, c)),
                    ClassItem::Set(set) => ranges.extend(
                        set.into_iter()
                            .map(|(lo, hi)| (u32::from(lo), u32::from(hi))),
                    ),
                }
                continue;
            }
            self.bump(); // '-'
            let hi_start = self.pos;
            let hi_char = self.bump().unwrap_or('\0');
            let hi = self.class_item(hi_char, hi_start)?;
            match (lo, hi) {
                (ClassItem::Char(lo), ClassItem::Char(hi)) => {
                    if lo > hi {
                        return Err(self.error(item_start, "range out of order in character class"));
                    }
                    ranges.push((lo, hi));
                }
                _ => {
                    return Err(self.error(
                        item_start,
                        "a class escape (\\d, \\w, \\s ...) cannot be a range endpoint",
                    ));
                }
            }
        }
        // Clip to scalar values: a lone surrogate, or the surrogate part of a
        // range, can never match.
        let mut scalars = Vec::with_capacity(ranges.len());
        for (lo, hi) in ranges {
            for (lo, hi) in [(lo, hi.min(0xD7FF)), (lo.max(0xE000), hi)] {
                if let (true, Some(lo), Some(hi)) =
                    (lo <= hi, char::from_u32(lo), char::from_u32(hi))
                {
                    scalars.push((lo, hi));
                }
            }
        }
        let ranges = normalize(scalars);
        Ok(if negated {
            complement(ranges)
        } else {
            ranges.into()
        })
    }

    fn class_item(&mut self, c: char, start: usize) -> Result<ClassItem, RegexError> {
        if c != '\\' {
            return Ok(ClassItem::Char(u32::from(c)));
        }
        let Some(e) = self.bump() else {
            return Err(self.error(start, "pattern ends with a lone backslash"));
        };
        Ok(match e {
            'd' | 'D' | 'w' | 'W' | 's' | 'S' => ClassItem::Set(class_escape(e).to_vec()),
            // Inside a class, `\b` is the backspace character.
            'b' => ClassItem::Char(0x08),
            'B' => return Err(self.error(start, "\\B is not allowed in a character class")),
            e => ClassItem::Char(self.char_escape(e, start, true)?),
        })
    }
}

/// `\d \D \w \W \s \S` as ECMA-262 defines them.
fn class_escape(c: char) -> Ranges {
    let digit = || vec![('0', '9')];
    let word = || vec![('0', '9'), ('A', 'Z'), ('_', '_'), ('a', 'z')];
    // WhiteSpace and LineTerminator: TAB, LF, VT, FF, CR, SPACE, NBSP, the
    // Zs category, LS, PS, and ZWNBSP.
    let space = || {
        vec![
            ('\t', '\r'),
            (' ', ' '),
            ('\u{00A0}', '\u{00A0}'),
            ('\u{1680}', '\u{1680}'),
            ('\u{2000}', '\u{200A}'),
            ('\u{2028}', '\u{2029}'),
            ('\u{202F}', '\u{202F}'),
            ('\u{205F}', '\u{205F}'),
            ('\u{3000}', '\u{3000}'),
            ('\u{FEFF}', '\u{FEFF}'),
        ]
    };
    match c {
        'd' => digit().into(),
        'D' => complement(digit()),
        'w' => word().into(),
        'W' => complement(word()),
        's' => space().into(),
        _ => complement(space()),
    }
}

/// Sorts and merges overlapping or adjacent ranges.
fn normalize(mut ranges: Vec<(char, char)>) -> Vec<(char, char)> {
    ranges.sort_unstable();
    let mut out: Vec<(char, char)> = Vec::with_capacity(ranges.len());
    for (lo, hi) in ranges {
        if let Some(last) = out.last_mut() {
            if u32::from(lo) <= u32::from(last.1).saturating_add(1) {
                if hi > last.1 {
                    last.1 = hi;
                }
                continue;
            }
        }
        out.push((lo, hi));
    }
    out
}

/// Every Unicode scalar value not in `ranges`.
fn complement(ranges: Vec<(char, char)>) -> Ranges {
    let ranges = normalize(ranges);
    let mut out = Vec::with_capacity(ranges.len() + 1);
    let mut next: u32 = 0;
    let mut push = |lo: u32, hi: u32| {
        // Skip the surrogate gap, which holds no `char`.
        let lo = if (0xD800..0xE000).contains(&lo) {
            0xE000
        } else {
            lo
        };
        let hi = if (0xD800..0xE000).contains(&hi) {
            0xD7FF
        } else {
            hi
        };
        if lo <= hi {
            if let (Some(lo), Some(hi)) = (char::from_u32(lo), char::from_u32(hi)) {
                out.push((lo, hi));
            }
        }
    };
    for (lo, hi) in ranges {
        let (lo, hi) = (u32::from(lo), u32::from(hi));
        if lo > next {
            push(next, lo - 1);
        }
        next = hi + 1;
    }
    if next <= 0x10FFFF {
        push(next, 0x10FFFF);
    }
    out.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, text: &str) -> bool {
        Regex::new(pattern)
            .unwrap_or_else(|e| panic!("{pattern:?} failed to compile: {e}"))
            .is_match(text)
    }

    #[test]
    fn match_table() {
        let cases: &[(&str, &str, bool)] = &[
            // Literals and unanchored search.
            ("", "", true),
            ("", "anything", true),
            ("abc", "abc", true),
            ("abc", "xxabcxx", true),
            ("abc", "ab", false),
            ("abc", "acb", false),
            // Anchors.
            ("^abc", "abcd", true),
            ("^abc", "xabc", false),
            ("abc$", "xabc", true),
            ("abc$", "abcx", false),
            ("abc$", "abc\n", false),
            ("^$", "", true),
            ("^$", "x", false),
            ("^a|b$", "ax", true),
            ("^a|b$", "xb", true),
            ("^a|b$", "xa", false),
            ("a^b", "a^b", false),
            // Dot.
            ("^.$", "x", true),
            ("^.$", "é", true),
            ("^.$", "😀", true),
            ("^.$", "\n", false),
            ("^.$", "\r", false),
            ("^.$", "\u{2028}", false),
            ("^...$", "a😀b", true),
            // Escapes.
            (r"^\.$", ".", true),
            (r"^\.$", "x", false),
            (r"^\\$", "\\", true),
            (r"^\/$", "/", true),
            (r"^\n\t\r\f\v\0$", "\n\t\r\u{c}\u{b}\0", true),
            (r"^\x41$", "A", true),
            (r"^é$", "é", true),
            (r"^\u{1F600}$", "😀", true),
            (r"^😀$", "😀", true),
            (r"^\cJ$", "\n", true),
            (r"^\(\)\[\]\{\}\|\*\+\?\^\$$", "()[]{}|*+?^$", true),
            // Shorthand classes.
            (r"^\d+$", "0123456789", true),
            (r"^\d$", "٣", false), // Arabic-Indic digit: \d is ASCII only.
            (r"^\D$", "a", true),
            (r"^\D$", "5", false),
            (r"^\w+$", "az_AZ09", true),
            (r"^\w$", "é", false),
            (r"^\W$", "é", true),
            (
                r"^\s+$",
                " \t\n\r\u{b}\u{c}\u{a0}\u{2003}\u{3000}\u{feff}\u{2028}",
                true,
            ),
            (r"^\s$", "x", false),
            (r"^\S$", "\u{a0}", false),
            (r"^\S$", "x", true),
            // Bracket classes.
            ("^[abc]$", "b", true),
            ("^[abc]$", "d", false),
            ("^[a-z]+$", "hello", true),
            ("^[a-z]+$", "Hello", false),
            ("^[^a-z]$", "A", true),
            ("^[^a-z]$", "q", false),
            ("^[^a-z]$", "😀", true),
            ("^[-a]$", "-", true),
            ("^[a-]$", "-", true),
            (r"^[\-]$", "-", true),
            (r"^[\d.]+$", "1.5", true),
            (r"^[\w-]+$", "a-b_c", true),
            (r"^[\s\S]$", "\n", true),
            (r"^[\b]$", "\u{8}", true),
            (r"^[\]]$", "]", true),
            ("^[[]$", "[", true),
            ("^[é-ü]$", "ö", true),
            ("^[é-ü]$", "e", false),
            (r"^[\u{1F600}-\u{1F64F}]$", "😃", true),
            ("[]", "anything", false),
            ("^[^]$", "\n", true),
            // Groups and alternation.
            ("^(ab)+$", "ababab", true),
            ("^(ab)+$", "ababa", false),
            ("^(?:a|bc)+$", "abca", true),
            ("^(?<year>\\d{4})-(?<m>\\d{2})$", "2026-11", true),
            ("^(cat|dog|)$", "", true),
            ("^(cat|dog)s?$", "dogs", true),
            ("^(cat|dog)s?$", "cow", false),
            ("a|b|c", "zzc", true),
            ("^()$", "", true),
            // Quantifier bounds.
            ("^a*$", "", true),
            ("^a*$", "aaaa", true),
            ("^a+$", "", false),
            ("^a?$", "", true),
            ("^a?$", "aa", false),
            ("^a{3}$", "aaa", true),
            ("^a{3}$", "aa", false),
            ("^a{3}$", "aaaa", false),
            ("^a{2,}$", "a", false),
            ("^a{2,}$", "aaaaaaa", true),
            ("^a{2,4}$", "a", false),
            ("^a{2,4}$", "aa", true),
            ("^a{2,4}$", "aaaa", true),
            ("^a{2,4}$", "aaaaa", false),
            ("^a{0}$", "", true),
            ("^a{0}$", "a", false),
            ("^a{0,1}b$", "b", true),
            ("^(?:ab){2}$", "abab", true),
            ("^(?:a|b){2,3}$", "aba", true),
            ("^(?:a|b){2,3}$", "abab", false),
            ("^a*?b+?c??d{1,2}?$", "abd", true),
            ("^(?:a*)*$", "aaa", true),
            ("^(?:a?)+b$", "b", true),
            ("^(?:)*$", "", true),
            ("^(?:){5}x$", "x", true),
            // Word boundaries.
            (r"\bcat\b", "a cat sat", true),
            (r"\bcat\b", "concatenate", false),
            (r"\Bcat\B", "concatenate", true),
            (r"^\b$", "", false),
            (r"\b", "é", false), // \w is ASCII, so é is not a word character.
            // Realistic schema patterns.
            (
                r"^[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,}$",
                "ada@example.org",
                true,
            ),
            (
                r"^[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,}$",
                "ada@example",
                false,
            ),
            (r"^\d{3}-\d{4}$", "555-1234", true),
            (r"^(\([0-9]{3}\))?[0-9]{3}-[0-9]{4}$", "(888)555-1212", true),
            (r"^(\([0-9]{3}\))?[0-9]{3}-[0-9]{4}$", "(800)FLOWERS", false),
            (
                r"^[A-Z]{2}\d{2}[A-Z0-9]{1,30}$",
                "DE44500105175407324931",
                true,
            ),
            // Unicode literals.
            ("日本", "こんにちは日本語", true),
            ("^日本$", "日本語", false),
            ("😀+", "x😀😀y", true),
            // Lone surrogate escapes match nothing (no scalar value is one).
            (r"\uD800", "anything", false),
            (r"\u{DC00}", "anything", false),
            (r"^a\uD800?b$", "ab", true),
            (r"^a\uD800b$", "ab", false),
            (r"[\uDC00]", "x", false),
            (r"^[\uD800-\uDFFF]$", "x", false),
            (r"^[^\uD800-\uDFFF]*$", "abc😀é", true),
            (r"^[a\uD800-\uDFFF]$", "a", true),
            (r"^[퟿-]+$", "\u{D7FF}\u{E000}", true),
            (r"^[퟿-]$", "\u{E001}", false),
            // Surrogate pairs still combine, in and out of a class.
            (r"^😀$", "😀", true),
            (r"^[😀-🙏]$", "😃", true),
            // A repeated group name across alternatives (ES2025).
            ("^(?<x>a)|(?<x>b)$", "b", true),
            ("^(?:(?<y>a)|(?<y>b))c$", "ac", true),
            // Counted repetition: counter and nested forms keep the bounds.
            ("^[a-z]{2,4}$", "a", false),
            ("^[a-z]{2,4}$", "abcd", true),
            ("^[a-z]{2,4}$", "abcde", false),
            ("^x{0,3}y$", "y", true),
            ("^x{0,3}y$", "xxxy", true),
            ("^x{0,3}y$", "xxxxy", false),
            ("^(?:ab){1,3}$", "ababab", true),
            ("^(?:ab){1,3}$", "abababab", false),
            ("^(?:ab){1,3}$", "aba", false),
            ("^(?:[a-c]{0,2}d)+$", "abdcdd", true),
            ("^(?:[a-c]{0,2}d)+$", "abcd", false),
            ("^(?:a{0,2})*$", "aaaaa", true),
            ("[a-z]{1,3}b", "zzzzzzb", true),
            ("^[a-z]{1,3}b", "zzzzb", false),
            (".{3,5}", "ab", false),
            (".{3,5}", "abcdefgh", true),
        ];
        for &(pattern, text, expected) in cases {
            assert_eq!(m(pattern, text), expected, "{pattern:?} on {text:?}");
        }
    }

    #[test]
    fn compile_errors_are_reported_not_literal() {
        let cases: &[(&str, &str)] = &[
            (r"(a)\1", "backreferences"),
            (r"(?<n>a)\k<n>", "named backreferences"),
            ("(?=a)", "lookahead"),
            ("(?!a)", "lookahead"),
            ("(?<=a)b", "lookbehind"),
            ("(?<!a)b", "lookbehind"),
            ("(?i)abc", "inline flags"),
            ("(?i:abc)", "inline flags"),
            (r"\p{L}", "property escapes"),
            (r"\P{L}", "property escapes"),
            (r"\01", "octal"),
            (r"\a", "unknown escape"),
            (r"\e", "unknown escape"),
            (r"\-", "unknown escape"),
            (r"\x4", "two hex digits"),
            (r"\u12", "four hex digits"),
            (r"\u{}", "malformed"),
            (r"\u{110000}", "beyond"),
            (r"\c1", "ASCII letter"),
            ("abc\\", "lone backslash"),
            ("[a", "unclosed character class"),
            ("(a", "unclosed group"),
            ("a)", "unmatched ')'"),
            ("*a", "nothing to repeat"),
            ("a**", "nothing to repeat"),
            ("a|+", "nothing to repeat"),
            ("^*", "cannot be quantified"),
            ("$+", "cannot be quantified"),
            (r"\b?", "cannot be quantified"),
            ("{", "unescaped '{'"),
            ("a{", "malformed"),
            ("a{1", "malformed"),
            ("a{,5}", "malformed"),
            ("a{x}", "malformed"),
            ("}", "unescaped '}'"),
            ("]", "unescaped ']'"),
            ("a{3,2}", "out of order"),
            ("[z-a]", "out of order"),
            (r"[\d-z]", "range endpoint"),
            (r"[a-\w]", "range endpoint"),
            (r"[\B]", "\\B"),
            ("(?<>a)", "group name"),
            ("(?<1a>a)", "group name"),
            ("(?<n>a)(?<n>b)", "duplicate group name"),
            ("(?<n>a)(?:(?<n>b)|c)", "duplicate group name"),
            ("(?:(?<n>a)|(?<n>b))(?<n>c)", "duplicate group name"),
            ("(?<n>(?<n>a)|b)", "duplicate group name"),
            ("a{2}{3}", "nothing to repeat"),
        ];
        for &(pattern, needle) in cases {
            let err = Regex::new(pattern)
                .err()
                .unwrap_or_else(|| panic!("{pattern:?} compiled"));
            assert!(
                err.to_string().contains(needle),
                "{pattern:?}: {err} (expected {needle:?})"
            );
        }
    }

    #[test]
    fn caps_are_enforced() {
        // Pattern length.
        let long = "a".repeat(MAX_PATTERN_BYTES + 1);
        assert!(Regex::new(&long).unwrap_err().to_string().contains("limit"));
        assert!(Regex::new(&"a".repeat(MAX_PATTERN_BYTES)).is_ok());

        // Nesting depth.
        let deep = format!(
            "{}a{}",
            "(".repeat(MAX_DEPTH + 1),
            ")".repeat(MAX_DEPTH + 1)
        );
        assert!(Regex::new(&deep)
            .unwrap_err()
            .to_string()
            .contains("nested"));
        let ok = format!("{}a{}", "(".repeat(MAX_DEPTH), ")".repeat(MAX_DEPTH));
        assert!(Regex::new(&ok).unwrap().is_match("a"));
        let deep_nc = "(?:".repeat(10_000);
        assert!(Regex::new(&deep_nc[..MAX_PATTERN_BYTES]).is_err());

        // Program size: counted repetition is sized before it is expanded.
        for pattern in [
            "(?:a{1000}){1000}",
            "a{1000}{1000}",
            "a{4294967295}",
            "a{99999999999999999999}",
            "(?:(?:(?:a{100}){100}){100}){100}",
            "a{0,10000}",
            "[a-z]{10000,}",
        ] {
            let started = std::time::Instant::now();
            assert!(Regex::new(pattern).is_err(), "{pattern}");
            assert!(started.elapsed().as_secs() < 2, "{pattern} took too long");
        }
        assert!(Regex::new("a{1000}").is_ok());
        assert!(Regex::new("(?:a{10}){10}")
            .unwrap()
            .is_match(&"a".repeat(100)));
        // Repeating an empty group any number of times compiles to nothing.
        assert!(Regex::new("(?:){4294967295}").unwrap().is_match(""));
    }

    #[test]
    fn pathological_patterns_run_in_linear_time() {
        let input = format!("{}!", "a".repeat(100_000));
        let started = std::time::Instant::now();
        for pattern in [
            "(a+)+$",
            "^(a+)+$",
            "^(a|a)*$",
            "^(a|aa)+$",
            "(a*)*b",
            "(?:a?){30}a{30}$",
            "^(\\w+\\s?)*$",
        ] {
            assert!(!m(pattern, &input), "{pattern}");
        }
        assert!(
            started.elapsed().as_secs() < 10,
            "pathological patterns took {:?}",
            started.elapsed()
        );
        assert!(m("(a+)+$", &"a".repeat(100_000)));
    }

    /// Thread insertions per input character for `pattern` on `text`.
    fn work_per_char(pattern: &str, text: &str) -> f64 {
        let (_, work) = Regex::new(pattern).unwrap().run(text);
        work as f64 / text.chars().count().max(1) as f64
    }

    #[test]
    fn counted_repetition_costs_about_as_much_as_plus() {
        // MISC-RX-2: the optional tail of `X{n,m}` used to keep about m
        // threads alive per character.
        let word = "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklm";
        let plus = work_per_char("^[a-z0-9]+$", word);
        for pattern in [
            "^[a-z0-9]{1,255}$",
            "^.{0,4900}$",
            "^[a-z0-9]{1,64}@?[a-z0-9]{0,255}$",
            "^(?:[a-z]|[0-9]){1,255}$",
            "^(?:ab|[a-z0-9]){0,300}$",
        ] {
            let counted = work_per_char(pattern, word);
            assert!(
                counted <= plus * 4.0,
                "{pattern}: {counted} insertions per char vs {plus} for `+`"
            );
        }
        // Unanchored: one live counter thread, not one per start position.
        let long = "a".repeat(100_000);
        let unanchored = work_per_char("[a-z]{1,4900}b", &long);
        assert!(unanchored < 16.0, "{unanchored} insertions per char");
    }

    #[test]
    fn counted_repetition_runs_in_bounded_time() {
        // Generous wall-clock bounds (debug builds are slow); the review
        // measured 70 s for 200k strings against `^.{0,4900}$`.
        let short: Vec<String> = (0..200_000).map(|i| format!("user{i}")).collect();
        let started = std::time::Instant::now();
        for pattern in ["^.{0,4900}$", "^[a-z0-9]{1,255}$"] {
            let re = Regex::new(pattern).unwrap();
            for s in &short {
                assert!(re.is_match(s));
            }
        }
        let re = Regex::new("[a-z]{1,4900}b").unwrap();
        assert!(!re.is_match(&"a".repeat(100_000)));
        assert!(
            started.elapsed().as_secs() < 20,
            "counted repetition took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn scratch_is_reused_across_patterns_of_different_sizes() {
        let big = Regex::new("^[0-9]{1,10}(?:ab){0,500}$").unwrap();
        let small = Regex::new("^a$").unwrap();
        assert!(big.is_match("12abab"));
        assert!(small.is_match("a"));
        assert!(!small.is_match("b"));
        assert!(big.is_match(&format!("1{}", "ab".repeat(500))));
        assert!(!big.is_match(&format!("1{}", "ab".repeat(501))));
        assert!(!small.is_match("aa"));
    }

    #[test]
    fn anchored_search_stops_early() {
        let re = Regex::new("^abc").unwrap();
        assert!(re.anchored);
        assert!(!Regex::new("x|^abc").unwrap().anchored);
        assert!(Regex::new("^a|^b").unwrap().anchored);
        let input = "x".repeat(1_000_000);
        assert!(!re.is_match(&input));
    }

    #[test]
    fn class_complement_skips_surrogates() {
        let r = complement(vec![('\0', '\u{D7FF}'), ('\u{E000}', '\u{10FFFF}')]);
        assert!(r.is_empty());
        // A range may span the surrogate gap: no `char` lives inside it.
        let r = complement(Vec::new());
        assert_eq!(&*r, &[('\0', '\u{10FFFF}')]);
        let r = complement(vec![('\u{10FFFF}', '\u{10FFFF}')]);
        assert_eq!(&*r, &[('\0', '\u{10FFFE}')]);
        let r = complement(vec![('\0', '\u{D7FF}')]);
        assert_eq!(&*r, &[('\u{E000}', '\u{10FFFF}')]);
    }
}
