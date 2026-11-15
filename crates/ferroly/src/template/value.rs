//! The values a template computes with: `codec::Value` data plus the
//! engine-internal kinds (undefined, namespace, loop, macro, functions).

use std::cell::{OnceCell, RefCell};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use crate::codec::Value;

use super::ast::MacroDef;
use super::builtins::Global;

/// Values nested deeper than this are refused (when built or converted from
/// the context), which keeps every recursive walk over a value, and its drop,
/// within a small, fixed stack budget.
pub(crate) const MAX_VALUE_DEPTH: usize = 512;

/// Above this many entries, name lookups go through a hash index instead of
/// a scan, so maps, scopes and namespaces with many names stay linear to
/// build and constant-time to read.
const INDEX_FROM: usize = 16;

#[derive(Debug)]
pub(crate) struct List {
    pub(crate) items: Vec<Val>,
    pub(crate) depth: usize,
}

impl List {
    /// A shared list of `items`, refused when nested too deeply.
    pub(crate) fn shared(items: Vec<Val>) -> Result<Rc<List>, TooDeep> {
        let depth = items.iter().map(Val::depth).max().unwrap_or(0) + 1;
        if depth > MAX_VALUE_DEPTH {
            return Err(TooDeep);
        }
        Ok(Rc::new(List { items, depth }))
    }
}

/// An immutable map: its entries in insertion order (keys unique), plus a
/// hash index built on the first keyed lookup of a large map.
#[derive(Debug)]
pub(crate) struct Map {
    pub(crate) entries: Vec<(Arc<str>, Val)>,
    pub(crate) depth: usize,
    index: OnceCell<HashMap<Arc<str>, usize>>,
}

impl Map {
    pub(crate) fn new(entries: Vec<(Arc<str>, Val)>, depth: usize) -> Map {
        Map {
            entries,
            depth,
            index: OnceCell::new(),
        }
    }

    pub(crate) fn get(&self, key: &str) -> Option<&Val> {
        if self.entries.len() <= INDEX_FROM {
            return self
                .entries
                .iter()
                .rev()
                .find(|(k, _)| &**k == key)
                .map(|(_, v)| v);
        }
        let index = self.index.get_or_init(|| {
            let mut index = HashMap::with_capacity(self.entries.len());
            for (i, (k, _)) in self.entries.iter().enumerate() {
                index.insert(k.clone(), i);
            }
            index
        });
        index
            .get(key)
            .and_then(|&i| self.entries.get(i))
            .map(|(_, v)| v)
    }
}

/// Named slots in insertion order: a scope's variables, a namespace's
/// attributes, a map being built. Setting an existing name replaces its
/// value in place; a hash index takes over from the scan once there are more
/// than a few names.
#[derive(Debug, Default, Clone)]
pub(crate) struct Slots {
    entries: Vec<(Arc<str>, Val)>,
    index: Option<HashMap<Arc<str>, usize>>,
}

impl Slots {
    pub(crate) fn new() -> Slots {
        Slots::default()
    }

    pub(crate) fn entries(&self) -> &[(Arc<str>, Val)] {
        &self.entries
    }

