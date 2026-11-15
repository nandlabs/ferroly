//! Tests for the `log` facade bridge (`ferroly::log::bridge`). The facade's
//! logger is process-wide and can be set once, so every test shares one
//! installed bridge and takes `serial()` first; each restores the level
//! filters it changed.

#![cfg(feature = "log-bridge")]

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use ferroly::codec::{json, Value};
use ferroly::log::bridge::{self, BridgeError};
use ferroly::log::{self as flog, Level, Logger};
use log_facade as log;

fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Holds bytes back until `flush`, like a `BufWriter`, so the tests can see
/// that the facade's `flush` reaches the ferroly sink.
#[derive(Clone, Default)]
struct Buffered {
    pending: Arc<Mutex<Vec<u8>>>,
    flushed: Arc<Mutex<Vec<u8>>>,
}

/// While set, the writer logs through the facade from inside `write` — a
/// re-entrant record.
static REENTER: AtomicBool = AtomicBool::new(false);

impl Write for Buffered {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if REENTER.load(Ordering::SeqCst) {
            log::error!("logged from inside the sink's writer");
        }
        self.pending.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let mut pending = self.pending.lock().unwrap();
        self.flushed.lock().unwrap().extend_from_slice(&pending);
        pending.clear();
        Ok(())
    }
}

impl Buffered {
    fn has_pending(&self) -> bool {
        !self.pending.lock().unwrap().is_empty()
    }

    /// Flushes through the facade, then takes every written line.
    fn take_lines(&self) -> Vec<Value> {
        log::logger().flush();
        let bytes = std::mem::take(&mut *self.flushed.lock().unwrap());
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| json::from_str(line).unwrap_or_else(|e| panic!("bad JSON {line:?}: {e:?}")))
            .collect()
    }
}

/// Installs the bridge once for the whole test binary, to a JSON logger with
/// caller capture. Its writer holds records until flushed; the sink flushes it
/// on its own only after `Warn`-and-above records, so the buffering check uses
/// `Info`/`Debug` records.
fn installed() -> &'static Buffered {
    static OUT: OnceLock<Buffered> = OnceLock::new();
    OUT.get_or_init(|| {
        let out = Buffered::default();
        let logger = Logger::json()
            .with_caller(true)
            .with("app", "bridge-test")
            .to_writer(out.clone());
        bridge::install(logger).expect("first install succeeds");
        out
    })
}

fn str_field<'a>(record: &'a Value, key: &str) -> &'a str {
    match record.get(key) {
        Some(Value::Str(s)) => s,
        other => panic!("{key}: expected a string, got {other:?} in {record:?}"),
    }
}

#[test]
fn records_from_log_macros_reach_the_ferroly_logger() {
    let _serial = serial();
    let out = installed();
    flog::set_max_level(Level::Trace);
    out.take_lines();

    let user = "ada";
    log::info!("hello {user}");
    log::debug!(target: "custom::target", "with a target");
    assert!(out.has_pending(), "Info/Debug records wait for a flush");

    let lines = out.take_lines();
    assert_eq!(lines.len(), 2, "{lines:?}");

    let first = &lines[0];
    assert_eq!(str_field(first, "msg"), "hello ada");
    assert_eq!(str_field(first, "level"), "info");
    assert_eq!(str_field(first, "target"), module_path!());
    assert_eq!(str_field(first, "app"), "bridge-test");
    assert!(
        str_field(first, "caller").contains("log_bridge.rs:"),
        "{first:?}"
    );
    // The module path equals the target, so it is not repeated.
    assert!(first.get("module").is_none(), "{first:?}");

    let second = &lines[1];
    assert_eq!(str_field(second, "level"), "debug");
    assert_eq!(str_field(second, "target"), "custom::target");
    assert_eq!(str_field(second, "module"), module_path!());

    flog::set_max_level(Level::Info);
}

