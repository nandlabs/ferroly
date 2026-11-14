//! Regression tests for `ferroly::log` output: record integrity (I10), flushing
//! (I11), sinks, rotation, overflow policy, redaction, sampling and caller
//! capture. Every test uses its own logger and buffer and logs at `Info` or
//! above, so the tests are independent of each other and of the global level.

#![cfg(feature = "log")]

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::codec::{json, Value};
use ferroly::log::{
    Format, Level, Logger, Overflow, Precision, Redactor, RotatingFile, Rotation, Sink,
};

// ---- helpers -------------------------------------------------------------------

/// A shared in-memory writer.
#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buf {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
    fn lines(&self) -> Vec<String> {
        self.text().lines().map(str::to_string).collect()
    }
}

/// A writer that, like `BufWriter`, holds bytes back until `flush`.
#[derive(Clone, Default)]
struct Buffered {
    pending: Arc<Mutex<Vec<u8>>>,
    flushed: Buf,
}

impl Write for Buffered {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.pending.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let mut pending = self.pending.lock().unwrap();
        self.flushed.0.lock().unwrap().extend_from_slice(&pending);
        pending.clear();
        Ok(())
    }
}

/// A unique, empty scratch directory under the system temp dir.
fn scratch(tag: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ferroly-log-test-{}-{}-{}",
        std::process::id(),
        tag,
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Parses one JSON log line, asserting that no object in it repeats a key.
fn parse_json_line(line: &str) -> Value {
    fn assert_unique(v: &Value, line: &str) {
        match v {
            Value::Object(entries) => {
                for (i, (k, child)) in entries.iter().enumerate() {
                    assert!(
                        entries[..i].iter().all(|(other, _)| other != k),
                        "key {k:?} appears twice in {line}"
                    );
                    assert_unique(child, line);
                }
            }
            Value::Array(items) => items.iter().for_each(|c| assert_unique(c, line)),
            _ => {}
        }
    }
    let v = json::from_str(line).unwrap_or_else(|e| panic!("invalid JSON {line:?}: {e:?}"));
    assert_unique(&v, line);
    v
}

/// Reads one plain-format token (bare or quoted) from the front of `s`.
fn plain_token(s: &str) -> (String, &str) {
    if let Some(mut rest) = s.strip_prefix('"') {
        let mut out = String::new();
        loop {
            let c = rest.chars().next().expect("unterminated quoted token");
            rest = &rest[c.len_utf8()..];
            match c {
                '"' => return (out, rest),
                '\\' => {
                    let e = rest.chars().next().expect("dangling escape");
                    rest = &rest[e.len_utf8()..];
                    match e {
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        'u' => {
                            let code = u32::from_str_radix(&rest[..4], 16).expect("hex escape");
                            out.push(char::from_u32(code).expect("scalar value"));
                            rest = &rest[4..];
                        }
                        other => panic!("unknown escape \\{other}"),
                    }
                }
                c => out.push(c),
            }
        }
    }
    let end = s.find([' ', '=']).unwrap_or(s.len());
    (s[..end].to_string(), &s[end..])
}

/// Splits a plain-format line whose message is one word into
/// `(timestamp, level, message, fields)`.
fn parse_plain_line(line: &str) -> (String, String, String, Vec<(String, String)>) {
    let mut head = line.splitn(2, ' ');
    let ts = head.next().unwrap().to_string();
    let rest = head.next().unwrap();
    let level = rest[..5].trim().to_string();
    let rest = &rest[6..];
    let (msg, mut rest) = plain_token(rest);
    let mut fields = Vec::new();
    while !rest.is_empty() {
        rest = rest.strip_prefix(' ').expect("fields are space-separated");
        let (key, after) = plain_token(rest);
        let after = after.strip_prefix('=').expect("key=value");
        let (value, after) = plain_token(after);
        fields.push((key, value));
        rest = after;
    }
    (ts, level, msg, fields)
}

// ---- I10: record integrity -------------------------------------------------------

const HOSTILE: &[&str] = &[
    "line one\nline two",
    "carriage\rreturn",
    "tab\tseparated",
    "esc \u{1b}[31mred\u{1b}[0m",
    "nul\u{0}byte",
    "fake=field other=1",
    "quote \" and backslash \\",
    "",
    "unicode\u{2028}line\u{2029}para\u{85}nel",
    "ok\n2026-01-01T00:00:00.000Z ERROR forged admin=true",
];

#[test]
fn plain_format_fields_cannot_forge_or_split_a_record() {
    for (i, hostile) in HOSTILE.iter().enumerate() {
        let buf = Buf::default();
        let log = Logger::new()
            .to_writer(buf.clone())
            .with(*hostile, "ctx-value")
            .with("ctx", *hostile);
        log.info(
            "evt",
            &[("value", (*hostile).into()), (hostile, "call".into())],
        );

        let text = buf.text();
        assert_eq!(text.matches('\n').count(), 1, "case {i}: {text:?}");
        assert!(text.ends_with('\n'));
        let line = text.trim_end_matches('\n');
        assert!(
            !line
                .chars()
                .any(|c| c.is_control() || c == '\u{2028}' || c == '\u{2029}'),
            "case {i}: raw control character in {line:?}"
        );

        // The line parses back to exactly the fields that were logged. The
        // hostile key was given twice (logger field, then call field): the
        // later value wins, at the first position.
        let (_ts, level, msg, fields) = parse_plain_line(line);
        assert_eq!((level.as_str(), msg.as_str()), ("INFO", "evt"));
        assert_eq!(
            fields,
            vec![
                (hostile.to_string(), "call".to_string()),
                ("ctx".to_string(), hostile.to_string()),
                ("value".to_string(), hostile.to_string()),
            ],
            "case {i}: {line:?}"
        );
    }
}

#[test]
fn plain_format_quotes_only_what_needs_it() {
    let buf = Buf::default();
    Logger::new().to_writer(buf.clone()).warn(
        "cache miss",
        &[
            ("key", "user:42".into()),
            ("n", 7.into()),
            ("ok", true.into()),
            ("path", "/a/b?x".into()),
            ("note", "two words".into()),
            ("eq", "a=b".into()),
            ("obj", Value::Object(vec![("a".into(), "x y".into())])),
        ],
    );
    let line = buf.text();
    assert!(
        line.ends_with(
            " WARN  cache miss key=user:42 n=7 ok=true path=/a/b?x note=\"two words\" \
             eq=\"a=b\" obj={\"a\":\"x y\"}\n"
        ),
        "{line:?}"
    );
}

#[test]
fn plain_format_message_stays_on_one_line() {
    let buf = Buf::default();
    Logger::new()
        .to_writer(buf.clone())
        .info("a\nb\r\nc\u{2028}d\u{1b}[0m\te", &[]);
    let text = buf.text();
    assert_eq!(text.matches('\n').count(), 1);
    assert!(text.ends_with(" INFO  a b  c d\\u001b[0m\te\n"), "{text:?}");
}

#[test]
fn json_fields_never_duplicate_the_record_keys() {
    let buf = Buf::default();
    let log = Logger::json()
        .to_writer(buf.clone())
        .with("level", "forged-by-context")
        .with("svc", "api");
    log.error(
        "real message",
        &[
            ("msg", "forged".into()),
            ("ts", "1999-01-01T00:00:00Z".into()),
            ("level", "debug".into()),
            ("status", 500.into()),
        ],
    );

    let text = buf.text();
    assert_eq!(text.matches("\"level\":").count(), 2, "top + nested only");
    let v = parse_json_line(text.trim_end());
    assert_eq!(v.get("level").and_then(Value::as_str), Some("error"));
    assert_eq!(v.get("msg").and_then(Value::as_str), Some("real message"));
    assert!(v
        .get("ts")
        .and_then(Value::as_str)
        .unwrap()
        .starts_with('2'));
    assert_eq!(v.get("svc").and_then(Value::as_str), Some("api"));
    assert_eq!(v.get("status").and_then(Value::as_i64), Some(500));

    // The colliding fields are kept, nested under `fields`; the call's `level`
    // replaced the logger's.
    let nested = v.get("fields").expect("nested object");
    assert_eq!(nested.get("level").and_then(Value::as_str), Some("debug"));
    assert_eq!(nested.get("msg").and_then(Value::as_str), Some("forged"));
    assert_eq!(
        nested.get("ts").and_then(Value::as_str),
        Some("1999-01-01T00:00:00Z")
    );
    assert_eq!(nested.as_object().unwrap().len(), 3);

    // The plain format prints the same fields with a `fields.` prefix.
    let buf = Buf::default();
    Logger::new()
        .to_writer(buf.clone())
        .info("evt", &[("level", "debug".into()), ("a", 1.into())]);
    let (_, level, _, fields) = parse_plain_line(buf.text().trim_end());
    assert_eq!(level, "INFO");
    assert_eq!(
        fields,
        vec![
            ("a".to_string(), "1".to_string()),
            ("fields.level".to_string(), "debug".to_string()),
        ]
    );
}

#[test]
fn json_handles_a_user_field_called_fields_and_repeated_keys() {
    // No collision: a field called `fields` is an ordinary field.
    let buf = Buf::default();
    Logger::json().to_writer(buf.clone()).info(
        "m",
        &[("fields", 3.into()), ("a", 1.into()), ("a", 2.into())],
    );
    let v = parse_json_line(buf.text().trim_end());
    assert_eq!(v.get("fields").and_then(Value::as_i64), Some(3));
    assert_eq!(v.get("a").and_then(Value::as_i64), Some(2), "last one wins");

    // With a collision it moves into the nested object instead of repeating
    // the `fields` key.
    let buf = Buf::default();
    Logger::json()
        .to_writer(buf.clone())
        .info("m", &[("fields", 3.into()), ("msg", "x".into())]);
    let v = parse_json_line(buf.text().trim_end());
    let nested = v.get("fields").unwrap();
    assert_eq!(nested.get("msg").and_then(Value::as_str), Some("x"));
    assert_eq!(nested.get("fields").and_then(Value::as_i64), Some(3));
}

#[test]
fn json_lines_stay_single_line_with_hostile_input() {
    for hostile in HOSTILE {
        let buf = Buf::default();
        Logger::json()
            .to_writer(buf.clone())
            .with(*hostile, 1)
            .info(hostile, &[("v", (*hostile).into())]);
        let text = buf.text();
        assert_eq!(text.matches('\n').count(), 1, "{text:?}");
        assert!(
            !text
                .trim_end_matches('\n')
                .chars()
                .any(|c| c.is_control() || c == '\u{2028}' || c == '\u{2029}'),
            "{text:?}"
        );
        let v = parse_json_line(text.trim_end_matches('\n'));
        assert_eq!(v.get("msg").and_then(Value::as_str), Some(*hostile));
        assert_eq!(v.get("v").and_then(Value::as_str), Some(*hostile));
        assert_eq!(v.get(hostile).and_then(Value::as_i64), Some(1));
    }
}

// ---- I11: flushing ---------------------------------------------------------------

#[test]
fn flush_reaches_a_buffering_synchronous_writer() {
    let w = Buffered::default();
    let log = Logger::new().to_writer(w.clone());

    log.info("held back", &[]);
    assert_eq!(w.flushed.text(), "", "below Warn: still in the buffer");
    log.flush();
    assert!(w.flushed.text().contains("held back"), "flush() flushes");

    // Warn and above flush the writer right after the record.
    log.info("second", &[]);
    log.warn("urgent", &[]);
    let out = w.flushed.text();
    assert!(out.contains("second") && out.contains("urgent"), "{out:?}");
    log.error("fatal", &[]);
    assert!(w.flushed.text().contains("fatal"));

    // The threshold is configurable per sink.
    let w = Buffered::default();
    let log = Logger::new().to_sink(Sink::writer(w.clone()).flush_level(Level::Info));
    log.info("eager", &[]);
    assert!(w.flushed.text().contains("eager"));
    let w = Buffered::default();
    let log = Logger::new().to_sink(Sink::writer(w.clone()).flush_level(Level::Error));
    log.warn("lazy", &[]);
    assert_eq!(w.flushed.text(), "");
    log.flush();
    assert!(w.flushed.text().contains("lazy"));
}

#[test]
fn flush_reaches_a_real_bufwriter_file() {
    let dir = scratch("bufwriter");
    let path = dir.join("app.log");
    let file = std::fs::File::create(&path).unwrap();
    let log = Logger::json().to_writer(std::io::BufWriter::new(file));

    log.info("buffered", &[("n", 1.into())]);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 0, "still buffered");
    log.flush();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("\"msg\":\"buffered\""), "{text:?}");

    drop(log);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---- sinks -----------------------------------------------------------------------

