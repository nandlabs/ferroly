//! The [`Task`] describing what a routed call needs.

use std::time::Instant;

use ferroly::genai::Capability;

/// A fixed-vocabulary rule key for [`RuleBasedStrategy`](super::RuleBasedStrategy)
/// lookups. Carries no model mapping on its own; finer-grained policy uses
/// [`Task::tags`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskKind {
    /// Multi-turn chat.
    Chat,
    /// Code generation.
    CodeGen,
    /// Summarization.
    Summarize,
    /// Structured extraction.
    Extract,
    /// Classification.
    Classify,
    /// Extended reasoning.
    Reason,
}

/// What to optimize the selection for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Priority {
    /// Prefer faster / smaller models.
    Speed,
    /// Prefer the cheapest model that fits.
    Cost,
    /// Prefer the most capable model (the default).
    #[default]
    Quality,
}

/// Bypass routing entirely and target one model (A/B tests, debugging).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    /// The provider name.
    pub provider: String,
    /// The model name.
    pub model: String,
}

/// A description of what a call needs; the router turns it into a `(provider,
/// model)` choice plus fallbacks.
///
/// Construct with `Task { .. }` and `..Default::default()`, or `Task::new()` and
/// the chained setters.
#[derive(Debug, Clone, Default)]
pub struct Task {
    /// Rule-lookup key (optional; route purely on capabilities when `None`).
    pub kind: Option<TaskKind>,
    /// Free-form policy tags, e.g. `["sensitive", "long-context"]`.
    pub tags: Vec<String>,
    /// Estimated input tokens (limit + cost checks).
    pub est_input_tokens: u32,
    /// Estimated output tokens (cost ranking; 0 → a per-kind default).
    pub est_output_tokens: u32,
    /// Optional per-call USD budget (0 = unbounded).
    pub max_cost_usd: f64,
    /// What to optimize for.
    pub priority: Priority,
    /// Hard capability filter — a model must advertise all of these.
    pub required: Vec<Capability>,
    /// Bypass routing and target one model.
    pub pin: Option<Pin>,
    /// Overall deadline across all attempts.
    pub deadline: Option<Instant>,
}

impl Task {
    /// A new, empty task (all defaults).
    pub fn new() -> Self {
        Task::default()
    }

    /// Sets the rule-lookup kind.
    pub fn kind(mut self, kind: TaskKind) -> Self {
        self.kind = Some(kind);
        self
    }

    /// Sets the optimization priority.
    pub fn priority(mut self, priority: Priority) -> Self {
        self.priority = priority;
        self
    }

    /// Requires a capability (may be called repeatedly).
    pub fn require(mut self, capability: Capability) -> Self {
        self.required.push(capability);
        self
    }

    /// Sets the estimated input/output token counts.
    pub fn tokens(mut self, est_input: u32, est_output: u32) -> Self {
        self.est_input_tokens = est_input;
        self.est_output_tokens = est_output;
        self
    }

    /// Sets a per-call USD budget.
    pub fn max_cost_usd(mut self, usd: f64) -> Self {
        self.max_cost_usd = usd;
        self
    }

    /// Pins the task to one model, bypassing routing.
    pub fn pin(mut self, provider: impl Into<String>, model: impl Into<String>) -> Self {
        self.pin = Some(Pin {
            provider: provider.into(),
            model: model.into(),
        });
        self
    }

    /// Sets an overall deadline.
    pub fn deadline(mut self, at: Instant) -> Self {
        self.deadline = Some(at);
        self
    }
}
