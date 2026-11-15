//! Schema generation from types: the [`JsonSchema`] trait, its std impls, and
//! the helpers `#[derive(JsonSchema)]` expands to.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ferroly::codec::{DateTime, Value};

/// A type that describes its encoded form as a JSON-Schema document.
///
/// Derive it with `#[derive(JsonSchema)]` next to `Encode` / `Decode`: the
/// derive reads the same `#[ferroly(...)]` attributes, so the schema always
/// matches the wire form. [`Schema::of`](super::Schema::of) compiles it for
/// validation; [`json_schema`](Self::json_schema) returns the document
/// itself, to publish or to hand to other JSON-Schema tooling.
///
/// ```
/// use ferroly::codec::schema::{JsonSchema, Schema};
/// use ferroly::codec::{json, Decode, Encode};
///
/// #[derive(Encode, Decode, JsonSchema)]
/// #[ferroly(rename_all = "camelCase")]
/// struct NewUser {
///     user_name: String,
///     age: Option<u8>,
///     #[ferroly(default)]
///     tags: Vec<String>,
/// }
///
/// assert_eq!(
///     json::to_string(&NewUser::json_schema()),
///     concat!(
///         r#"{"type":"object","properties":{"userName":{"type":"string"},"#,
///         r#""age":{"type":["integer","null"],"minimum":0,"maximum":255},"#,
///         r#""tags":{"type":"array","items":{"type":"string"}}},"required":["userName"]}"#,
///     )
/// );
///
/// let schema = Schema::of::<NewUser>().unwrap();
/// assert!(schema.validate(&json::from_str(r#"{"userName":"ada","age":36}"#).unwrap()).is_ok());
/// let errs = schema.validate(&json::from_str(r#"{"age":300}"#).unwrap()).unwrap_err();
/// assert_eq!(errs.len(), 2); // userName missing, age above 255
/// ```
///
/// The document uses the keywords this module validates, plus a few it
/// ignores but full JSON-Schema tools understand: `anyOf` for enum
/// alternatives, `additionalProperties` for maps and `deny_unknown_fields`,
/// `prefixItems` for tuples, `format` and `uniqueItems`. Field aliases are
/// not described: the schema is the canonical (encoded) form.
///
/// # Recursive types
///
/// The document has no `$ref`, so a recursive type (a tree, a linked list,
/// an AST) cannot be described to unbounded depth. The derive describes the
/// outermost occurrence fully; where the type recurses into itself (directly
/// or through other derived types) the nested occurrence is the empty schema
/// `{}`, which accepts any value:
///
/// ```
/// use ferroly::codec::schema::{JsonSchema, Schema};
/// use ferroly::codec::{json, Decode, Encode};
///
/// #[derive(Encode, Decode, JsonSchema)]
/// struct Tree {
///     name: String,
///     kids: Vec<Tree>,
/// }
///
/// assert_eq!(
///     json::to_string(&Tree::json_schema()),
///     concat!(
///         r#"{"type":"object","properties":{"name":{"type":"string"},"#,
///         r#""kids":{"type":"array","items":{}}},"required":["name","kids"]}"#,
///     )
/// );
/// let schema = Schema::of::<Tree>().unwrap();
/// let doc = r#"{"name":"root","kids":[{"name":"leaf","kids":[]}]}"#;
/// assert!(schema.validate(&json::from_str(doc).unwrap()).is_ok());
/// ```
///
/// Decoding still checks every level. A hand-written impl for a recursive
/// type must stop the recursion itself.
pub trait JsonSchema {
    /// The JSON-Schema document for `Self`.
    fn json_schema() -> Value;
}