#[test]
fn records_fan_out_to_sinks_with_their_own_format_and_level() {
    let (all, errors, audit) = (Buf::default(), Buf::default(), Buf::default());
    let log = Logger::new()
        .to_writer(all.clone())
        .add_sink(
            Sink::writer(errors.clone())
                .format(Format::Json)
                .level(Level::Error),
        )
        .add_sink(Sink::non_blocking(audit.clone(), 64).level(Level::Warn));

    log.info("routine", &[("k", "v".into())]);
    log.warn("odd", &[]);
    log.error("broken", &[("code", 7.into())]);
    log.flush();

    let all_lines = all.lines();
    assert_eq!(all_lines.len(), 3);
    assert!(
        all_lines[0].ends_with(" INFO  routine k=v"),
        "{all_lines:?}"
    );

    let error_lines = errors.lines();
    assert_eq!(error_lines.len(), 1, "only Error reaches the JSON sink");
    let v = parse_json_line(&error_lines[0]);
    assert_eq!(v.get("msg").and_then(Value::as_str), Some("broken"));
    assert_eq!(v.get("code").and_then(Value::as_i64), Some(7));

    // The third sink has no format of its own: it follows the logger (plain).
    let audit_lines = audit.lines();
    assert_eq!(audit_lines.len(), 2);
    assert!(audit_lines[0].ends_with(" WARN  odd"));
    assert!(audit_lines[1].ends_with(" ERROR broken code=7"));

    // `enabled` accounts for sink levels, and a clone shares the sinks.
    let strict = Logger::new().to_sink(Sink::writer(Buf::default()).level(Level::Error));
    assert!(!strict.enabled(Level::Warn));
    assert!(strict.enabled(Level::Error));
    log.clone().with("child", true).error("from clone", &[]);
    assert_eq!(all.lines().len(), 4);
    assert_eq!(log.dropped(), 0);
}

