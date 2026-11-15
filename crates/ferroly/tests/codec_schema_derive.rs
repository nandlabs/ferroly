#![cfg(feature = "codec")]
//! `#[derive(JsonSchema)]`: schemas generated from the derive model match
//! what `Encode` writes and what `Decode` accepts.

use std::collections::BTreeMap;

use ferroly::codec::schema::{JsonSchema, Schema};
use ferroly::codec::{json, DateTime, Decode, Encode, Value};

fn v(s: &str) -> Value {
    json::from_str(s).unwrap()
}

/// Asserts that validation and decoding agree on `doc`.
fn agrees<T: JsonSchema + Decode>(doc: &str, valid: bool) {
    let schema = Schema::of::<T>().unwrap();
    let data = v(doc);
    assert_eq!(schema.validate(&data).is_ok(), valid, "validate {doc}");
    assert_eq!(json::decode::<T>(doc).is_ok(), valid, "decode {doc}");
}

#[derive(Encode, Decode, JsonSchema, Debug, PartialEq)]
#[ferroly(rename_all = "camelCase")]
struct Account {
    account_id: u64,
    display_name: Option<String>,
    #[ferroly(default)]
    roles: Vec<Role>,
    #[ferroly(rename = "created")]
    created_at: DateTime,
    #[ferroly(skip)]
    cache: u8,
    #[ferroly(flatten)]
    audit: Audit,
}

#[derive(Encode, Decode, JsonSchema, Debug, PartialEq)]
struct Audit {
    by: String,
    #[ferroly(skip_none)]
    note: Option<String>,
}

#[derive(Encode, Decode, JsonSchema, Debug, PartialEq)]
#[ferroly(rename_all = "lowercase")]
enum Role {
    Admin,
    #[ferroly(alias = "member")]
    User,
}

