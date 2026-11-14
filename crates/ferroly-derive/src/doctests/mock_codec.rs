// A stand-in for `ferroly::codec`, textually included at the top of every
// doctest in `doctests/mod.rs`.
//
// `ferroly-derive` cannot depend on `ferroly` (that would be a dependency
// cycle), yet the code the derives generate names `::ferroly::codec::…`. This
// file declares the handful of items that code refers to, and
// `extern crate self as ferroly` makes those paths resolve to it, so the
// doctests can compile real expansions.
//
// Keep it in step with `crates/ferroly/src/codec/traits.rs`: it must declare
// every trait method the derives emit or call.
extern crate self as ferroly;

#[allow(dead_code)]
pub mod codec {
    #[derive(Debug, Clone, PartialEq)]
    pub enum Value {
        Null,
        Int(i64),
        Str(String),
        Array(Vec<Value>),
        Object(Vec<(String, Value)>),
    }

    impl Value {
        pub fn as_object(&self) -> Option<&Vec<(String, Value)>> {
            match self {
                Value::Object(o) => Some(o),
                _ => None,
            }
        }
        pub fn as_str(&self) -> Option<&str> {
            match self {
                Value::Str(s) => Some(s),
                _ => None,
            }
        }
    }

    #[derive(Debug)]
    pub enum CodecError {
        Expected(&'static str),
        MissingField(String),
        UnknownVariant(String),
        Message(String),
    }

    impl CodecError {
        pub fn expected(what: &'static str) -> Self {
            CodecError::Expected(what)
        }
        pub fn missing_field(name: impl Into<String>) -> Self {
            CodecError::MissingField(name.into())
        }
        pub fn unknown_variant(name: impl Into<String>) -> Self {
            CodecError::UnknownVariant(name.into())
        }
    }

    pub trait Encoder: Sized {
        fn encode_null(&mut self);
        fn encode_i64(&mut self, v: i64);
        fn encode_str(&mut self, v: &str);
        fn begin_seq(&mut self, len: usize);
        fn seq_entry<V: Encode + ?Sized>(&mut self, v: &V);
        fn end_seq(&mut self);
        fn begin_map(&mut self, len: usize);
        fn map_entry<V: Encode + ?Sized>(&mut self, key: &str, v: &V);
        fn end_map(&mut self);
    }

    pub trait Encode {
        fn encode(&self) -> Value;
        fn encode_to<E: Encoder>(&self, e: &mut E);
    }

    pub trait Decoder: Sized {
        fn decode_value(&mut self) -> Result<Value, CodecError>;
        fn decode_string(&mut self) -> Result<String, CodecError>;
        fn decode_null(&mut self) -> Result<(), CodecError>;
        fn read_seq<F>(&mut self, f: F) -> Result<(), CodecError>
        where
            F: FnMut(&mut Self) -> Result<(), CodecError>;
        fn read_map<F>(&mut self, f: F) -> Result<(), CodecError>
        where
            F: FnMut(&mut Self, &str) -> Result<(), CodecError>;
        fn skip_value(&mut self) -> Result<(), CodecError>;
    }

    pub trait Decode: Sized {
        fn decode(value: &Value) -> Result<Self, CodecError>;
        fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
            Self::decode(&d.decode_value()?)
        }
    }

    // Just enough leaf impls for the field types the doctests use.
    impl Encode for u8 {
        fn encode(&self) -> Value {
            Value::Int(i64::from(*self))
        }
        fn encode_to<E: Encoder>(&self, e: &mut E) {
            e.encode_i64(i64::from(*self));
        }
    }
    impl Decode for u8 {
        fn decode(value: &Value) -> Result<Self, CodecError> {
            match value {
                Value::Int(i) => u8::try_from(*i).map_err(|_| CodecError::expected("u8")),
                _ => Err(CodecError::expected("integer")),
            }
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
                .ok_or(CodecError::expected("string"))
        }
    }
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
    }
    impl Encode for Value {
        fn encode(&self) -> Value {
            self.clone()
        }
        fn encode_to<E: Encoder>(&self, e: &mut E) {
            match self {
                Value::Null => e.encode_null(),
                Value::Int(i) => e.encode_i64(*i),
                Value::Str(s) => e.encode_str(s),
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
    }
    impl Decode for Value {
        fn decode(value: &Value) -> Result<Self, CodecError> {
            Ok(value.clone())
        }
    }
}
