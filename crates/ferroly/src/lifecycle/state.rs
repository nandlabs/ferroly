//! Component state enum.

/// The lifecycle state of a component.
///
/// These variants carry no external wire format, so their discriminants are an
/// internal detail rather than a stable numeric contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComponentState {
    /// Never started; the initial state after registration.
    Unknown,
    /// A start or stop attempt failed.
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
}
