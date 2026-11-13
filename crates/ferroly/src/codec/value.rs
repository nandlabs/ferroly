//! The in-memory data model that all codecs encode and decode through.

use std::collections::HashMap;

/// A format-agnostic data value — the intermediate all codecs operate on.
///
/// `Object` preserves insertion order (a `Vec` of pairs), which keeps encoded
/// output stable and field order meaningful.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Value {
    /// The absence of a value.
    #[default]
    Null,
    /// A boolean.
    Bool(bool),
    /// A signed integer.
    Int(i64),
    /// An unsigned integer (for values exceeding `i64::MAX`).
    UInt(u64),
    /// A floating-point number.
    Float(f64),
    /// A UTF-8 string.
    Str(String),
    /// A raw byte string. Note: the text codecs have no native byte type, so
    /// `Bytes` encodes as an array of integers (JSON/YAML) or space-separated
    /// numbers (XML) and decodes back as that array/string — it does **not**
    /// round-trip as `Bytes`. Keep binary data out of the text codecs, or encode
    /// it yourself (e.g. base64 into a `Str`).
    Bytes(Vec<u8>),
    /// An ordered sequence of values.
    Array(Vec<Value>),
    /// An ordered map of string keys to values.
    Object(Vec<(String, Value)>),
}

impl Value {
    /// Returns the object entries if this is an [`Value::Object`].
    pub fn as_object(&self) -> Option<&Vec<(String, Value)>> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }

    /// Returns the array elements if this is a [`Value::Array`].
    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    /// Returns the string if this is a [`Value::Str`].
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Returns the boolean if this is a [`Value::Bool`].
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Returns the value as an `i64` if it is an integer.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::UInt(u) => i64::try_from(*u).ok(),
            _ => None,
        }
    }

    /// Returns the value as a `u64` if it is a non-negative integer.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::UInt(u) => Some(*u),
            Value::Int(i) if *i >= 0 => Some(*i as u64),
            _ => None,
        }
    }

    /// Returns the value as an `f64` if it is numeric.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            Value::Int(i) => Some(*i as f64),
            Value::UInt(u) => Some(*u as f64),
            _ => None,
        }
    }

    /// Looks up a key in an object value.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_object().and_then(|o| find(o, key))
    }

    /// Indexes into an array value.
    pub fn get_index(&self, i: usize) -> Option<&Value> {
        self.as_array().and_then(|a| a.get(i))
    }

    /// True if this is [`Value::Null`].
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Returns the object entries mutably if this is a [`Value::Object`].
    pub fn as_object_mut(&mut self) -> Option<&mut Vec<(String, Value)>> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }

    /// Returns the array elements mutably if this is a [`Value::Array`].
    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Value>> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    /// Looks up a key in an object value, mutably. Like [`get`](Self::get), the
    /// **last** occurrence of a duplicated key wins.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.as_object_mut()?
            .iter_mut()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    /// Indexes into an array value, mutably.
    pub fn get_index_mut(&mut self, i: usize) -> Option<&mut Value> {
        self.as_array_mut()?.get_mut(i)
    }

    /// Replaces this value with [`Value::Null`] and returns the original.
    pub fn take(&mut self) -> Value {
        std::mem::take(self)
    }

    /// Looks up a nested value by JSON Pointer (RFC 6901).
    ///
    /// The empty pointer `""` is the value itself; otherwise the pointer is a
    /// sequence of `/`-prefixed tokens, each an object key or an array index.
    /// Inside a token `~1` stands for `/` and `~0` for `~`.
    ///
    /// ```
    /// use ferroly::codec::{json, Value};
    ///
    /// let v = json::from_str(r#"{"a":{"b":[10,20]},"x/y":1}"#).unwrap();
    /// assert_eq!(v.pointer("/a/b/1").and_then(Value::as_i64), Some(20));
    /// assert_eq!(v.pointer("/x~1y").and_then(Value::as_i64), Some(1));
    /// assert!(v.pointer("/a/missing").is_none());
    /// ```
    pub fn pointer(&self, pointer: &str) -> Option<&Value> {
        if pointer.is_empty() {
            return Some(self);
        }
        let mut cur = self;
        for token in pointer.strip_prefix('/')?.split('/') {
            cur = match cur {
                Value::Object(o) => find(o, &unescape_pointer_token(token))?,
                Value::Array(a) => a.get(pointer_index(token)?)?,
                _ => return None,
            };
        }
        Some(cur)
    }

    /// The mutable form of [`pointer`](Self::pointer).
    pub fn pointer_mut(&mut self, pointer: &str) -> Option<&mut Value> {
        if pointer.is_empty() {
            return Some(self);
        }
        let mut cur = self;
        for token in pointer.strip_prefix('/')?.split('/') {
            cur = match cur {
                Value::Object(_) => cur.get_mut(&unescape_pointer_token(token))?,
                Value::Array(a) => a.get_mut(pointer_index(token)?)?,
                _ => return None,
            };
        }
        Some(cur)
    }

    /// Deep-merges `other` into `self`.
    ///
    /// When both sides are objects their members are merged key by key
    /// (recursively); keys only in `other` are appended in order. In every
    /// other case — scalars, arrays, mismatched kinds, and `Null` — `other`
    /// replaces `self`.
    ///
    /// ```
    /// use ferroly::codec::json;
    ///
    /// let mut base = json::from_str(r#"{"db":{"host":"a","port":1},"tags":[1]}"#).unwrap();
    /// base.merge(json::from_str(r#"{"db":{"port":2},"tags":[9],"new":true}"#).unwrap());
    /// assert_eq!(
    ///     json::to_string(&base),
    ///     r#"{"db":{"host":"a","port":2},"tags":[9],"new":true}"#
    /// );
    /// ```
    pub fn merge(&mut self, other: Value) {
        match (self, other) {
            (Value::Object(base), Value::Object(overlay)) => {
                // Index large objects so a wide merge stays linear.
                let mut index: Option<HashMap<String, usize>> = if base.len() > 16 {
                    Some(
                        base.iter()
                            .enumerate()
                            .map(|(i, (k, _))| (k.clone(), i))
                            .collect(),
                    )
                } else {
                    None
                };
                for (key, value) in overlay {
                    let pos = match &index {
                        Some(map) => map.get(&key).copied(),
                        None => base.iter().rposition(|(k, _)| *k == key),
                    };
                    match pos {
                        Some(i) => base[i].1.merge(value),
                        None => {
                            if let Some(map) = &mut index {
                                map.insert(key.clone(), base.len());
                            }
                            base.push((key, value));
                        }
                    }
                }
            }
            (slot, other) => *slot = other,
        }
    }
}