/// Builds a schema object from `(keyword, value)` pairs.
fn obj<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::Object(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

fn ty(name: &str) -> Value {
    obj([("type", Value::Str(name.to_string()))])
}

fn types(names: &[&str]) -> Value {
    Value::Array(names.iter().map(|n| Value::Str(n.to_string())).collect())
}

fn string_format(format: &str) -> Value {
    obj([
        ("type", Value::Str("string".into())),
        ("format", Value::Str(format.into())),
    ])
}

/// The shape of an RFC 3339 date-time, as a `pattern` this module checks.
const DATE_TIME_PATTERN: &str =
    r"^\d{4}-\d{2}-\d{2}[Tt ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:[Zz]|[+-]\d{2}:\d{2})$";

impl JsonSchema for bool {
    fn json_schema() -> Value {
        ty("boolean")
    }
}

macro_rules! bounded_integer {
    ($($t:ty),*) => {$(
        impl JsonSchema for $t {
            fn json_schema() -> Value {
                obj([
                    ("type", Value::Str("integer".into())),
                    ("minimum", Value::Int(<$t>::MIN as i64)),
                    ("maximum", Value::Int(<$t>::MAX as i64)),
                ])
            }
        }
    )*};
}
bounded_integer!(i8, i16, i32, u8, u16, u32);

macro_rules! wide_integer {
    ($($t:ty => $min:expr),*) => {$(
        impl JsonSchema for $t {
            fn json_schema() -> Value {
                match $min {
                    Some(min) => obj([
                        ("type", Value::Str("integer".into())),
                        ("minimum", Value::Int(min)),
                    ]),
                    None => ty("integer"),
                }
            }
        }
    )*};
}
wide_integer!(i64 => None::<i64>, isize => None::<i64>, u64 => Some(0), usize => Some(0));

/// Beyond 64 bits a 128-bit integer travels as a decimal string.
impl JsonSchema for i128 {
    fn json_schema() -> Value {
        obj([("type", types(&["integer", "string"]))])
    }
}

/// Beyond 64 bits a 128-bit integer travels as a decimal string.
impl JsonSchema for u128 {
    fn json_schema() -> Value {
        obj([
            ("type", types(&["integer", "string"])),
            ("minimum", Value::Int(0)),
        ])
    }
}

impl JsonSchema for f32 {
    fn json_schema() -> Value {
        ty("number")
    }
}

impl JsonSchema for f64 {
    fn json_schema() -> Value {
        ty("number")
    }
}

impl JsonSchema for String {
    fn json_schema() -> Value {
        ty("string")
    }
}

impl JsonSchema for str {
    fn json_schema() -> Value {
        ty("string")
    }
}

impl JsonSchema for char {
    fn json_schema() -> Value {
        obj([
            ("type", Value::Str("string".into())),
            ("minLength", Value::Int(1)),
            ("maxLength", Value::Int(1)),
        ])
    }
}

/// `T`'s schema, also accepting `null`.
impl<T: JsonSchema> JsonSchema for Option<T> {
    fn json_schema() -> Value {
        __private::nullable(T::json_schema())
    }
}

fn array_of(items: Value) -> Value {
    obj([("type", Value::Str("array".into())), ("items", items)])
}

impl<T: JsonSchema> JsonSchema for Vec<T> {
    fn json_schema() -> Value {
        array_of(T::json_schema())
    }
}

impl<T: JsonSchema> JsonSchema for [T] {
    fn json_schema() -> Value {
        array_of(T::json_schema())
    }
}

impl<T: JsonSchema, const N: usize> JsonSchema for [T; N] {
    fn json_schema() -> Value {
        obj([
            ("type", Value::Str("array".into())),
            ("items", T::json_schema()),
            ("minItems", Value::Int(N as i64)),
            ("maxItems", Value::Int(N as i64)),
        ])
    }
}

fn set_of(items: Value) -> Value {
    obj([
        ("type", Value::Str("array".into())),
        ("items", items),
        ("uniqueItems", Value::Bool(true)),
    ])
}

impl<T: JsonSchema, S> JsonSchema for HashSet<T, S> {
    fn json_schema() -> Value {
        set_of(T::json_schema())
    }
}

impl<T: JsonSchema> JsonSchema for BTreeSet<T> {
    fn json_schema() -> Value {
        set_of(T::json_schema())
    }
}

fn map_of(values: Value) -> Value {
    obj([
        ("type", Value::Str("object".into())),
        ("additionalProperties", values),
    ])
}

impl<V: JsonSchema, S> JsonSchema for HashMap<String, V, S> {
    fn json_schema() -> Value {
        map_of(V::json_schema())
    }
}

impl<V: JsonSchema> JsonSchema for BTreeMap<String, V> {
    fn json_schema() -> Value {
        map_of(V::json_schema())
    }
}

macro_rules! transparent {
    ($($p:ident),*) => {$(
        impl<T: JsonSchema + ?Sized> JsonSchema for $p<T> {
            fn json_schema() -> Value {
                T::json_schema()
            }
        }
    )*};
}
transparent!(Box, Rc, Arc);

impl<T: JsonSchema + ?Sized> JsonSchema for &T {
    fn json_schema() -> Value {
        T::json_schema()
    }
}

impl<T: JsonSchema + ToOwned + ?Sized> JsonSchema for Cow<'_, T> {
    fn json_schema() -> Value {
        T::json_schema()
    }
}

