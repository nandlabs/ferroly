//! A bridge from the [`log`](https://docs.rs/log/0.4) facade crate into
//! [`ferroly::log`](super): records that other crates emit with `log::info!`,
//! `log::warn!`, … are written by a ferroly [`Logger`], with its sinks, format,
//! redaction, sampling and context providers.
//!
//! Enabled by the `log-bridge` feature (off by default; included in `full`).
//! It is the only feature that adds a crate outside the `tokio`/`rustls`
//! budget — `log` itself, which has no dependencies of its own. There is no
//! bridge from `tracing`.
//!
//! ```
//! use ferroly::log::{bridge, Logger};
//!
//! // Route every `log` record to a JSON logger …
//! bridge::install(Logger::json()).expect("no other `log` logger installed");
//! // … or, instead, to whatever logger `ferroly::log::set_global` holds:
//! // bridge::install_global()
//! ```
//!
//! # What a bridged record carries
//!
//! | `log` record | ferroly record |
//! |---|---|
//! | level `Trace` … `Error` | the same [`Level`] |
//! | the formatted message | `msg` |
//! | `target` (the module path unless set with `target: "…"`) | `target` — it also selects the [per-target level](super::set_level_for), in place of the logger's own target |
//! | `module_path`, when it differs from the target | a `module` field |
//! | `file` / `line` | `caller` (`file:line`), when the logger has [`with_caller(true)`](Logger::with_caller) |
//! | key-value pairs (`log::info!(user = 7; "…")`) | typed fields (numbers, booleans and strings keep their type; anything else is its `Display` text), at most [`MAX_KV_FIELDS`] per record |
//!
//! # Level filtering
//!
//! A record is written when the logger's floor, the global or per-target
//! level, and at least one sink admit it — exactly as for a record logged
//! through the [`Logger`] directly, so `log::log_enabled!` agrees with
//! [`Logger::enabled`]. The facade's own max-level gate (`log::max_level()`)
//! is kept at the lowest level the logger could emit for any target, and is
//! updated whenever [`set_max_level`](super::set_max_level),
//! [`set_level_for`](super::set_level_for), the `clear_*` functions or
//! [`set_global`](super::set_global) change the filters, so disabled records
//! cost the calling crate a single atomic load.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use log_facade as facade;
use log_facade::kv::{self, VisitSource, VisitValue};

use ferroly::codec::Value;

use super::{global, Level, Logger};

/// The most key-value pairs taken from one `log` record; further pairs are
/// ignored, so a record's field list stays bounded.
pub const MAX_KV_FIELDS: usize = 64;

/// The error returned by [`install`] and [`install_global`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeError {
    /// The bridge is already installed; it can be installed once per process.
    AlreadyInstalled,
    /// Another `log` implementation (for example `env_logger`) was installed
    /// first; the facade accepts one logger per process.
    OtherLoggerInstalled,
}

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BridgeError::AlreadyInstalled => f.write_str("the log bridge is already installed"),
            BridgeError::OtherLoggerInstalled => {
                f.write_str("another logger is already installed for the log crate")
            }
        }
    }
}

impl std::error::Error for BridgeError {}

/// Where bridged records go.
#[derive(Clone)]
enum Destination {
    /// One fixed logger, given to [`install`].
    Fixed(Logger),
    /// The process-global logger, looked up on every record.
    Global,
}

impl Destination {
    fn with<R>(&self, f: impl FnOnce(&Logger) -> R) -> R {
        match self {
            Destination::Fixed(logger) => f(logger),
            Destination::Global => f(&global()),
        }
    }
}

/// The installed destination; set once, under `INSTALL`.
static DESTINATION: OnceLock<Destination> = OnceLock::new();
/// Held while installing, so two racing installs cannot both succeed.
static INSTALL: Mutex<()> = Mutex::new(());
/// Held while updating the facade's max level, one update at a time.
static SYNC: Mutex<()> = Mutex::new(());

static FORWARDED: AtomicU64 = AtomicU64::new(0);
static REENTRANT: AtomicU64 = AtomicU64::new(0);
static TRUNCATED: AtomicU64 = AtomicU64::new(0);

/// What the bridge did with the `log` records it received, counted since the
/// process started; see [`stats`].
///
/// Records the level filters reject are not counted: they cost one atomic
/// load and are not losses.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BridgeStats {
    /// Records that passed the filters and were handed to the ferroly logger
    /// (its sampler and sinks may still drop them, as for any record; an
    /// asynchronous sink counts its own drops in
    /// [`Logger::dropped`](super::Logger::dropped)).
    pub forwarded: u64,
    /// Records discarded because they were logged from inside a synchronous
    /// sink's writer — writing them would re-enter the sink's lock.
    pub reentrant: u64,
    /// Forwarded records that carried more than [`MAX_KV_FIELDS`] key-value
    /// pairs and lost the rest.
    pub truncated: u64,
}