    pub(crate) fn into_entries(self) -> Vec<(Arc<str>, Val)> {
        self.entries
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    fn position(&self, name: &str) -> Option<usize> {
        match &self.index {
            Some(index) => index.get(name).copied(),
            None => self.entries.iter().position(|(k, _)| &**k == name),
        }
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Val> {
        self.position(name)
            .and_then(|i| self.entries.get(i))
            .map(|(_, v)| v)
    }

    pub(crate) fn set(&mut self, name: Arc<str>, v: Val) {
        if let Some(slot) = self.position(&name).and_then(|i| self.entries.get_mut(i)) {
            slot.1 = v;
            return;
        }
        let i = self.entries.len();
        if let Some(index) = &mut self.index {
            index.insert(name.clone(), i);
        } else if i >= INDEX_FROM {
            let mut index: HashMap<Arc<str>, usize> = HashMap::with_capacity(i * 2);
            for (j, (k, _)) in self.entries.iter().enumerate() {
                index.insert(k.clone(), j);
            }
            index.insert(name.clone(), i);
            self.index = Some(index);
        }
        self.entries.push((name, v));
    }

    /// Takes every entry out, leaving the slots empty.
    pub(crate) fn take(&mut self) -> Vec<(Arc<str>, Val)> {
        self.index = None;
        std::mem::take(&mut self.entries)
    }
}

pub(crate) type NsCell = Rc<RefCell<Slots>>;

#[derive(Debug)]
pub(crate) struct LoopState {
    pub(crate) index0: usize,
    pub(crate) items: Rc<List>,
}

/// A macro value: its definition and, for a macro defined inside a loop or
/// another macro, the variables visible where it was defined.
#[derive(Debug)]
pub(crate) struct Closure {
    pub(crate) def: Arc<MacroDef>,
    pub(crate) env: Option<Rc<Slots>>,
    pub(crate) depth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Func {
    Global(Global),
    Host(usize),
}

#[derive(Debug, Clone)]
pub(crate) enum Val {
    /// An undefined value, with the name that produced it (for messages).
    Undef(Option<Arc<str>>),
    None,
    Bool(bool),
    Int(i128),
    Float(f64),
    Str(Arc<str>),
    List(Rc<List>),
    Map(Rc<Map>),
    Ns(NsCell),
    Loop(Rc<LoopState>),
    Macro(Rc<Closure>),
    Func(Func),
    /// A method bound to its receiver: `s.strip`, `m.items`, `loop.cycle`.
    Method(Rc<(Val, &'static str)>),
}

/// The value cannot be built: nested too deeply.
#[derive(Debug)]
pub(crate) struct TooDeep;

impl Val {
    pub(crate) fn str(s: impl Into<Arc<str>>) -> Val {
        Val::Str(s.into())
    }

    pub(crate) fn depth(&self) -> usize {
        match self {
            Val::List(l) => l.depth,
            Val::Map(m) => m.depth,
            Val::Loop(l) => l.items.depth,
            Val::Macro(c) => c.depth,
            Val::Method(m) => m.0.depth() + 1,
            _ => 1,
        }
    }

    pub(crate) fn list(items: Vec<Val>) -> Result<Val, TooDeep> {
        List::shared(items).map(Val::List)
    }

    pub(crate) fn map(entries: Vec<(Arc<str>, Val)>) -> Result<Val, TooDeep> {
        let depth = entries.iter().map(|(_, v)| v.depth()).max().unwrap_or(0) + 1;
        if depth > MAX_VALUE_DEPTH {
            return Err(TooDeep);
        }
        Ok(Val::Map(Rc::new(Map::new(entries, depth))))
    }

    pub(crate) fn is_undefined(&self) -> bool {
        matches!(self, Val::Undef(_))
    }

    pub(crate) fn truthy(&self) -> bool {
        match self {
            Val::Undef(_) | Val::None => false,
            Val::Bool(b) => *b,
            Val::Int(i) => *i != 0,
            Val::Float(f) => *f != 0.0,
            Val::Str(s) => !s.is_empty(),
            Val::List(l) => !l.items.is_empty(),
            Val::Map(m) => !m.entries.is_empty(),
            _ => true,
        }
    }

    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            Val::Undef(_) => "undefined",
            Val::None => "none",
            Val::Bool(_) => "boolean",
            Val::Int(_) => "integer",
            Val::Float(_) => "float",
            Val::Str(_) => "string",
            Val::List(_) => "list",
            Val::Map(_) => "map",
            Val::Ns(_) => "namespace",
            Val::Loop(_) => "loop",
            Val::Macro(_) => "macro",
            Val::Func(_) | Val::Method(_) => "function",
        }
    }

    /// The number value, booleans counting as 0 / 1 as in Python.
    pub(crate) fn as_num(&self) -> Option<Num> {
        match self {
            Val::Bool(b) => Some(Num::Int(i128::from(*b))),
            Val::Int(i) => Some(Num::Int(*i)),
            Val::Float(f) => Some(Num::Float(*f)),
            _ => None,
        }
    }

    pub(crate) fn as_int(&self) -> Option<i128> {
        match self {
            Val::Bool(b) => Some(i128::from(*b)),
            Val::Int(i) => Some(*i),
            _ => None,
        }
    }

    // ---- conversion from and to codec::Value ---------------------------

    pub(crate) fn from_value(v: &Value, depth: usize) -> Result<Val, TooDeep> {
        if depth > MAX_VALUE_DEPTH {
            return Err(TooDeep);
        }
        Ok(match v {
            Value::Null => Val::None,
            Value::Bool(b) => Val::Bool(*b),
            Value::Int(i) => Val::Int(i128::from(*i)),
            Value::UInt(u) => Val::Int(i128::from(*u)),
            Value::Float(f) => Val::Float(*f),
            Value::Str(s) => Val::str(s.as_str()),
            Value::Bytes(b) => Val::list(b.iter().map(|x| Val::Int(i128::from(*x))).collect())?,
            Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(Val::from_value(item, depth + 1)?);
                }
                Val::list(out)?
            }
            Value::Object(entries) => {
                let mut out: Vec<(Arc<str>, Val)> = Vec::with_capacity(entries.len());
                let mut index: std::collections::HashMap<&str, usize> =
                    std::collections::HashMap::new();
                for (k, item) in entries {
                    let val = Val::from_value(item, depth + 1)?;
                    // A repeated key keeps its first position and its last
                    // value, as a Python dict built from the same pairs would.
                    match index.get(k.as_str()) {
                        Some(&i) => {
                            if let Some(slot) = out.get_mut(i) {
                                slot.1 = val;
                            }
                        }
                        None => {
                            index.insert(k.as_str(), out.len());
                            out.push((Arc::from(k.as_str()), val));
                        }
                    }
                }
                Val::map(out)?
            }
        })
    }

    /// Converts to a `codec::Value` (for a host function), spending one step
    /// per value and one per 64 bytes of text copied.
    pub(crate) fn to_value(&self, depth: usize, steps: &mut usize) -> Result<Value, Halt> {
        if depth > MAX_VALUE_DEPTH {
            return Err(Halt::TooDeep);
        }
        spend(steps, 1)?;
        Ok(match self {
            Val::Undef(_) | Val::None => Value::Null,
            Val::Bool(b) => Value::Bool(*b),
            Val::Int(i) => {
                if let Ok(v) = i64::try_from(*i) {
                    Value::Int(v)
                } else if let Ok(v) = u64::try_from(*i) {
                    Value::UInt(v)
                } else {
                    Value::Float(*i as f64)
                }
            }
            Val::Float(f) => Value::Float(*f),
            Val::Str(s) => {
                spend(steps, s.len() / 64)?;
                Value::Str(s.to_string())
            }
            Val::List(l) => {
                let mut out = Vec::with_capacity(l.items.len());
                for item in &l.items {
                    out.push(item.to_value(depth + 1, steps)?);
                }
                Value::Array(out)
            }
            Val::Map(m) => {
                let mut out = Vec::with_capacity(m.entries.len());
                for (k, v) in &m.entries {
                    out.push((k.to_string(), v.to_value(depth + 1, steps)?));
                }
                Value::Object(out)
            }
            Val::Ns(ns) => {
                let ns = ns.try_borrow().map_err(|_| Halt::TooDeep)?;
                let mut out = Vec::with_capacity(ns.len());
                for (k, v) in ns.entries() {
                    out.push((k.to_string(), v.to_value(depth + 1, steps)?));
                }
                Value::Object(out)
            }
            Val::Loop(_) | Val::Macro(_) | Val::Func(_) | Val::Method(_) => Value::Null,
        })
    }

    // ---- printing ------------------------------------------------------

    /// `str(value)` in Python terms: what `{{ value }}` prints.
    pub(crate) fn write_str(&self, out: &mut Sink, depth: usize) -> Result<(), VErr> {
        match self {
            Val::Undef(_) => Ok(()),
            Val::Str(s) => out.push_str(s),
            _ => self.write_repr(out, depth),
        }
    }

    /// `repr(value)` in Python terms: how a value prints inside a list or map.
    pub(crate) fn write_repr(&self, out: &mut Sink, depth: usize) -> Result<(), VErr> {
        if depth > MAX_VALUE_DEPTH {
            return Err(VErr::TooDeep);
        }
        match self {
            Val::Undef(_) => Ok(()),
            Val::None => out.push_str("None"),
            Val::Bool(true) => out.push_str("True"),
            Val::Bool(false) => out.push_str("False"),
            Val::Int(i) => out.push_str(&i.to_string()),
            Val::Float(f) => out.push_str(&py_float(*f)),
            Val::Str(s) => py_str_repr(s, out),
            Val::List(l) => {
                out.push('[')?;
                for (i, item) in l.items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ")?;
                    }
                    item.write_repr(out, depth + 1)?;
                }
                out.push(']')
            }
            Val::Map(m) => write_map_repr(m.entries.iter(), out, depth),
            Val::Ns(ns) => {
                out.push_str("<Namespace ")?;
                let entries = ns.try_borrow().map_err(|_| VErr::TooDeep)?;
                write_map_repr(entries.entries().iter(), out, depth)?;
                out.push('>')
            }
            Val::Loop(l) => out.push_str(&format!(
                "<LoopContext {}/{}>",
                l.index0 + 1,
                l.items.items.len()
            )),
            Val::Macro(m) => out.push_str(&format!("<Macro '{}'>", m.def.name)),
            Val::Func(Func::Global(g)) => out.push_str(&format!("<function {}>", g.name())),
            Val::Func(Func::Host(_)) => out.push_str("<function>"),
            Val::Method(m) => out.push_str(&format!("<built-in method {}>", m.1)),
        }
    }
}

