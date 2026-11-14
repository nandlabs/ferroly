//! The capability strategy: filter by capability/limits/budget, rank by priority.

use std::cmp::Ordering;

use super::capability::ModelInfo;
use super::registry::ModelRegistry;
use super::route::{Candidate, Exclusion, Ranking};
use super::strategy::{satisfies, RoutingStrategy};
use super::task::{Priority, Task};

/// Filters models to those satisfying the task's required capabilities, input
/// limit, and budget, then ranks the survivors by [`Priority`]. Returns the
/// whole ranked list, so the router can use the tail as fallbacks. Rejected
/// models are recorded in [`Ranking::excluded`] with a reason.
///
/// Ranking is deterministic — see [`Priority`] for the order and the
/// tie-breaks. `Quality` and `Speed` use the explicit
/// [`quality_rank`](ModelInfo::quality_rank) and
/// [`speed_rank`](ModelInfo::speed_rank) of each model; context size is only a
/// fallback among models that have no rank.
#[derive(Default)]
pub struct CapabilityStrategy;

impl RoutingStrategy for CapabilityStrategy {
    fn name(&self) -> &str {
        "capability"
    }

    fn rank(&self, task: &Task, reg: &dyn ModelRegistry) -> Ranking {
        let out = task.output_estimate();

        let mut ranked: Vec<(ModelInfo, f64)> = Vec::new();
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
            ranked.push((m, est_cost));
        }

        ranked.sort_by(|(a, a_cost), (b, b_cost)| compare(task.priority, a, *a_cost, b, *b_cost));

        let candidates = ranked
            .into_iter()
            .map(|(m, est_cost)| Candidate {
                score: score(&m, task.priority, est_cost),
                est_cost,
                provider: m.provider,
                model: m.name,
            })
            .collect();

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

/// Orders two ranks, highest first; a missing rank sorts last.
fn by_rank(a: Option<u8>, b: Option<u8>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn by_cost(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

fn by_name(a: &ModelInfo, b: &ModelInfo) -> Ordering {
    a.provider
        .cmp(&b.provider)
        .then_with(|| a.name.cmp(&b.name))
}

/// Best-first ordering for a priority (`Less` = `a` is preferred).
fn compare(priority: Priority, a: &ModelInfo, a_cost: f64, b: &ModelInfo, b_cost: f64) -> Ordering {
    match priority {
        Priority::Cost => by_cost(a_cost, b_cost)
            .then_with(|| by_rank(a.quality_rank, b.quality_rank))
            .then_with(|| by_name(a, b)),
        Priority::Quality => by_rank(a.quality_rank, b.quality_rank)
            .then_with(|| {
                // Fallback among unranked models only: a larger context is a
                // rough stand-in for a more capable tier.
                if a.quality_rank.is_none() && b.quality_rank.is_none() {
                    b.max_input_tokens.cmp(&a.max_input_tokens)
                } else {
                    Ordering::Equal
                }
            })
            .then_with(|| by_cost(a_cost, b_cost))
            .then_with(|| by_name(a, b)),
        Priority::Speed => by_rank(a.speed_rank, b.speed_rank)
            .then_with(|| {
                // Fallback among unranked models only: a smaller context is a
                // rough stand-in for a smaller, faster tier.
                if a.speed_rank.is_none() && b.speed_rank.is_none() {
                    a.max_input_tokens.cmp(&b.max_input_tokens)
                } else {
                    Ordering::Equal
                }
            })
            .then_with(|| by_cost(a_cost, b_cost))
            .then_with(|| by_name(a, b)),
    }
}

/// The score reported in the decision trace (higher is better). The ordering
/// itself is decided by [`compare`], which also applies the tie-breaks.
fn score(m: &ModelInfo, priority: Priority, est_cost: f64) -> f64 {
    match priority {
        Priority::Cost => -est_cost,
        Priority::Speed => m.speed_rank.map_or(-1.0, f64::from),
        Priority::Quality => m.quality_rank.map_or(-1.0, f64::from),
    }
}
