//! A built-in [`WsObserver`] that records WebSocket activity in a
//! [`ferroly::metrics::Registry`] (feature `metrics`).

use std::sync::Arc;

use ferroly::metrics::{Counter, CounterVec, Gauge, Registry};

use super::observer::{TimeoutKind, WsEvent, WsObserver};

/// Records [`WsEvent`]s as Prometheus metrics. Requires the `metrics`
/// feature.
///
/// Install it on a server ([`WsServerConfig::observer`](super::WsServerConfig::observer))
/// or a client ([`WsOptions::observer`](super::WsOptions::observer)); one observer can
/// serve many connections. With the default `ws` prefix it exports:
///
/// | Metric | Kind | Labels |
/// |---|---|---|
/// | `ws_connections_opened_total` | counter | |
/// | `ws_connections_active` | gauge | |
/// | `ws_connections_closed_total` | counter | `code` |
/// | `ws_messages_received_total` / `ws_messages_sent_total` | counter | |
/// | `ws_received_bytes_total` / `ws_sent_bytes_total` | counter | |
/// | `ws_protocol_errors_total` | counter | `code` |
/// | `ws_timeouts_total` | counter | `kind` (`idle`, `pong`, `write`) |
/// | `ws_handshakes_rejected_total` | counter | `status` |
/// | `ws_queue_full_total` | counter | |
///
/// Byte counts are message payload sizes (frame headers are not counted).
/// The `code` label is the close code for the codes RFC 6455 defines
/// (`1000`–`1015`); application codes are folded into `3xxx` and `4xxx`, and
/// anything else into `other`, so a peer choosing close codes cannot grow the
/// number of series.
///
/// ```
/// use std::sync::Arc;
/// use ferroly::metrics::Registry;
/// use ferroly::ws::{MetricsObserver, WsServerConfig};
///
/// let registry = Registry::new();
/// let config = WsServerConfig::default()
///     .observer(Arc::new(MetricsObserver::new(&registry)));
/// # let _ = config;
/// assert!(registry.encode().contains("ws_connections_active 0"));
/// ```
#[derive(Debug, Clone)]
pub struct MetricsObserver {
    opened: Arc<Counter>,
    active: Arc<Gauge>,
    closed: CounterVec,
    messages_received: Arc<Counter>,
    messages_sent: Arc<Counter>,
    bytes_received: Arc<Counter>,
    bytes_sent: Arc<Counter>,
    protocol_errors: CounterVec,
    timeouts: CounterVec,
    rejected: CounterVec,
    queue_full: Arc<Counter>,
}

impl MetricsObserver {
    /// An observer recording into `registry` under the `ws_` prefix.
    pub fn new(registry: &Registry) -> Self {
        Self::with_prefix(registry, "ws")
    }

    /// An observer recording into the process-global registry
    /// ([`ferroly::metrics::global`]).
    pub fn global() -> Self {
        Self::new(ferroly::metrics::global())
    }

    /// An observer recording into `registry` with metric names starting with
    /// `{prefix}_` — for example to keep a server's and a client's
    /// connections apart in one registry.
    pub fn with_prefix(registry: &Registry, prefix: &str) -> Self {
        let name = |suffix: &str| format!("{prefix}_{suffix}");
        MetricsObserver {
            opened: registry.counter(
                &name("connections_opened_total"),
                "WebSocket connections that completed the opening handshake",
                &[],
            ),
            active: registry.gauge(
                &name("connections_active"),
                "WebSocket connections currently open",
                &[],
            ),
            closed: registry.counter_vec(
                &name("connections_closed_total"),
                "WebSocket connections that ended, by close code",
                &["code"],
            ),
            messages_received: registry.counter(
                &name("messages_received_total"),
                "Complete WebSocket messages received",
                &[],
            ),
            messages_sent: registry.counter(
                &name("messages_sent_total"),
                "WebSocket messages written to the transport",
                &[],
            ),
            bytes_received: registry.counter(
                &name("received_bytes_total"),
                "Payload bytes of the WebSocket messages received",
                &[],
            ),
            bytes_sent: registry.counter(
                &name("sent_bytes_total"),
                "Payload bytes of the WebSocket messages sent",
                &[],
            ),
            protocol_errors: registry.counter_vec(
                &name("protocol_errors_total"),
                "WebSocket connections failed for a protocol or size violation, by close code",
                &["code"],
            ),
            timeouts: registry.counter_vec(
                &name("timeouts_total"),
                "Deadlines that expired on established WebSocket connections, by kind",
                &["kind"],
            ),
            rejected: registry.counter_vec(
                &name("handshakes_rejected_total"),
                "WebSocket upgrade requests refused, by HTTP status",
                &["status"],
            ),
            queue_full: registry.counter(
                &name("queue_full_total"),
                "WebSocket messages refused because the outgoing queue was full",
                &[],
            ),
        }
    }
}

/// The `code` label value: RFC 6455 codes as themselves, application ranges
/// folded, anything else `other`.
fn code_label(code: u16) -> String {
    match code {
        1000..=1015 => code.to_string(),
        3000..=3999 => "3xxx".into(),
        4000..=4999 => "4xxx".into(),
        _ => "other".into(),
    }
}

