//! The built-in filters and tests.

use std::cmp::Ordering;
use std::sync::Arc;

use super::super::builtins::{Filter, Test, MAP_METHODS};
use super::super::error::{Fail, TemplateErrorKind, R};
use super::super::value::{sort_order, write_json, JsonStyle, Num, Val};
use super::{
    deep, halt, html_escape, path_key, py_capitalize, py_parse_float, py_parse_int, py_round,
    py_splitlines, py_strip, py_title, undefined_use, verr, CallArgs, Renderer,
};

/// Case-folds strings for the case-insensitive filters (`sort`, `unique`,
/// `min`, `max`, `dictsort`).
fn fold(v: &Val, case_sensitive: bool) -> Val {
    match v {
        Val::Str(s) if !case_sensitive => Val::str(s.to_lowercase()),
        other => other.clone(),
    }
}

fn int_arg(v: Option<Val>, default: i128, what: &str, pos: usize) -> R<i128> {
    match v {
        None => Ok(default),
        Some(v) => v
            .as_int()
            .ok_or_else(|| Fail::ty(format!("{what} must be an integer"), pos)),
    }
}

fn bool_arg(v: Option<Val>) -> bool {
    v.is_some_and(|v| v.truthy())
}

/// Python's `round(int, ndigits)`: an integer, rounded half to even at a
/// negative number of digits.
fn round_int(i: i128, precision: i32, pos: usize) -> R<Val> {
    if precision >= 0 {
        return Ok(Val::Int(i));
    }
    let Some(m) = 10i128.checked_pow(precision.unsigned_abs()) else {
        // Wider than any i128: everything rounds to zero.
        return Ok(Val::Int(0));
    };
    let (q, r) = (i.div_euclid(m), i.rem_euclid(m));
    let up = r > m - r || (r == m - r && q.rem_euclid(2) == 1);
    let q = if up { q + 1 } else { q };
    q.checked_mul(m)
        .map(Val::Int)
        .ok_or_else(|| Fail::ty("integer overflow", pos))
}

