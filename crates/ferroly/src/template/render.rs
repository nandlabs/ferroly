//! The evaluator: walks the AST against a context with Jinja2's scoping
//! rules, charging every loop iteration, call, nesting level and allocated
//! byte against the template's [`Limits`](super::Limits).

use std::cell::RefCell;
use std::cmp::Ordering;
use std::rc::Rc;
use std::sync::Arc;

use crate::codec::Value;

use super::ast::{Args, BinOp, CmpOp, Const, Expr, ExprKind, ForLoop, MacroDef, Stmt, Target};
use super::builtins::{Filter, Global, Test, LIST_METHODS, MAP_METHODS, STR_METHODS};
use super::error::{Fail, TemplateError, TemplateErrorKind, R};
use super::lexer::is_py_space;
use super::value::{
    compare, equals, spend, text_cost, Closure, Halt, List, LoopState, Map, NsCell, Num, Sink,
    Slots, TooDeep, VErr, Val,
};
use super::Inner;

mod filters;

const VAL_SIZE: usize = std::mem::size_of::<Val>();

/// What one character of a string costs once it is split into its own
/// value: the `Val` plus a small shared string allocation.
const CHAR_VAL_SIZE: usize = VAL_SIZE + 32;

#[derive(PartialEq, Eq)]
enum Flow {
    Normal,
    Break,
    Continue,
}

type Frame = Slots;

struct Renderer<'t> {
    tpl: &'t Inner,
    frames: Vec<Frame>,
    /// The first frame a lookup may see; frames below it (other than the
    /// template's top level) belong to a macro's caller.
    base: usize,
    ctx: Rc<Map>,
    out: String,
    depth: usize,
    iterations: usize,
    calls: usize,
    alloc: usize,
    /// What is left of the work budget (`Limits::max_steps`).
    steps: usize,
    /// The variables captured by the macro being called, if it was defined
    /// inside a loop or another macro.
    env: Option<Rc<Slots>>,
    namespaces: Vec<NsCell>,
}

pub(crate) fn render(tpl: &Inner, context: &Value) -> Result<String, TemplateError> {
    let ctx = match context {
        Value::Null => Ok(Rc::new(Map::new(Vec::new(), 1))),
        Value::Object(_) => match Val::from_value(context, 1) {
            Ok(Val::Map(m)) => Ok(m),
            Ok(_) => Err(Fail::ty("the context must be a map", 0)),
            Err(TooDeep) => Err(Fail::limit("max_depth", 0)),
        },
        _ => Err(Fail::ty("the context must be a map", 0)),
    };
    let ctx = match ctx {
        Ok(c) => c,
        Err(f) => return Err(f.locate(&tpl.source)),
    };
    let mut r = Renderer {
        tpl,
        frames: vec![Slots::new()],
        base: 0,
        ctx,
        out: String::new(),
        depth: 0,
        iterations: 0,
        calls: 0,
        alloc: 0,
        steps: tpl.limits.max_steps,
        env: None,
        namespaces: Vec::new(),
    };
    let result = r.exec(&tpl.body);
    // Namespaces are the one mutable value and can form reference cycles
    // (`ns.self = ns`); empty them all so nothing outlives the render.
    for ns in r.namespaces.drain(..) {
        if let Ok(mut entries) = ns.try_borrow_mut() {
            let taken = entries.take();
            drop(entries);
            drop(taken);
        }
    }
    r.frames.clear();
    match result {
        Ok(_) => Ok(std::mem::take(&mut r.out)),
        Err(f) => Err(f.locate(&tpl.source)),
    }
}

fn verr(e: VErr, pos: usize) -> Fail {
    match e {
        VErr::TooDeep => Fail::limit("max_depth", pos),
        VErr::TooLarge => Fail::limit("max_alloc_bytes", pos),
        VErr::Undefined => Fail::undefined("cannot encode an undefined value as JSON", pos),
        VErr::NotData(t) => Fail::ty(format!("cannot encode a {t} as JSON"), pos),
    }
}

fn deep(pos: usize) -> impl Fn(TooDeep) -> Fail {
    move |_| Fail::limit("max_depth", pos)
}

fn halt(pos: usize) -> impl Fn(Halt) -> Fail {
    move |h| match h {
        Halt::TooDeep => Fail::limit("max_depth", pos),
        Halt::Steps => Fail::limit("max_steps", pos),
    }
}

fn undefined_use(v: &Val, what: &str, pos: usize) -> Fail {
    let name = match v {
        Val::Undef(Some(n)) => format!("'{n}'"),
        _ => "value".to_string(),
    };
    Fail::undefined(format!("{name} is undefined; cannot {what} it"), pos)
}

/// Python's index normalisation: negative counts from the end.
fn norm_index(i: i128, len: usize) -> Option<usize> {
    let len = len as i128;
    let i = if i < 0 { i + len } else { i };
    if (0..len).contains(&i) {
        usize::try_from(i).ok()
    } else {
        None
    }
}

/// A slice resolved against a length: `count` positions from `start`,
/// `step` apart. Never materialised as a list of indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SliceSpec {
    start: usize,
    count: usize,
    step: i128,
}

impl SliceSpec {
    /// Python's slice index computation (`PySlice_AdjustIndices`); `step`
    /// is not zero.
    fn new(len: usize, start: Option<i128>, stop: Option<i128>, step: i128) -> SliceSpec {
        let n = i128::try_from(len).unwrap_or(i128::MAX);
        let (lower, upper) = if step > 0 { (0, n) } else { (-1, n - 1) };
        let adjust = |v: Option<i128>, default: i128| match v {
            None => default,
            Some(v) if v < 0 => v.saturating_add(n).max(lower),
            Some(v) => v.min(upper),
        };
        let start = adjust(start, if step > 0 { lower } else { upper });
        let stop = adjust(stop, if step > 0 { upper } else { lower });
        // Both bounds are within [-1, len], so the differences cannot
        // overflow; the step may be anything, so divide in u128.
        let span = if step > 0 { stop - start } else { start - stop };
        let count = if span <= 0 {
            0
        } else {
            (span.unsigned_abs() - 1) / step.unsigned_abs() + 1
        };
        SliceSpec {
            start: usize::try_from(start).unwrap_or(0),
            count: usize::try_from(count).unwrap_or(0),
            step,
        }
    }

    /// The step as a stride for `step_by` (only used when there is more than
    /// one position, so it is below the length).
    fn stride(&self) -> usize {
        usize::try_from(self.step.unsigned_abs()).unwrap_or(usize::MAX)
    }

    /// The items of `items` the slice selects.
    fn pick<'a, T>(&self, items: &'a [T]) -> Box<dyn Iterator<Item = &'a T> + 'a> {
        if self.count == 0 {
            return Box::new(std::iter::empty());
        }
        if self.step > 0 {
            Box::new(
                items
                    .iter()
                    .skip(self.start)
                    .step_by(self.stride())
                    .take(self.count),
            )
        } else {
            let from_end = items.len().saturating_sub(self.start + 1);
            Box::new(
                items
                    .iter()
                    .rev()
                    .skip(from_end)
                    .step_by(self.stride())
                    .take(self.count),
            )
        }
    }

    /// The text the slice selects from `s`, whose length in characters is
    /// `chars`. A forward slice with step 1 borrows; others are built.
    fn pick_str<'s>(&self, s: &'s str, chars: usize) -> std::borrow::Cow<'s, str> {
        if self.count == 0 {
            return "".into();
        }
        if self.step == 1 {
            return char_range(s, self.start, self.start + self.count).into();
        }
        if self.step > 0 {
            s.chars()
                .skip(self.start)
                .step_by(self.stride())
                .take(self.count)
                .collect::<String>()
                .into()
        } else {
            s.chars()
                .rev()
                .skip(chars.saturating_sub(self.start + 1))
                .step_by(self.stride())
                .take(self.count)
                .collect::<String>()
                .into()
        }
    }
}

/// `s[from..to]` counted in characters (clamped to the string).
fn char_range(s: &str, from: usize, to: usize) -> &str {
    let mut bounds = s
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(s.len()));
    let Some(a) = bounds.nth(from) else {
        return "";
    };
    let b = if to > from {
        bounds.nth(to - from - 1).unwrap_or(s.len())
    } else {
        a
    };
    s.get(a..b).unwrap_or("")
}

fn py_strip<'s>(s: &'s str, chars: Option<&str>, left: bool, right: bool) -> &'s str {
    let pred = |c: char| match chars {
        Some(set) => set.contains(c),
        None => is_py_space(c),
    };
    let s = if left { s.trim_start_matches(pred) } else { s };
    if right {
        s.trim_end_matches(pred)
    } else {
        s
    }
}

fn is_cased(c: char) -> bool {
    c.is_lowercase() || c.is_uppercase()
}

fn py_title(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_cased = false;
    for c in s.chars() {
        if prev_cased {
            out.extend(c.to_lowercase());
        } else {
            out.extend(c.to_uppercase());
        }
        prev_cased = is_cased(c);
    }
    out
}

fn py_capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => {
            let mut out: String = first.to_uppercase().collect();
            out.push_str(&chars.as_str().to_lowercase());
            out
        }
        None => String::new(),
    }
}