/// The `status` label value: the statuses the server's own checks send as
/// themselves, any other status folded to its class (`4xx`, `5xx`, …), and
/// anything outside `100..=599` `other`, so the label has a fixed, small set
/// of values whatever an accept hook chooses.
fn status_label(status: u16) -> String {
    match status {
        400 | 401 | 403 | 404 | 405 | 408 | 426 | 429 | 500 | 503 => status.to_string(),
        100..=599 => format!("{}xx", status / 100),
        _ => "other".into(),
    }
}

fn timeout_label(kind: TimeoutKind) -> &'static str {
    match kind {
        TimeoutKind::Idle => "idle",
        TimeoutKind::Pong => "pong",
        TimeoutKind::Write => "write",
    }
}

impl WsObserver for MetricsObserver {
    fn on_event(&self, event: &WsEvent) {
        match *event {
            WsEvent::Opened { .. } => {
                self.opened.inc();
                self.active.inc();
            }
            WsEvent::Closed { code, .. } => {
                self.active.dec();
                self.closed.with_labels(&[&code_label(code)]).inc();
            }
            WsEvent::MessageReceived { bytes, .. } => {
                self.messages_received.inc();
                self.bytes_received.inc_by(bytes as u64);
            }
            WsEvent::MessageSent { bytes, .. } => {
                self.messages_sent.inc();
                self.bytes_sent.inc_by(bytes as u64);
            }
            WsEvent::ProtocolError { code, .. } => {
                self.protocol_errors.with_labels(&[&code_label(code)]).inc();
            }
            WsEvent::QueueFull { .. } => self.queue_full.inc(),
            WsEvent::TimedOut { kind, .. } => {
                self.timeouts.with_labels(&[timeout_label(kind)]).inc();
            }
            WsEvent::HandshakeRejected { status } => {
                self.rejected.with_labels(&[&status_label(status)]).inc();
            }
            // Pings and pongs are keepalive noise; not recorded.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_become_series_with_bounded_labels() {
        let reg = Registry::new();
        let obs = MetricsObserver::new(&reg);
        obs.on_event(&WsEvent::Opened { conn_id: 1 });
        obs.on_event(&WsEvent::Opened { conn_id: 2 });
        obs.on_event(&WsEvent::MessageReceived {
            conn_id: 1,
            bytes: 5,
        });
        obs.on_event(&WsEvent::MessageSent {
            conn_id: 1,
            bytes: 7,
        });
        obs.on_event(&WsEvent::Closed {
            conn_id: 1,
            code: 1000,
        });
        for code in [4001, 4999, 3000, 2000] {
            obs.on_event(&WsEvent::Closed { conn_id: 2, code });
            obs.on_event(&WsEvent::Opened { conn_id: 2 });
        }
        obs.on_event(&WsEvent::ProtocolError {
            conn_id: 3,
            code: 1009,
            reason: "too big",
        });
        obs.on_event(&WsEvent::TimedOut {
            conn_id: 3,
            kind: TimeoutKind::Pong,
        });
        obs.on_event(&WsEvent::HandshakeRejected { status: 403 });
        // Other statuses fold to their class, so the label stays bounded.
        for status in [418, 451, 499, 502, 599, 700] {
            obs.on_event(&WsEvent::HandshakeRejected { status });
        }
        obs.on_event(&WsEvent::QueueFull { conn_id: 3 });
        obs.on_event(&WsEvent::PingSent { conn_id: 3 });

        let text = reg.encode();
        for line in [
            "ws_connections_opened_total 6",
            "ws_connections_active 1",
            "ws_connections_closed_total{code=\"1000\"} 1",
            "ws_connections_closed_total{code=\"4xxx\"} 2",
            "ws_connections_closed_total{code=\"3xxx\"} 1",
            "ws_connections_closed_total{code=\"other\"} 1",
            "ws_messages_received_total 1",
            "ws_messages_sent_total 1",
            "ws_received_bytes_total 5",
            "ws_sent_bytes_total 7",
            "ws_protocol_errors_total{code=\"1009\"} 1",
            "ws_timeouts_total{kind=\"pong\"} 1",
            "ws_handshakes_rejected_total{status=\"403\"} 1",
            "ws_handshakes_rejected_total{status=\"4xx\"} 3",
            "ws_handshakes_rejected_total{status=\"5xx\"} 2",
            "ws_handshakes_rejected_total{status=\"other\"} 1",
            "ws_queue_full_total 1",
        ] {
            assert!(text.contains(line), "{line} missing from\n{text}");
        }

        // A prefix keeps two observers apart in one registry.
        let client = MetricsObserver::with_prefix(&reg, "ws_client");
        client.on_event(&WsEvent::Opened { conn_id: 9 });
        let text = reg.encode();
        assert!(text.contains("ws_client_connections_opened_total 1"));
        assert!(text.contains("ws_connections_opened_total 6"));
        assert_eq!(reg.registration_errors(), 0);
    }
}
