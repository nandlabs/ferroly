//! Compile-pass / compile-fail coverage for the derives, as doctests.
//!
//! This module only exists under `cfg(doctest)` (`cargo test -p
//! ferroly-derive`). Each [`case!`] yields a doctest that must compile and a
//! `compile_fail` doctest that differs from it by exactly the rejected
//! construct, so a failure can only come from the derive refusing that
//! construct — not from an unrelated error in the snippet.
//!
//! Every snippet is prefixed with `mock_codec.rs`, a stand-in for
//! `ferroly::codec`, because this crate cannot depend on `ferroly`. The exact
//! error messages are asserted by the unit tests next to each expansion.

/// `accepts` must compile; `rejects` (optional) must not. `runs` is a whole
/// program (it brings its own `main`) that must compile and exit cleanly.
macro_rules! case {
    ($name:ident, runs: $program:literal) => {
        #[doc = concat!("```\n", include_str!("mock_codec.rs"), "\n", $program, "\n```")]
        #[allow(dead_code)]
        struct $name;
    };
    ($name:ident, accepts: $good:literal) => {
        #[doc = concat!(
            "```\n", include_str!("mock_codec.rs"), "\n", $good, "\nfn main() {}\n```"
        )]
        #[allow(dead_code)]
        struct $name;
    };
    ($name:ident, accepts: $good:literal, rejects: $bad:literal) => {
        #[doc = concat!(
            "```\n", include_str!("mock_codec.rs"), "\n", $good, "\nfn main() {}\n```"
        )]
        #[doc = concat!(
            "```compile_fail\n", include_str!("mock_codec.rs"), "\n", $bad, "\nfn main() {}\n```"
        )]
        #[allow(dead_code)]
        struct $name;
    };
}

// ---- Encode / Decode: every supported shape expands to code that compiles --