/// The bridge's counters: records forwarded, records dropped as re-entrant,
/// and records whose key-value pairs were cut. All zero until the bridge is
/// installed and receives records.
///
/// ```
/// let stats = ferroly::log::bridge::stats();
/// // e.g. export as metrics, or warn when records are being lost:
/// if stats.reentrant > 0 {
///     eprintln!("{} log records were dropped as re-entrant", stats.reentrant);
/// }
/// ```
pub fn stats() -> BridgeStats {
    BridgeStats {
        forwarded: FORWARDED.load(Ordering::Relaxed),
        reentrant: REENTRANT.load(Ordering::Relaxed),
        truncated: TRUNCATED.load(Ordering::Relaxed),
    }
}

/// Routes every record of the `log` facade to `logger`, and sets the facade's
/// max level from the logger's current filters.
///
/// The logger's own [target](Logger::with_target) is replaced on each record
/// by the `log` record's target. Can succeed once per process; a second call
/// returns [`BridgeError::AlreadyInstalled`], and a call after another `log`
/// implementation was installed returns [`BridgeError::OtherLoggerInstalled`].
/// It never panics.
pub fn install(logger: Logger) -> Result<(), BridgeError> {
    install_destination(Destination::Fixed(logger))
}

/// Like [`install`], but routes records to the process-global logger —
/// whichever logger [`set_global`](super::set_global) holds when each record
/// arrives (the stderr default until one is set).
pub fn install_global() -> Result<(), BridgeError> {
    install_destination(Destination::Global)
}

/// Whether the bridge has been installed in this process.
pub fn is_installed() -> bool {
    DESTINATION.get().is_some()
}

fn install_destination(destination: Destination) -> Result<(), BridgeError> {
    {
        let _guard = INSTALL.lock().unwrap_or_else(|e| e.into_inner());
        if DESTINATION.get().is_some() {
            return Err(BridgeError::AlreadyInstalled);
        }
        facade::set_boxed_logger(Box::new(Bridge {
            destination: destination.clone(),
        }))
        .map_err(|_| BridgeError::OtherLoggerInstalled)?;
        // Only reachable once: `INSTALL` is held and `DESTINATION` was empty.
        let _ = DESTINATION.set(destination);
    }
    sync_max_level();
    Ok(())
}

/// Sets the facade's max level to the lowest level the installed destination
/// could emit. A no-op until the bridge is installed, so it never touches the
/// facade while another `log` implementation owns it.
pub(super) fn sync_max_level() {
    let Some(destination) = DESTINATION.get() else {
        return;
    };
    // One update at a time, so the last one reflects the latest filter change.
    let _guard = SYNC.lock().unwrap_or_else(|e| e.into_inner());
    let lowest = destination.with(Logger::lowest_enabled);
    facade::set_max_level(level_filter(lowest));
}

fn level_filter(level: Option<Level>) -> facade::LevelFilter {
    match level {
        None => facade::LevelFilter::Off,
        Some(Level::Trace) => facade::LevelFilter::Trace,
        Some(Level::Debug) => facade::LevelFilter::Debug,
        Some(Level::Info) => facade::LevelFilter::Info,
        Some(Level::Warn) => facade::LevelFilter::Warn,
        Some(Level::Error) => facade::LevelFilter::Error,
    }
}

fn level(level: facade::Level) -> Level {
    match level {
        facade::Level::Trace => Level::Trace,
        facade::Level::Debug => Level::Debug,
        facade::Level::Info => Level::Info,
        facade::Level::Warn => Level::Warn,
        facade::Level::Error => Level::Error,
    }
}

/// The target a bridged record is filtered and printed under: the `log`
/// record's, or the logger's own when the record has none.
fn target<'a>(record_target: &'a str, logger: &'a Logger) -> Option<&'a str> {
    if record_target.is_empty() {
        logger.target()
    } else {
        Some(record_target)
    }
}

struct Bridge {
    destination: Destination,
}

impl facade::Log for Bridge {
    fn enabled(&self, metadata: &facade::Metadata<'_>) -> bool {
        self.destination.with(|logger| {
            logger.passes(target(metadata.target(), logger), level(metadata.level()))
        })
    }

    fn log(&self, record: &facade::Record<'_>) {
        self.destination.with(|logger| forward(logger, record));
    }

    fn flush(&self) {
        self.destination.with(Logger::flush);
    }
}