/// A string being built under a byte cap, so printing a value that holds the
/// same large string many times cannot allocate without bound.
pub(crate) struct Sink {
    pub(crate) out: String,
    cap: usize,
}

impl Sink {
    pub(crate) fn new(cap: usize) -> Sink {
        Sink {
            out: String::new(),
            cap,
        }
    }

    pub(crate) fn push_str(&mut self, s: &str) -> Result<(), VErr> {
        if self.out.len().saturating_add(s.len()) > self.cap {
            return Err(VErr::TooLarge);
        }
        self.out.push_str(s);
        Ok(())
    }

    pub(crate) fn push(&mut self, c: char) -> Result<(), VErr> {
        if self.out.len().saturating_add(c.len_utf8()) > self.cap {
            return Err(VErr::TooLarge);
        }
        self.out.push(c);
        Ok(())
    }
}

/// Why a value could not be printed, encoded or built.
#[derive(Debug)]
pub(crate) enum VErr {
    /// Nested deeper than [`MAX_VALUE_DEPTH`].
    TooDeep,
    /// The text would exceed the sink's cap.
    TooLarge,
    /// `tojson` of an undefined value.
    Undefined,
    /// `tojson` of a value that is not data (a macro, a function, a loop).
    NotData(&'static str),
}

impl From<TooDeep> for VErr {
    fn from(_: TooDeep) -> VErr {
        VErr::TooDeep
    }
}

fn write_map_repr<'v>(
    entries: impl Iterator<Item = &'v (Arc<str>, Val)>,
    out: &mut Sink,
    depth: usize,
) -> Result<(), VErr> {
    out.push('{')?;
    for (i, (k, v)) in entries.enumerate() {
        if i > 0 {
            out.push_str(", ")?;
        }
        py_str_repr(k, out)?;
        out.push_str(": ")?;
        v.write_repr(out, depth + 1)?;
    }
    out.push('}')
}