/// Python's `str.splitlines()` line boundaries.
fn py_splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut iter = s.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        let is_break = matches!(
            c,
            '\n' | '\r'
                | '\u{b}'
                | '\u{c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if is_break {
            out.push(&s[start..i]);
            let mut end = i + c.len_utf8();
            if c == '\r' {
                if let Some((j, '\n')) = iter.peek().copied() {
                    iter.next();
                    end = j + 1;
                }
            }
            start = end;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Python `str.split(sep, maxsplit)`.
fn py_split(s: &str, sep: Option<&str>, maxsplit: Option<usize>) -> Vec<String> {
    match sep {
        Some(sep) => match maxsplit {
            Some(n) => s
                .splitn(n.saturating_add(1), sep)
                .map(str::to_string)
                .collect(),
            None => s.split(sep).map(str::to_string).collect(),
        },
        None => {
            let mut out = Vec::new();
            let mut rest = s.trim_start_matches(is_py_space);
            while !rest.is_empty() {
                if maxsplit.is_some_and(|n| out.len() >= n) {
                    out.push(rest.to_string());
                    break;
                }
                let end = rest.find(is_py_space).unwrap_or(rest.len());
                out.push(rest[..end].to_string());
                rest = rest[end..].trim_start_matches(is_py_space);
            }
            out
        }
    }
}

/// Python `str.rsplit(sep, maxsplit)`.
fn py_rsplit(s: &str, sep: Option<&str>, maxsplit: Option<usize>) -> Vec<String> {
    match (sep, maxsplit) {
        (_, None) => py_split(s, sep, None),
        (Some(sep), Some(n)) => {
            let mut v: Vec<String> = s
                .rsplitn(n.saturating_add(1), sep)
                .map(str::to_string)
                .collect();
            v.reverse();
            v
        }
        (None, Some(n)) => {
            let mut out = Vec::new();
            let mut rest = s.trim_end_matches(is_py_space);
            while !rest.is_empty() {
                if out.len() >= n {
                    out.push(rest.to_string());
                    break;
                }
                let start = rest.rfind(is_py_space).map_or(0, |i| {
                    i + rest[i..].chars().next().map_or(1, char::len_utf8)
                });
                out.push(rest[start..].to_string());
                rest = rest[..start].trim_end_matches(is_py_space);
            }
            out.reverse();
            out
        }
    }
}

/// Python `int(text, base)`: surrounding whitespace, a sign, underscores
/// between digits and (for a matching or zero base) a radix prefix.
fn py_parse_int(text: &str, base: u32) -> Option<i128> {
    let t = text.trim_matches(is_py_space);
    let (neg, t) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    // One sign only: `i128::from_str_radix` would accept a second.
    if t.starts_with(['+', '-']) {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    let (radix, digits) = match base {
        0 => {
            if let Some(d) = lower.strip_prefix("0x") {
                (16, d.to_string())
            } else if let Some(d) = lower.strip_prefix("0o") {
                (8, d.to_string())
            } else if let Some(d) = lower.strip_prefix("0b") {
                (2, d.to_string())
            } else {
                (10, lower.clone())
            }
        }
        16 => (16, lower.strip_prefix("0x").unwrap_or(&lower).to_string()),
        8 => (8, lower.strip_prefix("0o").unwrap_or(&lower).to_string()),
        2 => (2, lower.strip_prefix("0b").unwrap_or(&lower).to_string()),
        b if (2..=36).contains(&b) => (b, lower.clone()),
        _ => return None,
    };
    let digits = digits.strip_prefix('_').unwrap_or(&digits);
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
    {
        return None;
    }
    let clean: String = digits.chars().filter(|c| *c != '_').collect();
    let v = i128::from_str_radix(&clean, radix).ok()?;
    Some(if neg { -v } else { v })
}

fn py_parse_float(text: &str) -> Option<f64> {
    let t = text.trim_matches(is_py_space);
    if t.contains("__") || t.starts_with('_') || t.ends_with('_') {
        return None;
    }
    let clean: String = t.chars().filter(|c| *c != '_').collect();
    match clean.to_ascii_lowercase().as_str() {
        "inf" | "+inf" | "infinity" | "+infinity" => Some(f64::INFINITY),
        "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
        "nan" | "+nan" | "-nan" => Some(f64::NAN),
        _ => clean.parse().ok(),
    }
}

fn py_round(f: f64, digits: i32) -> f64 {
    if !f.is_finite() {
        return f;
    }
    if digits >= 0 {
        // Rust's fixed-precision formatting is correctly rounded with ties
        // to even, which is what Python's round() does.
        let p = digits.min(300).unsigned_abs() as usize;
        format!("{f:.p$}").parse().unwrap_or(f)
    } else {
        let scale = 10f64.powi(-digits);
        if scale.is_infinite() {
            // Every finite float rounds to zero at this scale (keeping its
            // sign, as Python's does).
            return 0.0 * f;
        }
        let x = f / scale;
        let r = x.round();
        let r = if (x - x.trunc()).abs() == 0.5 && r % 2.0 != 0.0 {
            r - x.signum()
        } else {
            r
        };
        r * scale
    }
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// One part of a dotted attribute path: an index when it is all digits (as
/// Jinja2's `str.isdigit` check), a key otherwise.
fn path_key(part: &str) -> Val {
    if !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(i) = part.parse::<i128>() {
            return Val::Int(i);
        }
    }
    Val::str(part)
}

/// Positional and keyword arguments after evaluation.
struct CallArgs {
    pos: Vec<Val>,
    kw: Vec<(Arc<str>, Val)>,
}

impl CallArgs {
    /// Binds to named parameters: positional first, then keywords.
    fn bind<const N: usize>(
        self,
        func: &str,
        params: [&str; N],
        pos: usize,
    ) -> R<[Option<Val>; N]> {
        if self.pos.len() > N {
            return Err(Fail::ty(
                format!(
                    "{func}() takes at most {N} argument(s), {} given",
                    self.pos.len()
                ),
                pos,
            ));
        }
        let mut out: [Option<Val>; N] = std::array::from_fn(|_| None);
        for (slot, v) in out.iter_mut().zip(self.pos) {
            *slot = Some(v);
        }
        for (k, v) in self.kw {
            let Some(i) = params.iter().position(|p| **p == *k) else {
                return Err(Fail::ty(
                    format!("{func}() got an unexpected keyword argument '{k}'"),
                    pos,
                ));
            };
            if out[i].is_some() {
                return Err(Fail::ty(
                    format!("{func}() got multiple values for argument '{k}'"),
                    pos,
                ));
            }
            out[i] = Some(v);
        }
        Ok(out)
    }

    fn no_kwargs(&self, func: &str, pos: usize) -> R<()> {
        match self.kw.first() {
            Some((k, _)) => Err(Fail::ty(
                format!("{func}() got an unexpected keyword argument '{k}'"),
                pos,
            )),
            None => Ok(()),
        }
    }
}

impl<'t> Renderer<'t> {
    // ---- budgets -------------------------------------------------------

    fn enter(&mut self, pos: usize) -> R<()> {
        self.depth += 1;
        if self.depth > self.tpl.limits.max_depth {
            return Err(Fail::limit("max_depth", pos));
        }
        Ok(())
    }

    fn tick_iter(&mut self, n: usize, pos: usize) -> R<()> {
        self.iterations = self.iterations.saturating_add(n);
        if self.iterations > self.tpl.limits.max_iterations {
            return Err(Fail::limit("max_iterations", pos));
        }
        Ok(())
    }

    fn tick_call(&mut self, pos: usize) -> R<()> {
        self.calls += 1;
        if self.calls > self.tpl.limits.max_calls {
            return Err(Fail::limit("max_calls", pos));
        }
        Ok(())
    }

    fn charge(&mut self, bytes: usize, pos: usize) -> R<()> {
        match self.alloc.checked_add(bytes) {
            Some(total) if total <= self.tpl.limits.max_alloc_bytes => {
                self.alloc = total;
                Ok(())
            }
            _ => Err(Fail::limit("max_alloc_bytes", pos)),
        }
    }

    /// Charges `count` items of `size` bytes; a product that does not fit
    /// in a `usize` is over any budget.
    fn charge_n(&mut self, count: usize, size: usize, pos: usize) -> R<()> {
        match count.checked_mul(size) {
            Some(bytes) => self.charge(bytes, pos),
            None => Err(Fail::limit("max_alloc_bytes", pos)),
        }
    }

    /// Spends `n` steps of the work budget.
    fn step(&mut self, n: usize, pos: usize) -> R<()> {
        spend(&mut self.steps, n).map_err(halt(pos))
    }

    /// Spends the steps for scanning `bytes` bytes of text.
    fn step_text(&mut self, bytes: usize, pos: usize) -> R<()> {
        self.step(text_cost(bytes), pos)
    }

    fn equals(&mut self, a: &Val, b: &Val, pos: usize) -> R<bool> {
        equals(a, b, 0, &mut self.steps).map_err(halt(pos))
    }

    fn sink(&self) -> Sink {
        Sink::new(self.tpl.limits.max_alloc_bytes.saturating_sub(self.alloc))
    }

    fn new_str(&mut self, s: String, pos: usize) -> R<Val> {
        self.charge(s.len(), pos)?;
        Ok(Val::str(s))
    }

    fn new_list(&mut self, items: Vec<Val>, pos: usize) -> R<Val> {
        self.charge(items.len().saturating_mul(VAL_SIZE), pos)?;
        Val::list(items).map_err(deep(pos))
    }

    fn new_map(&mut self, entries: Vec<(Arc<str>, Val)>, pos: usize) -> R<Val> {
        self.charge(entries.len().saturating_mul(VAL_SIZE * 2), pos)?;
        Val::map(entries).map_err(deep(pos))
    }

    /// `str(v)`, charged against the allocation budget.
    fn text(&mut self, v: &Val, pos: usize) -> R<Arc<str>> {
        match v {
            Val::Str(s) => Ok(s.clone()),
            Val::Undef(_) => Ok(Arc::from("")),
            _ => {
                let mut sink = self.sink();
                v.write_str(&mut sink, 0).map_err(|e| verr(e, pos))?;
                self.charge(sink.out.len(), pos)?;
                Ok(Arc::from(sink.out))
            }
        }
    }

    fn write(&mut self, s: &str, pos: usize) -> R<()> {
        if self.out.len().saturating_add(s.len()) > self.tpl.limits.max_output_bytes {
            return Err(Fail::limit("max_output_bytes", pos));
        }
        self.out.push_str(s);
        Ok(())
    }

    // ---- variables -----------------------------------------------------

    fn lookup(&self, name: &Arc<str>) -> Val {
        for frame in self.frames[self.base..].iter().rev() {
            if let Some(v) = frame.get(name) {
                return v.clone();
            }
        }
        if let Some(v) = self.env.as_ref().and_then(|env| env.get(name)) {
            return v.clone();
        }
        if self.base > 0 {
            if let Some(v) = self.frames.first().and_then(|f| f.get(name)) {
                return v.clone();
            }
        }
        if let Some(v) = self.ctx.get(name) {
            return v.clone();
        }
        if let Some(f) = self.tpl.global(name) {
            return Val::Func(f);
        }
        Val::Undef(Some(name.clone()))
    }

    fn set_var(&mut self, name: Arc<str>, v: Val) {
        if let Some(frame) = self.frames.last_mut() {
            frame.set(name, v);
        }
    }

    fn assign(&mut self, target: &Target, v: Val, pos: usize) -> R<()> {
        match target {
            Target::Name(n) => self.set_var(n.clone(), v),
            Target::Tuple(names) => {
                let items = self.unpack(&v, names.len(), pos)?;
                for (n, item) in names.iter().zip(items) {
                    self.set_var(n.clone(), item);
                }
            }
            Target::Attr(ns, attr) => match self.lookup(ns) {
                Val::Ns(cell) => {
                    let mut entries = cell
                        .try_borrow_mut()
                        .map_err(|_| Fail::ty("namespace is in use", pos))?;
                    entries.set(attr.clone(), v);
                }
                other => {
                    return Err(Fail::ty(
                        format!("cannot assign attribute '{attr}' on a {}; only a namespace() can be assigned to", other.type_name()),
                        pos,
                    ))
                }
            },
        }
        Ok(())
    }

    /// The `n` values to bind to `a, b, ...`; the count is checked before
    /// anything is built, so unpacking a huge string costs nothing.
    fn unpack(&mut self, v: &Val, n: usize, pos: usize) -> R<Vec<Val>> {
        let count = match v {
            Val::List(l) => l.items.len(),
            Val::Str(s) => {
                self.step_text(s.len(), pos)?;
                s.chars().take(n.saturating_add(1)).count()
            }
            other => {
                return Err(Fail::ty(
                    format!("cannot unpack a {} into {n} names", other.type_name()),
                    pos,
                ))
            }
        };
        if count != n {
            let found = match v {
                Val::Str(s) if count > n => s.chars().count(),
                _ => count,
            };
            return Err(Fail::ty(
                format!("cannot unpack {found} values into {n} names"),
                pos,
            ));
        }
        self.step(n, pos)?;
        Ok(match v {
            Val::List(l) => l.items.clone(),
            Val::Str(s) => s.chars().map(|c| Val::str(c.to_string())).collect(),
            _ => Vec::new(),
        })
    }

    // ---- statements ----------------------------------------------------

    fn exec(&mut self, stmts: &[Stmt]) -> R<Flow> {
        for stmt in stmts {
            self.step(1, 0)?;
            let flow = match stmt {
                Stmt::Text(t, pos) => {
                    self.write(t, *pos)?;
                    Flow::Normal
                }
                Stmt::Output(e) => {
                    let v = self.eval(e)?;
                    match &v {
                        Val::Str(s) => self.write(s, e.pos)?,
                        Val::Undef(_) => {}
                        _ => {
                            let mut sink = self.sink();
                            v.write_str(&mut sink, 0).map_err(|err| verr(err, e.pos))?;
                            self.write(&sink.out, e.pos)?;
                        }
                    }
                    Flow::Normal
                }
                Stmt::If {
                    branches,
                    otherwise,
                } => {
                    let mut chosen: &[Stmt] = otherwise;
                    for (cond, body) in branches {
                        if self.eval(cond)?.truthy() {
                            chosen = body;
                            break;
                        }
                    }
                    self.block(chosen, 0)?
                }
                Stmt::For(f) => self.exec_for(f)?,
                Stmt::Set { target, value, pos } => {
                    let v = self.eval(value)?;
                    self.assign(target, v, *pos)?;
                    Flow::Normal
                }
                Stmt::SetBlock { name, body } => {
                    let saved = std::mem::take(&mut self.out);
                    let flow = self.block(body, 0);
                    let captured = std::mem::replace(&mut self.out, saved);
                    let flow = flow?;
                    let v = self.new_str(captured, 0)?;
                    self.set_var(name.clone(), v);
                    flow
                }
                Stmt::Macro(def) => {
                    let m = self.closure(def)?;
                    self.set_var(def.name.clone(), m);
                    Flow::Normal
                }
                Stmt::Break => Flow::Break,
                Stmt::Continue => Flow::Continue,
            };
            if flow != Flow::Normal {
                return Ok(flow);
            }
        }
        Ok(Flow::Normal)
    }

    fn block(&mut self, body: &[Stmt], pos: usize) -> R<Flow> {
        if body.is_empty() {
            return Ok(Flow::Normal);
        }
        self.enter(pos)?;
        let r = self.exec(body);
        self.depth -= 1;
        r
    }

    /// A macro value for `def`. A macro defined inside a loop or another
    /// macro keeps the variables visible there (their values at the
    /// definition), as a Jinja2 closure does; one defined at the top level
    /// sees the top-level variables as they are when it is called.
    fn closure(&mut self, def: &Arc<MacroDef>) -> R<Val> {
        let from = self.base.max(1);
        let inner = self.frames.get(from..).unwrap_or(&[]);
        let outer = self.env.as_deref();
        let size = inner.iter().map(Slots::len).sum::<usize>() + outer.map_or(0, Slots::len);
        if inner.is_empty() && outer.is_none() {
            return Ok(Val::Macro(Rc::new(Closure {
                def: def.clone(),
                env: None,
                depth: 1,
            })));
        }
        self.step(size, 0)?;
        let mut env = self.env.as_deref().cloned().unwrap_or_default();
        for frame in self.frames.get(from..).unwrap_or(&[]) {
            for (k, v) in frame.entries() {
                env.set(k.clone(), v.clone());
            }
        }
        let depth = env
            .entries()
            .iter()
            .map(|(_, v)| v.depth())
            .max()
            .unwrap_or(0)
            + 1;
        if depth > super::value::MAX_VALUE_DEPTH {
            return Err(Fail::limit("max_depth", 0));
        }
        Ok(Val::Macro(Rc::new(Closure {
            def: def.clone(),
            env: Some(Rc::new(env)),
            depth,
        })))
    }

    fn exec_for(&mut self, f: &ForLoop) -> R<Flow> {
        let iterable = self.eval(&f.iter)?;
        // A list is walked in place; only maps and strings are turned into
        // a list of keys or characters first.
        let mut seq: Rc<List> = match &iterable {
            Val::List(l) => l.clone(),
            other => {
                let items = self.iterate(other, f.iter.pos)?;
                List::shared(items).map_err(deep(f.pos))?
            }
        };
        if let Some(cond) = &f.filter {
            let mut kept = Vec::new();
            for item in &seq.items {
                self.tick_iter(1, f.pos)?;
                self.frames.push(Slots::new());
                let keep = self
                    .bind_loop_target(&f.target, item, f.pos)
                    .and_then(|_| self.eval(cond));
                self.frames.pop();
                if keep?.truthy() {
                    kept.push(item.clone());
                }
            }
            seq = List::shared(kept).map_err(deep(f.pos))?;
        }
        if seq.items.is_empty() {
            return self.block(&f.otherwise, f.pos);
        }
        // `seq` is a list, so it is within the value depth limit already;
        // the `loop` object counts as deep as the list it walks.
        for index0 in 0..seq.items.len() {
            self.tick_iter(1, f.pos)?;
            self.frames.push(Slots::new());
            let item = seq.items[index0].clone();
            let flow = self
                .bind_loop_target(&f.target, &item, f.pos)
                .and_then(|_| {
                    let state = Rc::new(LoopState {
                        index0,
                        items: seq.clone(),
                    });
                    self.set_var(Arc::from("loop"), Val::Loop(state));
                    self.block(&f.body, f.pos)
                });
            self.frames.pop();
            if flow? == Flow::Break {
                break;
            }
        }
        Ok(Flow::Normal)
    }

    fn bind_loop_target(&mut self, target: &Target, item: &Val, pos: usize) -> R<()> {
        self.assign(target, item.clone(), pos)
    }

    /// The items a `for` loop (or an iterating filter) walks over, charged
    /// as work in proportion to their number.
    fn iterate(&mut self, v: &Val, pos: usize) -> R<Vec<Val>> {
        Ok(match v {
            Val::List(l) => {
                self.step(l.items.len(), pos)?;
                l.items.clone()
            }
            Val::Map(m) => {
                self.step(m.entries.len(), pos)?;
                m.entries.iter().map(|(k, _)| Val::Str(k.clone())).collect()
            }
            Val::Str(s) => {
                let chars = s.chars().count();
                self.step(chars, pos)?;
                self.charge_n(chars, CHAR_VAL_SIZE, pos)?;
                s.chars().map(|c| Val::str(c.to_string())).collect()
            }
            Val::Undef(_) => Vec::new(),
            other => {
                return Err(Fail::ty(
                    format!("a {} is not iterable", other.type_name()),
                    pos,
                ))
            }
        })
    }

    // ---- expressions ---------------------------------------------------

    fn eval(&mut self, e: &Expr) -> R<Val> {
        self.step(1, e.pos)?;
        self.enter(e.pos)?;
        let r = self.eval_inner(e);
        self.depth -= 1;
        r
    }

    fn eval_args(&mut self, args: &Args) -> R<CallArgs> {
        let mut pos = Vec::with_capacity(args.pos.len());
        for a in &args.pos {
            pos.push(self.eval(a)?);
        }
        let mut kw = Vec::with_capacity(args.kw.len());
        for (k, a) in &args.kw {
            kw.push((k.clone(), self.eval(a)?));
        }
        Ok(CallArgs { pos, kw })
    }

    // Every arm that needs more than a line lives in its own method: the
    // recursion runs through `eval` / `eval_inner`, so keeping their frames
    // small is what bounds the stack a deeply nested template can use.
    fn eval_inner(&mut self, e: &Expr) -> R<Val> {
        let pos = e.pos;
        match &e.kind {
            ExprKind::Const(c) => Ok(match c {
                Const::None => Val::None,
                Const::Bool(b) => Val::Bool(*b),
                Const::Int(i) => Val::Int(*i),
                Const::Float(f) => Val::Float(*f),
                Const::Str(s) => Val::Str(s.clone()),
            }),
            ExprKind::Name(n) => Ok(self.lookup(n)),
            ExprKind::List(items) => self.eval_list(items, pos),
            ExprKind::Map(pairs) => self.eval_map(pairs, pos),
            ExprKind::Attr(obj, name) => {
                let o = self.eval(obj)?;
                self.get_attr(&o, name, pos)
            }
            ExprKind::Item(obj, index) => {
                let o = self.eval(obj)?;
                let i = self.eval(index)?;
                self.get_item(&o, &i, pos)
            }
            ExprKind::Slice {
                obj,
                start,
                stop,
                step,
            } => self.eval_slice(obj, [start, stop, step], pos),
            ExprKind::Call { callee, args } => self.eval_call(callee, args, pos),
            ExprKind::Filter {
                value,
                filter,
                args,
            } => self.eval_filter(value, *filter, args, pos),
            ExprKind::Test {
                value,
                test,
                negated,
                args,
            } => self.eval_test(value, *test, *negated, args, pos),
            ExprKind::Not(x) => Ok(Val::Bool(!self.eval(x)?.truthy())),
            ExprKind::Neg(x) => self.eval_sign(x, true, pos),
            ExprKind::Pos(x) => self.eval_sign(x, false, pos),
            ExprKind::And(items) => self.eval_bool_chain(items, false),
            ExprKind::Or(items) => self.eval_bool_chain(items, true),
            ExprKind::Chain(first, rest) => self.eval_chain(first, rest),
            ExprKind::Concat(parts) => self.eval_concat(parts, pos),
            ExprKind::Compare(first, ops) => self.eval_compare(first, ops),
            ExprKind::Cond {
                cond,
                then,
                otherwise,
            } => {
                if self.eval(cond)?.truthy() {
                    self.eval(then)
                } else if let Some(o) = otherwise {
                    self.eval(o)
                } else {
                    Ok(Val::Undef(None))
                }
            }
        }
    }

    fn eval_list(&mut self, items: &[Expr], pos: usize) -> R<Val> {
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            out.push(self.eval(item)?);
        }
        self.new_list(out, pos)
    }

    fn eval_map(&mut self, pairs: &[(Expr, Expr)], pos: usize) -> R<Val> {
        let mut out = Slots::new();
        for (k, v) in pairs {
            let key = match self.eval(k)? {
                Val::Str(s) => s,
                other => {
                    return Err(Fail::ty(
                        format!("map keys must be strings, not a {}", other.type_name()),
                        k.pos,
                    ))
                }
            };
            let val = self.eval(v)?;
            out.set(key, val);
        }
        self.new_map(out.into_entries(), pos)
    }

    fn eval_slice(&mut self, obj: &Expr, parts: [&Option<Box<Expr>>; 3], pos: usize) -> R<Val> {
        let o = self.eval(obj)?;
        let mut bounds = [None, None, None];
        for (slot, part) in bounds.iter_mut().zip(parts) {
            if let Some(p) = part {
                *slot = match self.eval(p)? {
                    Val::None | Val::Undef(_) => None,
                    v => Some(v.as_int().ok_or_else(|| {
                        Fail::ty("slice indices must be integers or none", p.pos)
                    })?),
                };
            }
        }
        self.slice(&o, bounds[0], bounds[1], bounds[2], pos)
    }

    fn eval_call(&mut self, callee: &Expr, args: &Args, pos: usize) -> R<Val> {
        let f = self.eval(callee)?;
        let args = self.eval_args(args)?;
        self.call(&f, args, pos)
    }

    fn eval_filter(&mut self, value: &Expr, filter: Filter, args: &Args, pos: usize) -> R<Val> {
        let v = self.eval(value)?;
        let args = self.eval_args(args)?;
        self.tick_call(pos)?;
        self.filter(filter, v, args, pos)
    }

    fn eval_test(
        &mut self,
        value: &Expr,
        test: Test,
        negated: bool,
        args: &Args,
        pos: usize,
    ) -> R<Val> {
        let v = self.eval(value)?;
        let args = self.eval_args(args)?;
        self.tick_call(pos)?;
        let r = self.test(test, &v, args, pos)?;
        Ok(Val::Bool(r != negated))
    }

    fn eval_sign(&mut self, x: &Expr, neg: bool, pos: usize) -> R<Val> {
        let v = self.eval(x)?;
        match v.as_num() {
            Some(Num::Int(i)) if neg => i
                .checked_neg()
                .map(Val::Int)
                .ok_or_else(|| Fail::ty("integer overflow", pos)),
            Some(Num::Int(i)) => Ok(Val::Int(i)),
            Some(Num::Float(f)) => Ok(Val::Float(if neg { -f } else { f })),
            None if v.is_undefined() => Err(undefined_use(&v, "negate", pos)),
            None => Err(Fail::ty(
                format!("bad operand type for unary minus: {}", v.type_name()),
                pos,
            )),
        }
    }

    /// `and` returns the first falsy operand (or the last); `or` the first
    /// truthy one (or the last).
    fn eval_bool_chain(&mut self, items: &[Expr], stop_when: bool) -> R<Val> {
        let mut last = Val::Undef(None);
        for item in items {
            last = self.eval(item)?;
            if last.truthy() == stop_when {
                return Ok(last);
            }
        }
        Ok(last)
    }

    fn eval_chain(&mut self, first: &Expr, rest: &[(BinOp, usize, Expr)]) -> R<Val> {
        let mut acc = self.eval(first)?;
        for (op, op_pos, e) in rest {
            let right = self.eval(e)?;
            acc = self.binop(*op, &acc, &right, *op_pos)?;
        }
        Ok(acc)
    }

    fn eval_concat(&mut self, parts: &[Expr], pos: usize) -> R<Val> {
        let mut pieces = Vec::with_capacity(parts.len());
        for p in parts {
            let v = self.eval(p)?;
            pieces.push(self.text(&v, p.pos)?);
        }
        let total = pieces
            .iter()
            .map(|p| p.len())
            .fold(0usize, usize::saturating_add);
        self.charge(total, pos)?;
        self.step_text(total, pos)?;
        let mut s = String::with_capacity(total);
        for p in &pieces {
            s.push_str(p);
        }
        Ok(Val::str(s))
    }

    fn eval_compare(&mut self, first: &Expr, ops: &[(CmpOp, Expr)]) -> R<Val> {
        let mut left = self.eval(first)?;
        for (op, e) in ops {
            let right = self.eval(e)?;
            if !self.cmp(*op, &left, &right, e.pos)? {
                return Ok(Val::Bool(false));
            }
            left = right;
        }
        Ok(Val::Bool(true))
    }

    fn cmp(&mut self, op: CmpOp, a: &Val, b: &Val, pos: usize) -> R<bool> {
        Ok(match op {
            CmpOp::Eq => self.equals(a, b, pos)?,
            CmpOp::Ne => !self.equals(a, b, pos)?,
            CmpOp::In => self.contains(b, a, pos)?,
            CmpOp::NotIn => !self.contains(b, a, pos)?,
            CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge => {
                let ord = self.order(a, b, op_symbol(op), pos)?;
                match op {
                    CmpOp::Lt => ord == Ordering::Less,
                    CmpOp::Le => ord != Ordering::Greater,
                    CmpOp::Gt => ord == Ordering::Greater,
                    _ => ord != Ordering::Less,
                }
            }
        })
    }

    fn order(&mut self, a: &Val, b: &Val, sym: &str, pos: usize) -> R<Ordering> {
        for v in [a, b] {
            if v.is_undefined() {
                return Err(undefined_use(v, "compare", pos));
            }
        }
        match compare(a, b, 0, &mut self.steps).map_err(halt(pos))? {
            Some(o) => Ok(o),
            None => {
                if matches!((a.as_num(), b.as_num()), (Some(_), Some(_))) {
                    // NaN: every ordering comparison is false.
                    return Ok(if sym.starts_with('<') {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    });
                }
                Err(Fail::ty(
                    format!(
                        "'{sym}' is not supported between a {} and a {}",
                        a.type_name(),
                        b.type_name()
                    ),
                    pos,
                ))
            }
        }
    }

    fn contains(&mut self, container: &Val, item: &Val, pos: usize) -> R<bool> {
        match container {
            Val::Str(s) => match item {
                Val::Str(x) => {
                    self.step_text(s.len(), pos)?;
                    Ok(s.contains(&**x))
                }
                other => Err(Fail::ty(
                    format!(
                        "'in <string>' requires a string, not a {}",
                        other.type_name()
                    ),
                    pos,
                )),
            },
            Val::List(l) => {
                self.tick_iter(l.items.len(), pos)?;
                for x in &l.items {
                    if self.equals(x, item, pos)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Val::Map(m) => Ok(match item {
                Val::Str(k) => m.get(k).is_some(),
                _ => false,
            }),
            Val::Undef(_) => Ok(false),
            other => Err(Fail::ty(
                format!("a {} does not support 'in'", other.type_name()),
                pos,
            )),
        }
    }

    fn binop(&mut self, op: BinOp, a: &Val, b: &Val, pos: usize) -> R<Val> {
        for v in [a, b] {
            if v.is_undefined() {
                return Err(undefined_use(v, "use in arithmetic", pos));
            }
        }
        match (op, a, b) {
            (BinOp::Add, Val::Str(x), Val::Str(y)) => {
                let len = x.len().saturating_add(y.len());
                self.charge(len, pos)?;
                self.step_text(len, pos)?;
                let mut s = String::with_capacity(len);
                s.push_str(x);
                s.push_str(y);
                return Ok(Val::str(s));
            }
            (BinOp::Add, Val::List(x), Val::List(y)) => {
                let len = x.items.len().saturating_add(y.items.len());
                self.charge_n(len, VAL_SIZE, pos)?;
                self.step(len, pos)?;
                let mut items = Vec::with_capacity(len);
                items.extend(x.items.iter().cloned());
                items.extend(y.items.iter().cloned());
                return Val::list(items).map_err(deep(pos));
            }
            (BinOp::Mul, Val::Str(s), n) | (BinOp::Mul, n, Val::Str(s)) if n.as_int().is_some() => {
                let n = usize::try_from(n.as_int().unwrap_or(0).max(0)).unwrap_or(usize::MAX);
                if s.is_empty() || n == 0 {
                    return Ok(Val::str(""));
                }
                self.charge_n(s.len(), n, pos)?;
                self.step_text(s.len().saturating_mul(n), pos)?;
                return Ok(Val::str(s.repeat(n)));
            }
            (BinOp::Mul, Val::List(l), n) | (BinOp::Mul, n, Val::List(l))
                if n.as_int().is_some() =>
            {
                let n = usize::try_from(n.as_int().unwrap_or(0).max(0)).unwrap_or(usize::MAX);
                // Nothing to repeat: `[] * 10**17` is `[]` at once, not a
                // loop that runs 10**17 times copying nothing.
                if l.items.is_empty() || n == 0 {
                    return Val::list(Vec::new()).map_err(deep(pos));
                }
                let Some(len) = l.items.len().checked_mul(n) else {
                    return Err(Fail::limit("max_alloc_bytes", pos));
                };
                self.charge_n(len, VAL_SIZE, pos)?;
                self.step(len, pos)?;
                let mut items = Vec::with_capacity(len);
                for _ in 0..n {
                    items.extend(l.items.iter().cloned());
                }
                return Val::list(items).map_err(deep(pos));
            }
            (BinOp::Mod, Val::Str(_), _) => {
                return Err(Fail::new(
                    TemplateErrorKind::Unsupported("printf-style '%' string formatting".into()),
                    pos,
                ))
            }
            _ => {}
        }
        let (Some(x), Some(y)) = (a.as_num(), b.as_num()) else {
            return Err(Fail::ty(
                format!(
                    "unsupported operand types for '{}': {} and {}",
                    bin_symbol(op),
                    a.type_name(),
                    b.type_name()
                ),
                pos,
            ));
        };
        num_op(op, x, y, pos)
    }

    // ---- attribute and item access -------------------------------------

    fn get_attr(&mut self, obj: &Val, name: &str, pos: usize) -> R<Val> {
        let method = |table: &[&'static str]| table.iter().find(|m| **m == name).copied();
        Ok(match obj {
            Val::Undef(_) => {
                return Err(undefined_use(
                    obj,
                    &format!("read attribute '{name}' of"),
                    pos,
                ))
            }
            Val::Map(m) => match method(MAP_METHODS) {
                Some(meth) => Val::Method(Rc::new((obj.clone(), meth))),
                None => m.get(name).cloned().unwrap_or(Val::Undef(None)),
            },
            Val::Ns(ns) => ns
                .try_borrow()
                .ok()
                .and_then(|e| e.get(name).cloned())
                .unwrap_or(Val::Undef(None)),
            Val::Loop(l) => {
                let len = l.items.items.len();
                let i = l.index0;
                let int = |v: usize| Val::Int(v as i128);
                match name {
                    "index" => int(i + 1),
                    "index0" => int(i),
                    "revindex" => int(len - i),
                    "revindex0" => int(len - i - 1),
                    "first" => Val::Bool(i == 0),
                    "last" => Val::Bool(i + 1 == len),
                    "length" => int(len),
                    "depth" => int(1),
                    "depth0" => int(0),
                    "previtem" => i
                        .checked_sub(1)
                        .and_then(|p| l.items.items.get(p).cloned())
                        .unwrap_or(Val::Undef(None)),
                    "nextitem" => l
                        .items
                        .items
                        .get(i + 1)
                        .cloned()
                        .unwrap_or(Val::Undef(None)),
                    "cycle" => Val::Method(Rc::new((obj.clone(), "cycle"))),
                    _ => Val::Undef(None),
                }
            }
            Val::Str(_) => match method(STR_METHODS) {
                Some(meth) => Val::Method(Rc::new((obj.clone(), meth))),
                None => Val::Undef(None),
            },
            Val::List(_) => match method(LIST_METHODS) {
                Some(meth) => Val::Method(Rc::new((obj.clone(), meth))),
                None => Val::Undef(None),
            },
            _ => Val::Undef(None),
        })
    }

    fn get_item(&mut self, obj: &Val, index: &Val, pos: usize) -> R<Val> {
        match (obj, index) {
            (Val::Undef(_), _) => Err(undefined_use(obj, "subscript", pos)),
            (Val::Map(m), Val::Str(k)) => match m.get(k) {
                Some(v) => Ok(v.clone()),
                None => self.get_attr(obj, k, pos),
            },
            (Val::List(l), i) if i.as_int().is_some() => Ok(i
                .as_int()
                .and_then(|i| norm_index(i, l.items.len()))
                .and_then(|i| l.items.get(i).cloned())
                .unwrap_or(Val::Undef(None))),
            (Val::Str(s), i) if i.as_int().is_some() => {
                self.step_text(s.len(), pos)?;
                let i = i.as_int().unwrap_or(0);
                let c = if i >= 0 {
                    usize::try_from(i).ok().and_then(|i| s.chars().nth(i))
                } else {
                    norm_index(i, s.chars().count()).and_then(|i| s.chars().nth(i))
                };
                Ok(c.map_or(Val::Undef(None), |c| Val::str(c.to_string())))
            }
            (_, Val::Str(k)) => self.get_attr(obj, k, pos),
            _ => Ok(Val::Undef(None)),
        }
    }

    fn slice(
        &mut self,
        obj: &Val,
        start: Option<i128>,
        stop: Option<i128>,
        step: Option<i128>,
        pos: usize,
    ) -> R<Val> {
        let step = step.unwrap_or(1);
        if step == 0 {
            return Err(Fail::ty("slice step cannot be zero", pos));
        }
        match obj {
            Val::Undef(_) => Err(undefined_use(obj, "slice", pos)),
            Val::List(l) => {
                let spec = SliceSpec::new(l.items.len(), start, stop, step);
                self.step(spec.count, pos)?;
                self.charge_n(spec.count, VAL_SIZE, pos)?;
                let items = spec.pick(&l.items).cloned().collect();
                Val::list(items).map_err(deep(pos))
            }
            Val::Str(s) => {
                self.step_text(s.len(), pos)?;
                let chars = s.chars().count();
                let spec = SliceSpec::new(chars, start, stop, step);
                // At most four bytes per character selected, and never more
                // than the string itself: charged before anything is built.
                self.charge(spec.count.saturating_mul(4).min(s.len()), pos)?;
                Ok(Val::str(spec.pick_str(s, chars)))
            }
            _ => Ok(Val::Undef(None)),
        }
    }

    /// Looks up a dotted attribute path (`a.b.0`), as `selectattr` and
    /// `map(attribute=...)` do.
    fn attr_path(&mut self, v: &Val, path: &str, pos: usize) -> R<Val> {
        let mut cur = v.clone();
        for part in path.split('.') {
            cur = self.get_item(&cur, &path_key(part), pos)?;
        }
        Ok(cur)
    }

    // ---- calls ---------------------------------------------------------

    fn call(&mut self, f: &Val, args: CallArgs, pos: usize) -> R<Val> {
        self.tick_call(pos)?;
        match f {
            Val::Macro(def) => self.call_macro(def, args, pos),
            Val::Method(m) => self.call_method(&m.0, m.1, args, pos),
            Val::Func(super::value::Func::Global(g)) => self.call_global(*g, args, pos),
            Val::Func(super::value::Func::Host(i)) => self.call_host(*i, args, pos),
            Val::Undef(_) => Err(undefined_use(f, "call", pos)),
            other => Err(Fail::ty(
                format!("a {} is not callable", other.type_name()),
                pos,
            )),
        }
    }

    fn call_host(&mut self, index: usize, args: CallArgs, pos: usize) -> R<Val> {
        let Some((name, f)) = self.tpl.functions.get(index) else {
            return Err(Fail::new(
                TemplateErrorKind::UnknownFunction("?".into()),
                pos,
            ));
        };
        args.no_kwargs(name, pos)?;
        let mut values = Vec::with_capacity(args.pos.len());
        for a in &args.pos {
            values.push(a.to_value(0, &mut self.steps).map_err(halt(pos))?);
        }
        match f(&values) {
            Ok(v) => Val::from_value(&v, 0).map_err(deep(pos)),
            Err(message) => Err(Fail::new(
                TemplateErrorKind::Function {
                    name: name.clone(),
                    message,
                },
                pos,
            )),
        }
    }

    fn call_global(&mut self, g: Global, args: CallArgs, pos: usize) -> R<Val> {
        match g {
            Global::RaiseException => {
                args.no_kwargs("raise_exception", pos)?;
                let msg = match args.pos.first() {
                    Some(v) => self.text(v, pos)?.to_string(),
                    None => String::new(),
                };
                Err(Fail::new(TemplateErrorKind::Raised(msg), pos))
            }
            Global::Range => {
                args.no_kwargs("range", pos)?;
                let mut ints = Vec::with_capacity(args.pos.len());
                for a in &args.pos {
                    ints.push(a.as_int().ok_or_else(|| {
                        Fail::ty(
                            format!("range() needs integers, not a {}", a.type_name()),
                            pos,
                        )
                    })?);
                }
                let (start, stop, step) = match ints.as_slice() {
                    [stop] => (0, *stop, 1),
                    [start, stop] => (*start, *stop, 1),
                    [start, stop, step] => (*start, *stop, *step),
                    _ => return Err(Fail::ty("range() takes 1 to 3 arguments", pos)),
                };
                if step == 0 {
                    return Err(Fail::ty("range() step must not be zero", pos));
                }
                // Count in u128: the bounds and the step may be anything an
                // i128 holds, and their difference may not fit in one.
                let ahead = if step > 0 { start < stop } else { stop < start };
                let count = if ahead {
                    (start.abs_diff(stop) - 1) / step.unsigned_abs() + 1
                } else {
                    0
                };
                let count = usize::try_from(count).unwrap_or(usize::MAX);
                self.charge_n(count, VAL_SIZE, pos)?;
                self.step(count, pos)?;
                let mut items = Vec::with_capacity(count);
                let mut next = Some(start);
                for _ in 0..count {
                    // Every value produced lies between start and stop, so
                    // only the step past the last one can overflow.
                    let Some(v) = next else { break };
                    items.push(Val::Int(v));
                    next = v.checked_add(step);
                }
                Val::list(items).map_err(deep(pos))
            }
            Global::Namespace | Global::Dict => {
                let mut entries = Slots::new();
                let name = g.name();
                match args.pos.as_slice() {
                    [] => {}
                    [Val::Map(m)] => {
                        self.step(m.entries.len(), pos)?;
                        for (k, v) in &m.entries {
                            entries.set(k.clone(), v.clone());
                        }
                    }
                    [other] => {
                        return Err(Fail::ty(
                            format!("{name}() takes a map, not a {}", other.type_name()),
                            pos,
                        ))
                    }
                    _ => {
                        return Err(Fail::ty(
                            format!("{name}() takes at most 1 positional argument"),
                            pos,
                        ))
                    }
                }
                self.step(args.kw.len(), pos)?;
                for (k, v) in args.kw {
                    entries.set(k, v);
                }
                if g == Global::Dict {
                    return self.new_map(entries.into_entries(), pos);
                }
                self.charge(entries.len().saturating_mul(VAL_SIZE * 2), pos)?;
                let cell: NsCell = Rc::new(RefCell::new(entries));
                self.namespaces.push(cell.clone());
                Ok(Val::Ns(cell))
            }
        }
    }

    fn call_macro(&mut self, closure: &Rc<Closure>, args: CallArgs, pos: usize) -> R<Val> {
        let def = &closure.def;
        let name = &def.name;
        if args.pos.len() > def.params.len() {
            return Err(Fail::ty(
                format!(
                    "macro '{name}' takes not more than {} argument(s)",
                    def.params.len()
                ),
                pos,
            ));
        }
        self.step(args.pos.len() + args.kw.len() + def.params.len(), pos)?;
        // Bind every argument to its parameter's slot.
        let npos = args.pos.len();
        let mut slots: Vec<Option<Val>> = args.pos.into_iter().map(Some).collect();
        slots.resize(def.params.len(), None);
        for (k, v) in args.kw {
            match def.param_position(&k) {
                None => {
                    return Err(Fail::ty(
                        format!("macro '{name}' takes no keyword argument '{k}'"),
                        pos,
                    ))
                }
                Some(i) if i < npos => {
                    return Err(Fail::ty(
                        format!("macro '{name}' got multiple values for argument '{k}'"),
                        pos,
                    ))
                }
                Some(i) => {
                    if let Some(slot) = slots.get_mut(i) {
                        *slot = Some(v);
                    }
                }
            }
        }
        self.enter(pos)?;
        let saved_base = self.base;
        let saved_env = std::mem::replace(&mut self.env, closure.env.clone());
        self.frames.push(Slots::new());
        self.base = self.frames.len() - 1;
        if closure.env.is_some() {
            // A macro defined in a nested scope can call itself by name, as
            // the Jinja2 closure it stands for can.
            self.set_var(name.clone(), Val::Macro(closure.clone()));
        }
        let saved_out = std::mem::take(&mut self.out);
        let result = self.run_macro(def, slots);
        let produced = std::mem::replace(&mut self.out, saved_out);
        self.frames.pop();
        self.base = saved_base;
        self.env = saved_env;
        self.depth -= 1;
        result?;
        self.new_str(produced, pos)
    }

    fn run_macro(&mut self, def: &MacroDef, slots: Vec<Option<Val>>) -> R<()> {
        for ((param, default), given) in def.params.iter().zip(slots) {
            let v = match (given, default) {
                (Some(v), _) => v,
                (None, Some(d)) => self.eval(d)?,
                (None, None) => Val::Undef(Some(param.clone())),
            };
            self.set_var(param.clone(), v);
        }
        self.exec(&def.body)?;
        Ok(())
    }

    // ---- methods -------------------------------------------------------

    fn call_method(&mut self, recv: &Val, name: &str, args: CallArgs, pos: usize) -> R<Val> {
        match recv {
            Val::Str(s) => self.str_method(s, name, args, pos),
            Val::Map(m) => self.map_method(m, name, args, pos),
            Val::List(l) => {
                let [x] = args.bind(name, ["value"], pos)?;
                let x = x.unwrap_or(Val::None);
                self.tick_iter(l.items.len(), pos)?;
                let mut count = 0usize;
                for (i, item) in l.items.iter().enumerate() {
                    if self.equals(item, &x, pos)? {
                        if name == "index" {
                            return Ok(Val::Int(i as i128));
                        }
                        count += 1;
                    }
                }
                if name == "index" {
                    return Err(Fail::ty("value is not in the list", pos));
                }
                Ok(Val::Int(count as i128))
            }
            Val::Loop(l) => {
                args.no_kwargs("cycle", pos)?;
                if args.pos.is_empty() {
                    return Err(Fail::ty("loop.cycle() needs at least one argument", pos));
                }
                let n = args.pos.len();
                Ok(args.pos.into_iter().nth(l.index0 % n).unwrap_or(Val::None))
            }
            other => Err(Fail::ty(
                format!("a {} has no method '{name}'", other.type_name()),
                pos,
            )),
        }
    }

    fn opt_text(&mut self, v: Option<Val>, pos: usize) -> R<Option<Arc<str>>> {
        match v {
            None | Some(Val::None) | Some(Val::Undef(_)) => Ok(None),
            Some(v) => Ok(Some(self.text(&v, pos)?)),
        }
    }

    fn opt_int(v: Option<Val>, what: &str, pos: usize) -> R<Option<i128>> {
        match v {
            None | Some(Val::None) => Ok(None),
            Some(v) => v
                .as_int()
                .map(Some)
                .ok_or_else(|| Fail::ty(format!("{what} must be an integer"), pos)),
        }
    }

    fn str_method(&mut self, s: &Arc<str>, name: &str, args: CallArgs, pos: usize) -> R<Val> {
        // Every string method reads its receiver at most a few times over.
        self.step_text(s.len(), pos)?;
        let bool_of = |f: &dyn Fn(&str) -> bool| Ok(Val::Bool(f(s)));
        match name {
            "strip" | "lstrip" | "rstrip" => {
                let [chars] = args.bind(name, ["chars"], pos)?;
                let chars = self.opt_text(chars, pos)?;
                let out = py_strip(s, chars.as_deref(), name != "rstrip", name != "lstrip");
                self.new_str(out.to_string(), pos)
            }
            "lower" | "upper" | "title" | "capitalize" => {
                args.bind(name, [], pos)?;
                let out = match name {
                    "lower" => s.to_lowercase(),
                    "upper" => s.to_uppercase(),
                    "title" => py_title(s),
                    _ => py_capitalize(s),
                };
                self.new_str(out, pos)
            }
            "startswith" | "endswith" => {
                let [prefix, start, end] = args.bind(name, ["prefix", "start", "end"], pos)?;
                let start = Self::opt_int(start, "start", pos)?;
                let end = Self::opt_int(end, "end", pos)?;
                let hay: std::borrow::Cow<'_, str> = if start.is_some() || end.is_some() {
                    let chars = s.chars().count();
                    SliceSpec::new(chars, start, end, 1).pick_str(s, chars)
                } else {
                    (&**s).into()
                };
                let candidates: Vec<Val> = match prefix {
                    Some(Val::List(l)) => {
                        self.step(l.items.len(), pos)?;
                        l.items.clone()
                    }
                    Some(v) => vec![v],
                    None => return Err(Fail::ty(format!("{name}() needs an argument"), pos)),
                };
                for c in candidates {
                    let Val::Str(c) = c else {
                        return Err(Fail::ty(format!("{name}() needs a string"), pos));
                    };
                    let hit = if name == "startswith" {
                        hay.starts_with(&*c)
                    } else {
                        hay.ends_with(&*c)
                    };
                    if hit {
                        return Ok(Val::Bool(true));
                    }
                }
                Ok(Val::Bool(false))
            }
            "split" | "rsplit" => {
                let [sep, maxsplit] = args.bind(name, ["sep", "maxsplit"], pos)?;
                let sep = self.opt_text(sep, pos)?;
                if sep.as_deref() == Some("") {
                    return Err(Fail::ty("empty separator", pos));
                }
                let max = Self::opt_int(maxsplit, "maxsplit", pos)?
                    .filter(|m| *m >= 0)
                    .map(|m| usize::try_from(m).unwrap_or(usize::MAX));
                let pieces = match sep.as_deref() {
                    Some(sep) => s.split(sep).count(),
                    None => s.len() / 2 + 1,
                };
                self.charge(pieces.saturating_mul(VAL_SIZE).saturating_add(s.len()), pos)?;
                let parts = if name == "split" {
                    py_split(s, sep.as_deref(), max)
                } else {
                    py_rsplit(s, sep.as_deref(), max)
                };
                Val::list(parts.into_iter().map(Val::str).collect()).map_err(deep(pos))
            }
            "splitlines" => {
                args.bind(name, [], pos)?;
                let lines = py_splitlines(s);
                self.charge(
                    lines.len().saturating_mul(VAL_SIZE).saturating_add(s.len()),
                    pos,
                )?;
                Val::list(lines.into_iter().map(Val::str).collect()).map_err(deep(pos))
            }
            "replace" => {
                let [old, new, count] = args.bind(name, ["old", "new", "count"], pos)?;
                let old = self.opt_text(old, pos)?.unwrap_or_else(|| Arc::from(""));
                let new = self.opt_text(new, pos)?.unwrap_or_else(|| Arc::from(""));
                let count = Self::opt_int(count, "count", pos)?;
                self.replace(s, &old, &new, count, pos)
            }
            "find" | "count" => {
                let [sub] = args.bind(name, ["sub"], pos)?;
                let sub = self.opt_text(sub, pos)?.unwrap_or_else(|| Arc::from(""));
                if name == "find" {
                    Ok(Val::Int(match s.find(&*sub) {
                        Some(b) => s[..b].chars().count() as i128,
                        None => -1,
                    }))
                } else if sub.is_empty() {
                    Ok(Val::Int(s.chars().count() as i128 + 1))
                } else {
                    Ok(Val::Int(s.matches(&*sub).count() as i128))
                }
            }
            "join" => {
                let [items] = args.bind(name, ["iterable"], pos)?;
                let items = self.iterate(&items.unwrap_or(Val::Undef(None)), pos)?;
                self.tick_iter(items.len(), pos)?;
                let mut parts = Vec::with_capacity(items.len());
                for item in &items {
                    match item {
                        Val::Str(x) => parts.push(x.clone()),
                        other => {
                            return Err(Fail::ty(
                                format!("join() expects strings, found a {}", other.type_name()),
                                pos,
                            ))
                        }
                    }
                }
                self.join_parts(&parts, s, pos)
            }
            "isalnum" => bool_of(&|t| !t.is_empty() && t.chars().all(char::is_alphanumeric)),
            "isalpha" => bool_of(&|t| !t.is_empty() && t.chars().all(char::is_alphabetic)),
            "isdigit" => bool_of(&|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_digit())),
            "isspace" => bool_of(&|t| !t.is_empty() && t.chars().all(is_py_space)),
            "islower" => bool_of(&|t| {
                t.chars().any(is_cased)
                    && t.chars().filter(|c| is_cased(*c)).all(char::is_lowercase)
            }),
            "isupper" => bool_of(&|t| {
                t.chars().any(is_cased)
                    && t.chars().filter(|c| is_cased(*c)).all(char::is_uppercase)
            }),
            _ => Err(Fail::ty(format!("a string has no method '{name}'"), pos)),
        }
    }

    fn replace(
        &mut self,
        s: &str,
        old: &str,
        new: &str,
        count: Option<i128>,
        pos: usize,
    ) -> R<Val> {
        let limit = count
            .filter(|c| *c >= 0)
            .map(|c| usize::try_from(c).unwrap_or(usize::MAX));
        let occurrences = if old.is_empty() {
            s.chars().count() + 1
        } else {
            s.matches(old).count()
        };
        let n = limit.map_or(occurrences, |l| l.min(occurrences));
        self.step_text(s.len(), pos)?;
        self.step(n, pos)?;
        let size = s
            .len()
            .saturating_add(n.saturating_mul(new.len()))
            .saturating_sub(n.saturating_mul(old.len()));
        self.charge(size, pos)?;
        let out = if old.is_empty() {
            let mut out = String::with_capacity(size);
            let mut done = 0;
            for c in s.chars() {
                if done < n {
                    out.push_str(new);
                    done += 1;
                }
                out.push(c);
            }
            if done < n {
                out.push_str(new);
            }
            out
        } else {
            s.replacen(old, new, n)
        };
        Ok(Val::str(out))
    }

    fn join_parts(&mut self, parts: &[Arc<str>], sep: &str, pos: usize) -> R<Val> {
        let total = parts
            .iter()
            .map(|p| p.len())
            .fold(0usize, usize::saturating_add)
            .saturating_add(sep.len().saturating_mul(parts.len().saturating_sub(1)));
        self.charge(total, pos)?;
        self.step_text(total, pos)?;
        self.step(parts.len(), pos)?;
        let mut out = String::with_capacity(total);
        for (i, p) in parts.iter().enumerate() {
            if i > 0 {
                out.push_str(sep);
            }
            out.push_str(p);
        }
        Ok(Val::str(out))
    }

    fn pairs(&mut self, m: &Map, pos: usize) -> R<Vec<Val>> {
        let mut out = Vec::with_capacity(m.entries.len());
        self.charge_n(m.entries.len(), VAL_SIZE * 3, pos)?;
        self.step(m.entries.len(), pos)?;
        for (k, v) in &m.entries {
            out.push(Val::list(vec![Val::Str(k.clone()), v.clone()]).map_err(deep(pos))?);
        }
        Ok(out)
    }

    fn map_method(&mut self, m: &Rc<Map>, name: &str, args: CallArgs, pos: usize) -> R<Val> {
        match name {
            "get" => {
                let [key, default] = args.bind(name, ["key", "default"], pos)?;
                let found = match &key {
                    Some(Val::Str(k)) => m.get(k).cloned(),
                    _ => None,
                };
                Ok(found.unwrap_or_else(|| default.unwrap_or(Val::None)))
            }
            "items" => {
                args.bind(name, [], pos)?;
                let pairs = self.pairs(m, pos)?;
                Val::list(pairs).map_err(deep(pos))
            }
            "keys" | "values" => {
                args.bind(name, [], pos)?;
                self.step(m.entries.len(), pos)?;
                let items = m
                    .entries
                    .iter()
                    .map(|(k, v)| {
                        if name == "keys" {
                            Val::Str(k.clone())
                        } else {
                            v.clone()
                        }
                    })
                    .collect();
                self.new_list(items, pos)
            }
            _ => Err(Fail::ty(format!("a map has no method '{name}'"), pos)),
        }
    }
}

fn op_symbol(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
        CmpOp::Eq => "==",
        CmpOp::Ne => "!=",
        CmpOp::In => "in",
        CmpOp::NotIn => "not in",
    }
}

fn bin_symbol(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::FloorDiv => "//",
        BinOp::Mod => "%",
        BinOp::Pow => "**",
    }
}