fn forward(logger: &Logger, record: &facade::Record<'_>) {
    let level = level(record.level());
    let target = target(record.target(), logger);
    if !logger.passes(target, level) {
        return;
    }
    // Logged from inside a sink's writer: the logger would drop it anyway
    // (re-entering the sink would deadlock); count the loss here.
    if super::sink::writing() {
        REENTRANT.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let formatted;
    let msg = match record.args().as_str() {
        Some(literal) => literal,
        None => {
            formatted = record.args().to_string();
            formatted.as_str()
        }
    };

    let mut pairs = Pairs::default();
    if let Some(module) = record.module_path() {
        if Some(module) != target {
            pairs.push("module".to_string(), Value::Str(module.to_string()));
        }
    }
    // A visitor error only ends the walk early; the pairs read so far stay.
    let _ = record.key_values().visit(&mut pairs);
    if pairs.truncated {
        TRUNCATED.fetch_add(1, Ordering::Relaxed);
    }
    let fields: Vec<(&str, Value)> = pairs
        .keys
        .iter()
        .map(String::as_str)
        .zip(pairs.values)
        .collect();

    let caller = record.file().zip(record.line());
    FORWARDED.fetch_add(1, Ordering::Relaxed);
    logger.emit(target, level, msg, &fields, caller);
}

/// Collects a record's key-value pairs as typed fields.
#[derive(Default)]
struct Pairs {
    keys: Vec<String>,
    values: Vec<Value>,
    taken: usize,
    /// Whether pairs beyond [`MAX_KV_FIELDS`] were left out.
    truncated: bool,
}

impl Pairs {
    fn push(&mut self, key: String, value: Value) {
        self.keys.push(key);
        self.values.push(value);
    }
}

impl<'kvs> VisitSource<'kvs> for Pairs {
    fn visit_pair(&mut self, key: kv::Key<'kvs>, value: kv::Value<'kvs>) -> Result<(), kv::Error> {
        if self.taken >= MAX_KV_FIELDS {
            self.truncated = true;
            return Err(kv::Error::msg("too many key-value pairs"));
        }
        self.taken += 1;
        let mut typed = Typed(Value::Null);
        // Each visit method below records a value and never fails.
        let _ = value.visit(&mut typed);
        self.push(key.as_str().to_string(), typed.0);
        Ok(())
    }
}

/// Converts one `log` key-value value to a [`Value`], keeping its type.
struct Typed(Value);

impl<'v> VisitValue<'v> for Typed {
    fn visit_any(&mut self, value: kv::Value<'_>) -> Result<(), kv::Error> {
        self.0 = Value::Str(value.to_string());
        Ok(())
    }

    fn visit_null(&mut self) -> Result<(), kv::Error> {
        self.0 = Value::Null;
        Ok(())
    }

    fn visit_u64(&mut self, value: u64) -> Result<(), kv::Error> {
        self.0 = Value::UInt(value);
        Ok(())
    }

    fn visit_i64(&mut self, value: i64) -> Result<(), kv::Error> {
        self.0 = Value::Int(value);
        Ok(())
    }

    fn visit_u128(&mut self, value: u128) -> Result<(), kv::Error> {
        self.0 = match u64::try_from(value) {
            Ok(v) => Value::UInt(v),
            Err(_) => Value::Str(value.to_string()),
        };
        Ok(())
    }

    fn visit_i128(&mut self, value: i128) -> Result<(), kv::Error> {
        self.0 = match i64::try_from(value) {
            Ok(v) => Value::Int(v),
            Err(_) => Value::Str(value.to_string()),
        };
        Ok(())
    }

    fn visit_f64(&mut self, value: f64) -> Result<(), kv::Error> {
        self.0 = Value::Float(value);
        Ok(())
    }

    fn visit_bool(&mut self, value: bool) -> Result<(), kv::Error> {
        self.0 = Value::Bool(value);
        Ok(())
    }

    fn visit_str(&mut self, value: &str) -> Result<(), kv::Error> {
        self.0 = Value::Str(value.to_string());
        Ok(())
    }

    fn visit_char(&mut self, value: char) -> Result<(), kv::Error> {
        self.0 = Value::Str(value.to_string());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_map_one_to_one() {
        let pairs = [
            (facade::Level::Trace, Level::Trace),
            (facade::Level::Debug, Level::Debug),
            (facade::Level::Info, Level::Info),
            (facade::Level::Warn, Level::Warn),
            (facade::Level::Error, Level::Error),
        ];
        for (from, to) in pairs {
            assert_eq!(level(from), to);
            assert_eq!(level_filter(Some(to)), from.to_level_filter());
        }
        assert_eq!(level_filter(None), facade::LevelFilter::Off);
    }

    #[test]
    fn key_values_keep_their_type_and_are_capped() {
        let source: Vec<(String, i64)> = (0..(MAX_KV_FIELDS as i64 + 10))
            .map(|i| (format!("k{i}"), i))
            .collect();
        let borrowed: Vec<(&str, i64)> = source.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        let mut pairs = Pairs::default();
        let _ = kv::Source::visit(&borrowed[..], &mut pairs);
        assert_eq!(pairs.keys.len(), MAX_KV_FIELDS);
        assert_eq!(pairs.values[3], Value::Int(3));

        let mut typed = Typed(Value::Null);
        let _ = kv::Value::from(true).visit(&mut typed);
        assert_eq!(typed.0, Value::Bool(true));
        let _ = kv::Value::from(u128::MAX).visit(&mut typed);
        assert_eq!(typed.0, Value::Str(u128::MAX.to_string()));
        let _ = kv::Value::from_display(&"shown").visit(&mut typed);
        assert_eq!(typed.0, Value::Str("shown".to_string()));
    }

    #[test]
    fn errors_display() {
        assert!(BridgeError::AlreadyInstalled
            .to_string()
            .contains("already installed"));
        assert!(BridgeError::OtherLoggerInstalled
            .to_string()
            .contains("another logger"));
    }
}