/// Python's `repr()` of a string: single quotes unless the text contains a
/// single quote and no double quote.
pub(crate) fn py_str_repr(s: &str, out: &mut Sink) -> Result<(), VErr> {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    out.push(quote)?;
    write_escaped(s, out, |c| match c {
        '\\' => Some("\\\\".into()),
        '\n' => Some("\\n".into()),
        '\r' => Some("\\r".into()),
        '\t' => Some("\\t".into()),
        c if c == quote => Some(format!("\\{c}")),
        c if (c as u32) < 0x20 || (0x7f..0xa0).contains(&(c as u32)) => {
            Some(format!("\\x{:02x}", c as u32))
        }
        _ => None,
    })?;
    out.push(quote)
}

/// Writes `s`, replacing the characters `escape` maps; runs of characters it
/// leaves alone are copied in one piece.
fn write_escaped(
    s: &str,
    out: &mut Sink,
    escape: impl Fn(char) -> Option<String>,
) -> Result<(), VErr> {
    let mut run = 0;
    for (i, c) in s.char_indices() {
        if let Some(rep) = escape(c) {
            out.push_str(&s[run..i])?;
            out.push_str(&rep)?;
            run = i + c.len_utf8();
        }
    }
    out.push_str(&s[run..])
}

/// Python's `repr()` of a float: the shortest round-trip digits, positional
/// for exponents in `-4..16`, otherwise `1e+16` style.
pub(crate) fn py_float(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    // `{:e}` yields the shortest round-trip digits: "-1.2345e-7".
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let (neg, mantissa) = match mantissa.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, mantissa),
    };
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if !(-4..16).contains(&exp) {
        let (first, rest) = digits.split_at(1.min(digits.len()));
        out.push_str(first);
        if !rest.is_empty() {
            out.push('.');
            out.push_str(rest);
        }
        let sign = if exp < 0 { '-' } else { '+' };
        out.push_str(&format!("e{sign}{:02}", exp.unsigned_abs()));
    } else if exp >= 0 {
        let int_len = exp.unsigned_abs() as usize + 1;
        if digits.len() <= int_len {
            out.push_str(&digits);
            out.extend(std::iter::repeat('0').take(int_len - digits.len()));
            out.push_str(".0");
        } else {
            out.push_str(&digits[..int_len]);
            out.push('.');
            out.push_str(&digits[int_len..]);
        }
    } else {
        out.push_str("0.");
        out.extend(std::iter::repeat('0').take(exp.unsigned_abs() as usize - 1));
        out.push_str(&digits);
    }
    out
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Num {
    Int(i128),
    Float(f64),
}

