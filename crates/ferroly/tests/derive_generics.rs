#![cfg(feature = "codec")]
//! `#[derive(Encode)]` / `#[derive(Decode)]` on generic types: type parameters,
//! lifetimes, const parameters, where-clauses, inferred and explicit bounds.

use std::fmt::Debug;
use std::marker::PhantomData;

use ferroly::codec::{json, Decode, Encode};

/// Checks one value against its JSON text on all four generated paths: the
/// streaming and `Value` encoders must both produce `text`, and the streaming
/// and `Value` decoders must both read it back.
fn round_trip<T: Encode + Decode + PartialEq + Debug>(value: &T, text: &str) {
    assert_eq!(json::encode(value), text, "streaming encode");
    assert_eq!(json::to_string(&value.encode()), text, "value encode");
    assert_eq!(
        &json::decode::<T>(text).expect("streaming decode"),
        value,
        "streaming decode"
    );
    let tree = json::from_str(text).expect("valid JSON");
    assert_eq!(
        &T::decode(&tree).expect("value decode"),
        value,
        "value decode"
    );
}

#[derive(Encode, Decode, PartialEq, Debug)]
struct Page<T> {
    items: Vec<T>,
    total: u64,
}

#[derive(Encode, Decode, PartialEq, Debug)]
struct Item {
    name: String,
}

#[test]
fn generic_struct_round_trips_with_inferred_bounds() {
    round_trip(
        &Page {
            items: vec![1u32, 2, 3],
            total: 3,
        },
        r#"{"items":[1,2,3],"total":3}"#,
    );
    round_trip(
        &Page {
            items: vec![Item { name: "a".into() }],
            total: 1,
        },
        r#"{"items":[{"name":"a"}],"total":1}"#,
    );
    round_trip(
        &Page::<Page<String>> {
            items: vec![Page {
                items: vec!["x".into()],
                total: 1,
            }],
            total: 1,
        },
        r#"{"items":[{"items":["x"],"total":1}],"total":1}"#,
    );
}

#[derive(Encode, Decode, PartialEq, Debug)]
struct Bounded<K, V: Clone>
where
    K: Clone + Debug,
{
    key: K,
    value: Option<V>,
}

#[test]
fn inline_bounds_and_where_clauses_are_kept() {
    round_trip(
        &Bounded {
            key: "k".to_string(),
            value: Some(5u8),
        },
        r#"{"key":"k","value":5}"#,
    );
    round_trip(
        &Bounded::<u8, String> {
            key: 1,
            value: None,
        },
        r#"{"key":1,"value":null}"#,
    );
}

#[derive(Encode, PartialEq, Debug)]
struct View<'a> {
    name: &'a str,
    tags: Vec<&'a str>,
}

#[test]
fn lifetime_parameters_encode_borrowed_data() {
    let owned = String::from("api");
    let view = View {
        name: &owned,
        tags: vec!["a", "b"],
    };
    let text = r#"{"name":"api","tags":["a","b"]}"#;
    assert_eq!(json::encode(&view), text);
    assert_eq!(json::to_string(&view.encode()), text);
}

#[derive(Encode, Decode, PartialEq, Debug)]
struct Scoped<'a, T> {
    value: T,
    #[ferroly(skip)]
    scope: PhantomData<&'a ()>,
}

#[test]
fn lifetime_parameters_decode_when_the_borrow_is_skipped() {
    round_trip(
        &Scoped {
            value: 9u8,
            scope: PhantomData,
        },
        r#"{"value":9}"#,
    );
}

/// Deliberately implements neither `Encode` nor `Decode`.
#[derive(PartialEq, Debug)]
struct Meters;

