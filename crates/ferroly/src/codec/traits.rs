//! The `Encode` / `Decode` traits and their standard-library impls.

use std::collections::HashMap;

use ferroly::codec::{CodecError, Value};

/// A streaming output sink that a format (e.g. JSON) implements so that
/// [`Encode::encode_to`] can write a value directly, without building an
/// intermediate [`Value`] tree. The sink owns all punctuation/separators.
pub trait Encoder: Sized {
    /// Emits a null.
    fn encode_null(&mut self);
    /// Emits a boolean.
    fn encode_bool(&mut self, v: bool);
    /// Emits a signed integer.
    fn encode_i64(&mut self, v: i64);
    /// Emits an unsigned integer.
    fn encode_u64(&mut self, v: u64);
    /// Emits a floating-point number.
    fn encode_f64(&mut self, v: f64);
    /// Emits a string.
    fn encode_str(&mut self, v: &str);
    /// Emits a byte string.
    fn encode_bytes(&mut self, v: &[u8]);
    /// Begins a sequence of `len` items.
    fn begin_seq(&mut self, len: usize);
    /// Emits one sequence element (handling any separator).
    fn seq_entry<V: Encode + ?Sized>(&mut self, v: &V);
    /// Ends the current sequence.
    fn end_seq(&mut self);
    /// Begins a map of `len` entries.
    fn begin_map(&mut self, len: usize);
    /// Emits one map entry (handling any separator and the key).
    fn map_entry<V: Encode + ?Sized>(&mut self, key: &str, v: &V);
    /// Ends the current map.
    fn end_map(&mut self);
}

/// Converts a value into the [`Value`] model, or streams it to an [`Encoder`].
pub trait Encode {
    /// Produces the [`Value`] representation of `self`.
    fn encode(&self) -> Value;

    /// Streams `self` directly to `e`, bypassing the [`Value`] tree. The default
    /// replays `self.encode()`; concrete impls and the derive override it to
    /// write field by field.
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        emit_value(e, &self.encode());
    }
}

/// Walks a [`Value`] into an [`Encoder`] — the fallback path and the
/// implementation of `Encode for Value`.
pub fn emit_value<E: Encoder>(e: &mut E, value: &Value) {
    match value {
        Value::Null => e.encode_null(),
        Value::Bool(b) => e.encode_bool(*b),
        Value::Int(i) => e.encode_i64(*i),
        Value::UInt(u) => e.encode_u64(*u),
        Value::Float(f) => e.encode_f64(*f),
        Value::Str(s) => e.encode_str(s),
        Value::Bytes(b) => e.encode_bytes(b),
        Value::Array(a) => {
            e.begin_seq(a.len());
            for x in a {
                e.seq_entry(x);
            }
            e.end_seq();
        }
        Value::Object(o) => {
            e.begin_map(o.len());
            for (k, x) in o {
                e.map_entry(k, x);
            }
            e.end_map();
        }
    }
}

/// A streaming input source that a format (e.g. JSON) implements so that
/// [`Decode::decode_from`] can build a value directly, without materializing the
/// whole [`Value`] tree. Scalar leaves may still be pulled as a small [`Value`]
/// via [`decode_value`](Decoder::decode_value).
pub trait Decoder: Sized {
    /// Pulls the next value as a [`Value`] (used for scalar leaves and the
    /// fallback path). For a scalar this allocates nothing beyond the scalar.
    fn decode_value(&mut self) -> Result<Value, CodecError>;
    /// Pulls the next value as a string (single allocation; no double-copy).
    fn decode_string(&mut self) -> Result<String, CodecError>;
    /// Returns `true` if the next value is `null` (without consuming it).
    fn peek_null(&mut self) -> bool;
    /// Consumes a `null`.
    fn decode_null(&mut self) -> Result<(), CodecError>;
    /// Reads a sequence, invoking `f` positioned at each element.
    fn read_seq<F>(&mut self, f: F) -> Result<(), CodecError>
    where
        F: FnMut(&mut Self) -> Result<(), CodecError>;
    /// Reads a map, invoking `f` with each key positioned at its value.
    fn read_map<F>(&mut self, f: F) -> Result<(), CodecError>
    where
        F: FnMut(&mut Self, &str) -> Result<(), CodecError>;
    /// Skips the next value (an unknown field).
    fn skip_value(&mut self) -> Result<(), CodecError>;
}

