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
    kind: Option<TaskKind>,
    tags: Vec<String>,
    provider: String,
    model: String,
}

impl Rule {
    fn matches(&self, task: &Task) -> bool {
        if let Some(kind) = self.kind {
            if task.kind != Some(kind) {
                return false;
            }
        }
        self.tags.iter().all(|t| task.tags.contains(t))
    }
}

/// Maps tasks to an ordered list of `(provider, model)` targets by rule.
///
/// A rule matches a task when the task has the rule's [`TaskKind`] (if the
/// rule names one) **and** carries every tag the rule requires. The matching
/// rules, in the order they were added, become the primary and its fallbacks;
/// the default model, if set, is the final fallback. With no matching rule
/// and no default the strategy abstains.
///
/// ```
/// use ferroly::genai::router::{RuleBasedStrategy, TaskKind};
///
/// let rules = RuleBasedStrategy::new()
///     // Tagged tasks stay on the local model, whatever their kind.
///     .tag_rule(["sensitive"], "ollama", "llama3")
///     .rule(TaskKind::CodeGen, "claude", "claude-opus-5-5")
///     .default_model("openai", "gpt-4o-mini");
/// # let _ = rules;
/// ```
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

    /// Adds a `(kind → provider/model)` rule. Order matters: earlier rules
    /// rank ahead of later ones.
    pub fn rule(
        self,
        kind: TaskKind,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        self.rule_for(Some(kind), Vec::<String>::new(), provider, model)
    }

    /// Adds a rule matching every task that carries all of `tags`, whatever
    /// its kind.
    pub fn tag_rule<I, S>(
        self,
        tags: I,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.rule_for(None, tags, provider, model)
    }

    /// Adds a rule with an optional kind and a set of required tags. A rule
    /// with neither matches every task.
    pub fn rule_for<I, S>(
        mut self,
        kind: Option<TaskKind>,
        tags: I,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.rules.push(Rule {
            kind,
            tags: tags.into_iter().map(Into::into).collect(),
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
        let mut candidates: Vec<Candidate> = Vec::new();
        let push = |provider: &str, model: &str, candidates: &mut Vec<Candidate>| {
            if candidates
                .iter()
                .any(|c| c.provider == provider && c.model == model)
            {
                return; // already ranked by an earlier rule
            }
            let score = -(candidates.len() as f64);
            candidates.push(make_candidate(reg, provider, model, task, score));
        };
        for r in self.rules.iter().filter(|r| r.matches(task)) {
            push(&r.provider, &r.model, &mut candidates);
        }
        if let Some((p, m)) = &self.default {
            // The default ranks last (below any matched rule).
            push(p, m, &mut candidates);
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
        .map(|m| m.est_cost(task.est_input_tokens, task.output_estimate()))
        .unwrap_or(0.0);
    Candidate {
        provider: provider.to_string(),
        model: model.to_string(),
        score,
        est_cost,
    }
}
