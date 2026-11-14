#![cfg(feature = "codec")]
//! The standard-library `Encode`/`Decode` impls, the strict/lenient scalar
//! split, float → integer conversion, and the `Value` navigation helpers.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Debug;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use ferroly::codec::{
    from_value_strict, json, toml, xml, yaml, CodecError, Decode, Encode, Value, ValueDecoder,
};

/// Round-trips `v` through every path a type can take: streaming JSON, the
/// lenient `Value` path, the strict `Value` path, YAML and XML.
fn rt<T>(v: T)
where
    T: Encode + Decode + PartialEq + Debug,
{
    let s = json::encode(&v);
    assert_eq!(json::decode::<T>(&s).unwrap(), v, "json via {s}");
    let tree = v.encode();
    assert_eq!(json::to_string(&tree), s, "encode() and encode_to() agree");
    assert_eq!(T::decode(&tree).unwrap(), v, "lenient Value path");
    assert_eq!(
        from_value_strict::<T>(&tree).unwrap(),
        v,
        "strict Value path"
    );
    let y = yaml::encode(&v);
    assert_eq!(yaml::decode::<T>(&y).unwrap(), v, "yaml via {y:?}");
    let x = xml::encode(&v);
    assert_eq!(xml::decode::<T>(&x).unwrap(), v, "xml via {x}");
}

// ---- new std impls -------------------------------------------------------

