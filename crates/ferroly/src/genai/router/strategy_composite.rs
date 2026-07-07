//! The composite strategy: try sub-strategies in order, first non-empty wins.

use super::registry::ModelRegistry;
use super::route::Ranking;
use super::strategy::RoutingStrategy;
use super::task::Task;

/// Tries its sub-strategies in order and returns the first non-empty ranking.
/// Lets you layer policy, e.g. a sensitive-tag strategy, then capability, then
/// rules.
pub struct CompositeStrategy {
    strategies: Vec<Box<dyn RoutingStrategy>>,
}

impl CompositeStrategy {
    /// Builds a composite from ordered sub-strategies.
    pub fn new(strategies: Vec<Box<dyn RoutingStrategy>>) -> Self {
        CompositeStrategy { strategies }
    }
}

impl RoutingStrategy for CompositeStrategy {
    fn name(&self) -> &str {
        "composite"
    }

    fn rank(&self, task: &Task, reg: &dyn ModelRegistry) -> Ranking {
        for s in &self.strategies {
            let ranking = s.rank(task, reg);
            if !ranking.candidates.is_empty() {
                return ranking;
            }
        }
        Ranking::default()
    }
}
