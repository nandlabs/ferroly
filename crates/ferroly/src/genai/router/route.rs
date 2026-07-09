//! The executable plan ([`Route`]), scored options ([`Candidate`]), and the
//! decision trace ([`RouteDecision`]).

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
/// [`ModelRouter::resolve`](super::ModelRouter::resolve) and logged by `route`.
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
}