/// Decodes the `~1` / `~0` escapes of one JSON Pointer reference token.
fn unescape_pointer_token(token: &str) -> std::borrow::Cow<'_, str> {
    if token.contains('~') {
        std::borrow::Cow::Owned(token.replace("~1", "/").replace("~0", "~"))
    } else {
        std::borrow::Cow::Borrowed(token)
    }
}

/// Parses a JSON Pointer array index: decimal digits with no leading zero.
fn pointer_index(token: &str) -> Option<usize> {
    if token.is_empty() || (token.len() > 1 && token.starts_with('0')) {
        return None;
    }
    if !token.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    token.parse().ok()
}

/// Finds a key's value in an object's entry slice. On duplicate keys the **last**
/// wins, matching how `#[derive(Decode)]` resolves repeated keys (JSON's
/// conventional last-wins semantics), so `Value::get` and struct decoding agree.
pub fn find<'a>(obj: &'a [(String, Value)], key: &str) -> Option<&'a Value> {
    obj.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
}

macro_rules! from_int {
    ($($t:ty),*) => {$(
        impl From<$t> for Value { fn from(v: $t) -> Value { Value::Int(v as i64) } }
    )*};
}
from_int!(i8, i16, i32, i64, isize);

macro_rules! from_uint {
    ($($t:ty),*) => {$(
        impl From<$t> for Value { fn from(v: $t) -> Value { Value::UInt(v as u64) } }
    )*};
}
from_uint!(u8, u16, u32, u64, usize);

impl From<f32> for Value {
    fn from(v: f32) -> Value {
        Value::Float(v as f64)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Value {
        Value::Float(v)
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Value {
        Value::Bool(v)
    }
}
impl From<String> for Value {
    fn from(v: String) -> Value {
        Value::Str(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Value {
        Value::Str(v.to_string())
    }
}
impl From<Vec<Value>> for Value {
    fn from(v: Vec<Value>) -> Value {
        Value::Array(v)
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Value {
        match v {
            Some(x) => x.into(),
            None => Value::Null,
        }
    }
}