/// A writer that blocks in `write` until a token arrives on its gate.
struct Gated {
    gate: Receiver<()>,
    out: Buf,
}

impl Write for Gated {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        // A closed gate (sender dropped) lets everything through.
        let _ = self.gate.recv();
        self.out.write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn overflow_drop_counts_what_it_sheds() {
    let (gate_tx, gate_rx) = channel::<()>();
    let out = Buf::default();
    let log = Logger::new().async_to(
        Gated {
            gate: gate_rx,
            out: out.clone(),
        },
        1,
    );

    // The writer is stuck on the first record, so at most two records (one in
    // the writer, one queued) are accepted; the rest are shed, never blocking.
    for i in 0..10 {
        log.info("burst", &[("i", i.into())]);
    }
    let dropped = log.dropped();
    assert!((8..=9).contains(&dropped), "dropped={dropped}");

    drop(gate_tx); // open the gate
    log.flush();
    assert_eq!(out.lines().len() as u64, 10 - dropped);
    assert_eq!(log.dropped(), dropped, "nothing further was lost");
}

#[test]
fn overflow_block_loses_nothing() {
    let (gate_tx, gate_rx) = channel::<()>();
    let out = Buf::default();
    let log = Logger::new().to_sink(
        Sink::non_blocking(
            Gated {
                gate: gate_rx,
                out: out.clone(),
            },
            1,
        )
        .overflow(Overflow::Block),
    );

    let producer = {
        let log = log.clone();
        std::thread::spawn(move || {
            for i in 0..50 {
                log.info("audit", &[("i", i.into())]);
            }
        })
    };
    drop(gate_tx); // the producer waits for space rather than dropping
    producer.join().unwrap();
    log.flush();

    let lines = out.lines();
    assert_eq!(lines.len(), 50);
    for (i, line) in lines.iter().enumerate() {
        assert!(line.ends_with(&format!(" INFO  audit i={i}")), "{line}");
    }
    assert_eq!(log.dropped(), 0);
}

#[test]
fn a_writer_that_logs_does_not_deadlock() {
    /// Logs through its own logger from inside `write`.
    struct Chatty {
        me: Arc<Mutex<Option<Logger>>>,
        out: Buf,
    }
    impl Write for Chatty {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if let Some(log) = self.me.lock().unwrap().as_ref() {
                log.error("from inside write", &[]);
                log.flush();
            }
            self.out.write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let me = Arc::new(Mutex::new(None));
    let out = Buf::default();
    let log = Logger::new().to_writer(Chatty {
        me: me.clone(),
        out: out.clone(),
    });
    *me.lock().unwrap() = Some(log.clone());

    log.error("outer", &[]);
    let lines = out.lines();
    assert_eq!(lines.len(), 1, "the nested record is discarded: {lines:?}");
    assert!(lines[0].ends_with(" ERROR outer"));
    *me.lock().unwrap() = None;
}

// ---- rotating file ---------------------------------------------------------------

#[test]
fn rotating_file_rotates_by_size_and_keeps_n_files() {
    let dir = scratch("rotate");
    let path = dir.join("app.log");
    // Every record below is exactly 30 bytes; two fit in 64 bytes, three do not.
    let file = RotatingFile::open(&path, Rotation::new().max_bytes(64).keep(2)).unwrap();
    assert_eq!(file.path(), path.as_path());
    let log = Logger::new()
        .time_precision(Precision::Seconds)
        .to_writer(file);

    for i in 0..7 {
        log.info(&format!("r{i}"), &[]);
    }
    log.flush();

    let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap();
    let msgs = |text: &str| -> Vec<String> {
        text.lines()
            .map(|l| l.rsplit(' ').next().unwrap().to_string())
            .collect()
    };
    // Newest in the live file, older files have higher numbers.
    assert_eq!(msgs(&read("app.log")), ["r6"]);
    assert_eq!(msgs(&read("app.log.1")), ["r4", "r5"]);
    assert_eq!(msgs(&read("app.log.2")), ["r2", "r3"]);
    assert!(!dir.join("app.log.3").exists(), "r0/r1 were deleted");
    for name in ["app.log", "app.log.1", "app.log.2"] {
        let text = read(name);
        assert!(text.len() <= 64, "{name} is {} bytes", text.len());
        assert!(text.ends_with('\n'), "{name}: no record is split");
        assert!(text.lines().all(|l| l.len() == 29), "{name}: {text:?}");
    }

    drop(log);
    // Reopening appends to the live file and keeps counting its size.
    let mut file = RotatingFile::open(&path, Rotation::new().max_bytes(64).keep(2)).unwrap();
    file.write_all(b"appended-appended-appended-30b\n").unwrap();
    assert_eq!(read("app.log").lines().count(), 2);
    file.write_all(b"this one makes it rotate again\n").unwrap();
    assert_eq!(read("app.log").lines().count(), 1);
    assert_eq!(read("app.log.1").lines().count(), 2);

    // A forced rotation, and `keep(0)` (truncate only).
    file.rotate().unwrap();
    assert_eq!(read("app.log"), "");
    drop(file);
    let mut file = RotatingFile::open(&path, Rotation::new().max_bytes(8).keep(0)).unwrap();
    file.write_all(b"12345678").unwrap();
    file.write_all(b"abc").unwrap();
    assert_eq!(read("app.log"), "abc");
    // An oversized record is written whole into a file of its own.
    file.write_all(b"a record far larger than the limit\n")
        .unwrap();
    assert_eq!(read("app.log"), "a record far larger than the limit\n");
    drop(file);

    assert!(RotatingFile::open(dir.join("no/such/dir/app.log"), Rotation::new()).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---- redaction -------------------------------------------------------------------

#[test]
fn sensitive_fields_are_redacted_in_both_formats() {
    let nested = Value::Object(vec![
        ("user".into(), "ada".into()),
        ("Password".into(), "hunter2".into()),
        (
            "headers".into(),
            Value::Array(vec![Value::Object(vec![
                ("Authorization".into(), "Bearer abc.def".into()),
                ("accept".into(), "*/*".into()),
            ])]),
        ),
    ]);
    let fields = [
        ("user", Value::from("ada")),
        ("password", "hunter2".into()),
        ("api_key", "k-123".into()),
        ("X-Auth-Token", "t-456".into()),
        ("max_tokens", 256.into()),
        ("request", nested),
    ];

    for format in [Format::Plain, Format::Json] {
        let buf = Buf::default();
        Logger::new()
            .format(format)
            .to_writer(buf.clone())
            .with("client_secret", "s3cr3t")
            .info("login", &fields);
        let text = buf.text();
        for leaked in ["hunter2", "k-123", "t-456", "s3cr3t", "Bearer", "abc.def"] {
            assert!(!text.contains(leaked), "{format:?} leaked {leaked}: {text}");
        }
        assert!(text.contains("ada") && text.contains("256") && text.contains("*/*"));
        assert_eq!(text.matches("***").count(), 6, "{text}");
    }

    // The structure survives: only the values change.
    let buf = Buf::default();
    Logger::json().to_writer(buf.clone()).info("login", &fields);
    let v = parse_json_line(buf.text().trim_end());
    assert_eq!(v.get("password").and_then(Value::as_str), Some("***"));
    assert_eq!(v.get("max_tokens").and_then(Value::as_i64), Some(256));
    let request = v.get("request").unwrap();
    assert_eq!(request.get("user").and_then(Value::as_str), Some("ada"));
    assert_eq!(request.get("Password").and_then(Value::as_str), Some("***"));
    let header = request.get("headers").unwrap().get_index(0).unwrap();
    assert_eq!(
        header.get("Authorization").and_then(Value::as_str),
        Some("***")
    );

    // Custom patterns and mask; and switching redaction off.
    let buf = Buf::default();
    Logger::json()
        .to_writer(buf.clone())
        .redact(Redactor::with_patterns(&["ssn"]).mask("[redacted]"))
        .info(
            "x",
            &[("ssn", "123-45-6789".into()), ("password", "pw".into())],
        );
    let v = parse_json_line(buf.text().trim_end());
    assert_eq!(v.get("ssn").and_then(Value::as_str), Some("[redacted]"));
    assert_eq!(v.get("password").and_then(Value::as_str), Some("pw"));

    let buf = Buf::default();
    Logger::json()
        .to_writer(buf.clone())
        .redact(Redactor::none())
        .info("x", &[("password", "pw".into())]);
    assert!(buf.text().contains("\"password\":\"pw\""));
}

// ---- sampling, caller, target, timestamps ------------------------------------------

#[test]
fn sampling_limits_repeats_per_message() {
    let buf = Buf::default();
    let log = Logger::new()
        .to_writer(buf.clone())
        .sample(2, Duration::from_secs(3600));

    for i in 0..5 {
        log.warn("disk almost full", &[("i", i.into())]);
    }
    log.warn("another message", &[]);
    log.error("disk almost full", &[]); // another level: another key
    let child = log.clone().with("child", true);
    child.warn("disk almost full", &[]); // clones share the budget

    let lines = buf.lines();
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert!(lines[0].ends_with("i=0") && lines[1].ends_with("i=1"));
    assert!(lines[2].ends_with("another message"));
    assert!(lines[3].contains("ERROR"));
    assert_eq!(log.suppressed(), 4);
    assert_eq!(Logger::new().suppressed(), 0);
}

#[test]
fn caller_capture_is_opt_in() {
    let buf = Buf::default();
    let log = Logger::json().to_writer(buf.clone());
    log.info("without", &[]);
    let with = log.clone().with_caller(true);
    let line = line!() + 1;
    with.info("with", &[("caller", "user-supplied".into())]);

    let lines = buf.lines();
    assert!(parse_json_line(&lines[0]).get("caller").is_none());
    let v = parse_json_line(&lines[1]);
    let caller = v.get("caller").and_then(Value::as_str).unwrap();
    assert!(
        caller.ends_with(&format!("log_format.rs:{line}")),
        "caller={caller}"
    );
    // A user field of the same name cannot shadow it.
    assert_eq!(
        v.get("fields")
            .and_then(|f| f.get("caller"))
            .and_then(Value::as_str),
        Some("user-supplied")
    );

    let buf = Buf::default();
    let log = Logger::new().to_writer(buf.clone()).with_caller(true);
    let line = line!() + 1;
    log.info("evt", &[]);
    // On Windows the path has backslashes, so the text format quotes it.
    assert!(
        buf.text()
            .trim_end()
            .trim_end_matches('"')
            .ends_with(&format!("log_format.rs:{line}")),
        "{}",
        buf.text()
    );
}

#[test]
fn target_is_printed_and_reserved() {
    let buf = Buf::default();
    let log = Logger::json()
        .to_writer(buf.clone())
        .with_target("app::billing");
    assert_eq!(log.target(), Some("app::billing"));
    log.info("charged", &[("target", "user-field".into())]);
    let v = parse_json_line(buf.text().trim_end());
    assert_eq!(
        v.get("target").and_then(Value::as_str),
        Some("app::billing")
    );
    assert_eq!(
        v.get("fields")
            .and_then(|f| f.get("target"))
            .and_then(Value::as_str),
        Some("user-field")
    );

    // Without a target, `target` is an ordinary field name.
    let buf = Buf::default();
    let plain = Logger::json().to_writer(buf.clone());
    assert_eq!(plain.target(), None);
    plain.info("m", &[("target", "x".into())]);
    let v = parse_json_line(buf.text().trim_end());
    assert_eq!(v.get("target").and_then(Value::as_str), Some("x"));
    assert!(v.get("fields").is_none());

    let buf = Buf::default();
    Logger::new()
        .to_writer(buf.clone())
        .with_target("app::billing")
        .info("evt", &[("k", 1.into())]);
    assert!(
        buf.text().ends_with(" INFO  evt target=app::billing k=1\n"),
        "{}",
        buf.text()
    );
}

#[test]
fn timestamp_precision_and_offset_are_configurable() {
    let stamp = |log: Logger| {
        let buf = Buf::default();
        log.to_writer(buf.clone()).info("m", &[]);
        buf.text().split(' ').next().unwrap().to_string()
    };
    let shape = |ts: &str| -> String {
        ts.chars()
            .map(|c| if c.is_ascii_digit() { '9' } else { c })
            .collect()
    };
    assert_eq!(shape(&stamp(Logger::new())), "9999-99-99T99:99:99.999Z");
    assert_eq!(
        shape(&stamp(Logger::new().time_precision(Precision::Seconds))),
        "9999-99-99T99:99:99Z"
    );
    assert_eq!(
        shape(&stamp(Logger::new().time_precision(Precision::Micros))),
        "9999-99-99T99:99:99.999999Z"
    );
    assert_eq!(
        shape(&stamp(Logger::new().time_precision(Precision::Nanos))),
        "9999-99-99T99:99:99.999999999Z"
    );
    let ts = stamp(Logger::new().utc_offset(5 * 3600 + 30 * 60));
    assert!(ts.ends_with("+05:30"), "{ts}");
    let ts = stamp(Logger::new().utc_offset(-8 * 3600));
    assert!(ts.ends_with("-08:00"), "{ts}");
}

#[test]
fn levels_parse_from_configuration_strings() {
    assert_eq!("WARN".parse::<Level>(), Ok(Level::Warn));
    assert_eq!("info".parse::<Level>(), Ok(Level::Info));
    assert_eq!(Level::Error.to_string(), "error");
    let err = "verbose".parse::<Level>().unwrap_err();
    assert!(err.to_string().contains("verbose"));
}