impl JsonSchema for () {
    fn json_schema() -> Value {
        __private::null()
    }
}

/// Any value: the empty schema.
impl JsonSchema for Value {
    fn json_schema() -> Value {
        Value::Object(Vec::new())
    }
}

/// `{ "secs", "nanos" }`, or a bare number of seconds.
impl JsonSchema for Duration {
    fn json_schema() -> Value {
        obj([
            ("type", types(&["object", "number"])),
            ("minimum", Value::Int(0)),
            (
                "properties",
                obj([
                    (
                        "secs",
                        obj([
                            ("type", Value::Str("integer".into())),
                            ("minimum", Value::Int(0)),
                        ]),
                    ),
                    (
                        "nanos",
                        obj([
                            ("type", Value::Str("integer".into())),
                            ("minimum", Value::Int(0)),
                            ("maximum", Value::Int(999_999_999)),
                        ]),
                    ),
                ]),
            ),
            ("required", types(&["secs"])),
        ])
    }
}

/// An RFC 3339 string.
impl JsonSchema for DateTime {
    fn json_schema() -> Value {
        obj([
            ("type", Value::Str("string".into())),
            ("format", Value::Str("date-time".into())),
            ("pattern", Value::Str(DATE_TIME_PATTERN.into())),
        ])
    }
}

/// An RFC 3339 string (the encoded form), `{ "secs", "nanos" }` (the encoded
/// form outside years 0000–9999), or seconds since the epoch.
impl JsonSchema for SystemTime {
    fn json_schema() -> Value {
        obj([
            ("type", types(&["string", "number", "object"])),
            ("format", Value::Str("date-time".into())),
            ("pattern", Value::Str(DATE_TIME_PATTERN.into())),
            (
                "properties",
                obj([
                    ("secs", ty("integer")),
                    (
                        "nanos",
                        obj([
                            ("type", Value::Str("integer".into())),
                            ("minimum", Value::Int(0)),
                            ("maximum", Value::Int(999_999_999)),
                        ]),
                    ),
                ]),
            ),
            ("required", types(&["secs"])),
        ])
    }
}

macro_rules! text_form {
    ($($t:ty => $format:expr),*) => {$(
        impl JsonSchema for $t {
            fn json_schema() -> Value {
                match $format {
                    Some(format) => string_format(format),
                    None => ty("string"),
                }
            }
        }
    )*};
}
text_form!(
    PathBuf => None::<&str>,
    Path => None::<&str>,
    IpAddr => None::<&str>,
    Ipv4Addr => Some("ipv4"),
    Ipv6Addr => Some("ipv6"),
    SocketAddr => None::<&str>,
    SocketAddrV4 => None::<&str>,
    SocketAddrV6 => None::<&str>
);

macro_rules! tuple {
    ($($t:ident),+) => {
        impl<$($t: JsonSchema),+> JsonSchema for ($($t,)+) {
            fn json_schema() -> Value {
                __private::tuple(vec![$($t::json_schema()),+])
            }
        }
    };
}
tuple!(T0);
tuple!(T0, T1);
tuple!(T0, T1, T2);
tuple!(T0, T1, T2, T3);
tuple!(T0, T1, T2, T3, T4);
tuple!(T0, T1, T2, T3, T4, T5);
tuple!(T0, T1, T2, T3, T4, T5, T6);
tuple!(T0, T1, T2, T3, T4, T5, T6, T7);
tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8);
tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9);
tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10);
tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11);