#[derive(Encode, Decode, PartialEq, Debug)]
struct Quantity<Unit>(u64, #[ferroly(skip)] PhantomData<Unit>);

#[test]
fn parameters_used_only_by_skipped_fields_get_no_bound() {
    // Compiles only because `Unit` is not required to be `Encode`/`Decode`.
    round_trip(&Quantity::<Meters>(42, PhantomData), "42");
}

#[derive(Encode, Decode, PartialEq, Debug)]
struct Fixed<const N: usize> {
    label: String,
}

#[derive(Encode, Decode, PartialEq, Debug)]
struct Defaulted<T = u8> {
    value: T,
}

#[test]
fn const_parameters_and_parameter_defaults() {
    round_trip(&Fixed::<4> { label: "x".into() }, r#"{"label":"x"}"#);
    let d: Defaulted = Defaulted { value: 7 };
    round_trip(&d, r#"{"value":7}"#);
    round_trip(
        &Defaulted::<String> { value: "s".into() },
        r#"{"value":"s"}"#,
    );
}

#[derive(Encode, Decode, PartialEq, Debug)]
enum Either<L, R> {
    Neither,
    Left(L),
    Right(R),
    Both { left: L, right: R },
    Pair(L, R),
}

#[test]
fn generic_enum_round_trips_in_every_variant_style() {
    type E = Either<u8, String>;
    round_trip(&E::Neither, r#""Neither""#);
    round_trip(&E::Left(1), r#"{"Left":1}"#);
    round_trip(&E::Right("r".into()), r#"{"Right":"r"}"#);
    round_trip(
        &E::Both {
            left: 1,
            right: "r".into(),
        },
        r#"{"Both":{"left":1,"right":"r"}}"#,
    );
    round_trip(&E::Pair(1, "r".into()), r#"{"Pair":[1,"r"]}"#);
    // Generic arguments may themselves be derived generic types.
    round_trip(
        &Either::<Page<u8>, ()>::Left(Page {
            items: vec![1],
            total: 1,
        }),
        r#"{"Left":{"items":[1],"total":1}}"#,
    );
}

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(tag = "kind")]
enum Envelope<T> {
    Empty,
    Full { payload: T },
}

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(untagged)]
enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

#[test]
fn generic_enums_in_the_other_representations() {
    round_trip(&Envelope::<u8>::Empty, r#"{"kind":"Empty"}"#);
    round_trip(
        &Envelope::Full {
            payload: Item { name: "a".into() },
        },
        r#"{"kind":"Full","payload":{"name":"a"}}"#,
    );
    round_trip(
        &OneOrMany::One(Item { name: "a".into() }),
        r#"{"name":"a"}"#,
    );
    round_trip(
        &OneOrMany::Many(vec![Item { name: "a".into() }, Item { name: "b".into() }]),
        r#"[{"name":"a"},{"name":"b"}]"#,
    );
}

trait Source {
    type Output;
}

#[derive(PartialEq, Debug)]
struct Sensor;

impl Source for Sensor {
    type Output = u32;
}

// The field's type is an associated type, so the inferred `S: Encode` would be
// both unnecessary and unsatisfiable; the bound is spelled out instead.
#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(bound(encode = "S::Output: Encode", decode = "S::Output: Decode"))]
struct Reading<S: Source> {
    output: S::Output,
}

// One string applies to both derives.
#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(bound = "T: Encode + Decode + Copy")]
struct Strict<T> {
    value: T,
}

// An empty bound removes the inferred one (nothing here needs `T`).
#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(bound = "")]
struct Unbounded<T> {
    count: u8,
    #[ferroly(skip)]
    marker: PhantomData<T>,
}

#[test]
fn explicit_bounds_replace_the_inferred_ones() {
    round_trip(&Reading::<Sensor> { output: 21 }, r#"{"output":21}"#);
    round_trip(&Strict { value: 3u8 }, r#"{"value":3}"#);
    round_trip(
        &Unbounded::<Meters> {
            count: 1,
            marker: PhantomData,
        },
        r#"{"count":1}"#,
    );
}

#[derive(Encode, Decode, PartialEq, Debug)]
struct WithFallback<T> {
    #[ferroly(default)]
    value: T,
    #[ferroly(skip)]
    cache: Vec<T>,
}

#[test]
fn default_and_skip_on_generic_fields() {
    // `T: Default` is inferred for the `default` field.
    let decoded: WithFallback<u8> = json::decode("{}").unwrap();
    assert_eq!(
        decoded,
        WithFallback {
            value: 0,
            cache: vec![]
        }
    );
    round_trip(
        &WithFallback {
            value: "v".to_string(),
            cache: vec![],
        },
        r#"{"value":"v"}"#,
    );
}