impl<'t> Renderer<'t> {
    pub(super) fn filter(&mut self, f: Filter, v: Val, args: CallArgs, pos: usize) -> R<Val> {
        match f {
            Filter::Abs => {
                args.bind("abs", [], pos)?;
                match v.as_num() {
                    Some(Num::Int(i)) => i
                        .checked_abs()
                        .map(Val::Int)
                        .ok_or_else(|| Fail::ty("integer overflow", pos)),
                    Some(Num::Float(x)) => Ok(Val::Float(x.abs())),
                    None => Err(Fail::ty(format!("abs() of a {}", v.type_name()), pos)),
                }
            }
            Filter::Attr => {
                let [name] = args.bind("attr", ["name"], pos)?;
                let name = self.opt_text(name, pos)?.unwrap_or_else(|| Arc::from(""));
                // Jinja2's `attr` reads attributes only, never map items.
                if matches!(v, Val::Map(_)) && !MAP_METHODS.contains(&&*name) {
                    return Ok(Val::Undef(None));
                }
                self.get_attr(&v, &name, pos)
            }
            Filter::Capitalize | Filter::Lower | Filter::Upper | Filter::Title | Filter::Escape => {
                args.bind("filter", [], pos)?;
                let s = self.text(&v, pos)?;
                self.step_text(s.len(), pos)?;
                let out = match f {
                    Filter::Capitalize => py_capitalize(&s),
                    Filter::Lower => s.to_lowercase(),
                    Filter::Upper => s.to_uppercase(),
                    Filter::Title => py_title(&s),
                    _ => html_escape(&s),
                };
                self.new_str(out, pos)
            }
            Filter::Center => {
                let [width] = args.bind("center", ["width"], pos)?;
                let width = int_arg(width, 80, "width", pos)?;
                let s = self.text(&v, pos)?;
                self.step_text(s.len(), pos)?;
                let len = s.chars().count() as i128;
                if width <= len {
                    return Ok(Val::Str(s));
                }
                // Python's str.center; all in i128, so a huge width is just
                // too large to allocate, never an overflow.
                let pad = width - len;
                let left = pad / 2 + (pad & width & 1);
                let right = pad - left;
                let (Ok(left), Ok(right)) = (usize::try_from(left), usize::try_from(right)) else {
                    return Err(Fail::limit("max_alloc_bytes", pos));
                };
                self.charge(s.len().saturating_add(left).saturating_add(right), pos)?;
                let mut out = " ".repeat(left);
                out.push_str(&s);
                out.push_str(&" ".repeat(right));
                Ok(Val::str(out))
            }
            Filter::Default => {
                let [value, boolean] = args.bind("default", ["default_value", "boolean"], pos)?;
                let use_default = v.is_undefined() || (bool_arg(boolean) && !v.truthy());
                if use_default {
                    Ok(value.unwrap_or_else(|| Val::str("")))
                } else {
                    Ok(v)
                }
            }
            Filter::Dictsort => {
                let [case_sensitive, by, reverse] =
                    args.bind("dictsort", ["case_sensitive", "by", "reverse"], pos)?;
                let Val::Map(m) = &v else {
                    return Err(Fail::ty(
                        format!("dictsort() needs a map, not a {}", v.type_name()),
                        pos,
                    ));
                };
                let cs = bool_arg(case_sensitive);
                let by_value = match self.opt_text(by, pos)?.as_deref() {
                    None | Some("key") => false,
                    Some("value") => true,
                    Some(_) => {
                        return Err(Fail::ty("dictsort(by=...) must be 'key' or 'value'", pos))
                    }
                };
                let mut pairs = m.entries.clone();
                self.sort_by_key(
                    &mut pairs,
                    |(k, val)| {
                        if by_value {
                            fold(val, cs)
                        } else {
                            fold(&Val::Str(k.clone()), cs)
                        }
                    },
                    pos,
                )?;
                if bool_arg(reverse) {
                    pairs.reverse();
                }
                let mut items = Vec::with_capacity(pairs.len());
                for (k, val) in pairs {
                    items.push(Val::list(vec![Val::Str(k), val]).map_err(deep(pos))?);
                }
                self.new_list(items, pos)
            }
            Filter::First | Filter::Last => {
                args.bind("first", [], pos)?;
                Ok(match &v {
                    Val::List(l) => {
                        let item = if f == Filter::First {
                            l.items.first()
                        } else {
                            l.items.last()
                        };
                        item.cloned().unwrap_or(Val::Undef(None))
                    }
                    Val::Str(s) => {
                        let c = if f == Filter::First {
                            s.chars().next()
                        } else {
                            s.chars().next_back()
                        };
                        c.map_or(Val::Undef(None), |c| Val::str(c.to_string()))
                    }
                    Val::Map(m) => {
                        let e = if f == Filter::First {
                            m.entries.first()
                        } else {
                            m.entries.last()
                        };
                        e.map_or(Val::Undef(None), |(k, _)| Val::Str(k.clone()))
                    }
                    Val::Undef(_) => Val::Undef(None),
                    other => {
                        return Err(Fail::ty(
                            format!("a {} has no first or last item", other.type_name()),
                            pos,
                        ))
                    }
                })
            }
            Filter::Float => {
                let [default] = args.bind("float", ["default"], pos)?;
                let default = default.unwrap_or(Val::Float(0.0));
                Ok(match &v {
                    Val::Float(x) => Val::Float(*x),
                    Val::Int(_) | Val::Bool(_) => Val::Float(v.as_num().map_or(0.0, Num::as_f64)),
                    Val::Str(s) => {
                        self.step_text(s.len(), pos)?;
                        py_parse_float(s).map_or(default, Val::Float)
                    }
                    Val::Undef(_) => return Err(undefined_use(&v, "convert", pos)),
                    _ => default,
                })
            }
            Filter::Indent => {
                let [width, first, blank] =
                    args.bind("indent", ["width", "first", "blank"], pos)?;
                let indent: String = match width {
                    None => "    ".into(),
                    Some(Val::Str(s)) => s.to_string(),
                    Some(w) => {
                        let n = w.as_int().ok_or_else(|| {
                            Fail::ty("indent width must be an integer or a string", pos)
                        })?;
                        let n = usize::try_from(n.max(0)).unwrap_or(0);
                        self.charge(n, pos)?;
                        " ".repeat(n)
                    }
                };
                if v.is_undefined() {
                    return Err(undefined_use(&v, "indent", pos));
                }
                let mut s = self.text(&v, pos)?.to_string();
                self.step_text(s.len(), pos)?;
                s.push('\n');
                let lines = py_splitlines(&s);
                self.charge(
                    s.len()
                        .saturating_add(lines.len().saturating_mul(indent.len().saturating_add(1))),
                    pos,
                )?;
                let mut out = String::new();
                if bool_arg(blank) {
                    for (i, line) in lines.iter().enumerate() {
                        if i > 0 {
                            out.push('\n');
                            out.push_str(&indent);
                        }
                        out.push_str(line);
                    }
                } else {
                    for (i, line) in lines.iter().enumerate() {
                        if i > 0 {
                            out.push('\n');
                            if !line.is_empty() {
                                out.push_str(&indent);
                            }
                        }
                        out.push_str(line);
                    }
                }
                if bool_arg(first) {
                    out.insert_str(0, &indent);
                }
                Ok(Val::str(out))
            }
            Filter::Int => {
                let [default, base] = args.bind("int", ["default", "base"], pos)?;
                let default = default.unwrap_or(Val::Int(0));
                let base = int_arg(base, 10, "base", pos)?;
                let base = u32::try_from(base).unwrap_or(10);
                match &v {
                    Val::Undef(_) => return Err(undefined_use(&v, "convert", pos)),
                    Val::Str(s) => self.step_text(s.len(), pos)?,
                    _ => {}
                }
                Ok(match &v {
                    Val::Int(i) => Val::Int(*i),
                    Val::Bool(b) => Val::Int(i128::from(*b)),
                    Val::Float(x) if x.is_finite() && x.abs() < 1e38 => Val::Int(x.trunc() as i128),
                    Val::Str(s) => match py_parse_int(s, base) {
                        Some(i) => Val::Int(i),
                        None => match py_parse_float(s) {
                            Some(x) if x.is_finite() && x.abs() < 1e38 => {
                                Val::Int(x.trunc() as i128)
                            }
                            _ => default,
                        },
                    },
                    _ => default,
                })
            }
            Filter::Items => {
                args.bind("items", [], pos)?;
                match &v {
                    Val::Map(m) => {
                        let pairs = self.pairs(m, pos)?;
                        Val::list(pairs).map_err(deep(pos))
                    }
                    Val::Undef(_) => self.new_list(Vec::new(), pos),
                    other => Err(Fail::ty(
                        format!("items() needs a map, not a {}", other.type_name()),
                        pos,
                    )),
                }
            }
            Filter::Join => {
                let [sep, attribute] = args.bind("join", ["d", "attribute"], pos)?;
                let sep = self.opt_text(sep, pos)?.unwrap_or_else(|| Arc::from(""));
                let attribute = self.opt_text(attribute, pos)?;
                let items = self.iterate(&v, pos)?;
                self.tick_iter(items.len(), pos)?;
                let mut parts = Vec::with_capacity(items.len());
                for item in &items {
                    let item = match &attribute {
                        Some(a) => self.attr_path(item, a, pos)?,
                        None => item.clone(),
                    };
                    parts.push(self.text(&item, pos)?);
                }
                self.join_parts(&parts, &sep, pos)
            }
            Filter::Length => {
                args.bind("length", [], pos)?;
                Ok(Val::Int(match &v {
                    Val::List(l) => l.items.len(),
                    Val::Map(m) => m.entries.len(),
                    Val::Str(s) => {
                        self.step_text(s.len(), pos)?;
                        s.chars().count()
                    }
                    Val::Undef(_) => 0,
                    other => {
                        return Err(Fail::ty(
                            format!("a {} has no length", other.type_name()),
                            pos,
                        ))
                    }
                } as i128))
            }
            Filter::List => {
                args.bind("list", [], pos)?;
                let items = self.iterate(&v, pos)?;
                self.new_list(items, pos)
            }
            Filter::Map => self.filter_map(v, args, pos),
            Filter::Max | Filter::Min => {
                let [case_sensitive, attribute] =
                    args.bind("max", ["case_sensitive", "attribute"], pos)?;
                let cs = bool_arg(case_sensitive);
                let attribute = self.opt_text(attribute, pos)?;
                let items = self.iterate(&v, pos)?;
                self.tick_iter(items.len(), pos)?;
                let mut best: Option<(Val, Val)> = None;
                for item in items {
                    let key = match &attribute {
                        Some(a) => self.attr_path(&item, a, pos)?,
                        None => item.clone(),
                    };
                    let key = fold(&key, cs);
                    let better = match &best {
                        None => true,
                        Some((bk, _)) => {
                            let o = self.order(&key, bk, "<", pos)?;
                            if f == Filter::Max {
                                o == Ordering::Greater
                            } else {
                                o == Ordering::Less
                            }
                        }
                    };
                    if better {
                        best = Some((key, item));
                    }
                }
                Ok(best.map_or(Val::Undef(None), |(_, item)| item))
            }
            Filter::Select | Filter::Reject | Filter::Selectattr | Filter::Rejectattr => {
                self.filter_select(f, v, args, pos)
            }
            Filter::Replace => {
                let [old, new, count] = args.bind("replace", ["old", "new", "count"], pos)?;
                let s = self.text(&v, pos)?;
                let old = self.opt_text(old, pos)?.unwrap_or_else(|| Arc::from(""));
                let new = self.opt_text(new, pos)?.unwrap_or_else(|| Arc::from(""));
                let count = match count {
                    None | Some(Val::None) => None,
                    Some(c) => Some(
                        c.as_int()
                            .ok_or_else(|| Fail::ty("count must be an integer", pos))?,
                    ),
                };
                self.replace(&s, &old, &new, count, pos)
            }
            Filter::Reverse => {
                args.bind("reverse", [], pos)?;
                match &v {
                    Val::Str(s) => {
                        self.step_text(s.len(), pos)?;
                        let out: String = s.chars().rev().collect();
                        self.new_str(out, pos)
                    }
                    _ => {
                        let mut items = self.iterate(&v, pos)?;
                        items.reverse();
                        self.new_list(items, pos)
                    }
                }
            }
            Filter::Round => {
                let [precision, method] = args.bind("round", ["precision", "method"], pos)?;
                let precision = int_arg(precision, 0, "precision", pos)?;
                let precision = i32::try_from(precision.clamp(-400, 400)).unwrap_or(0);
                let method = self.opt_text(method, pos)?;
                let common = method.as_deref().unwrap_or("common") == "common";
                let x = match v.as_num() {
                    Some(Num::Int(i)) if common => return round_int(i, precision, pos),
                    Some(n) => n.as_f64(),
                    None => return Err(Fail::ty(format!("round() of a {}", v.type_name()), pos)),
                };
                let scale = 10f64.powi(precision);
                if !common && (scale == 0.0 || scale.is_infinite()) {
                    // Python: `x * 10**p` overflows, or divides by 0.0.
                    return Err(Fail::ty("round() precision out of range", pos));
                }
                Ok(Val::Float(match method.as_deref().unwrap_or("common") {
                    "common" => py_round(x, precision),
                    "ceil" => (x * scale).ceil() / scale,
                    "floor" => (x * scale).floor() / scale,
                    _ => {
                        return Err(Fail::ty(
                            "round method must be 'common', 'ceil' or 'floor'",
                            pos,
                        ))
                    }
                }))
            }
            Filter::Safe => {
                args.bind("safe", [], pos)?;
                Ok(v)
            }
            Filter::Sort => {
                let [reverse, case_sensitive, attribute] =
                    args.bind("sort", ["reverse", "case_sensitive", "attribute"], pos)?;
                let cs = bool_arg(case_sensitive);
                let attribute = self.opt_text(attribute, pos)?;
                let items = self.iterate(&v, pos)?;
                let mut keyed = Vec::with_capacity(items.len());
                for item in items {
                    let key = match &attribute {
                        Some(a) => self.attr_path(&item, a, pos)?,
                        None => item.clone(),
                    };
                    keyed.push((fold(&key, cs), item));
                }
                self.sort_by_key(&mut keyed, |(k, _)| k.clone(), pos)?;
                if bool_arg(reverse) {
                    keyed.reverse();
                }
                self.new_list(keyed.into_iter().map(|(_, item)| item).collect(), pos)
            }
            Filter::String => {
                args.bind("string", [], pos)?;
                let s = self.text(&v, pos)?;
                Ok(Val::Str(s))
            }
            Filter::Sum => {
                let [attribute, start] = args.bind("sum", ["attribute", "start"], pos)?;
                let attribute = self.opt_text(attribute, pos)?;
                let mut acc = start.unwrap_or(Val::Int(0));
                let items = self.iterate(&v, pos)?;
                self.tick_iter(items.len(), pos)?;
                for item in items {
                    let item = match &attribute {
                        Some(a) => self.attr_path(&item, a, pos)?,
                        None => item,
                    };
                    acc = self.binop(super::super::ast::BinOp::Add, &acc, &item, pos)?;
                }
                Ok(acc)
            }
            Filter::Tojson => {
                let [indent, ensure_ascii, separators, sort_keys] = args.bind(
                    "tojson",
                    ["indent", "ensure_ascii", "separators", "sort_keys"],
                    pos,
                )?;
                let indent = match indent {
                    None | Some(Val::None) => None,
                    Some(Val::Str(s)) => Some(s.to_string()),
                    Some(w) => {
                        let n = w.as_int().ok_or_else(|| {
                            Fail::ty("indent must be an integer or a string", pos)
                        })?;
                        Some(" ".repeat(usize::try_from(n.clamp(0, 64)).unwrap_or(0)))
                    }
                };
                let (item_sep, key_sep) = match separators {
                    None | Some(Val::None) => (
                        if indent.is_some() { "," } else { ", " }.to_string(),
                        ": ".to_string(),
                    ),
                    Some(Val::List(l)) if l.items.len() == 2 => match (&l.items[0], &l.items[1]) {
                        (Val::Str(a), Val::Str(b)) => (a.to_string(), b.to_string()),
                        _ => return Err(Fail::ty("separators must be two strings", pos)),
                    },
                    Some(_) => return Err(Fail::ty("separators must be two strings", pos)),
                };
                let style = JsonStyle {
                    indent,
                    item_sep,
                    key_sep,
                    sort_keys: bool_arg(sort_keys),
                    ensure_ascii: bool_arg(ensure_ascii),
                };
                let mut sink = self.sink();
                write_json(&v, &style, &mut sink, 0).map_err(|e| verr(e, pos))?;
                self.new_str(sink.out, pos)
            }
            Filter::Trim => {
                let [chars] = args.bind("trim", ["chars"], pos)?;
                let s = self.text(&v, pos)?;
                self.step_text(s.len(), pos)?;
                let chars = self.opt_text(chars, pos)?;
                let out = py_strip(&s, chars.as_deref(), true, true);
                if out.len() == s.len() {
                    return Ok(Val::Str(s));
                }
                self.new_str(out.to_string(), pos)
            }
            Filter::Unique => {
                let [case_sensitive, attribute] =
                    args.bind("unique", ["case_sensitive", "attribute"], pos)?;
                let cs = bool_arg(case_sensitive);
                let attribute = self.opt_text(attribute, pos)?;
                let items = self.iterate(&v, pos)?;
                let mut seen: Vec<Val> = Vec::new();
                let mut out = Vec::new();
                for item in items {
                    let key = match &attribute {
                        Some(a) => self.attr_path(&item, a, pos)?,
                        None => item.clone(),
                    };
                    let key = fold(&key, cs);
                    self.tick_iter(seen.len().max(1), pos)?;
                    let mut dup = false;
                    for s in &seen {
                        if self.equals(s, &key, pos)? {
                            dup = true;
                            break;
                        }
                    }
                    if !dup {
                        seen.push(key);
                        out.push(item);
                    }
                }
                self.new_list(out, pos)
            }
            Filter::Wordcount => {
                args.bind("wordcount", [], pos)?;
                let s = self.text(&v, pos)?;
                self.step_text(s.len(), pos)?;
                let count = s
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .filter(|w| !w.is_empty())
                    .count();
                Ok(Val::Int(count as i128))
            }
        }
    }

