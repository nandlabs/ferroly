//! The in-memory data model that all codecs (de)encode through.

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