impl Num {
    pub(crate) fn as_f64(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Float(f) => f,
        }
    }
}

/// Why a walk over values stopped early.
#[derive(Debug)]
pub(crate) enum Halt {
    /// Nested deeper than [`MAX_VALUE_DEPTH`].
    TooDeep,
    /// The render's work budget (`Limits::max_steps`) ran out.
    Steps,
}

/// Spends `n` steps of the work budget.
pub(crate) fn spend(steps: &mut usize, n: usize) -> Result<(), Halt> {
    match steps.checked_sub(n) {
        Some(left) => {
            *steps = left;
            Ok(())
        }
        None => {
            *steps = 0;
            Err(Halt::Steps)
        }
    }
}

/// The work of comparing or scanning `bytes` bytes of text: one step per 64
/// bytes, and at least one.
pub(crate) fn text_cost(bytes: usize) -> usize {
    1 + bytes / 64
}

/// Python `==`: numbers (booleans included) compare by value, other kinds
/// only with their own kind, mixed kinds are unequal. Spends one step per
/// value visited (and per 64 bytes of text compared).
pub(crate) fn equals(a: &Val, b: &Val, depth: usize, steps: &mut usize) -> Result<bool, Halt> {
    if depth > MAX_VALUE_DEPTH {
        return Err(Halt::TooDeep);
    }
    spend(steps, 1)?;
    Ok(match (a, b) {
        (Val::Undef(_), Val::Undef(_)) | (Val::None, Val::None) => true,
        (Val::Str(x), Val::Str(y)) => {
            if x.len() != y.len() {
                return Ok(false);
            }
            spend(steps, x.len() / 64)?;
            x == y
        }
        (Val::List(x), Val::List(y)) => {
            if Rc::ptr_eq(x, y) {
                return Ok(true);
            }
            if x.items.len() != y.items.len() {
                return Ok(false);
            }
            for (p, q) in x.items.iter().zip(&y.items) {
                if !equals(p, q, depth + 1, steps)? {
                    return Ok(false);
                }
            }
            true
        }
        (Val::Map(x), Val::Map(y)) => {
            if Rc::ptr_eq(x, y) {
                return Ok(true);
            }
            if x.entries.len() != y.entries.len() {
                return Ok(false);
            }
            for (k, v) in &x.entries {
                match y.get(k) {
                    Some(w) if equals(v, w, depth + 1, steps)? => {}
                    _ => return Ok(false),
                }
            }
            true
        }
        (Val::Ns(x), Val::Ns(y)) => Rc::ptr_eq(x, y),
        (Val::Loop(x), Val::Loop(y)) => Rc::ptr_eq(x, y),
        (Val::Macro(x), Val::Macro(y)) => Rc::ptr_eq(x, y),
        (Val::Func(x), Val::Func(y)) => x == y,
        _ => match (a.as_num(), b.as_num()) {
            (Some(Num::Int(x)), Some(Num::Int(y))) => x == y,
            (Some(x), Some(y)) => x.as_f64() == y.as_f64(),
            _ => false,
        },
    })
}