fn num_op(op: BinOp, x: Num, y: Num, pos: usize) -> R<Val> {
    let overflow = || Fail::ty("integer overflow", pos);
    let zero = || Fail::ty("division by zero", pos);
    if let (Num::Int(a), Num::Int(b)) = (x, y) {
        return match op {
            BinOp::Add => a.checked_add(b).map(Val::Int).ok_or_else(overflow),
            BinOp::Sub => a.checked_sub(b).map(Val::Int).ok_or_else(overflow),
            BinOp::Mul => a.checked_mul(b).map(Val::Int).ok_or_else(overflow),
            BinOp::Div => {
                if b == 0 {
                    Err(zero())
                } else {
                    Ok(Val::Float(a as f64 / b as f64))
                }
            }
            BinOp::FloorDiv | BinOp::Mod => {
                if b == 0 {
                    return Err(zero());
                }
                let q = a.checked_div(b).ok_or_else(overflow)?;
                let r = a.checked_rem(b).ok_or_else(overflow)?;
                let (q, r) = if r != 0 && ((r < 0) != (b < 0)) {
                    (q - 1, r + b)
                } else {
                    (q, r)
                };
                Ok(Val::Int(if op == BinOp::FloorDiv { q } else { r }))
            }
            BinOp::Pow => {
                if b < 0 {
                    if a == 0 {
                        return Err(Fail::ty("0 cannot be raised to a negative power", pos));
                    }
                    Ok(Val::Float((a as f64).powf(b as f64)))
                } else {
                    let e = u32::try_from(b).map_err(|_| overflow())?;
                    a.checked_pow(e).map(Val::Int).ok_or_else(overflow)
                }
            }
        };
    }
    let (a, b) = (x.as_f64(), y.as_f64());
    Ok(Val::Float(match op {
        BinOp::Add => a + b,
        BinOp::Sub => a - b,
        BinOp::Mul => a * b,
        BinOp::Div => {
            if b == 0.0 {
                return Err(zero());
            }
            a / b
        }
        BinOp::FloorDiv => {
            if b == 0.0 {
                return Err(zero());
            }
            (a / b).floor()
        }
        BinOp::Mod => {
            if b == 0.0 {
                return Err(zero());
            }
            let r = a % b;
            if r != 0.0 && ((r < 0.0) != (b < 0.0)) {
                r + b
            } else {
                r
            }
        }
        BinOp::Pow => {
            if a == 0.0 && b < 0.0 {
                return Err(Fail::ty("0 cannot be raised to a negative power", pos));
            }
            a.powf(b)
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::value::py_float;

    fn positions(len: usize, start: Option<i128>, stop: Option<i128>, step: i128) -> Vec<usize> {
        let items: Vec<usize> = (0..len).collect();
        let spec = SliceSpec::new(len, start, stop, step);
        let picked: Vec<usize> = spec.pick(&items).copied().collect();
        let text: String = (0..len).map(|i| char::from(b'a' + i as u8)).collect();
        let from_str: Vec<usize> = spec
            .pick_str(&text, len)
            .bytes()
            .map(|b| usize::from(b - b'a'))
            .collect();
        assert_eq!(picked, from_str, "{start:?}:{stop:?}:{step}");
        picked
    }

    #[test]
    fn slices_follow_python() {
        assert_eq!(positions(3, Some(1), None, 1), vec![1, 2]);
        assert_eq!(positions(3, None, None, -1), vec![2, 1, 0]);
        assert_eq!(positions(3, None, Some(-1), 1), vec![0, 1]);
        assert_eq!(positions(5, None, None, 2), vec![0, 2, 4]);
        assert_eq!(positions(3, Some(-10), Some(10), 1), vec![0, 1, 2]);
        assert_eq!(positions(5, None, None, -2), vec![4, 2, 0]);
        assert_eq!(positions(5, Some(3), Some(0), -1), vec![3, 2, 1]);
        assert_eq!(positions(5, Some(10), None, -3), vec![4, 1]);
        assert_eq!(positions(5, Some(1), Some(1), 1), Vec::<usize>::new());
        assert_eq!(positions(0, None, None, -1), Vec::<usize>::new());
        // Extreme bounds and steps neither overflow nor walk index by index.
        let max = i128::MAX;
        assert_eq!(positions(3, Some(1), None, max), vec![1]);
        assert_eq!(positions(3, None, None, i128::MIN), vec![2]);
        assert_eq!(positions(3, Some(i128::MIN), Some(max), 1), vec![0, 1, 2]);
        assert_eq!(positions(3, Some(max), Some(i128::MIN), -1), vec![2, 1, 0]);
    }

    #[test]
    fn char_ranges_are_counted_in_characters() {
        assert_eq!(char_range("héllo", 1, 4), "éll");
        assert_eq!(char_range("héllo", 4, 99), "o");
        assert_eq!(char_range("héllo", 9, 12), "");
        assert_eq!(char_range("héllo", 2, 2), "");
    }

    #[test]
    fn python_number_parsing() {
        assert_eq!(py_parse_int(" 12 ", 10), Some(12));
        assert_eq!(py_parse_int("1_000", 10), Some(1000));
        assert_eq!(py_parse_int("0x1A", 16), Some(26));
        assert_eq!(py_parse_int("0b11", 0), Some(3));
        assert_eq!(py_parse_int("4.5", 10), None);
        assert_eq!(py_parse_int("--5", 10), None);
        assert_eq!(py_parse_int("+-5", 10), None);
        assert_eq!(py_parse_int(" -0x5", 16), Some(-5));
        assert_eq!(py_round(1.0, -400), 0.0);
        assert!(py_round(-5.0, -400).is_sign_negative());
        assert_eq!(py_round(2.5, 0), 2.0);
        assert_eq!(py_round(3.5, 0), 4.0);
        assert_eq!(py_round(2.567, 2), 2.57);
        assert_eq!(py_float(1e16), "1e+16");
        assert_eq!(py_float(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(py_float(1.5e-5), "1.5e-05");
        assert_eq!(py_float(-0.0), "-0.0");
        assert_eq!(py_float(100.0), "100.0");
    }

    #[test]
    fn python_string_helpers() {
        assert_eq!(py_title("hello world"), "Hello World");
        assert_eq!(py_title("they're bill's"), "They'Re Bill'S");
        assert_eq!(py_capitalize("hELLO"), "Hello");
        assert_eq!(py_splitlines("a\nb\r\nc\n"), vec!["a", "b", "c"]);
        assert_eq!(py_split(" a  b ", None, None), vec!["a", "b"]);
        assert_eq!(py_split("a,b,,c", Some(","), None), vec!["a", "b", "", "c"]);
        assert_eq!(py_split("a b c", Some(" "), Some(1)), vec!["a", "b c"]);
        assert_eq!(py_rsplit("a b c", None, Some(1)), vec!["a b", "c"]);
        // A huge maxsplit saturates rather than wrapping to zero.
        assert_eq!(py_split("a b", Some(" "), Some(usize::MAX)), vec!["a", "b"]);
        assert_eq!(
            py_rsplit("a b", Some(" "), Some(usize::MAX)),
            vec!["a", "b"]
        );
    }
}