    /// Stable sort by a key, failing (instead of panicking or producing an
    /// arbitrary order) when two keys cannot be ordered.
    ///
    /// The keys are ordered by [`sort_order`], a total order (NaN sorts after
    /// every other number), and sorted with a merge sort of our own that
    /// stops at the first comparison that fails, so a comparison error can
    /// never leave the order inconsistent. Every pair of keys that ends up
    /// adjacent is compared directly, so two kinds that cannot be ordered
    /// are always reported.
    fn sort_by_key<T>(&mut self, items: &mut Vec<T>, key: impl Fn(&T) -> Val, pos: usize) -> R<()> {
        let n = items.len();
        let cost = n.saturating_mul(usize::BITS as usize - n.leading_zeros() as usize);
        self.tick_iter(cost, pos)?;
        let keys: Vec<Val> = items.iter().map(&key).collect();
        let order = self.merge_sort(&keys, pos)?;
        let mut slots: Vec<Option<T>> = items.drain(..).map(Some).collect();
        for i in order {
            if let Some(item) = slots.get_mut(i).and_then(Option::take) {
                items.push(item);
            }
        }
        Ok(())
    }

    /// The positions of `keys` in sorted order (stable, bottom-up merge).
    fn merge_sort(&mut self, keys: &[Val], pos: usize) -> R<Vec<usize>> {
        let n = keys.len();
        let mut order: Vec<usize> = (0..n).collect();
        let mut merged: Vec<usize> = Vec::with_capacity(n);
        let mut width = 1usize;
        while width < n {
            merged.clear();
            let mut lo = 0;
            while lo < n {
                let mid = lo.saturating_add(width).min(n);
                let hi = mid.saturating_add(width).min(n);
                let (mut i, mut j) = (lo, mid);
                while i < mid && j < hi {
                    // Take from the right run only when strictly smaller,
                    // so equal keys keep their order.
                    if self.sort_cmp(&keys[order[j]], &keys[order[i]], pos)? == Ordering::Less {
                        merged.push(order[j]);
                        j += 1;
                    } else {
                        merged.push(order[i]);
                        i += 1;
                    }
                }
                merged.extend_from_slice(&order[i..mid]);
                merged.extend_from_slice(&order[j..hi]);
                lo = hi;
            }
            std::mem::swap(&mut order, &mut merged);
            width = width.saturating_mul(2);
        }
        Ok(order)
    }

