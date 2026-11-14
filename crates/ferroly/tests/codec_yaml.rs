#![cfg(feature = "codec")]
//! YAML as people actually write it: the review's fail-open reproductions, and
//! realistic hand-written documents checked against their JSON equivalent.

use ferroly::codec::{json, yaml, Decode, Value};

/// Parses YAML and renders the result as compact JSON.
fn j(doc: &str) -> String {
    match yaml::from_str(doc) {
        Ok(v) => json::to_string(&v),
        Err(e) => panic!("failed to parse: {e}\n{doc}"),
    }
}

#[test]
fn review_reproductions_now_parse_correctly() {
    // Each of these used to load "successfully" with data missing or mangled.
    // `{tags: null}` — the list and `port` were dropped.
    assert_eq!(
        j("tags:\n- a\n- b\nport: 1"),
        r#"{"tags":["a","b"],"port":1}"#
    );
    // `Str("8080 # comment")`.
    assert_eq!(j("port: 8080 # comment"), r#"{"port":8080}"#);
    // `Str("[a, b]")`.
    assert_eq!(j("tags: [a, b]"), r#"{"tags":["a","b"]}"#);
    // `[{name: a}]` — everything after the first key was dropped.
    assert_eq!(
        j("- name: a\n  port: 1\n- name: b"),
        r#"[{"name":"a","port":1},{"name":"b"}]"#
    );
}

#[test]
fn typed_config_decodes_from_the_common_styles() {
    #[derive(Decode, PartialEq, Debug)]
    struct Upstream {
        name: String,
        port: u16,
        weight: Option<f64>,
    }
    #[derive(Decode, PartialEq, Debug)]
    struct Config {
        port: u16,
        tags: Vec<String>,
        upstreams: Vec<Upstream>,
        debug: bool,
    }
    let want = Config {
        port: 8080,
        tags: vec!["a".into(), "b".into()],
        upstreams: vec![
            Upstream {
                name: "one".into(),
                port: 1,
                weight: Some(0.5),
            },
            Upstream {
                name: "two".into(),
                port: 2,
                weight: None,
            },
        ],
        debug: false,
    };
    let styles = [
        // Indented sequences, compact items.
        "port: 8080\ntags:\n  - a\n  - b\nupstreams:\n  - name: one\n    port: 1\n    weight: 0.5\n  - name: two\n    port: 2\ndebug: false\n",
        // Same-indent sequences with comments.
        "port: 8080 # listen\ntags:\n- a\n- b\nupstreams:\n- name: one\n  port: 1\n  weight: 0.5\n- name: two\n  port: 2\ndebug: false\n",
        // Flow collections.
        "port: 8080\ntags: [a, b]\nupstreams: [{name: one, port: 1, weight: 0.5}, {name: two, port: 2}]\ndebug: false\n",
        // Items on their own lines, quoted strings, document markers.
        "---\nport: 8080\ntags:\n  - \"a\"\n  - 'b'\nupstreams:\n  -\n    name: one\n    port: 1\n    weight: 0.5\n  -\n    name: two\n    port: 2\n    weight: ~\ndebug: false\n...\n",
        // JSON is (nearly) a subset.
        r#"{"port": 8080, "tags": ["a", "b"], "upstreams": [{"name": "one", "port": 1, "weight": 0.5}, {"name": "two", "port": 2, "weight": null}], "debug": false}"#,
    ];
    for doc in styles {
        let got: Config = yaml::decode(doc).unwrap_or_else(|e| panic!("{e}\n{doc}"));
        assert_eq!(got, want, "{doc}");
    }
}

#[test]
fn a_compose_style_document() {
    let doc = r#"
# A docker-compose style file.
version: "3.9"

services:
  web:
    image: "nginx:1.25"   # pinned
    ports:
      - "80:80"
      - 443:443
    environment:
      - KEY=value
      - EMPTY=
    depends_on: [db, cache]
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost"]
      interval: 30s
      retries: 3
    command: >-
      sh -c
      "nginx -g 'daemon off;'"
  db:
    image: postgres
    volumes:
    - db-data:/var/lib/postgresql/data
    - ./init.sql:/docker-entrypoint-initdb.d/init.sql:ro
    restart: unless-stopped

volumes:
  db-data: {}
  cache:
"#;
    let v = yaml::from_str(doc).unwrap();
    assert_eq!(v.pointer("/version"), Some(&Value::Str("3.9".into())));
    assert_eq!(
        v.pointer("/services/web/image").and_then(Value::as_str),
        Some("nginx:1.25")
    );
    assert_eq!(
        json::to_string(v.pointer("/services/web/ports").unwrap()),
        r#"["80:80","443:443"]"#
    );
    assert_eq!(
        json::to_string(v.pointer("/services/web/environment").unwrap()),
        r#"["KEY=value","EMPTY="]"#
    );
    assert_eq!(
        json::to_string(v.pointer("/services/web/depends_on").unwrap()),
        r#"["db","cache"]"#
    );
    assert_eq!(
        json::to_string(v.pointer("/services/web/healthcheck").unwrap()),
        r#"{"test":["CMD","curl","-f","http://localhost"],"interval":"30s","retries":3}"#
    );
    assert_eq!(
        v.pointer("/services/web/command").and_then(Value::as_str),
        Some("sh -c \"nginx -g 'daemon off;'\"")
    );
    assert_eq!(
        json::to_string(v.pointer("/services/db/volumes").unwrap()),
        r#"["db-data:/var/lib/postgresql/data","./init.sql:/docker-entrypoint-initdb.d/init.sql:ro"]"#
    );
    assert_eq!(
        v.pointer("/services/db/restart").and_then(Value::as_str),
        Some("unless-stopped")
    );
    assert_eq!(
        json::to_string(v.get("volumes").unwrap()),
        r#"{"db-data":{},"cache":null}"#
    );
}

#[test]
fn a_manifest_style_document() {
    let doc = r#"apiVersion: apps/v1
kind: Deployment
metadata:
  name: api
  labels: {app: api, tier: "backend"}
  annotations:
    description: |
      Line one.
      Line two: with a colon # and a hash.
spec:
  replicas: 3
  template:
    spec:
      containers:
      - name: api
        image: registry.example.com/api:1.2.3
        args: ["--port", "8080", --verbose]
        env:
        - name: MODE
          value: "production"
        - name: RATIO
          value: "0.5"
        resources:
          limits: {cpu: 500m, memory: 128Mi}
      - name: sidecar
        image: sidecar
"#;
    let v = yaml::from_str(doc).unwrap();
    assert_eq!(v.pointer("/spec/replicas"), Some(&Value::Int(3)));
    assert_eq!(
        json::to_string(v.pointer("/metadata/labels").unwrap()),
        r#"{"app":"api","tier":"backend"}"#
    );
    assert_eq!(
        v.pointer("/metadata/annotations/description")
            .and_then(Value::as_str),
        Some("Line one.\nLine two: with a colon # and a hash.\n")
    );
    let containers = v
        .pointer("/spec/template/spec/containers")
        .and_then(Value::as_array)
        .unwrap();
    assert_eq!(containers.len(), 2);
    assert_eq!(
        json::to_string(&containers[0]),
        r#"{"name":"api","image":"registry.example.com/api:1.2.3","args":["--port","8080","--verbose"],"env":[{"name":"MODE","value":"production"},{"name":"RATIO","value":"0.5"}],"resources":{"limits":{"cpu":"500m","memory":"128Mi"}}}"#
    );
    assert_eq!(
        json::to_string(&containers[1]),
        r#"{"name":"sidecar","image":"sidecar"}"#
    );
}

#[test]
fn unsupported_documents_are_errors_with_a_reason() {
    for (doc, needle) in [
        ("base: &base\n  a: 1\nderived: *base\n", "anchors"),
        ("a: 1\nb: *ref\n", "aliases"),
        ("when: !!timestamp 2001-12-14\n", "tags"),
        ("a: 1\n---\nb: 2\n", "multiple documents"),
        ("%YAML 1.2\n---\na: 1\n", "directives"),
        ("? [a, b]\n: 1\n", "explicit keys"),
        ("list: [a,\n  b]\n", "one line"),
        ("text: first line\n  second line\n", "indentation"),
        ("a:\n\tb: 1\n", "tabs"),
        ("a: b: c\n", "mapping values"),
    ] {
        let err = yaml::from_str(doc).unwrap_err().to_string();
        assert!(
            err.contains(needle),
            "{doc:?}: expected {needle:?} in {err:?}"
        );
        assert!(err.contains("at line "), "{doc:?}: no position in {err:?}");
    }
}