#[test]
fn tuples_encode_as_fixed_length_arrays() {
    rt((1u8,));
    rt((1i32, "two".to_string()));
    rt((true, 2.5f64, 'c'));
    rt((
        1u8, 2u16, 3u32, 4u64, 5i8, 6i16, 7i32, 8i64, 9usize, 10isize, 11u8, 12u8,
    ));
    rt(((1, 2), (vec![3], Some(4))));
    assert_eq!(json::encode(&(1, "a", true)), r#"[1,"a",true]"#);

    // Wrong arity is an error on every path.
    assert!(json::decode::<(i32, i32)>("[1]").is_err());
    assert!(json::decode::<(i32, i32)>("[1,2,3]").is_err());
    assert!(json::decode::<(i32, i32)>("{}").is_err());
    assert!(<(i32, i32)>::decode(&json::from_str("[1]").unwrap()).is_err());
    assert!(<(i32, i32)>::decode(&json::from_str("[1,2,3]").unwrap()).is_err());
    assert!(<(i32, i32)>::decode(&Value::Int(1)).is_err());
    // Element errors propagate.
    assert!(json::decode::<(i32, bool)>("[1,2]").is_err());
}

#[test]
fn fixed_size_arrays() {
    rt([1u8, 2, 3]);
    rt([[1i32, 2], [3, 4]]);
    rt(["a".to_string(), "b".to_string()]);
    rt::<[i32; 0]>([]);
    assert_eq!(json::encode(&[1, 2, 3]), "[1,2,3]");
    assert!(json::decode::<[u8; 3]>("[1,2]").is_err());
    assert!(json::decode::<[u8; 3]>("[1,2,3,4]").is_err());
    assert!(<[u8; 2]>::decode(&json::from_str("[1,2,3]").unwrap()).is_err());
}

#[test]
fn slices_and_references_encode() {
    let v = vec![1, 2, 3];
    assert_eq!(json::encode(v.as_slice()), "[1,2,3]");
    assert_eq!(json::encode(&v.as_slice()), "[1,2,3]");
    assert_eq!(json::encode(&&v), "[1,2,3]");
    assert_eq!(json::encode("text"), "\"text\"");
    assert_eq!(json::encode(&"text"), "\"text\"");
    assert_eq!(v[..].encode(), v.encode());
}

#[test]
fn ordered_and_hashed_collections() {
    let map: BTreeMap<String, i32> = [("b".to_string(), 2), ("a".to_string(), 1)]
        .into_iter()
        .collect();
    assert_eq!(json::encode(&map), r#"{"a":1,"b":2}"#);
    rt(map);
    rt(BTreeMap::<String, Vec<String>>::new());

    let set: BTreeSet<i32> = [3, 1, 2, 3].into_iter().collect();
    assert_eq!(json::encode(&set), "[1,2,3]");
    rt(set);
    rt(BTreeSet::<String>::new());
    assert_eq!(
        json::decode::<BTreeSet<i32>>("[2,1,2,1]").unwrap().len(),
        2,
        "duplicates collapse"
    );

    let hashed: HashSet<String> = ["x".to_string(), "y".to_string()].into_iter().collect();
    rt(hashed);
    let nested: HashMap<String, BTreeSet<u8>> = [("k".to_string(), [1u8, 2].into_iter().collect())]
        .into_iter()
        .collect();
    rt(nested);
    assert!(json::decode::<BTreeSet<i32>>("{}").is_err());
    assert!(json::decode::<BTreeMap<String, i32>>("[]").is_err());
}

#[test]
fn shared_pointers_and_cow() {
    rt(Arc::new(5i32));
    rt(Rc::new("text".to_string()));
    rt(Box::new(vec![1, 2]));
    rt(Rc::new(vec![Some(Arc::new(1u8)), None]));

    let a: Arc<str> = Arc::from("shared");
    assert_eq!(json::encode(&a), "\"shared\"");
    assert_eq!(&*json::decode::<Arc<str>>("\"shared\"").unwrap(), "shared");
    assert_eq!(&*json::decode::<Rc<str>>("\"x\"").unwrap(), "x");
    assert_eq!(&*json::decode::<Box<str>>("\"y\"").unwrap(), "y");
    assert_eq!(&*<Arc<str>>::decode(&Value::Str("z".into())).unwrap(), "z");
    assert!(json::decode::<Arc<str>>("5").is_err());
    assert!(<Rc<str>>::decode(&Value::Int(5)).is_err());

    let borrowed: Cow<'_, str> = Cow::Borrowed("cow");
    assert_eq!(json::encode(&borrowed), "\"cow\"");
    let owned: Cow<'static, str> = json::decode("\"cow\"").unwrap();
    assert!(matches!(owned, Cow::Owned(_)));
    assert_eq!(owned, "cow");
    rt::<Cow<'static, str>>(Cow::Owned("x: y".to_string()));
    let list: Cow<'static, [i32]> = json::decode("[1,2]").unwrap();
    assert_eq!(&*list, &[1, 2]);
    assert_eq!(json::encode(&Cow::Borrowed(&[1, 2][..])), "[1,2]");
}

#[test]
fn wide_integers() {
    // Within 64 bits they are plain numbers.
    assert_eq!(json::encode(&5i128), "5");
    assert_eq!(json::encode(&-5i128), "-5");
    assert_eq!(json::encode(&u128::from(u64::MAX)), "18446744073709551615");
    // Beyond 64 bits they travel as decimal strings.
    assert_eq!(
        json::encode(&u128::MAX),
        "\"340282366920938463463374607431768211455\""
    );
    assert_eq!(
        json::encode(&i128::MIN),
        "\"-170141183460469231731687303715884105728\""
    );
    for v in [
        0i128,
        -1,
        i128::from(i64::MIN),
        i128::from(u64::MAX),
        i128::MAX,
        i128::MIN,
    ] {
        rt(v);
    }
    for v in [
        0u128,
        1,
        u128::from(u64::MAX),
        u128::from(u64::MAX) + 1,
        u128::MAX,
    ] {
        rt(v);
    }
    assert!(json::decode::<u128>("-1").is_err());
    assert!(json::decode::<u128>("\"-1\"").is_err());
    assert!(json::decode::<i128>("\"12x\"").is_err());
    assert!(json::decode::<i128>("1.5").is_err());
    assert!(json::decode::<i128>("true").is_err());
    // Strict mode takes the string form exactly; lenient mode trims.
    assert!(json::decode::<i128>("\" 5 \"").is_err());
    assert_eq!(i128::decode(&Value::Str(" 5 ".into())).unwrap(), 5);
    assert_eq!(
        json::decode::<i128>("1e20").unwrap(),
        100_000_000_000_000_000_000
    );
}

#[test]
fn durations() {
    let d = Duration::new(90, 500_000_000);
    assert_eq!(json::encode(&d), r#"{"secs":90,"nanos":500000000}"#);
    rt(d);
    rt(Duration::ZERO);
    rt(Duration::MAX);
    // A bare number of seconds is accepted too.
    assert_eq!(
        json::decode::<Duration>("30").unwrap(),
        Duration::from_secs(30)
    );
    assert_eq!(
        json::decode::<Duration>("1.5").unwrap(),
        Duration::from_millis(1500)
    );
    assert_eq!(
        json::decode::<Duration>(r#"{"secs":2}"#).unwrap(),
        Duration::from_secs(2)
    );
    for bad in [
        "-1",
        "-0.5",
        "1e400",
        "\"30\"",
        "true",
        "[1,2]",
        r#"{"nanos":5}"#,
        r#"{"secs":1,"nanos":1000000000}"#,
        r#"{"secs":-1,"nanos":0}"#,
        r#"{"secs":"1","nanos":0}"#,
    ] {
        assert!(
            json::decode::<Duration>(bad).is_err(),
            "{bad} must be rejected"
        );
    }
    // Text-only sources go through the lenient path.
    assert_eq!(
        Duration::decode(&Value::Str(" 30 ".into())).unwrap(),
        Duration::from_secs(30)
    );
    assert_eq!(
        Duration::decode(&Value::Str("0.25".into())).unwrap(),
        Duration::from_millis(250)
    );
    assert!(Duration::decode(&Value::Str("soon".into())).is_err());
    #[derive(Encode, Decode, PartialEq, Debug)]
    struct Timeouts {
        connect: Duration,
        idle: Option<Duration>,
    }
    let t = Timeouts {
        connect: Duration::from_millis(250),
        idle: None,
    };
    assert_eq!(toml::decode::<Timeouts>(&toml::encode(&t)).unwrap(), t);
    assert_eq!(
        toml::decode::<Timeouts>("connect = 5\nidle = 0.5\n").unwrap(),
        Timeouts {
            connect: Duration::from_secs(5),
            idle: Some(Duration::from_millis(500)),
        }
    );
}

#[test]
fn paths_and_network_addresses() {
    rt(PathBuf::from("/var/lib/app data/x.toml"));
    rt(PathBuf::new());
    assert_eq!(json::encode(Path::new("a/b")), "\"a/b\"");
    assert!(json::decode::<PathBuf>("5").is_err());

    rt(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
    rt(IpAddr::V6(Ipv6Addr::LOCALHOST));
    rt(Ipv4Addr::new(10, 0, 0, 1));
    rt(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
    rt(SocketAddr::from(([127, 0, 0, 1], 8080)));
    rt(SocketAddr::from((Ipv6Addr::LOCALHOST, 443)));
    rt(SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 5));
    rt(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 6, 0, 0));
    assert_eq!(
        json::encode(&SocketAddr::from((Ipv6Addr::LOCALHOST, 443))),
        "\"[::1]:443\""
    );
    for bad in [
        "\"\"",
        "\"localhost\"",
        "\"1.2.3\"",
        "\"1.2.3.4.5\"",
        "5",
        "null",
    ] {
        assert!(json::decode::<IpAddr>(bad).is_err(), "{bad}");
    }
    assert!(json::decode::<SocketAddr>("\"127.0.0.1\"").is_err());
    assert!(json::decode::<Ipv4Addr>("\"::1\"").is_err());

    #[derive(Encode, Decode, PartialEq, Debug)]
    struct Listen {
        bind: SocketAddr,
        allow: Vec<IpAddr>,
        root: PathBuf,
    }
    let listen = Listen {
        bind: "0.0.0.0:80".parse().unwrap(),
        allow: vec!["10.0.0.1".parse().unwrap(), "::1".parse().unwrap()],
        root: "/srv".into(),
    };
    assert_eq!(
        toml::decode::<Listen>(&toml::encode(&listen)).unwrap(),
        listen
    );
    rt(listen);
}

// ---- C4: float -> integer ------------------------------------------------

#[test]
fn float_to_integer_requires_a_whole_in_range_value() {
    // The two reproductions from the review.
    assert!(json::decode::<u64>("1e300").is_err());
    assert!(json::decode::<i64>("1.9").is_err());
    assert!(u64::decode(&Value::Float(1e300)).is_err());
    assert!(i64::decode(&Value::Float(1.9)).is_err());

    // Whole floats convert; fractional ones never do.
    assert_eq!(json::decode::<i64>("3.0").unwrap(), 3);
    assert_eq!(json::decode::<i64>("-3.0").unwrap(), -3);
    assert_eq!(json::decode::<u8>("255.0").unwrap(), 255);
    assert_eq!(json::decode::<i64>("-0.0").unwrap(), 0);
    assert_eq!(
        json::decode::<i64>("1e18").unwrap(),
        1_000_000_000_000_000_000
    );
    for bad in ["0.5", "-0.5", "1.0000001", "1e-5", "2.5e0"] {
        assert!(json::decode::<i64>(bad).is_err(), "{bad}");
        assert!(json::decode::<u64>(bad).is_err(), "{bad}");
        assert!(json::decode::<i8>(bad).is_err(), "{bad}");
    }
    // The error says which: not an integer vs. does not fit.
    assert!(matches!(
        json::decode::<i64>("1.9"),
        Err(CodecError::Expected(_))
    ));
    assert!(matches!(
        json::decode::<i64>("1e300"),
        Err(CodecError::OutOfRange(_))
    ));

    // Range edges for the 64-bit targets (2^63 and 2^64 are exact in f64).
    assert!(i64::decode(&Value::Float(9_223_372_036_854_775_808.0)).is_err());
    assert_eq!(
        i64::decode(&Value::Float(-9_223_372_036_854_775_808.0)).unwrap(),
        i64::MIN
    );
    assert!(i64::decode(&Value::Float(-9_223_372_036_854_777_856.0)).is_err());
    assert!(u64::decode(&Value::Float(18_446_744_073_709_551_616.0)).is_err());
    assert_eq!(
        u64::decode(&Value::Float(9_223_372_036_854_775_808.0)).unwrap(),
        1u64 << 63
    );
    assert!(u64::decode(&Value::Float(-1.0)).is_err());
    assert_eq!(u64::decode(&Value::Float(-0.0)).unwrap(), 0);

    // Narrow targets are range-checked after the conversion.
    assert!(u8::decode(&Value::Float(256.0)).is_err());
    assert!(i8::decode(&Value::Float(-129.0)).is_err());
    assert_eq!(i8::decode(&Value::Float(-128.0)).unwrap(), -128);

    // Non-finite values never convert.
    for f in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(i64::decode(&Value::Float(f)).is_err());
        assert!(u64::decode(&Value::Float(f)).is_err());
        assert!(i128::decode(&Value::Float(f)).is_err());
        assert!(u128::decode(&Value::Float(f)).is_err());
    }

    // The same rule holds through every format.
    #[derive(Decode, Debug)]
    struct P {
        #[allow(dead_code)]
        port: u16,
    }
    assert!(json::decode::<P>(r#"{"port":80.5}"#).is_err());
    assert!(yaml::decode::<P>("port: 80.5").is_err());
    assert!(toml::decode::<P>("port = 80.5").is_err());
    assert!(xml::decode::<P>("<r><port>80.5</port></r>").is_err());
    assert!(yaml::decode::<P>("port: 1e300").is_err());
    assert!(yaml::decode::<P>("port: 80.0").is_ok());
}

// ---- C6: strict vs lenient ------------------------------------------------

#[derive(Encode, Decode, PartialEq, Debug)]
struct Settings {
    port: u16,
    ratio: f64,
    debug: bool,
    offset: i32,
    tags: Vec<String>,
}

const TYPED: Settings = Settings {
    port: 8080,
    ratio: 0.5,
    debug: true,
    offset: -3,
    tags: Vec::new(),
};

#[test]
fn json_and_toml_never_coerce_strings() {
    // Correctly typed input decodes.
    let json_ok = r#"{"port":8080,"ratio":0.5,"debug":true,"offset":-3,"tags":[]}"#;
    assert_eq!(json::decode::<Settings>(json_ok).unwrap(), TYPED);
    let toml_ok = "port = 8080\nratio = 0.5\ndebug = true\noffset = -3\ntags = []\n";
    assert_eq!(toml::decode::<Settings>(toml_ok).unwrap(), TYPED);

    // Each field given as a string is rejected, in both formats.
    for (field, text) in [
        ("port", "\"8080\""),
        ("port", "\" 8080 \""),
        ("ratio", "\"0.5\""),
        ("debug", "\"true\""),
        ("offset", "\"-3\""),
    ] {
        let j = json_ok.replacen(
            &format!("\"{field}\":{}", json_field(json_ok, field)),
            &format!("\"{field}\":{text}"),
            1,
        );
        assert_ne!(j, json_ok);
        assert!(json::decode::<Settings>(&j).is_err(), "json {j}");
        // The registry entry point is the same decoder.
        assert!(ferroly::codec::decode::<Settings>("application/json", j.as_bytes()).is_err());

        let t: String = toml_ok
            .lines()
            .map(|l| {
                if l.starts_with(&format!("{field} =")) {
                    format!("{field} = {text}\n")
                } else {
                    format!("{l}\n")
                }
            })
            .collect();
        assert_ne!(t, toml_ok);
        assert!(toml::decode::<Settings>(&t).is_err(), "toml {t}");
        assert!(ferroly::codec::decode::<Settings>("application/toml", t.as_bytes()).is_err());
    }
    // A scalar is not promoted to a one-element array, and null is not an
    // empty array.
    assert!(json::decode::<Vec<String>>("\"a\"").is_err());
    assert!(json::decode::<Vec<String>>("null").is_err());
    // The scalars on their own.
    assert!(json::decode::<i64>("\"1\"").is_err());
    assert!(json::decode::<u64>("\"1\"").is_err());
    assert!(json::decode::<f32>("\"1\"").is_err());
    assert!(json::decode::<bool>("\"false\"").is_err());
    assert!(json::decode::<Option<i32>>("\"1\"").is_err());
    assert!(json::decode::<HashMap<String, i32>>(r#"{"a":"1"}"#).is_err());
    assert!(json::decode::<(i32, bool)>(r#"["1","true"]"#).is_err());
}

/// The raw text of `field`'s value in a flat JSON object literal.
fn json_field<'a>(doc: &'a str, field: &str) -> &'a str {
    let start = doc.find(&format!("\"{field}\":")).unwrap() + field.len() + 3;
    let rest = &doc[start..];
    &rest[..rest.find([',', '}']).unwrap()]
}

#[test]
fn text_only_sources_stay_lenient() {
    // XML: every scalar arrives as text.
    let xml_doc = "<root><port> 8080 </port><ratio>0.5</ratio><debug>true</debug>\
                   <offset>-3</offset><tags/></root>";
    assert_eq!(xml::decode::<Settings>(xml_doc).unwrap(), TYPED);
    assert_eq!(
        ferroly::codec::decode::<Settings>("application/xml", xml_doc.as_bytes()).unwrap(),
        TYPED
    );

    // Environment variables / `.properties`: a tree of `Value::Str`, decoded
    // through `Decode::decode` — the path `config` uses.
    let from_env = Value::Object(vec![
        ("port".into(), Value::Str("8080".into())),
        ("ratio".into(), Value::Str(" 0.5".into())),
        ("debug".into(), Value::Str("true".into())),
        ("offset".into(), Value::Str("-3".into())),
        ("tags".into(), Value::Null),
    ]);
    assert_eq!(Settings::decode(&from_env).unwrap(), TYPED);
    // The strict walk over the very same tree refuses it.
    assert!(from_value_strict::<Settings>(&from_env).is_err());
    assert!(Settings::decode_from(&mut ValueDecoder::new(&from_env)).is_err());

    // YAML is loosely typed, so quoted scalars are accepted there too.
    let yaml_doc = "port: \"8080\"\nratio: 0.5\ndebug: 'true'\noffset: -3\ntags: []\n";
    assert_eq!(yaml::decode::<Settings>(yaml_doc).unwrap(), TYPED);

    // Leniency is about strings only: wrong kinds are still errors.
    assert!(u16::decode(&Value::Bool(true)).is_err());
    assert!(bool::decode(&Value::Int(1)).is_err());
    assert!(u16::decode(&Value::Str("eighty".into())).is_err());
    assert!(bool::decode(&Value::Str("yes".into())).is_err());
    assert!(u16::decode(&Value::Str("70000".into())).is_err());
}

#[test]
fn strict_value_decoder_matches_the_streaming_decoder() {
    // Anything JSON accepts for a type, the strict Value walk accepts, and
    // vice versa — the two are the same rules over different sources.
    #[derive(Encode, Decode, PartialEq, Debug)]
    struct Mixed {
        id: u64,
        name: String,
        score: Option<f32>,
        tags: Vec<String>,
        attrs: BTreeMap<String, i64>,
        pair: (i32, bool),
        raw: Value,
    }
    let cases = [
        r#"{"id":1,"name":"a","score":null,"tags":[],"attrs":{},"pair":[1,true],"raw":{"k":[1,"2"]}}"#,
        r#"{"id":1,"name":"a","score":2,"tags":["x"],"attrs":{"a":-1},"pair":[0,false],"raw":null,"extra":[1,{"deep":true}]}"#,
        r#"{"id":"1","name":"a","tags":[],"attrs":{},"pair":[1,true],"raw":1}"#,
        r#"{"id":1,"name":2,"tags":[],"attrs":{},"pair":[1,true],"raw":1}"#,
        r#"{"id":1,"name":"a","tags":"x","attrs":{},"pair":[1,true],"raw":1}"#,
        r#"{"id":1,"name":"a","tags":[],"attrs":[],"pair":[1,true],"raw":1}"#,
        r#"{"id":1,"name":"a","tags":[],"attrs":{"a":"1"},"pair":[1,true],"raw":1}"#,
        r#"{"id":1,"name":"a","tags":[],"attrs":{},"pair":[1],"raw":1}"#,
        r#"{"id":1,"name":"a","tags":[],"attrs":{},"pair":[1,"true"],"raw":1}"#,
        r#"{"id":1,"tags":[],"attrs":{},"pair":[1,true],"raw":1}"#,
        r#"[1,2]"#,
        r#"null"#,
    ];
    for src in cases {
        let streaming = json::decode::<Mixed>(src);
        let tree = json::from_str(src).unwrap();
        let strict = from_value_strict::<Mixed>(&tree);
        assert_eq!(
            streaming.is_ok(),
            strict.is_ok(),
            "{src}: {streaming:?} vs {strict:?}"
        );
        if let (Ok(a), Ok(b)) = (streaming, strict) {
            assert_eq!(a, b, "{src}");
        }
    }
    // A decoder can be driven by hand as well.
    let tree = json::from_str(r#"[1,2,3]"#).unwrap();
    assert_eq!(
        Vec::<u8>::decode_from(&mut ValueDecoder::new(&tree)).unwrap(),
        vec![1, 2, 3]
    );
}

// ---- Value helpers ---------------------------------------------------------

fn obj(entries: Vec<(&str, Value)>) -> Value {
    Value::Object(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

#[test]
fn pointer_walks_objects_and_arrays() {
    let v = obj(vec![
        (
            "a",
            obj(vec![("b", Value::Array(vec![1.into(), 2.into()]))]),
        ),
        ("", 7.into()),
        ("m~n", 8.into()),
        ("x/y", 9.into()),
        ("0", 10.into()),
    ]);
    assert_eq!(v.pointer(""), Some(&v));
    assert_eq!(v.pointer("/a/b/0"), Some(&Value::Int(1)));
    assert_eq!(v.pointer("/a/b/1"), Some(&Value::Int(2)));
    assert_eq!(v.pointer("/"), Some(&Value::Int(7)));
    assert_eq!(v.pointer("/m~0n"), Some(&Value::Int(8)));
    assert_eq!(v.pointer("/x~1y"), Some(&Value::Int(9)));
    // A numeric token is a key when the node is an object.
    assert_eq!(v.pointer("/0"), Some(&Value::Int(10)));
    // Out of range, malformed indexes, a missing leading slash, scalars.
    for missing in [
        "/a/b/2", "/a/b/01", "/a/b/-", "/a/b/+1", "/a/b/ 1", "/a/b/1.0", "a", "/a/b/0/x", "/nope",
        "/a/b/",
    ] {
        assert_eq!(v.pointer(missing), None, "{missing:?}");
    }
    assert_eq!(Value::Int(1).pointer(""), Some(&Value::Int(1)));
    assert_eq!(Value::Int(1).pointer("/"), None);
}

#[test]
fn pointer_mut_and_get_mut_edit_in_place() {
    let mut v = obj(vec![("a", obj(vec![("b", Value::Array(vec![1.into()]))]))]);
    *v.pointer_mut("/a/b/0").unwrap() = Value::Str("x".into());
    assert_eq!(v.pointer("/a/b/0").and_then(Value::as_str), Some("x"));
    v.get_mut("a")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .push(("c".into(), true.into()));
    assert_eq!(v.pointer("/a/c"), Some(&Value::Bool(true)));
    assert!(v.get_mut("zzz").is_none());
    assert!(v.pointer_mut("/a/zzz").is_none());
    assert!(v.pointer_mut("no-slash").is_none());
    v.pointer_mut("/a/b")
        .unwrap()
        .as_array_mut()
        .unwrap()
        .push(2.into());
    assert_eq!(
        v.pointer("/a/b").and_then(Value::as_array).map(Vec::len),
        Some(2)
    );
    *v.get_mut("a")
        .unwrap()
        .get_mut("b")
        .unwrap()
        .get_index_mut(1)
        .unwrap() = 5.into();
    assert_eq!(v.pointer("/a/b/1"), Some(&Value::Int(5)));
    assert!(v.get_index_mut(0).is_none());
    assert!(Value::Int(1).as_object_mut().is_none());
    assert!(Value::Int(1).as_array_mut().is_none());

    let taken = v.take();
    assert!(v.is_null());
    assert_eq!(json::to_string(&taken), r#"{"a":{"b":["x",5],"c":true}}"#);
    // The whole-document pointer is writable too.
    *v.pointer_mut("").unwrap() = taken;
    assert!(v.pointer("/a/c").is_some());
}

#[test]
fn get_mut_resolves_duplicates_last_wins() {
    let mut v = obj(vec![("a", 1.into()), ("a", 2.into())]);
    *v.get_mut("a").unwrap() = 3.into();
    assert_eq!(v.get("a"), Some(&Value::Int(3)));
    assert_eq!(v.as_object().unwrap()[0].1, Value::Int(1));
    *v.pointer_mut("/a").unwrap() = 4.into();
    assert_eq!(v.pointer("/a"), Some(&Value::Int(4)));
}

#[test]
fn merge_is_deep_for_objects_and_replaces_everything_else() {
    let mut base = obj(vec![
        ("keep", 1.into()),
        ("db", obj(vec![("host", "a".into()), ("port", 1.into())])),
        ("list", Value::Array(vec![1.into(), 2.into()])),
        ("kind", obj(vec![("x", 1.into())])),
        ("gone", 5.into()),
    ]);
    base.merge(obj(vec![
        ("db", obj(vec![("port", 2.into()), ("tls", true.into())])),
        ("list", Value::Array(vec![9.into()])),
        ("kind", "scalar".into()),
        ("gone", Value::Null),
        ("added", obj(vec![("n", 1.into())])),
    ]));
    assert_eq!(
        json::to_string(&base),
        r#"{"keep":1,"db":{"host":"a","port":2,"tls":true},"list":[9],"kind":"scalar","gone":null,"added":{"n":1}}"#
    );
    // A non-object on either side replaces.
    let mut s = Value::Int(1);
    s.merge(obj(vec![("a", 1.into())]));
    assert_eq!(s, obj(vec![("a", 1.into())]));
    s.merge(Value::Array(vec![]));
    assert_eq!(s, Value::Array(vec![]));
    // Merging an empty object changes nothing.
    let mut same = obj(vec![("a", 1.into())]);
    same.merge(obj(vec![]));
    assert_eq!(same, obj(vec![("a", 1.into())]));
}

#[test]
fn merge_of_wide_objects_agrees_with_the_narrow_path() {
    // More than 16 keys switches to the indexed path; the result must be the
    // same as the linear one, including for repeated overlay keys.
    let wide = |offset: i64| {
        Value::Object(
            (0..200)
                .map(|i| (format!("k{i}"), Value::Int(i + offset)))
                .collect(),
        )
    };
    let mut base = wide(0);
    let mut overlay = wide(1000);
    let extra = overlay.as_object_mut().unwrap();
    extra.push(("extra".into(), Value::Bool(true)));
    extra.push(("extra".into(), Value::Bool(false)));
    base.merge(overlay);
    assert_eq!(base.as_object().unwrap().len(), 201);
    assert_eq!(base.get("k7"), Some(&Value::Int(1007)));
    assert_eq!(base.get("k199"), Some(&Value::Int(1199)));
    assert_eq!(base.get("extra"), Some(&Value::Bool(false)));
}
