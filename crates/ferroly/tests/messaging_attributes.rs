#![cfg(feature = "messaging")]
//! Tests for the message model: typed attributes, system properties, the wire
//! mapping, and content-type-aware encoding.

use std::time::{Duration, UNIX_EPOCH};

use ferroly::messaging::{
    handler, headers, Ack, AttrValue, Attributes, LocalProvider, Message, Producer, Receiver,
};
use tokio::sync::mpsc::unbounded_channel;

#[test]
fn attributes_are_typed_and_ordered() {
    let msg = Message::text("x")
        .with_attribute("s", "text")
        .with_attribute("i", 42)
        .with_attribute("f", 2.5)
        .with_attribute("b", true)
        .with_attribute("y", vec![1u8, 2, 3]);
    assert_eq!(msg.attribute_str("s"), Some("text"));
    assert_eq!(msg.attributes.get_i64("i"), Some(42));
    assert_eq!(msg.attributes.get_f64("f"), Some(2.5));
    assert_eq!(msg.attributes.get_bool("b"), Some(true));
    assert_eq!(msg.attributes.get_bytes("y"), Some(&[1u8, 2, 3][..]));
    assert_eq!(msg.attribute("i"), Some(&AttrValue::Int(42)));
    let names: Vec<&str> = msg.attributes.keys().collect();
    assert_eq!(names, ["s", "i", "f", "b", "y"]);
}

#[test]
fn properties_are_separate_from_attributes() {
    let msg = Message::text("x")
        .with_id("m1")
        .with_attribute("message-id", "app");
    assert_eq!(msg.id(), Some("m1"));
    assert_eq!(msg.attribute_str("message-id"), Some("app"));
}

#[test]
fn wire_attributes_put_properties_first_and_drop_shadowed_names() {
    let msg = Message::text("x")
        .with_id("m1")
        .with_attribute("message-id", "app")
        .with_attribute("tenant", "acme");
    let wire = msg.to_wire_attributes();
    let ids: Vec<&AttrValue> = wire.get_all(headers::MESSAGE_ID).collect();
    assert_eq!(ids, [&AttrValue::Str("m1".into())]);
    let names: Vec<&str> = wire.keys().collect();
    assert_eq!(names, [headers::MESSAGE_ID, "tenant"]);
}

#[test]
fn from_wire_restores_properties_and_keeps_the_rest() {
    let at = UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
    let m = Message::text("body")
        .with_id("m1")
        .with_correlation_id("c1")
        .with_timestamp(at)
        .with_content_type("text/plain")
        .with_priority(7)
        .with_attribute("tenant", "acme")
        .with_attribute("n", 5);
    let back = Message::from_wire(m.body.clone(), m.to_wire_attributes());
    assert_eq!(back.id(), Some("m1"));
    assert_eq!(back.correlation_id(), Some("c1"));
    assert_eq!(back.timestamp(), Some(at));
    assert_eq!(back.content_type(), Some("text/plain"));
    assert_eq!(back.priority(), 7);
    assert_eq!(back.attributes, m.attributes);
    assert_eq!(back.properties, m.properties);
    assert_eq!(back.body, m.body);
}

#[test]
fn from_wire_keeps_a_mistyped_property_as_an_attribute() {
    let wire: Attributes = [("timestamp", AttrValue::Str("soon".into()))]
        .into_iter()
        .collect();
    let msg = Message::from_wire(Vec::new(), wire);
    assert_eq!(msg.timestamp(), None);
    assert_eq!(msg.attribute_str("timestamp"), Some("soon"));

    // A priority out of range stays an attribute too.
    let wire: Attributes = [("priority", 300i64)].into_iter().collect();
    let msg = Message::from_wire(Vec::new(), wire);
    assert_eq!(msg.priority(), 0);
    assert_eq!(msg.attributes.get_i64("priority"), Some(300));
}

