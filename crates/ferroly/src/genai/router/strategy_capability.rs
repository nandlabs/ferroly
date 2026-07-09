//! The capability strategy: filter by capability/limits/budget, rank by priority.

use super::capability::ModelInfo;
use super::registry::ModelRegistry;
use super::route::{Candidate, Exclusion, Ranking};
use super::strategy::{default_output_estimate, satisfies, RoutingStrategy};
use super::task::{Priority, Task};

/// Filters models to those satisfying the task's required capabilities, input
/// limit, and budget, then ranks the survivors by [`Priority`]. Returns the
/// whole ranked list, so the router can use the tail as fallbacks. Rejected
/// models are recorded in [`Ranking::excluded`] with a reason.
#[derive(Default)]
pub struct CapabilityStrategy;

impl RoutingStrategy for CapabilityStrategy {
    fn name(&self) -> &str {
        "capability"
    }

    fn rank(&self, task: &Task, reg: &dyn ModelRegistry) -> Ranking {
        let out = if task.est_output_tokens > 0 {
            task.est_output_tokens
        } else {
            default_output_estimate(task.kind)
        };

        let mut candidates = Vec::new();
        let mut excluded = Vec::new();

        for m in reg.all() {
            if let Some(reason) = reject_reason(&m, task, out) {
                excluded.push(Exclusion {
                    provider: m.provider,
                    model: m.name,
                    reason,
                });
                continue;
            }
            let est_cost = m.est_cost(task.est_input_tokens, out);
            candidates.push(Candidate {
                score: score(&m, task.priority, est_cost),
                est_cost,
                provider: m.provider,
                model: m.name,
            });
        }

        // Best-first; break ties deterministically by (provider, model).
        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.provider.cmp(&b.provider))
                .then_with(|| a.model.cmp(&b.model))
        });

        Ranking {
            candidates,
            excluded,
        }
    }
}

/// Why a model fails the filter, or `None` if it passes.
fn reject_reason(m: &ModelInfo, task: &Task, out_est: u32) -> Option<String> {
    if !satisfies(m, &task.required) {
        let missing: Vec<String> = task
            .required
            .iter()
            .filter(|c| !m.has(**c))
            .map(|c| format!("{c:?}"))
            .collect();
        return Some(format!("missing capability: {}", missing.join(", ")));
    }
    if task.est_input_tokens > 0
        && m.max_input_tokens > 0
        && task.est_input_tokens > m.max_input_tokens
    {
        return Some(format!(
            "input {} > max_input_tokens {}",
            task.est_input_tokens, m.max_input_tokens
        ));
    }
    if task.max_cost_usd > 0.0 && m.est_cost(task.est_input_tokens, out_est) > task.max_cost_usd {
        return Some(format!(
            "est cost {:.4} > budget {:.4}",
            m.est_cost(task.est_input_tokens, out_est),
            task.max_cost_usd
        ));
    }
    None
}

/// Higher is better. Cost → cheapest; Speed → smallest context (proxy for a
/// faster/smaller tier); Quality → largest context.
fn score(m: &ModelInfo, priority: Priority, est_cost: f64) -> f64 {
    match priority {
        Priority::Cost => -est_cost,
        Priority::Speed => -(m.max_input_tokens as f64),
        Priority::Quality => m.max_input_tokens as f64,
    }
}