case!(
    AllShapes,
    accepts: r##"
use ferroly_derive::{Decode, Encode};
use std::marker::PhantomData;

type Maybe<T> = Option<T>;
fn seven() -> u8 { 7 }

#[derive(Encode, Decode)]
#[ferroly(rename_all = "camelCase", deny_unknown_fields)]
struct Named {
    user_id: u8,
    #[ferroly(rename = "n", alias = "old", alias = "older")]
    name: String,
    #[ferroly(default)]
    retries: u8,
    #[ferroly(default = "seven")]
    level: u8,
    #[ferroly(default)]
    alias_of_option: Maybe<u8>,
    #[ferroly(skip)]
    cache: u8,
    #[ferroly(skip_none)]
    note: Option<String>,
    r#type: u8,
}

#[derive(Encode, Decode)]
struct Flat {
    id: u8,
    #[ferroly(flatten)]
    inner: Inner,
    #[ferroly(flatten)]
    maybe: Option<Inner>,
    #[ferroly(flatten)]
    rest: ferroly::codec::Value,
}

#[derive(Encode, Decode)]
struct Inner { x: u8 }

#[derive(Encode, Decode)]
struct Empty {}

#[derive(Encode, Decode)]
struct Unit;

#[derive(Encode, Decode)]
struct Newtype(u8);

#[derive(Encode, Decode)]
struct Pair(u8, String);

#[derive(Encode, Decode)]
struct Nothing();

#[derive(Encode, Decode)]
struct TypedId<T>(u8, #[ferroly(skip)] PhantomData<T>);

#[derive(Encode, Decode)]
struct Page<'a, T, const N: usize> where T: Clone {
    items: Option<T>,
    #[ferroly(skip)]
    marker: PhantomData<&'a [u8; N]>,
}

#[derive(Encode, Decode)]
#[ferroly(bound(encode = "T: ferroly::codec::Encode", decode = "T: ferroly::codec::Decode"))]
struct Explicit<T> { value: T }

#[derive(Encode, Decode)]
#[ferroly(rename_all = "snake_case")]
enum UnitOnly { FirstOne, #[ferroly(alias = "2nd")] SecondOne }

#[derive(Encode, Decode)]
enum External<T> {
    Unit,
    Newtype(T),
    Tuple(u8, String),
    #[ferroly(rename_all = "camelCase")]
    Struct { some_field: u8, #[ferroly(skip_none)] note: Option<u8> },
    Empty(),
}

#[derive(Encode, Decode)]
enum DataOnly { A(u8), B { x: u8 }, C(u8, u8) }

#[derive(Encode, Decode)]
#[ferroly(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum Internal { Ping, Move { x: u8, y: u8 } }

#[derive(Encode, Decode)]
#[ferroly(untagged)]
enum Untagged { Nothing, Number(u8), Pair(u8, u8), Named { name: String } }

#[derive(Encode, Decode)]
enum Never {}
"##
);

// ---- Encode: the two generated paths emit the same events ----------------

case!(
    EncodePathsAgree,
    runs: r##"
use ferroly::codec::{Encode, Encoder};
use ferroly_derive::Encode;

/// Records every `Encoder` call, including the lengths announced up front.
#[derive(Default)]
struct Recorder(Vec<String>);

impl Encoder for Recorder {
    fn encode_null(&mut self) { self.0.push("null".into()); }
    fn encode_i64(&mut self, v: i64) { self.0.push(v.to_string()); }
    fn encode_str(&mut self, v: &str) { self.0.push(format!("{v:?}")); }
    fn begin_seq(&mut self, len: usize) { self.0.push(format!("seq({len})")); }
    fn seq_entry<V: Encode + ?Sized>(&mut self, v: &V) { v.encode_to(self); }
    fn end_seq(&mut self) { self.0.push("end".into()); }
    fn begin_map(&mut self, len: usize) { self.0.push(format!("map({len})")); }
    fn map_entry<V: Encode + ?Sized>(&mut self, key: &str, v: &V) {
        self.0.push(format!("{key}:"));
        v.encode_to(self);
    }
    fn end_map(&mut self) { self.0.push("end".into()); }
}

fn events<T: Encode>(value: &T) -> String {
    let mut recorder = Recorder::default();
    value.encode_to(&mut recorder);
    recorder.0.join(" ")
}

/// Streams `value`, checks the `Value` path replays identically, and returns
/// the event trace.
fn trace<T: Encode>(value: &T) -> String {
    let streamed = events(value);
    assert_eq!(events(&value.encode()), streamed);
    streamed
}

#[derive(Encode)]
struct Inner { x: u8, y: u8 }

#[derive(Encode)]
struct Outer {
    a: u8,
    #[ferroly(skip_none)]
    b: Option<u8>,
    #[ferroly(flatten)]
    inner: Inner,
    #[ferroly(flatten)]
    again: Option<Inner>,
    #[ferroly(skip)]
    hidden: u8,
    #[ferroly(rename = "z")]
    last: Option<u8>,
}

#[derive(Encode)]
struct Unit;
#[derive(Encode)]
struct Newtype(u8);
#[derive(Encode)]
struct Pair(u8, #[ferroly(skip)] u8, String);

#[derive(Encode)]
enum External { Unit, One(u8), Two(u8, u8), Named { n: u8 } }

#[derive(Encode)]
#[ferroly(tag = "t")]
enum Internal { Unit, Named { n: u8, #[ferroly(skip_none)] m: Option<u8> } }

#[derive(Encode)]
#[ferroly(untagged)]
enum Untagged { Unit, One(u8), Two(u8, u8), Named { n: u8 } }

fn main() {
    // The announced map length is the number of entries actually written:
    // `b` is omitted, `again` contributes nothing, `hidden` is skipped.
    let outer = Outer { a: 1, b: None, inner: Inner { x: 2, y: 3 }, again: None, hidden: 9, last: None };
    assert_eq!(trace(&outer), "map(4) a: 1 x: 2 y: 3 z: null end");
    // `again` repeats keys `inner` already wrote, so its entries are left out.
    let outer = Outer {
        a: 1,
        b: Some(4),
        inner: Inner { x: 2, y: 3 },
        again: Some(Inner { x: 7, y: 8 }),
        hidden: 9,
        last: Some(5),
    };
    assert_eq!(trace(&outer), "map(5) a: 1 b: 4 x: 2 y: 3 z: 5 end");

    assert_eq!(trace(&Unit), "null");
    assert_eq!(trace(&Newtype(7)), "7");
    assert_eq!(trace(&Pair(1, 2, "s".into())), "seq(2) 1 \"s\" end");

    assert_eq!(trace(&External::Unit), "\"Unit\"");
    assert_eq!(trace(&External::One(1)), "map(1) One: 1 end");
    assert_eq!(trace(&External::Two(1, 2)), "map(1) Two: seq(2) 1 2 end end");
    assert_eq!(trace(&External::Named { n: 1 }), "map(1) Named: map(1) n: 1 end end");

    assert_eq!(trace(&Internal::Unit), "map(1) t: \"Unit\" end");
    assert_eq!(trace(&Internal::Named { n: 1, m: None }), "map(2) t: \"Named\" n: 1 end");
    assert_eq!(trace(&Internal::Named { n: 1, m: Some(2) }), "map(3) t: \"Named\" n: 1 m: 2 end");

    assert_eq!(trace(&Untagged::Unit), "null");
    assert_eq!(trace(&Untagged::One(1)), "1");
    assert_eq!(trace(&Untagged::Two(1, 2)), "seq(2) 1 2 end");
    assert_eq!(trace(&Untagged::Named { n: 1 }), "map(1) n: 1 end");
}
"##
);

// ---- Encode / Decode: rejected shapes ------------------------------------

case!(
    UnknownContainerAttribute,
    accepts: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
#[ferroly(rename_all = "camelCase")]
struct S { some_field: u8 }
"##,
    rejects: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
#[ferroly(rename_al = "camelCase")]
struct S { some_field: u8 }
"##
);

case!(
    UnknownFieldAttribute,
    accepts: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
struct S { #[ferroly(skip_none)] a: Option<u8> }
"##,
    rejects: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
struct S { #[ferroly(skip_if_none)] a: Option<u8> }
"##
);

case!(
    UnknownVariantAttribute,
    accepts: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
enum E { #[ferroly(rename = "a")] A }
"##,
    rejects: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
enum E { #[ferroly(skip_none)] A }
"##
);

case!(
    UnknownRenameRule,
    accepts: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
#[ferroly(rename_all = "snake_case")]
struct S { a: u8 }
"##,
    rejects: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
#[ferroly(rename_all = "snakecase")]
struct S { a: u8 }
"##
);

case!(
    RepeatedAttribute,
    accepts: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
struct S { #[ferroly(rename = "a", alias = "b")] x: u8 }
"##,
    rejects: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
struct S { #[ferroly(rename = "a", rename = "b")] x: u8 }
"##
);

case!(
    DuplicateKeyFromRename,
    accepts: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
struct S { a: u8, #[ferroly(rename = "c")] b: u8 }
"##,
    rejects: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
struct S { a: u8, #[ferroly(rename = "a")] b: u8 }
"##
);

case!(
    DuplicateKeyFromRenameAll,
    accepts: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
#[ferroly(rename_all = "camelCase")]
#[allow(non_snake_case)]
struct S { user_id: u8, userName: u8 }
"##,
    rejects: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
#[ferroly(rename_all = "camelCase")]
#[allow(non_snake_case)]
struct S { user_id: u8, userId: u8 }
"##
);

case!(
    DuplicateKeyFromAlias,
    accepts: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
struct S { a: u8, #[ferroly(alias = "old_b")] b: u8 }
"##,
    rejects: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
struct S { a: u8, #[ferroly(alias = "a")] b: u8 }
"##
);

case!(
    DuplicateKeyFromRawIdentifier,
    accepts: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
struct S { r#type: u8, #[ferroly(rename = "kind")] kind_of: u8 }
"##,
    rejects: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
struct S { r#type: u8, #[ferroly(rename = "type")] kind_of: u8 }
"##
);

case!(
    DuplicateVariantKey,
    accepts: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
enum E { A, #[ferroly(rename = "b")] B }
"##,
    rejects: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
enum E { A, #[ferroly(rename = "A")] B }
"##
);

case!(
    Union,
    accepts: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
struct U { a: u8, b: u8 }
"##,
    rejects: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
union U { a: u8, b: u8 }
"##
);

case!(
    TagOnTupleVariant,
    accepts: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
#[ferroly(tag = "type")]
enum E { A, B { x: u8 } }
"##,
    rejects: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
#[ferroly(tag = "type")]
enum E { A, B(u8) }
"##
);

case!(
    TagWithUntagged,
    accepts: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
#[ferroly(untagged)]
enum E { A, B { x: u8 } }
"##,
    rejects: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
#[ferroly(tag = "type", untagged)]
enum E { A, B { x: u8 } }
"##
);

case!(
    TagCollidesWithField,
    accepts: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
#[ferroly(tag = "type")]
enum E { A { kind: u8 } }
"##,
    rejects: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
#[ferroly(tag = "type")]
enum E { A { r#type: u8 } }
"##
);

case!(
    TagOnStruct,
    accepts: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
#[ferroly(deny_unknown_fields)]
struct S { a: u8 }
"##,
    rejects: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
#[ferroly(tag = "type")]
struct S { a: u8 }
"##
);

case!(
    SkipWithRename,
    accepts: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
struct S { #[ferroly(skip)] a: u8 }
"##,
    rejects: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
struct S { #[ferroly(skip, rename = "b")] a: u8 }
"##
);

case!(
    FlattenWithDenyUnknownFields,
    accepts: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
struct S { a: u8, #[ferroly(flatten)] rest: ferroly::codec::Value }
"##,
    rejects: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
#[ferroly(deny_unknown_fields)]
struct S { a: u8, #[ferroly(flatten)] rest: ferroly::codec::Value }
"##
);

case!(
    RenameOnTupleField,
    accepts: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
struct S(u8, #[ferroly(skip)] u8);
"##,
    rejects: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
struct S(u8, #[ferroly(rename = "x")] u8);
"##
);

case!(
    FlagWithValue,
    accepts: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
struct S { #[ferroly(skip)] a: u8 }
"##,
    rejects: r##"
#[derive(ferroly_derive::Encode, ferroly_derive::Decode)]
struct S { #[ferroly(skip = true)] a: u8 }
"##
);

// ---- FerrolyError --------------------------------------------------------

case!(
    ErrorStructShapes,
    accepts: r##"
use ferroly_derive::FerrolyError;
use std::error::Error;
use std::io;

#[derive(Debug, FerrolyError)]
#[error("request {id} failed")]
struct WithSource { id: u64, #[source] cause: io::Error }

#[derive(Debug, FerrolyError)]
#[error("io failed")]
struct TupleFrom(#[from] io::Error);

#[derive(Debug, FerrolyError)]
#[error("io failed")]
struct NamedFrom { #[from] cause: io::Error }

#[derive(Debug, FerrolyError)]
#[error(transparent)]
struct Transparent(#[from] io::Error);

#[derive(Debug, FerrolyError)]
#[error(transparent)]
struct Boxed(Box<dyn Error + Send + Sync>);

#[derive(Debug, FerrolyError)]
#[error("maybe caused")]
struct Optional { #[source] cause: Option<io::Error> }

#[derive(Debug, FerrolyError)]
#[error("{code:?} / {detail}")]
struct Generic<C, D> { code: C, detail: D }

#[derive(Debug, FerrolyError)]
enum Variants<S: Error + 'static> {
    #[error("named from")]
    Named { #[from] cause: io::Error },
    #[error(transparent)]
    Transparent { #[from] inner: std::fmt::Error },
    #[error("generic source")]
    Generic(#[source] S),
    #[error("keyword field {type}")]
    Raw { r#type: u8 },
    #[error("braces {{0}} and {0:>4}")]
    Braces(u8),
}
"##
);

case!(
    FromNeedsExactlyOneField,
    accepts: r##"
#[derive(Debug, ferroly_derive::FerrolyError)]
enum E { #[error("io failed")] Io(#[from] std::io::Error) }
"##,
    rejects: r##"
#[derive(Debug, ferroly_derive::FerrolyError)]
enum E { #[error("io failed")] Io(#[from] std::io::Error, String) }
"##
);

case!(
    FromNeedsExactlyOneNamedField,
    accepts: r##"
#[derive(Debug, ferroly_derive::FerrolyError)]
#[error("io failed")]
struct E { #[from] cause: std::io::Error }
"##,
    rejects: r##"
#[derive(Debug, ferroly_derive::FerrolyError)]
#[error("io failed")]
struct E { #[from] cause: std::io::Error, attempt: u8 }
"##
);

case!(
    FromTakesNoArguments,
    accepts: r##"
#[derive(Debug, ferroly_derive::FerrolyError)]
enum E { #[error("io failed")] Io(#[from] std::io::Error) }
"##,
    rejects: r##"
#[derive(Debug, ferroly_derive::FerrolyError)]
enum E { #[error("io failed")] Io(#[from(std::io::Error)] std::io::Error) }
"##
);

case!(
    FromBelongsOnAField,
    accepts: r##"
#[derive(Debug, ferroly_derive::FerrolyError)]
enum E { #[error("io failed")] Io(#[from] std::io::Error) }
"##,
    rejects: r##"
#[derive(Debug, ferroly_derive::FerrolyError)]
enum E { #[error("io failed")] #[from] Io(std::io::Error) }
"##
);

case!(
    FromTypeUsedTwice,
    accepts: r##"
#[derive(Debug, ferroly_derive::FerrolyError)]
enum E {
    #[error("a")] A(#[from] std::io::Error),
    #[error("b")] B(#[source] std::io::Error),
}
"##,
    rejects: r##"
#[derive(Debug, ferroly_derive::FerrolyError)]
enum E {
    #[error("a")] A(#[from] std::io::Error),
    #[error("b")] B(#[from] std::io::Error),
}
"##
);

case!(
    VariantWithoutErrorAttribute,
    accepts: r##"
#[derive(Debug, ferroly_derive::FerrolyError)]
enum E { #[error("a")] A, #[error("b")] B }
"##,
    rejects: r##"
#[derive(Debug, ferroly_derive::FerrolyError)]
enum E { #[error("a")] A, B }
"##
);

case!(
    TransparentNeedsOneField,
    accepts: r##"
#[derive(Debug, ferroly_derive::FerrolyError)]
#[error(transparent)]
struct E(std::io::Error);
"##,
    rejects: r##"
#[derive(Debug, ferroly_derive::FerrolyError)]
#[error(transparent)]
struct E(std::io::Error, u8);
"##
);

case!(
    MalformedErrorAttribute,
    accepts: r##"
#[derive(Debug, ferroly_derive::FerrolyError)]
#[error("code {0}")]
struct E(u8);
"##,
    rejects: r##"
#[derive(Debug, ferroly_derive::FerrolyError)]
#[error("code {}", 0)]
struct E(u8);
"##
);

case!(
    TwoSources,
    accepts: r##"
#[derive(Debug, ferroly_derive::FerrolyError)]
#[error("failed")]
struct E { #[source] a: std::io::Error, b: std::io::Error }
"##,
    rejects: r##"
#[derive(Debug, ferroly_derive::FerrolyError)]
#[error("failed")]
struct E { #[source] a: std::io::Error, #[source] b: std::io::Error }
"##
);

case!(
    ErrorUnion,
    accepts: r##"
#[derive(Debug, ferroly_derive::FerrolyError)]
#[error("failed")]
struct E { a: u8, b: u8 }
"##,
    rejects: r##"
#[derive(ferroly_derive::FerrolyError)]
#[error("failed")]
union E { a: u8, b: u8 }
"##
);