#[test]
fn key_values_become_typed_fields() {
    let _serial = serial();
    let out = installed();
    flog::set_max_level(Level::Info);
    out.take_lines();

    log::warn!(status = 503, retry = true, ratio = 0.5, peer = "10.0.0.1", password = "hunter2"; "upstream failed");
    let lines = out.take_lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let record = &lines[0];
    assert_eq!(record.get("status"), Some(&Value::Int(503)));
    assert_eq!(record.get("retry"), Some(&Value::Bool(true)));
    assert_eq!(record.get("ratio"), Some(&Value::Float(0.5)));
    assert_eq!(str_field(record, "peer"), "10.0.0.1");
    // Redaction applies to bridged fields as to any other.
    assert_eq!(str_field(record, "password"), "***");
}

#[test]
fn level_filters_apply_and_drive_the_facade_max_level() {
    let _serial = serial();
    let out = installed();
    out.take_lines();

    flog::set_max_level(Level::Warn);
    assert_eq!(log::max_level(), log::LevelFilter::Warn);
    assert!(!log::log_enabled!(log::Level::Info));
    assert!(log::log_enabled!(log::Level::Warn));
    log::info!("dropped");
    log::error!("kept");
    let lines = out.take_lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(str_field(&lines[0], "msg"), "kept");

    // A per-target override lowers the facade gate for that target only.
    flog::set_level_for("bridge_noisy", Level::Trace);
    assert_eq!(log::max_level(), log::LevelFilter::Trace);
    assert!(log::log_enabled!(target: "bridge_noisy::inner", log::Level::Trace));
    assert!(!log::log_enabled!(target: "bridge_quiet", log::Level::Trace));
    log::trace!(target: "bridge_noisy::inner", "noisy kept");
    log::trace!(target: "bridge_quiet", "quiet dropped");
    let lines = out.take_lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(str_field(&lines[0], "msg"), "noisy kept");

    assert!(flog::clear_level_for("bridge_noisy"));
    assert_eq!(log::max_level(), log::LevelFilter::Warn);

    flog::set_max_level(Level::Info);
    assert_eq!(log::max_level(), log::LevelFilter::Info);
}

#[test]
fn a_second_install_fails_without_panicking() {
    let _serial = serial();
    installed();
    assert!(bridge::is_installed());
    assert_eq!(
        bridge::install(Logger::new()),
        Err(BridgeError::AlreadyInstalled)
    );
    assert_eq!(bridge::install_global(), Err(BridgeError::AlreadyInstalled));
}

#[test]
fn reentrant_and_truncated_records_are_counted() {
    let _serial = serial();
    let out = installed();
    flog::set_max_level(Level::Info);
    out.take_lines();

    let before = bridge::stats();
    log::info!("plain");
    log::debug!("filtered out: not counted");
    let after = bridge::stats();
    assert_eq!(after.forwarded, before.forwarded + 1);
    assert_eq!(after.reentrant, before.reentrant);

    // A record logged from inside the sink's writer is dropped, and counted.
    REENTER.store(true, Ordering::SeqCst);
    log::warn!("outer");
    REENTER.store(false, Ordering::SeqCst);
    let lines = out.take_lines();
    let msgs: Vec<&str> = lines.iter().map(|l| str_field(l, "msg")).collect();
    assert_eq!(msgs, ["plain", "outer"], "{lines:?}");
    let reentered = bridge::stats();
    assert_eq!(reentered.reentrant, after.reentrant + 1);
    assert_eq!(reentered.forwarded, after.forwarded + 1);

    // More than MAX_KV_FIELDS pairs: the record is kept, cut, and counted.
    let many: Vec<(String, i64)> = (0..bridge::MAX_KV_FIELDS as i64 + 5)
        .map(|i| (format!("k{i}"), i))
        .collect();
    let record = log::Record::builder()
        .level(log::Level::Warn)
        .target(module_path!())
        .args(format_args!("wide"))
        .key_values(&many)
        .build();
    log::logger().log(&record);
    let lines = out.take_lines();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].get("k63").is_some() && lines[0].get("k64").is_none());
    assert_eq!(bridge::stats().truncated, reentered.truncated + 1);
}