#[test]
fn struct_schema_follows_the_wire_form() {
    let schema = Account::json_schema();
    let props = schema.get("properties").unwrap();
    let keys: Vec<&str> = match props {
        Value::Object(entries) => entries.iter().map(|(k, _)| k.as_str()).collect(),
        _ => panic!("{props:?}"),
    };
    assert_eq!(
        keys,
        ["accountId", "displayName", "roles", "created", "by", "note"]
    );
    assert_eq!(
        schema.get("required"),
        Some(&v(r#"["accountId","created","by"]"#))
    );
    assert_eq!(
        props.get("displayName"),
        Some(&v(r#"{"type":["string","null"]}"#))
    );
    assert_eq!(
        props.pointer("/roles/items"),
        Some(&v(r#"{"type":"string","enum":["admin","user","member"]}"#))
    );

    let good = r#"{"accountId":7,"displayName":null,"created":"2026-11-15T09:30:00Z","by":"ops"}"#;
    agrees::<Account>(good, true);
    agrees::<Account>(
        r#"{"accountId":7,"roles":["admin","member"],"created":"2026-11-15T09:30:00+01:00","by":"x","note":"n"}"#,
        true,
    );
    for bad in [
        r#"{"displayName":"a","created":"2026-11-15T09:30:00Z","by":"ops"}"#,
        r#"{"accountId":-1,"created":"2026-11-15T09:30:00Z","by":"ops"}"#,
        r#"{"accountId":7,"created":"yesterday","by":"ops"}"#,
        r#"{"accountId":7,"created":"2026-11-15T09:30:00Z"}"#,
        r#"{"accountId":7,"roles":["root"],"created":"2026-11-15T09:30:00Z","by":"ops"}"#,
    ] {
        agrees::<Account>(bad, false);
    }

    // What `Encode` writes validates.
    let account = Account {
        account_id: 1,
        display_name: Some("Ada".into()),
        roles: vec![Role::Admin],
        created_at: DateTime::parse("2026-11-15T09:30:00Z").unwrap(),
        cache: 0,
        audit: Audit {
            by: "ops".into(),
            note: None,
        },
    };
    assert!(Schema::of::<Account>()
        .unwrap()
        .validate(&account.encode())
        .is_ok());
}

#[derive(Encode, Decode, JsonSchema)]
#[ferroly(deny_unknown_fields)]
struct Strict {
    a: u8,
}

#[derive(Encode, Decode, JsonSchema)]
struct Meters(f64);

#[derive(Encode, Decode, JsonSchema)]
struct Pair(i32, String);

#[derive(Encode, Decode, JsonSchema)]
struct Marker;

#[derive(Encode, Decode, JsonSchema)]
struct Wrapper<T> {
    inner: T,
    map: BTreeMap<String, T>,
}

#[test]
fn shapes_and_generics() {
    assert_eq!(
        json::to_string(&Strict::json_schema()),
        r#"{"type":"object","properties":{"a":{"type":"integer","minimum":0,"maximum":255}},"required":["a"],"additionalProperties":false}"#
    );
    assert_eq!(Meters::json_schema(), v(r#"{"type":"number"}"#));
    assert_eq!(
        Pair::json_schema(),
        v(
            r#"{"type":"array","prefixItems":[{"type":"integer","minimum":-2147483648,"maximum":2147483647},{"type":"string"}],"minItems":2,"maxItems":2}"#
        )
    );
    assert_eq!(Marker::json_schema(), v(r#"{"type":"null"}"#));
    agrees::<Pair>(r#"[1,"x"]"#, true);
    agrees::<Pair>(r#"[1]"#, false);
    agrees::<Wrapper<bool>>(r#"{"inner":true,"map":{"k":false}}"#, true);
    agrees::<Wrapper<bool>>(r#"{"inner":1,"map":{}}"#, false);
    assert_eq!(
        Wrapper::<bool>::json_schema().pointer("/properties/map/additionalProperties"),
        Some(&v(r#"{"type":"boolean"}"#))
    );
}

#[derive(Encode, Decode, JsonSchema)]
enum Shape {
    Empty,
    Circle(f64),
    Rect { w: u32, h: u32 },
}

#[derive(Encode, Decode, JsonSchema)]
#[ferroly(tag = "kind")]
enum Event {
    Ping,
    Moved { x: i32, y: i32 },
}

#[derive(Encode, Decode, JsonSchema)]
#[ferroly(untagged)]
enum Id {
    Num(u64),
    Text(String),
}

#[test]
fn enum_representations() {
    let shape = Shape::json_schema();
    assert_eq!(shape.get("type"), Some(&v(r#"["string","object"]"#)));
    assert_eq!(
        shape.get("anyOf").and_then(Value::as_array).map(Vec::len),
        Some(3)
    );
    agrees::<Shape>(r#""Empty""#, true);
    agrees::<Shape>(r#"{"Circle":1.5}"#, true);
    agrees::<Shape>(r#"{"Rect":{"w":1,"h":2}}"#, true);
    // The validator checks the type union; anyOf is for full tooling.
    assert!(Schema::of::<Shape>().unwrap().validate(&v("5")).is_err());

    let event = Event::json_schema();
    assert_eq!(
        event.pointer("/properties/kind/enum"),
        Some(&v(r#"["Ping","Moved"]"#))
    );
    agrees::<Event>(r#"{"kind":"Ping"}"#, true);
    agrees::<Event>(r#"{"kind":"Moved","x":1,"y":2}"#, true);
    agrees::<Event>(r#"{"kind":"Jump"}"#, false);
    agrees::<Event>(r#"{"x":1}"#, false);

    assert_eq!(
        Id::json_schema().get("type"),
        Some(&v(r#"["integer","string"]"#))
    );
    agrees::<Id>("7", true);
    agrees::<Id>(r#""a7""#, true);
    agrees::<Id>("true", false);
}

#[derive(Encode, Decode, JsonSchema)]
struct Tree {
    kids: Vec<Tree>,
}

#[derive(Encode, Decode, JsonSchema)]
struct Node {
    value: i32,
    next: Option<Box<Node>>,
}

/// Mutual recursion, through an enum.
#[derive(Encode, Decode, JsonSchema)]
enum Expr {
    Lit(i64),
    Call(Call),
}

#[derive(Encode, Decode, JsonSchema)]
struct Call {
    name: String,
    args: Vec<Expr>,
}

#[test]
fn recursive_types_have_finite_schemas() {
    // CORE-1: these used to recurse without bound (a stack overflow).
    assert_eq!(
        json::to_string(&Tree::json_schema()),
        r#"{"type":"object","properties":{"kids":{"type":"array","items":{}}},"required":["kids"]}"#
    );
    agrees::<Tree>(r#"{"kids":[{"kids":[]}]}"#, true);
    agrees::<Tree>(r#"{"kids":5}"#, false);
    let tree = Tree {
        kids: vec![Tree {
            kids: vec![Tree { kids: vec![] }],
        }],
    };
    let schema = Schema::of::<Tree>().unwrap();
    assert!(schema.validate(&tree.encode()).is_ok());

    // The back-edge is `{}`: `null` is fine there, and the outer level is
    // still checked.
    assert_eq!(
        Node::json_schema().pointer("/properties/next"),
        Some(&v("{}"))
    );
    agrees::<Node>(r#"{"value":1,"next":{"value":2,"next":null}}"#, true);
    agrees::<Node>(r#"{"value":"x","next":null}"#, false);

    let expr = Expr::json_schema();
    assert_eq!(
        expr.pointer("/anyOf/1/properties/Call/properties/args/items"),
        Some(&v("{}"))
    );
    agrees::<Expr>(r#"{"Call":{"name":"f","args":[{"Lit":1}]}}"#, true);
    agrees::<Expr>("5", false);
    // Starting from the other type of the cycle works the same way.
    assert_eq!(
        Call::json_schema().pointer("/properties/args/items/anyOf/1/properties/Call"),
        Some(&v("{}"))
    );

    // Different instantiations of a generic type are not a cycle.
    assert_eq!(
        Wrapper::<Wrapper<bool>>::json_schema().pointer("/properties/inner/properties/inner"),
        Some(&v(r#"{"type":"boolean"}"#))
    );
    // Nor is the same type used twice side by side.
    let pair = <(Tree, Tree)>::json_schema();
    assert_eq!(
        pair.pointer("/prefixItems/0"),
        pair.pointer("/prefixItems/1")
    );
    assert_eq!(pair.pointer("/prefixItems/1"), Some(&Tree::json_schema()));
}

#[test]
fn system_time_schema_accepts_its_far_encoding() {
    // CORE-9: outside years 0000–9999 `SystemTime` encodes as
    // `{ "secs", "nanos" }`, which the schema used to reject.
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    let schema = Schema::of::<SystemTime>().unwrap();
    // Before Windows' 1601 start the far past is not representable there.
    let far_past = UNIX_EPOCH.checked_sub(Duration::from_secs(100_000_000_000));
    for t in [
        Some(UNIX_EPOCH + Duration::from_secs(400_000_000_000)),
        far_past,
        Some(UNIX_EPOCH + Duration::new(1_794_731_400, 5)),
    ]
    .into_iter()
    .flatten()
    {
        let doc = json::encode(&t);
        assert!(schema.validate(&v(&doc)).is_ok(), "{doc}");
        assert_eq!(json::decode::<SystemTime>(&doc).unwrap(), t);
    }
    agrees::<SystemTime>("1794731400", true);
    agrees::<SystemTime>(r#"{"secs":1,"nanos":1000000000}"#, false);
    agrees::<SystemTime>(r#"{"nanos":1}"#, false);
    agrees::<SystemTime>(r#"{"secs":"1"}"#, false);
}

/// Whether some `anyOf` alternative of `schema` is exactly `{"type":"null"}`.
fn any_of_allows_null(schema: &Value) -> bool {
    schema
        .get("anyOf")
        .and_then(Value::as_array)
        .is_some_and(|alts| alts.contains(&v(r#"{"type":"null"}"#)))
}

#[test]
fn optional_enums_accept_null_in_every_keyword() {
    // CORE-10: `nullable` only touched `type`, so a full JSON-Schema
    // validator still rejected `null` through the `anyOf`.
    for (name, schema) in [
        ("external", Option::<Shape>::json_schema()),
        ("internal", Option::<Event>::json_schema()),
    ] {
        assert!(
            any_of_allows_null(&schema),
            "{name}: {}",
            json::to_string(&schema)
        );
        let types = schema.get("type").and_then(Value::as_array).unwrap();
        assert!(types.contains(&v(r#""null""#)), "{name}");
    }
    agrees::<Option<Shape>>("null", true);
    agrees::<Option<Event>>("null", true);
    agrees::<Option<Event>>(r#"{"kind":"Ping"}"#, true);
    // An untagged enum whose alternatives already allow `null` gets no
    // second null alternative; nor does a nested `Option`.
    #[derive(Encode, Decode, JsonSchema)]
    #[ferroly(untagged)]
    enum MaybeId {
        Num(u64),
        Nothing,
    }
    let schema = Option::<MaybeId>::json_schema();
    let alts = schema.get("anyOf").and_then(Value::as_array).unwrap();
    assert_eq!(alts.len(), 2, "{}", json::to_string(&schema));
    assert_eq!(
        Option::<Option<Shape>>::json_schema(),
        Option::<Shape>::json_schema()
    );
}