/// Python ordering: numbers, strings (by code point) and lists
/// (lexicographically). `None` when the kinds cannot be ordered. Spends steps
/// as [`equals`] does.
pub(crate) fn compare(
    a: &Val,
    b: &Val,
    depth: usize,
    steps: &mut usize,
) -> Result<Option<Ordering>, Halt> {
    if depth > MAX_VALUE_DEPTH {
        return Err(Halt::TooDeep);
    }
    spend(steps, 1)?;
    Ok(match (a, b) {
        (Val::Str(x), Val::Str(y)) => {
            spend(steps, x.len().min(y.len()) / 64)?;
            Some(x.cmp(y))
        }
        (Val::List(x), Val::List(y)) => {
            for (p, q) in x.items.iter().zip(&y.items) {
                if equals(p, q, depth + 1, steps)? {
                    continue;
                }
                return compare(p, q, depth + 1, steps);
            }
            Some(x.items.len().cmp(&y.items.len()))
        }
        _ => match (a.as_num(), b.as_num()) {
            (Some(Num::Int(x)), Some(Num::Int(y))) => Some(x.cmp(&y)),
            (Some(x), Some(y)) => x.as_f64().partial_cmp(&y.as_f64()),
            _ => None,
        },
    })
}

/// Exact order between an integer and a float that is not NaN.
fn cmp_int_float(i: i128, f: f64) -> Ordering {
    // 2^127: every i128 is below it and at or above its negation.
    const TWO_127: f64 = 170_141_183_460_469_231_731_687_303_715_884_105_728.0;
    if f >= TWO_127 {
        return Ordering::Less;
    }
    if f < -TWO_127 {
        return Ordering::Greater;
    }
    let t = f.trunc();
    // `t` is integral and within the i128 range, so the cast is exact.
    match i.cmp(&(t as i128)) {
        Ordering::Equal if f > t => Ordering::Less,
        Ordering::Equal if f < t => Ordering::Greater,
        o => o,
    }
}

/// The total order the `sort` and `dictsort` filters use: numbers by value
/// (exactly, integers against floats too; NaN after every other number),
/// strings by code point, lists lexicographically. `None` when the two kinds
/// cannot be ordered, as Python's `<` would raise.
pub(crate) fn sort_order(
    a: &Val,
    b: &Val,
    depth: usize,
    steps: &mut usize,
) -> Result<Option<Ordering>, Halt> {
    if depth > MAX_VALUE_DEPTH {
        return Err(Halt::TooDeep);
    }
    spend(steps, 1)?;
    Ok(match (a, b) {
        (Val::Str(x), Val::Str(y)) => {
            spend(steps, x.len().min(y.len()) / 64)?;
            Some(x.cmp(y))
        }
        (Val::List(x), Val::List(y)) => {
            for (p, q) in x.items.iter().zip(&y.items) {
                match sort_order(p, q, depth + 1, steps)? {
                    Some(Ordering::Equal) => {}
                    // As in Python, equal items are skipped before any
                    // ordering is asked of them.
                    None if equals(p, q, depth + 1, steps)? => {}
                    other => return Ok(other),
                }
            }
            Some(x.items.len().cmp(&y.items.len()))
        }
        _ => match (a.as_num(), b.as_num()) {
            (Some(Num::Int(x)), Some(Num::Int(y))) => Some(x.cmp(&y)),
            (Some(x), Some(y)) => {
                let nan = |n: Num| matches!(n, Num::Float(f) if f.is_nan());
                Some(match (nan(x), nan(y)) {
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) => match (x, y) {
                        (Num::Int(i), Num::Float(f)) => cmp_int_float(i, f),
                        (Num::Float(f), Num::Int(i)) => cmp_int_float(i, f).reverse(),
                        (x, y) => x
                            .as_f64()
                            .partial_cmp(&y.as_f64())
                            .unwrap_or(Ordering::Equal),
                    },
                })
            }
            _ => None,
        },
    })
}