/// Constructs a value from the [`Value`] model, or pulls it from a [`Decoder`].
pub trait Decode: Sized {
    /// Attempts to build `Self` from a [`Value`].
    fn decode(value: &Value) -> Result<Self, CodecError>;

    /// Builds `Self` directly from a [`Decoder`], bypassing the full [`Value`]
    /// tree. The default pulls a [`Value`] and defers to [`decode`](Decode::decode)
    /// — cheap and lenient for scalars; containers and the derive override it.
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        Self::decode(&d.decode_value()?)
    }
}

// ---- primitives ----------------------------------------------------------

impl Encode for bool {
    fn encode(&self) -> Value {
        Value::Bool(*self)
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        e.encode_bool(*self);
    }
}
impl Decode for bool {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        match value {
            Value::Bool(b) => Ok(*b),
            Value::Str(s) => match s.trim() {
                "true" => Ok(true),
                "false" => Ok(false),
                _ => Err(CodecError::expected("bool")),
            },
            _ => Err(CodecError::expected("bool")),
        }
    }
}

macro_rules! impl_signed {
    ($($t:ty),*) => {$(
        impl Encode for $t {
            fn encode(&self) -> Value { Value::Int(*self as i64) }
            fn encode_to<E: Encoder>(&self, e: &mut E) { e.encode_i64(*self as i64); }
        }
        impl Decode for $t {
            fn decode(value: &Value) -> Result<Self, CodecError> {
                let n = match value {
                    Value::Int(i) => *i,
                    Value::UInt(u) => i64::try_from(*u)
                        .map_err(|_| CodecError::out_of_range(stringify!($t)))?,
                    Value::Float(f) => *f as i64,
                    Value::Str(s) => s
                        .trim()
                        .parse::<i64>()
                        .map_err(|_| CodecError::expected("integer"))?,
                    _ => return Err(CodecError::expected("integer")),
                };
                <$t>::try_from(n).map_err(|_| CodecError::out_of_range(stringify!($t)))
            }
        }
    )*};
}
impl_signed!(i8, i16, i32, i64, isize);

macro_rules! impl_unsigned {
    ($($t:ty),*) => {$(
        impl Encode for $t {
            fn encode(&self) -> Value { Value::UInt(*self as u64) }
            fn encode_to<E: Encoder>(&self, e: &mut E) { e.encode_u64(*self as u64); }
        }
        impl Decode for $t {
            fn decode(value: &Value) -> Result<Self, CodecError> {
                let n = match value {
                    Value::UInt(u) => *u,
                    Value::Int(i) if *i >= 0 => *i as u64,
                    Value::Float(f) if *f >= 0.0 => *f as u64,
                    Value::Str(s) => s
                        .trim()
                        .parse::<u64>()
                        .map_err(|_| CodecError::expected("unsigned integer"))?,
                    _ => return Err(CodecError::expected("unsigned integer")),
                };
                <$t>::try_from(n).map_err(|_| CodecError::out_of_range(stringify!($t)))
            }
        }
    )*};
}
impl_unsigned!(u8, u16, u32, u64, usize);

macro_rules! impl_float {
    ($($t:ty),*) => {$(
        impl Encode for $t {
            fn encode(&self) -> Value { Value::Float(*self as f64) }
            fn encode_to<E: Encoder>(&self, e: &mut E) { e.encode_f64(*self as f64); }
        }
        impl Decode for $t {
            fn decode(value: &Value) -> Result<Self, CodecError> {
                match value {
                    Value::Float(f) => Ok(*f as $t),
                    Value::Int(i) => Ok(*i as $t),
                    Value::UInt(u) => Ok(*u as $t),
                    Value::Str(s) => s
                        .trim()
                        .parse::<f64>()
                        .map(|v| v as $t)
                        .map_err(|_| CodecError::expected("number")),
                    _ => Err(CodecError::expected("number")),
                }
            }
        }
    )*};
}
impl_float!(f32, f64);

impl Encode for String {
    fn encode(&self) -> Value {
        Value::Str(self.clone())
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        e.encode_str(self);
    }
}
impl Decode for String {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        value
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| CodecError::expected("string"))
    }
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        d.decode_string()
    }
}

