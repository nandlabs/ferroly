//! The [`RoutingStrategy`] trait and the rule-based strategy.

use ferroly::genai::Capability;

use super::capability::ModelInfo;
use super::registry::ModelRegistry;
use super::route::{Candidate, Ranking};
use super::task::{Task, TaskKind};

/// Ranks models for a task against the registry.
pub trait RoutingStrategy: Send + Sync {
    /// A short identifier (appears in the decision trace).
    fn name(&self) -> &str;
    /// Ranks candidates best-first. An empty [`Ranking::candidates`] abstains,
    /// letting a composite strategy move on.
    fn rank(&self, task: &Task, reg: &dyn ModelRegistry) -> Ranking;
}

/// The default estimate of output tokens for a task kind, used for cost ranking
/// when `task.est_output_tokens` is 0.
pub fn default_output_estimate(kind: Option<TaskKind>) -> u32 {
    match kind {
        Some(TaskKind::Classify) => 64,
        Some(TaskKind::Extract) => 256,
        Some(TaskKind::Chat) | Some(TaskKind::Summarize) => 512,
        Some(TaskKind::CodeGen) => 1024,
        Some(TaskKind::Reason) => 2048,
        None => 512,
    }
}

/// Whether a model advertises every required capability.
pub fn satisfies(m: &ModelInfo, required: &[Capability]) -> bool {
    required.iter().all(|c| m.has(*c))
}

// ---- rule-based ----------------------------------------------------------

struct Rule {
    kind: TaskKind,
    provider: String,
    model: String,
}

/// Maps `TaskKind` to an ordered list of `(provider, model)` targets; the first
/// matching rule is the primary and the remaining same-kind rules (plus the
/// default) become the fallback tail.
#[derive(Default)]
pub struct RuleBasedStrategy {
    rules: Vec<Rule>,
    default: Option<(String, String)>,
}

impl RuleBasedStrategy {
    /// A new, empty rule set.
    pub fn new() -> Self {
        RuleBasedStrategy::default()
    }

    /// Adds a `(kind → provider/model)` rule. Order matters: earlier rules for a
    /// kind rank ahead of later ones.
    pub fn rule(
        mut self,
        kind: TaskKind,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        self.rules.push(Rule {
            kind,
            provider: provider.into(),
            model: model.into(),
        });
        self
    }

    /// Sets the fallback used when no rule matches (or as the final fallback).
    pub fn default_model(mut self, provider: impl Into<String>, model: impl Into<String>) -> Self {
        self.default = Some((provider.into(), model.into()));
        self
    }
}

impl RoutingStrategy for RuleBasedStrategy {
    fn name(&self) -> &str {
        "rule_based"
    }

    fn rank(&self, task: &Task, reg: &dyn ModelRegistry) -> Ranking {
        let mut candidates = Vec::new();
        if let Some(kind) = task.kind {
            for (i, r) in self.rules.iter().filter(|r| r.kind == kind).enumerate() {
                candidates.push(make_candidate(
                    reg,
                    &r.provider,
                    &r.model,
                    task,
                    -(i as f64),
                ));
            }
        }
        if let Some((p, m)) = &self.default {
            // The default ranks last (below any matched rule).
            let score = -(candidates.len() as f64) - 1.0;
            candidates.push(make_candidate(reg, p, m, task, score));
        }
        Ranking {
            candidates,
            excluded: Vec::new(),
        }
    }
}

fn make_candidate(
    reg: &dyn ModelRegistry,
    provider: &str,
    model: &str,
    task: &Task,
    score: f64,
) -> Candidate {
    let est_cost = reg
        .get(provider, model)
        .map(|m| {
            let out = if task.est_output_tokens > 0 {
                task.est_output_tokens
            } else {
                default_output_estimate(task.kind)
            };
            m.est_cost(task.est_input_tokens, out)
        })
        .unwrap_or(0.0);
    Candidate {
        provider: provider.to_string(),
        model: model.to_string(),
        score,
        est_cost,
    }
}