/// `json.dumps(value, ensure_ascii=..., indent=..., separators=...,
/// sort_keys=...)` as Hugging Face's `tojson` calls it: no HTML escaping, keys
/// in input order unless sorted.
pub(crate) struct JsonStyle {
    pub(crate) indent: Option<String>,
    pub(crate) item_sep: String,
    pub(crate) key_sep: String,
    pub(crate) sort_keys: bool,
    pub(crate) ensure_ascii: bool,
}

pub(crate) fn write_json(
    v: &Val,
    style: &JsonStyle,
    out: &mut Sink,
    level: usize,
) -> Result<(), VErr> {
    if level > MAX_VALUE_DEPTH {
        return Err(VErr::TooDeep);
    }
    let newline = |out: &mut Sink, level: usize| -> Result<(), VErr> {
        if let Some(ind) = &style.indent {
            out.push('\n')?;
            for _ in 0..level {
                out.push_str(ind)?;
            }
        }
        Ok(())
    };
    match v {
        Val::Undef(_) => return Err(VErr::Undefined),
        Val::None => out.push_str("null")?,
        Val::Bool(true) => out.push_str("true")?,
        Val::Bool(false) => out.push_str("false")?,
        Val::Int(i) => out.push_str(&i.to_string())?,
        Val::Float(f) => {
            if f.is_nan() {
                out.push_str("NaN")?;
            } else if f.is_infinite() {
                out.push_str(if *f > 0.0 { "Infinity" } else { "-Infinity" })?;
            } else {
                out.push_str(&py_float(*f))?;
            }
        }
        Val::Str(s) => json_str(s, style.ensure_ascii, out)?,
        Val::List(l) => {
            if l.items.is_empty() {
                return out.push_str("[]");
            }
            out.push('[')?;
            for (i, item) in l.items.iter().enumerate() {
                if i > 0 {
                    out.push_str(&style.item_sep)?;
                }
                newline(out, level + 1)?;
                write_json(item, style, out, level + 1)?;
            }
            newline(out, level)?;
            out.push(']')?;
        }
        Val::Map(_) | Val::Ns(_) => {
            let entries: Vec<(Arc<str>, Val)> = match v {
                Val::Map(m) => m.entries.clone(),
                Val::Ns(ns) => ns
                    .try_borrow()
                    .map_err(|_| VErr::TooDeep)?
                    .entries()
                    .to_vec(),
                _ => Vec::new(),
            };
            if entries.is_empty() {
                return out.push_str("{}");
            }
            let mut order: Vec<usize> = (0..entries.len()).collect();
            if style.sort_keys {
                order.sort_by(|a, b| entries[*a].0.cmp(&entries[*b].0));
            }
            out.push('{')?;
            for (n, i) in order.into_iter().enumerate() {
                let (k, item) = &entries[i];
                if n > 0 {
                    out.push_str(&style.item_sep)?;
                }
                newline(out, level + 1)?;
                json_str(k, style.ensure_ascii, out)?;
                out.push_str(&style.key_sep)?;
                write_json(item, style, out, level + 1)?;
            }
            newline(out, level)?;
            out.push('}')?;
        }
        other => return Err(VErr::NotData(other.type_name())),
    }
    Ok(())
}