impl Encode for str {
    fn encode(&self) -> Value {
        Value::Str(self.to_string())
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        e.encode_str(self);
    }
}
impl Encode for &str {
    fn encode(&self) -> Value {
        Value::Str((*self).to_string())
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        e.encode_str(self);
    }
}

impl Encode for char {
    fn encode(&self) -> Value {
        Value::Str(self.to_string())
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        let mut buf = [0u8; 4];
        e.encode_str(self.encode_utf8(&mut buf));
    }
}
impl Decode for char {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        value
            .as_str()
            .and_then(|s| s.chars().next().filter(|_| s.chars().count() == 1))
            .ok_or_else(|| CodecError::expected("single-character string"))
    }
}

// ---- containers ----------------------------------------------------------

impl<T: Encode> Encode for Option<T> {
    fn encode(&self) -> Value {
        match self {
            Some(v) => v.encode(),
            None => Value::Null,
        }
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        match self {
            Some(v) => v.encode_to(e),
            None => e.encode_null(),
        }
    }
}
impl<T: Decode> Decode for Option<T> {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        match value {
            Value::Null => Ok(None),
            other => Ok(Some(T::decode(other)?)),
        }
    }
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        if d.peek_null() {
            d.decode_null()?;
            Ok(None)
        } else {
            Ok(Some(T::decode_from(d)?))
        }
    }
}

impl<T: Encode> Encode for Vec<T> {
    fn encode(&self) -> Value {
        Value::Array(self.iter().map(Encode::encode).collect())
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        e.begin_seq(self.len());
        for v in self {
            e.seq_entry(v);
        }
        e.end_seq();
    }
}
impl<T: Decode> Decode for Vec<T> {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        // Tolerant `Value`-tree decoding so formats without a distinct array
        // syntax round-trip: `Null` (an absent/empty element) → empty vec, and a
        // lone scalar/object → one-element vec. This is what lets XML's
        // repeated-sibling convention reconstruct 0- and 1-element arrays, which
        // are otherwise indistinguishable from "absent" and "scalar". JSON's
        // normal path uses the strict streaming `decode_from` (which still
        // requires `[...]`), so this leniency does not loosen JSON parsing.
        match value {
            Value::Null => Ok(Vec::new()),
            Value::Array(a) => a.iter().map(T::decode).collect(),
            other => Ok(vec![T::decode(other)?]),
        }
    }
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        let mut out = Vec::new();
        d.read_seq(|d| {
            out.push(T::decode_from(d)?);
            Ok(())
        })?;
        Ok(out)
    }
}

impl<T: Encode> Encode for Box<T> {
    fn encode(&self) -> Value {
        (**self).encode()
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        (**self).encode_to(e);
    }
}
impl<T: Decode> Decode for Box<T> {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        Ok(Box::new(T::decode(value)?))
    }
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        Ok(Box::new(T::decode_from(d)?))
    }
}

impl<V: Encode> Encode for HashMap<String, V> {
    fn encode(&self) -> Value {
        Value::Object(self.iter().map(|(k, v)| (k.clone(), v.encode())).collect())
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        e.begin_map(self.len());
        for (k, v) in self {
            e.map_entry(k, v);
        }
        e.end_map();
    }
}
impl<V: Decode> Decode for HashMap<String, V> {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        let obj = value
            .as_object()
            .ok_or_else(|| CodecError::expected("object"))?;
        obj.iter()
            .map(|(k, v)| Ok((k.clone(), V::decode(v)?)))
            .collect()
    }
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        let mut out = HashMap::new();
        d.read_map(|d, key| {
            out.insert(key.to_string(), V::decode_from(d)?);
            Ok(())
        })?;
        Ok(out)
    }
}

impl Encode for () {
    fn encode(&self) -> Value {
        Value::Null
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        e.encode_null();
    }
}
impl Decode for () {
    fn decode(_value: &Value) -> Result<Self, CodecError> {
        Ok(())
    }
}

impl Encode for Value {
    fn encode(&self) -> Value {
        self.clone()
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        emit_value(e, self);
    }
}
impl Decode for Value {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        Ok(value.clone())
    }
}