/// Support code for `#[derive(JsonSchema)]`. Not public API.
#[doc(hidden)]
pub mod __private {
    use std::cell::RefCell;

    use ferroly::codec::Value;

    use super::{obj, ty, types};

    type Props = Vec<(String, Value)>;

    thread_local! {
        /// The derived types whose schema is being built on this thread,
        /// outermost first.
        static IN_PROGRESS: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    }

    /// Pops [`IN_PROGRESS`] when a derived schema is done (or unwinds).
    struct Leave;

    impl Drop for Leave {
        fn drop(&mut self) {
            IN_PROGRESS.with(|stack| {
                stack.borrow_mut().pop();
            });
        }
    }

    /// The schema `build` makes for the derived type `T`; or, when `T`'s
    /// schema is already being built further up this thread's stack (the
    /// type recurses into itself), the empty schema `{}`, so that a
    /// recursive type's schema is finite.
    pub fn recursive<T: ?Sized>(build: impl FnOnce() -> Value) -> Value {
        let name = std::any::type_name::<T>();
        let entered = IN_PROGRESS.with(|stack| {
            let mut stack = stack.borrow_mut();
            if stack.contains(&name) {
                false
            } else {
                stack.push(name);
                true
            }
        });
        if !entered {
            return Value::Object(Vec::new());
        }
        let _leave = Leave;
        build()
    }

    /// Whether the schema `alt` accepts `null`: it is empty, or its `type`
    /// names `null`.
    fn accepts_null(alt: &Value) -> bool {
        match alt {
            Value::Object(entries) if entries.is_empty() => true,
            _ => type_names(alt).is_some_and(|names| names.iter().any(|n| n == "null")),
        }
    }

    /// `{"type": "null"}`.
    pub fn null() -> Value {
        ty("null")
    }

    /// A fixed-length array of the given item schemas.
    pub fn tuple(items: Vec<Value>) -> Value {
        let n = Value::Int(items.len() as i64);
        obj([
            ("type", Value::Str("array".into())),
            ("prefixItems", Value::Array(items)),
            ("minItems", n.clone()),
            ("maxItems", n),
        ])
    }

    /// `schema`, also accepting `null`: `null` joins its `type` and `enum`,
    /// and an `anyOf` gains a `{"type":"null"}` alternative (a full
    /// JSON-Schema validator applies every keyword, so each must accept it).
    pub fn nullable(mut schema: Value) -> Value {
        if let Value::Object(entries) = &mut schema {
            for (key, value) in entries.iter_mut() {
                match (key.as_str(), value) {
                    ("type", t @ Value::Str(_)) => {
                        let name = std::mem::take(t);
                        if name.as_str() != Some("null") {
                            *t = Value::Array(vec![name, Value::Str("null".into())]);
                        } else {
                            *t = name;
                        }
                    }
                    ("type", Value::Array(names))
                        if !names.iter().any(|n| n.as_str() == Some("null")) =>
                    {
                        names.push(Value::Str("null".into()));
                    }
                    ("enum", Value::Array(choices)) if !choices.iter().any(Value::is_null) => {
                        choices.push(Value::Null);
                    }
                    ("anyOf", Value::Array(alts)) if !alts.iter().any(accepts_null) => {
                        alts.push(null());
                    }
                    _ => {}
                }
            }
        }
        schema
    }

    /// Adds the property `key` with `schema`, listing it as required if
    /// `required`.
    pub fn field(
        props: &mut Props,
        required: &mut Vec<Value>,
        key: &str,
        schema: Value,
        is_required: bool,
    ) {
        props.push((key.to_string(), schema));
        if is_required {
            required.push(Value::Str(key.to_string()));
        }
    }

