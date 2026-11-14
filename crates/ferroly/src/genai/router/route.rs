//! The executable plan ([`Route`]), scored options ([`Candidate`]), the
//! decision trace ([`RouteDecision`]) and the execution report
//! ([`RouteReport`]).

use std::time::Duration;

use ferroly::genai::Usage;

/// A scored routing option produced by a strategy.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// The provider name.
    pub provider: String,
    /// The model name.
    pub model: String,
    /// Strategy-defined score (higher = better).
    pub score: f64,
    /// Estimated USD cost (for the decision trace).
    pub est_cost: f64,
}

impl Candidate {
    /// `provider/model` — the key used for circuit breakers and traces.
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

/// The executable plan: a primary plus ordered fallbacks.
#[derive(Debug, Clone)]
pub struct Route {
    /// The first model to try.
    pub primary: Candidate,
    /// Ordered fallbacks, tried on transient failure of earlier attempts.
    pub fallbacks: Vec<Candidate>,
}

impl Route {
    /// The primary followed by the fallbacks, in attempt order.
    pub fn attempts(&self) -> Vec<&Candidate> {
        std::iter::once(&self.primary)
            .chain(self.fallbacks.iter())
            .collect()
    }
}

/// A model that a strategy filtered out, with the reason.
#[derive(Debug, Clone, PartialEq)]
pub struct Exclusion {
    /// The provider name.
    pub provider: String,
    /// The model name.
    pub model: String,
    /// Why it was excluded, e.g. `"missing capability: Vision"`.
    pub reason: String,
}

/// A strategy's ranked output: candidates best-first, plus what it excluded.
#[derive(Debug, Clone, Default)]
pub struct Ranking {
    /// Candidates, best-first. Empty means the strategy abstains.
    pub candidates: Vec<Candidate>,
    /// Models the strategy considered but rejected, with reasons.
    pub excluded: Vec<Exclusion>,
}

/// The full record of a routing decision: chosen model, fallbacks, everything
/// considered, and everything excluded (with reasons). Returned by
/// [`ModelRouter::resolve`](super::ModelRouter::resolve) and reported to the
/// router's observer.
#[derive(Debug, Clone)]
pub struct RouteDecision {
    /// Which strategy produced the ranking.
    pub strategy: String,
    /// The chosen (primary) candidate.
    pub chosen: Candidate,
    /// The ordered fallbacks.
    pub fallbacks: Vec<Candidate>,
    /// Everything the strategy ranked (best-first).
    pub considered: Vec<Candidate>,
    /// Models excluded during filtering, with reasons.
    pub excluded: Vec<Exclusion>,
    /// Whether the task was pinned (routing bypassed).
    pub pinned: bool,
    /// Whether the primary was chosen because the task's sticky key was last
    /// served by it.
    pub sticky: bool,
}

/// What happened to one candidate while a route was executed.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// The candidate served the call.
    Succeeded,
    /// The candidate failed in a way another model may not: the router moved
    /// on to the next candidate.
    FailedOver,
    /// The candidate failed in a way no other model would fix (a caller
    /// error, a timeout after the request was sent): routing stopped.
    Failed,
    /// Not attempted: the candidate's circuit breaker is open.
    SkippedCircuitOpen,
    /// Not attempted: the candidate's provider is not registered.
    SkippedUnknownProvider,
    /// The task's deadline passed while the candidate was running.
    DeadlineExceeded,
}

/// One entry of a [`RouteReport`]: a candidate and what happened to it.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct RouteAttempt {
    /// The provider name.
    pub provider: String,
    /// The model name.
    pub model: String,
    /// What happened.
    pub outcome: AttemptOutcome,
    /// How long the attempt took (zero for skipped candidates).
    pub latency: Duration,
    /// The failure, as text, for attempts that failed.
    pub error: Option<String>,
}

/// What a routed call actually did: the decision, every attempt, and the
/// estimated versus actual cost.
///
/// Returned by [`DefaultRouter::route_with_report`](super::DefaultRouter::route_with_report)
/// and handed to the router's
/// [`GenAiObserver::on_route`](crate::genai::GenAiObserver::on_route).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RouteReport {
    /// The routing decision the execution started from.
    pub decision: RouteDecision,
    /// Every candidate that was tried or skipped, in order.
    pub attempts: Vec<RouteAttempt>,
    /// The candidate that served the call, if one did.
    pub served_by: Option<Candidate>,
    /// The pre-call cost estimate of the serving candidate (of the chosen one
    /// when the call failed).
    pub estimated_cost_usd: f64,
    /// The cost computed from the usage the provider reported and the
    /// registry's prices. `None` when the provider reported no usage. For a
    /// streamed call it is known only once the stream has ended.
    pub actual_cost_usd: Option<f64>,
    /// The usage the provider reported.
    pub usage: Option<Usage>,
    /// Wall-clock time of the whole routed call.
    pub latency: Duration,
    /// `None` on success; otherwise a short label for the failure
    /// (`"all_routes_failed"`, `"circuit_open"`, `"budget_exceeded"`,
    /// `"deadline_exceeded"`, or the provider error's kind).
    pub error_kind: Option<&'static str>,
    /// The budget key the call was charged to, if any.
    pub budget_key: Option<String>,
}