#[test]
fn lookup_prefers_the_property() {
    let msg = Message::text("x")
        .with_subject("invoice")
        .with_attribute("subject", "app")
        .with_attribute("region", "eu");
    assert_eq!(
        msg.lookup("subject"),
        Some(AttrValue::Str("invoice".into()))
    );
    assert_eq!(msg.lookup("region"), Some(AttrValue::Str("eu".into())));
    assert_eq!(msg.lookup("missing"), None);
    // An unset property falls through to the attribute.
    let msg = Message::text("x").with_attribute("subject", "app");
    assert_eq!(msg.lookup("subject"), Some(AttrValue::Str("app".into())));
}

#[test]
fn timestamp_is_stored_with_millisecond_precision() {
    let precise = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789);
    let msg = Message::text("x").with_timestamp(precise);
    assert_eq!(
        msg.timestamp(),
        Some(UNIX_EPOCH + Duration::from_millis(1_700_000_000_123))
    );
}

#[tokio::test]
async fn attributes_survive_delivery_through_the_local_provider() {
    let bus = LocalProvider::new("t");
    let (tx, mut rx) = unbounded_channel();
    bus.add_listener(
        "chan://a",
        handler(move |msg: Message| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(msg);
                Ack::Ok
            }
        }),
        Default::default(),
    )
    .await
    .unwrap();

    let sent = Message::text("x")
        .with_id("m-1")
        .with_content_type("text/plain")
        .with_attribute("tenant", "acme")
        .with_attribute("n", 3)
        .with_attribute("raw", vec![9u8]);
    bus.send("chan://a", sent.clone(), Default::default())
        .await
        .unwrap();
    let got = rx.recv().await.unwrap();
    assert_eq!(got.attributes, sent.attributes);
    assert_eq!(got.id(), Some("m-1"));
    assert_eq!(got.content_type(), Some("text/plain"));
    assert!(bus.shutdown(Duration::from_secs(5)).await.is_clean());
}

#[test]
fn json_sets_the_content_type() {
    #[derive(ferroly::codec::Encode)]
    struct Order {
        id: u32,
    }
    let msg = Message::json(&Order { id: 1 });
    assert_eq!(msg.content_type(), Some("application/json"));
    assert_eq!(Message::text("x").content_type(), None);
    assert_eq!(Message::new(vec![1u8]).content_type(), None);
}

// ---- content-type-aware encoding (WP6) ----------------------------------------------

#[derive(ferroly::codec::Encode, ferroly::codec::Decode, Debug, PartialEq, Clone)]
struct Order {
    id: u32,
    sku: String,
    express: bool,
}

fn order() -> Order {
    Order {
        id: 7,
        sku: "A-1".to_string(),
        express: true,
    }
}

#[test]
fn encode_as_round_trips_each_format() {
    for content_type in [
        "application/json",
        "application/yaml",
        "application/toml",
        "application/xml",
    ] {
        let msg = Message::encode_as(content_type, &order()).unwrap();
        assert_eq!(msg.content_type(), Some(content_type));
        let back: Order = msg.decode().unwrap();
        assert_eq!(back, order(), "{content_type}");
    }
}

#[test]
fn decode_falls_back_to_json_without_a_content_type() {
    #[derive(ferroly::codec::Decode, Debug, PartialEq)]
    struct T {
        a: u32,
    }
    let msg = Message::new(br#"{"a":1}"#.to_vec());
    assert_eq!(msg.decode::<T>().unwrap(), T { a: 1 });
}

#[test]
fn an_unknown_content_type_decodes_as_json() {
    #[derive(ferroly::codec::Decode, Debug, PartialEq)]
    struct T {
        a: u32,
    }
    let msg = Message::new(br#"{"a":2}"#.to_vec()).with_content_type("application/x-custom");
    assert_eq!(msg.decode::<T>().unwrap(), T { a: 2 });
}

#[test]
fn encode_as_rejects_an_unknown_content_type() {
    assert!(Message::encode_as("application/x-custom", &order()).is_err());
}