    /// Adds an internally tagged enum's tag property, fixed to `variant`.
    pub fn tag(props: &mut Props, required: &mut Vec<Value>, tag: &str, variant: &str) {
        let schema = obj([("const", Value::Str(variant.to_string()))]);
        field(props, required, tag, schema, true);
    }

    /// Merges a flattened field's object schema: its properties, and (unless
    /// the field is optional) its required list.
    pub fn flatten(props: &mut Props, required: &mut Vec<Value>, schema: Value, optional: bool) {
        let Value::Object(entries) = schema else {
            return;
        };
        for (key, value) in entries {
            match (key.as_str(), value) {
                ("properties", Value::Object(inner)) => {
                    for (k, v) in inner {
                        if !props.iter().any(|(p, _)| *p == k) {
                            props.push((k, v));
                        }
                    }
                }
                ("required", Value::Array(names)) if !optional => {
                    for name in names {
                        if !required.contains(&name) {
                            required.push(name);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// An object schema with `props`; `deny` (from `deny_unknown_fields`)
    /// forbids other members.
    pub fn object(props: Props, required: Vec<Value>, deny: bool) -> Value {
        let mut entries = vec![
            ("type".to_string(), Value::Str("object".into())),
            ("properties".to_string(), Value::Object(props)),
        ];
        if !required.is_empty() {
            entries.push(("required".to_string(), Value::Array(required)));
        }
        if deny {
            entries.push(("additionalProperties".to_string(), Value::Bool(false)));
        }
        Value::Object(entries)
    }

    /// An externally tagged data variant: `{ name: content }`.
    pub fn wrap(name: &str, content: Value) -> Value {
        object(
            vec![(name.to_string(), content)],
            vec![Value::Str(name.to_string())],
            true,
        )
    }

    /// The type names `schema` allows, if it states them.
    fn type_names(schema: &Value) -> Option<Vec<String>> {
        match schema.get("type")? {
            Value::Str(name) => Some(vec![name.clone()]),
            Value::Array(names) => names
                .iter()
                .map(|n| n.as_str().map(str::to_string))
                .collect(),
            _ => None,
        }
    }

    /// Unit variant names (a string `enum`) and data alternatives (`anyOf`).
    /// The validator checks a `type` union of the alternatives; full
    /// JSON-Schema tools also check `anyOf`.
    pub fn one_of(units: &[&str], alternatives: Vec<Value>) -> Value {
        let mut alts = Vec::with_capacity(alternatives.len() + 1);
        if !units.is_empty() {
            alts.push(obj([
                ("type", Value::Str("string".into())),
                ("enum", types(units)),
            ]));
        }
        alts.extend(alternatives);
        match alts.len() {
            // An empty enum has no value.
            0 => obj([("enum", Value::Array(Vec::new()))]),
            1 => alts.pop().unwrap_or_default(),
            _ => {
                let mut union: Option<Vec<String>> = Some(Vec::new());
                for alt in &alts {
                    union = match (union, type_names(alt)) {
                        (Some(mut acc), Some(names)) => {
                            for name in names {
                                if !acc.contains(&name) {
                                    acc.push(name);
                                }
                            }
                            Some(acc)
                        }
                        _ => None,
                    };
                }
                let mut entries = Vec::new();
                if let Some(names) = union {
                    let names: Vec<&str> = names.iter().map(String::as_str).collect();
                    entries.push(("type".to_string(), types(&names)));
                }
                entries.push(("anyOf".to_string(), Value::Array(alts)));
                Value::Object(entries)
            }
        }
    }

    /// An internally tagged enum: an object whose `tag` member is one of
    /// `names`, plus one `anyOf` alternative per variant.
    pub fn internal(tag: &str, names: &[&str], mut alternatives: Vec<Value>) -> Value {
        match alternatives.len() {
            0 => obj([("enum", Value::Array(Vec::new()))]),
            1 => alternatives.pop().unwrap_or_default(),
            _ => obj([
                ("type", Value::Str("object".into())),
                ("properties", obj([(tag, obj([("enum", types(names))]))])),
                ("required", types(&[tag])),
                ("anyOf", Value::Array(alternatives)),
            ]),
        }
    }
}
