//! Tests for the process-global parts of `ferroly::log`: the global logger,
//! context providers, per-target levels and the panic hook. They share global
//! state, so every test takes `serial()` first and restores what it changed.

#![cfg(feature = "log")]

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use ferroly::codec::{json, Value};
use ferroly::log::{self, Level, Logger};

fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

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

/// Holds bytes back until `flush`, like a `BufWriter`.
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

fn parse(line: &str) -> Value {
    json::from_str(line).unwrap_or_else(|e| panic!("invalid JSON {line:?}: {e:?}"))
}

#[test]
fn providers_run_without_the_lock_and_cannot_recurse() {
    let _serial = serial();
    log::clear_context_providers();
    let buf = Buf::default();
    log::set_global(Logger::json().to_writer(buf.clone()));

    let calls = Arc::new(AtomicUsize::new(0));
    let registered_more = Arc::new(AtomicBool::new(false));
    {
        let (calls, registered_more) = (calls.clone(), registered_more.clone());
        log::add_context_provider(move || {
            calls.fetch_add(1, Ordering::Relaxed);
            // Logging from a provider used to recurse without bound.
            log::warn("from provider", &[]);
            // Registering from a provider used to deadlock on the list lock.
            if !registered_more.swap(true, Ordering::Relaxed) {
                log::add_context_provider(|| vec![("late".to_string(), Value::Bool(true))]);
            }
            vec![("request_id".to_string(), Value::Str("r-1".into()))]
        });
    }

    // Run on a helper thread so a regression shows up as a failure, not a hang.
    let (done_tx, done_rx) = channel();
    let worker = std::thread::spawn(move || {
        log::info("outer", &[]);
        log::info("second", &[]);
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("logging with a re-entrant provider must terminate");
    worker.join().unwrap();

    assert_eq!(calls.load(Ordering::Relaxed), 2, "once per outer record");
    let lines: Vec<Value> = buf.lines().iter().map(|l| parse(l)).collect();
    let msgs: Vec<&str> = lines
        .iter()
        .map(|v| v.get("msg").and_then(Value::as_str).unwrap())
        .collect();
    assert_eq!(msgs, ["from provider", "outer", "from provider", "second"]);
    // The nested record carries no provider fields; the outer ones do.
    assert!(lines[0].get("request_id").is_none());
    assert_eq!(
        lines[1].get("request_id").and_then(Value::as_str),
        Some("r-1")
    );
    assert!(
        lines[1].get("late").is_none(),
        "registered after the snapshot"
    );
    assert_eq!(lines[3].get("late").and_then(Value::as_bool), Some(true));

    log::clear_context_providers();
    log::set_global(Logger::new().to_writer(Buf::default()));
}

#[test]
fn provider_fields_are_escaped_and_redacted_like_any_other() {
    let _serial = serial();
    log::clear_context_providers();
    log::add_context_provider(|| {
        vec![
            (
                "trace\nid".to_string(),
                Value::Str("abc\n2026-01-01T00:00:00.000Z ERROR forged".into()),
            ),
            ("session_token".to_string(), Value::Str("s3cr3t".into())),
            ("level".to_string(), Value::Str("forged".into())),
        ]
    });

    let plain = Buf::default();
    Logger::new().to_writer(plain.clone()).info("evt", &[]);
    let text = plain.text();
    assert_eq!(text.matches('\n').count(), 1, "{text:?}");
    assert!(
        text.ends_with(
            " INFO  evt \"trace\\nid\"=\"abc\\n2026-01-01T00:00:00.000Z ERROR forged\" \
             session_token=*** fields.level=forged\n"
        ),
        "{text:?}"
    );

    let jsonl = Buf::default();
    Logger::json().to_writer(jsonl.clone()).info("evt", &[]);
    let text = jsonl.text();
    assert_eq!(text.matches('\n').count(), 1);
    let v = parse(text.trim_end());
    assert_eq!(v.get("level").and_then(Value::as_str), Some("info"));
    assert_eq!(v.get("session_token").and_then(Value::as_str), Some("***"));
    assert_eq!(
        v.get("fields")
            .and_then(|f| f.get("level"))
            .and_then(Value::as_str),
        Some("forged")
    );
    assert!(v.get("trace\nid").is_some());

    log::clear_context_providers();
}

#[test]
fn per_target_levels_override_the_global_level() {
    let _serial = serial();
    log::clear_target_levels();
    log::set_max_level(Level::Info);
    assert_eq!(log::max_level(), Level::Info);

    let buf = Buf::default();
    let base = Logger::new().to_writer(buf.clone());
    let http = base.clone().with_target("tgt::http::client");
    let httpx = base.clone().with_target("tgt::httpx");
    let db = base.clone().with_target("tgt::db");

    // Quieter than the global level.
    log::set_level_for("tgt::http", Level::Warn);
    assert_eq!(log::level_for("tgt::http::client"), Level::Warn);
    assert_eq!(
        log::level_for("tgt::httpx"),
        Level::Info,
        "not a segment match"
    );
    http.info("http-info", &[]);
    http.warn("http-warn", &[]);
    httpx.info("httpx-info", &[]);
    assert!(!http.enabled(Level::Info) && http.enabled(Level::Warn));

    // Louder than the global level.
    log::set_level_for("tgt::db", Level::Trace);
    db.debug("db-debug", &[]);
    base.debug("untargeted-debug", &[]);
    assert!(log::enabled_for("tgt::db", Level::Trace));
    assert!(!log::enabled(Level::Debug));

    // The longest matching prefix wins; a per-logger floor still applies.
    log::set_level_for("tgt::http::client", Level::Debug);
    http.debug("http-debug", &[]);
    db.clone().level(Level::Error).warn("db-floor", &[]);

    let text = buf.text();
    for kept in ["http-warn", "httpx-info", "db-debug", "http-debug"] {
        assert!(text.contains(kept), "{kept} missing from {text}");
    }
    for dropped in ["http-info", "untargeted-debug", "db-floor"] {
        assert!(!text.contains(dropped), "{dropped} present in {text}");
    }

    // Overrides can be replaced and removed.
    log::set_level_for("tgt::db", Level::Error);
    assert_eq!(log::level_for("tgt::db"), Level::Error);
    assert!(log::clear_level_for("tgt::db"));
    assert!(!log::clear_level_for("tgt::db"));
    assert_eq!(log::level_for("tgt::db"), Level::Info);
    log::set_level_for("", Level::Error); // ignored
    assert_eq!(log::level_for("anything"), Level::Info);
    log::clear_target_levels();
    assert_eq!(log::level_for("tgt::http::client"), Level::Info);
}

#[test]
fn the_global_logger_can_be_flushed_at_shutdown() {
    let _serial = serial();
    log::clear_context_providers();
    log::set_max_level(Level::Info);
    let w = Buffered::default();
    log::set_global(Logger::new().to_writer(w.clone()));

    log::info("buffered", &[]);
    assert_eq!(w.flushed.text(), "", "an Info record stays in the buffer");
    log::flush();
    assert!(w.flushed.text().contains("buffered"));

    {
        let _flush = log::flush_guard();
        log::info("guarded", &[]);
        assert!(!w.flushed.text().contains("guarded"));
    }
    assert!(w.flushed.text().contains("guarded"), "flushed on drop");

    // A child of the global logger shares its sink.
    let child = log::logger().with_target("tests::child").with("k", 1);
    child.error("from child", &[]);
    assert!(w
        .flushed
        .text()
        .ends_with(" ERROR from child target=tests::child k=1\n"));

    log::set_global(Logger::new().to_writer(Buf::default()));
}

#[test]
fn the_panic_hook_logs_and_flushes_through_the_global_logger() {
    let _serial = serial();
    log::clear_context_providers();
    log::set_max_level(Level::Info);
    let w = Buffered::default();
    log::set_global(Logger::json().to_writer(w.clone()));

    let default_hook = std::panic::take_hook();
    log::install_panic_hook_with(false);
    let result = std::thread::Builder::new()
        .name("worker-7".into())
        .spawn(|| panic!("boom {}", 42))
        .unwrap()
        .join();
    // Restore the standard hook before asserting anything.
    let _ = std::panic::take_hook();
    std::panic::set_hook(default_hook);
    assert!(result.is_err());

    let text = w.flushed.text();
    let v = parse(text.trim_end());
    assert_eq!(v.get("level").and_then(Value::as_str), Some("error"));
    assert_eq!(v.get("msg").and_then(Value::as_str), Some("panic"));
    assert_eq!(
        v.get("panic.message").and_then(Value::as_str),
        Some("boom 42")
    );
    assert_eq!(
        v.get("panic.thread").and_then(Value::as_str),
        Some("worker-7")
    );
    let location = v.get("panic.location").and_then(Value::as_str).unwrap();
    assert!(location.contains("log_global.rs:"), "{location}");

    // The chaining variant still logs, then runs the previous hook.
    let chained = Arc::new(AtomicBool::new(false));
    let default_hook = std::panic::take_hook();
    {
        let chained = chained.clone();
        std::panic::set_hook(Box::new(move |_| chained.store(true, Ordering::Relaxed)));
    }
    log::install_panic_hook();
    let result = std::thread::spawn(|| panic!("{}", String::from("second"))).join();
    let _ = std::panic::take_hook();
    std::panic::set_hook(default_hook);
    assert!(result.is_err());
    assert!(chained.load(Ordering::Relaxed), "previous hook ran");
    let text = w.flushed.text();
    let last = parse(text.lines().last().unwrap());
    assert_eq!(
        last.get("panic.message").and_then(Value::as_str),
        Some("second")
    );

    log::set_global(Logger::new().to_writer(Buf::default()));
}

#[test]
fn the_trace_context_provider_is_safe_to_install_without_a_span() {
    let _serial = serial();
    log::clear_context_providers();
    // No span is active here, with or without the `obs` feature.
    assert!(log::trace_context_fields().is_empty());
    log::add_trace_context_provider();
    log::add_trace_context_provider(); // idempotent

    let buf = Buf::default();
    Logger::json().to_writer(buf.clone()).info("no span", &[]);
    let v = parse(buf.text().trim_end());
    assert!(v.get("trace_id").is_none());
    assert_eq!(v.as_object().unwrap().len(), 3, "ts, level and msg only");

    log::clear_context_providers();
}