    fn sort_cmp(&mut self, a: &Val, b: &Val, pos: usize) -> R<Ordering> {
        match sort_order(a, b, 0, &mut self.steps).map_err(halt(pos))? {
            Some(o) => Ok(o),
            // Jinja2 sorts by `[key]` lists, which compare `==` first: equal
            // keys never need ordering (`[none, none]|sort` is fine).
            None if self.equals(a, b, pos)? => Ok(Ordering::Equal),
            None if a.is_undefined() || b.is_undefined() => Err(undefined_use(
                if a.is_undefined() { a } else { b },
                "sort by",
                pos,
            )),
            None => Err(Fail::ty(
                format!(
                    "cannot sort a {} and a {} together",
                    a.type_name(),
                    b.type_name()
                ),
                pos,
            )),
        }
    }

    fn filter_map(&mut self, v: Val, args: CallArgs, pos: usize) -> R<Val> {
        let items = self.iterate(&v, pos)?;
        self.tick_iter(items.len(), pos)?;
        let attribute = args
            .kw
            .iter()
            .find(|(k, _)| &**k == "attribute")
            .map(|(_, v)| v.clone());
        if let Some(attribute) = attribute {
            let mut default = None;
            for (k, val) in &args.kw {
                match &**k {
                    "attribute" => {}
                    // `default=none` is no default, as in Jinja2.
                    "default" if matches!(val, Val::None) => default = None,
                    "default" => default = Some(val.clone()),
                    other => {
                        return Err(Fail::ty(
                            format!("map() got an unexpected keyword argument '{other}'"),
                            pos,
                        ))
                    }
                }
            }
            if !args.pos.is_empty() {
                return Err(Fail::ty(
                    "map(attribute=...) takes no positional arguments",
                    pos,
                ));
            }
            let path = self.text(&attribute, pos)?;
            let mut out = Vec::with_capacity(items.len());
            for item in &items {
                let got = match &default {
                    None => self.attr_path(item, &path, pos)?,
                    Some(d) => self.attr_path_or(item, &path, d, pos)?,
                };
                out.push(got);
            }
            return self.new_list(out, pos);
        }
        let mut positional = args.pos.into_iter();
        let name = match positional.next() {
            Some(Val::Str(s)) => s,
            _ => return Err(Fail::ty("map() needs a filter name or attribute=", pos)),
        };
        let filter = Filter::from_name(&name)
            .ok_or_else(|| Fail::new(TemplateErrorKind::UnknownFilter(name.to_string()), pos))?;
        let rest: Vec<Val> = positional.collect();
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            self.tick_call(pos)?;
            self.step(rest.len() + args.kw.len(), pos)?;
            let a = CallArgs {
                pos: rest.clone(),
                kw: args.kw.clone(),
            };
            // The named filter may be `map` again (`map('map', 'map', ...)`
            // over nested lists): each level is a level of nesting, so it is
            // bounded by `max_depth` like any other.
            // Calling `filter_map` directly for that case keeps each level
            // to this one frame rather than the whole filter dispatcher.
            self.enter(pos)?;
            let r = if filter == Filter::Map {
                self.filter_map(item, a, pos)
            } else {
                self.filter(filter, item, a, pos)
            };
            self.depth -= 1;
            out.push(r?);
        }
        self.new_list(out, pos)
    }

    /// `map(attribute=path, default=d)`: Jinja2 replaces each undefined part
    /// of the path with the default as it walks, so a missing intermediate
    /// part gives the default too (and the walk goes on from it).
    fn attr_path_or(&mut self, v: &Val, path: &str, default: &Val, pos: usize) -> R<Val> {
        let mut cur = v.clone();
        for part in path.split('.') {
            cur = self.get_item(&cur, &path_key(part), pos)?;
            if cur.is_undefined() {
                cur = default.clone();
            }
        }
        Ok(cur)
    }

    fn filter_select(&mut self, f: Filter, v: Val, args: CallArgs, pos: usize) -> R<Val> {
        let by_attr = matches!(f, Filter::Selectattr | Filter::Rejectattr);
        let keep_on = matches!(f, Filter::Select | Filter::Selectattr);
        args.no_kwargs("select", pos)?;
        let mut positional = args.pos.into_iter();
        let attr = if by_attr {
            match positional.next() {
                Some(a) => Some(self.text(&a, pos)?),
                None => return Err(Fail::ty("selectattr() needs an attribute name", pos)),
            }
        } else {
            None
        };
        let test =
            match positional.next() {
                None => None,
                Some(Val::Str(name)) => Some(Test::from_name(&name).ok_or_else(|| {
                    Fail::new(TemplateErrorKind::UnknownTest(name.to_string()), pos)
                })?),
                Some(other) => {
                    return Err(Fail::ty(
                        format!("a test name must be a string, not a {}", other.type_name()),
                        pos,
                    ))
                }
            };
        let test_args: Vec<Val> = positional.collect();
        let items = self.iterate(&v, pos)?;
        self.tick_iter(items.len(), pos)?;
        let mut out = Vec::new();
        for item in items {
            let subject = match &attr {
                Some(a) => self.attr_path(&item, a, pos)?,
                None => item.clone(),
            };
            let passed = match test {
                Some(t) => {
                    self.tick_call(pos)?;
                    self.step(test_args.len(), pos)?;
                    let a = CallArgs {
                        pos: test_args.clone(),
                        kw: Vec::new(),
                    };
                    self.test(t, &subject, a, pos)?
                }
                None => subject.truthy(),
            };
            if passed == keep_on {
                out.push(item);
            }
        }
        self.new_list(out, pos)
    }

    pub(super) fn test(&mut self, t: Test, v: &Val, args: CallArgs, pos: usize) -> R<bool> {
        if t.takes_arg() {
            let [other] = args.bind("test", ["other"], pos)?;
            let other = other.unwrap_or(Val::Undef(None));
            return Ok(match t {
                Test::Eq => self.equals(v, &other, pos)?,
                Test::Ne => !self.equals(v, &other, pos)?,
                Test::Lt => self.order(v, &other, "<", pos)? == Ordering::Less,
                Test::Le => self.order(v, &other, "<=", pos)? != Ordering::Greater,
                Test::Gt => self.order(v, &other, ">", pos)? == Ordering::Greater,
                Test::Ge => self.order(v, &other, ">=", pos)? != Ordering::Less,
                Test::In => self.contains(&other, v, pos)?,
                Test::Divisibleby => match (v.as_num(), other.as_num()) {
                    (Some(Num::Int(a)), Some(Num::Int(b))) => {
                        if b == 0 {
                            return Err(Fail::ty("division by zero", pos));
                        }
                        a.checked_rem(b).map_or(true, |r| r == 0)
                    }
                    (Some(a), Some(b)) => {
                        if b.as_f64() == 0.0 {
                            return Err(Fail::ty("division by zero", pos));
                        }
                        a.as_f64() % b.as_f64() == 0.0
                    }
                    _ => return Err(Fail::ty("divisibleby needs numbers", pos)),
                },
                _ => match (v, &other) {
                    (Val::None, Val::None) | (Val::Undef(_), Val::Undef(_)) => true,
                    (Val::Bool(a), Val::Bool(b)) => a == b,
                    (Val::Int(a), Val::Int(b)) => a == b,
                    (Val::Str(a), Val::Str(b)) => Arc::ptr_eq(a, b),
                    (Val::List(a), Val::List(b)) => std::rc::Rc::ptr_eq(a, b),
                    (Val::Map(a), Val::Map(b)) => std::rc::Rc::ptr_eq(a, b),
                    (Val::Ns(a), Val::Ns(b)) => std::rc::Rc::ptr_eq(a, b),
                    _ => false,
                },
            });
        }
        args.bind("test", [], pos)?;
        Ok(match t {
            Test::Boolean => matches!(v, Val::Bool(_)),
            Test::Callable => matches!(v, Val::Macro(_) | Val::Func(_) | Val::Method(_)),
            Test::Defined => !v.is_undefined(),
            Test::Undefined => v.is_undefined(),
            Test::Escaped => false,
            Test::Even | Test::Odd => match v.as_int() {
                Some(i) => (i.rem_euclid(2) == 1) == (t == Test::Odd),
                None if v.is_undefined() => return Err(undefined_use(v, "test", pos)),
                None => {
                    return Err(Fail::ty(
                        format!("a {} is not an integer", v.type_name()),
                        pos,
                    ))
                }
            },
            Test::False => matches!(v, Val::Bool(false)),
            Test::True => matches!(v, Val::Bool(true)),
            Test::Float => matches!(v, Val::Float(_)),
            Test::Integer => matches!(v, Val::Int(_)),
            Test::Number => matches!(v, Val::Int(_) | Val::Float(_) | Val::Bool(_)),
            Test::Iterable | Test::Sequence => {
                matches!(v, Val::List(_) | Val::Map(_) | Val::Str(_) | Val::Undef(_))
            }
            Test::Lower | Test::Upper => match v {
                Val::Str(s) => {
                    self.step_text(s.len(), pos)?;
                    let cased: Vec<char> = s
                        .chars()
                        .filter(|c| c.is_lowercase() || c.is_uppercase())
                        .collect();
                    !cased.is_empty()
                        && cased.iter().all(|c| {
                            if t == Test::Lower {
                                c.is_lowercase()
                            } else {
                                c.is_uppercase()
                            }
                        })
                }
                _ => false,
            },
            Test::Mapping => matches!(v, Val::Map(_)),
            Test::None => matches!(v, Val::None),
            Test::String => matches!(v, Val::Str(_)),
            _ => false,
        })
    }
}
