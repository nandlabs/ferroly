//! Encode a struct to JSON/YAML/XML and decode it back — using ferroly's own
//! `Encode`/`Decode` derives, with zero external dependencies.
//!
//! Run with: `cargo run -p ferroly --example codec_roundtrip`

use ferroly::codec::{json, xml, yaml, Decode, Encode};

#[derive(Encode, Decode, Debug, PartialEq)]
struct Service {
    name: String,
    port: u16,
    tags: Vec<String>,
    tls: bool,
}

fn main() {
    let svc = Service {
        name: "api".into(),
        port: 8080,
        tags: vec!["public".into(), "v1".into()],
        tls: true,
    };

    // JSON (the streaming fast path).
    let as_json = json::encode(&svc);
    println!("JSON: {as_json}");
    let from_json: Service = json::decode(&as_json).unwrap();
    assert_eq!(from_json, svc);

    // YAML.
    let as_yaml = yaml::encode(&svc);
    println!("\nYAML:\n{as_yaml}");
    let from_yaml: Service = yaml::decode(&as_yaml).unwrap();
    assert_eq!(from_yaml, svc);

    // XML.
    let as_xml = xml::encode(&svc);
    println!("XML: {as_xml}");
    let from_xml: Service = xml::decode(&as_xml).unwrap();
    assert_eq!(from_xml, svc);

    println!("\nAll three codecs round-tripped identically. ✓");
}
