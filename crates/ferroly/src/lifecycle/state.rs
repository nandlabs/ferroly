//! Component state enum and the state-change event.

use std::time::Duration;

/// The lifecycle state of a component.
///
/// These variants carry no external wire format, so their discriminants are an
/// internal detail rather than a stable numeric contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComponentState {
    /// Never started; the initial state after registration.
    Unknown,
    /// A start or stop attempt failed (returned an error, panicked, timed out
    /// or was cancelled), or the component reported a failure while running.
    Error,
    /// Cleanly stopped.
    Stopped,
    /// In the middle of stopping.
    Stopping,
    /// Running normally.
    Running,
    /// In the middle of starting.
    Starting,
}

impl ComponentState {
    /// Returns `true` if the component is fully running.
    pub fn is_running(self) -> bool {
        self == ComponentState::Running
    }

    /// Returns `true` while a start or stop is in flight
    /// ([`Starting`](Self::Starting) or [`Stopping`](Self::Stopping)).
    pub fn is_transitional(self) -> bool {
        matches!(self, ComponentState::Starting | ComponentState::Stopping)
    }

    /// The lowercase name of the state (`"running"`, `"error"`, …), handy as a
    /// metrics label.
    pub fn as_str(self) -> &'static str {
        match self {
            ComponentState::Unknown => "unknown",
            ComponentState::Error => "error",
            ComponentState::Stopped => "stopped",
            ComponentState::Stopping => "stopping",
            ComponentState::Running => "running",
            ComponentState::Starting => "starting",
        }
    }
}

/// One component state transition, as published on the manager's global event
/// stream ([`ComponentManager::subscribe`](crate::lifecycle::ComponentManager::subscribe))
/// and passed to a [`LifecycleObserver`](crate::lifecycle::LifecycleObserver).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateChange {
    /// The component id.
    pub id: String,
    /// The state the component left.
    pub from: ComponentState,
    /// The state the component entered.
    pub to: ComponentState,
    /// How long the component spent in `from`. For a `Starting → Running`
    /// change this is the start duration; for `Stopping → Stopped` the stop
    /// duration.
    pub elapsed: Duration,
}