fn json_str(s: &str, ensure_ascii: bool, out: &mut Sink) -> Result<(), VErr> {
    out.push('"')?;
    write_escaped(s, out, |c| match c {
        '"' => Some("\\\"".into()),
        '\\' => Some("\\\\".into()),
        '\n' => Some("\\n".into()),
        '\r' => Some("\\r".into()),
        '\t' => Some("\\t".into()),
        '\u{8}' => Some("\\b".into()),
        '\u{c}' => Some("\\f".into()),
        c if (c as u32) < 0x20 => Some(format!("\\u{:04x}", c as u32)),
        c if ensure_ascii && !c.is_ascii() => {
            let mut buf = [0u16; 2];
            Some(
                c.encode_utf16(&mut buf)
                    .iter()
                    .map(|u| format!("\\u{u:04x}"))
                    .collect(),
            )
        }
        _ => None,
    })?;
    out.push('"')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_keep_insertion_order_and_index_many_names() {
        let mut slots = Slots::new();
        for i in 0..100 {
            slots.set(Arc::from(format!("k{i}")), Val::Int(i));
        }
        // Setting a name again replaces it in place, before and after the
        // index takes over.
        slots.set(Arc::from("k3"), Val::Int(-3));
        slots.set(Arc::from("k90"), Val::Int(-90));
        assert_eq!(slots.len(), 100);
        assert!(matches!(slots.get("k3"), Some(Val::Int(-3))));
        assert!(matches!(slots.get("k90"), Some(Val::Int(-90))));
        assert!(matches!(slots.get("k99"), Some(Val::Int(99))));
        assert!(slots.get("k100").is_none());
        let names: Vec<&str> = slots.entries().iter().map(|(k, _)| &**k).collect();
        assert_eq!(names[..4], ["k0", "k1", "k2", "k3"]);
        assert_eq!(slots.take().len(), 100);
        assert!(slots.get("k0").is_none());
    }

    #[test]
    fn large_maps_look_up_through_the_index() {
        let entries: Vec<(Arc<str>, Val)> = (0..50)
            .map(|i| (Arc::from(format!("k{i}")), Val::Int(i)))
            .collect();
        let m = Map::new(entries, 2);
        assert!(matches!(m.get("k0"), Some(Val::Int(0))));
        assert!(matches!(m.get("k49"), Some(Val::Int(49))));
        assert!(m.get("nope").is_none());
    }

    #[test]
    fn integers_and_floats_order_exactly() {
        let big = 1i128 << 53;
        assert_eq!(cmp_int_float(big + 1, big as f64), Ordering::Greater);
        assert_eq!(cmp_int_float(big, big as f64), Ordering::Equal);
        assert_eq!(cmp_int_float(1, 1.5), Ordering::Less);
        assert_eq!(cmp_int_float(-1, -1.5), Ordering::Greater);
        assert_eq!(cmp_int_float(i128::MAX, f64::INFINITY), Ordering::Less);
        assert_eq!(
            cmp_int_float(i128::MIN, f64::NEG_INFINITY),
            Ordering::Greater
        );
        assert_eq!(cmp_int_float(i128::MIN, -(2f64.powi(127))), Ordering::Equal);
        assert_eq!(cmp_int_float(i128::MAX, 2f64.powi(127)), Ordering::Less);
    }

    #[test]
    fn sort_order_is_a_total_order_on_numbers() {
        let vals = [
            Val::Float(f64::NAN),
            Val::Float(f64::INFINITY),
            Val::Float(f64::NEG_INFINITY),
            Val::Float(-0.0),
            Val::Float(0.0),
            Val::Float(1.5),
            Val::Float(9_007_199_254_740_992.0),
            Val::Int(9_007_199_254_740_993),
            Val::Int(0),
            Val::Int(i128::MAX),
            Val::Int(i128::MIN),
            Val::Bool(true),
        ];
        let mut steps = usize::MAX;
        let mut ord = |a: &Val, b: &Val| sort_order(a, b, 0, &mut steps).unwrap().unwrap();
        for a in &vals {
            assert_eq!(ord(a, a), Ordering::Equal);
            for b in &vals {
                assert_eq!(ord(a, b), ord(b, a).reverse());
                for c in &vals {
                    if ord(a, b) != Ordering::Greater && ord(b, c) != Ordering::Greater {
                        assert_ne!(ord(a, c), Ordering::Greater);
                    }
                }
            }
        }
    }

    #[test]
    fn comparisons_spend_the_work_budget() {
        let long = Val::list((0..1000).map(Val::Int).collect()).unwrap();
        let other = Val::list((0..1000).map(Val::Int).collect()).unwrap();
        let mut steps = 500;
        assert!(matches!(
            equals(&long, &other, 0, &mut steps),
            Err(Halt::Steps)
        ));
        let mut steps = 10_000;
        assert!(equals(&long, &other, 0, &mut steps).unwrap());
        assert_eq!(steps, 10_000 - 1001);
    }
}
