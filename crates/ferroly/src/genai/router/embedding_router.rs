//! A parallel router for embeddings, sharing the registry and strategies.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use ferroly::genai::{BoxFuture, Capability, EmbedRequest, EmbedResponse, Embedder, GenAiError};

use super::execute::is_retryable;
use super::registry::ModelRegistry;
use super::route::{Candidate, Route};
use super::strategy::RoutingStrategy;
use super::{RouterError, Task};

/// Routes embedding calls across providers, sharing the model registry with the
/// completion [`ModelRouter`](super::ModelRouter). It filters to
/// `Capability::Embeddings`, ranks with the given strategy, and dispatches to
/// the matching [`Embedder`], failing over on transient errors.
pub struct EmbeddingRouter {
    embedders: HashMap<String, Arc<dyn Embedder>>,
    registry: RwLock<Arc<dyn ModelRegistry>>,
    strategy: Box<dyn RoutingStrategy>,
    fallback_depth: usize,
}

impl EmbeddingRouter {
    /// Builds an embedding router from named embedders, a shared registry, and a
    /// ranking strategy.
    pub fn new(
        embedders: Vec<(String, Arc<dyn Embedder>)>,
        registry: Arc<dyn ModelRegistry>,
        strategy: Box<dyn RoutingStrategy>,
    ) -> Self {
        EmbeddingRouter {
            embedders: embedders.into_iter().collect(),
            registry: RwLock::new(registry),
            strategy,
            fallback_depth: 2,
        }
    }

    /// Sets how many ranked tail entries become fallbacks (default 2).
    pub fn fallback_depth(mut self, depth: usize) -> Self {
        self.fallback_depth = depth;
        self
    }

    /// Replaces the registry atomically.
    pub fn reload(&self, registry: Arc<dyn ModelRegistry>) {
        *self.registry.write().expect("registry lock") = registry;
    }

    /// Resolves the embedding plan without executing (adds `Embeddings` to the
    /// required capabilities).
    pub fn resolve(&self, task: &Task) -> Result<Route, RouterError> {
        let mut task = task.clone();
        if !task.required.contains(&Capability::Embeddings) {
            task.required.push(Capability::Embeddings);
        }
        let reg = self.registry.read().expect("registry lock").clone();
        let ranking = self.strategy.rank(&task, reg.as_ref());
        if ranking.candidates.is_empty() {
            return Err(RouterError::NoRoute {
                reason: format!(
                    "no embedding model satisfied the task (strategy: {})",
                    self.strategy.name()
                ),
            });
        }
        let mut iter = ranking.candidates.into_iter();
        let primary = iter.next().expect("non-empty");
        let fallbacks: Vec<Candidate> = iter.take(self.fallback_depth).collect();
        Ok(Route { primary, fallbacks })
    }

    /// Routes and runs an embedding request with fallback.
    pub fn embed(
        &self,
        task: &Task,
        request: EmbedRequest,
    ) -> BoxFuture<'_, Result<EmbedResponse, RouterError>> {
        let route = self.resolve(task);
        Box::pin(async move {
            let route = route?;
            let mut last_err: Option<GenAiError> = None;
            let mut tried = 0usize;
            for c in route.attempts() {
                let embedder = match self.embedders.get(&c.provider) {
                    Some(e) => e.clone(),
                    None => continue,
                };
                let mut req = request.clone();
                req.model = c.model.clone();
                tried += 1;
                match embedder.embed(req).await {
                    Ok(resp) => return Ok(resp),
                    Err(e) => {
                        let retry = is_retryable(&e, None);
                        last_err = Some(e);
                        if !retry {
                            return Err(RouterError::GenAi(last_err.expect("just set")));
                        }
                    }
                }
            }
            match last_err {
                Some(e) => Err(RouterError::AllRoutesFailed {
                    attempts: tried,
                    last: e,
                }),
                None => Err(RouterError::NoRoute {
                    reason: "no attemptable embedder".to_string(),
                }),
            }
        })
    }
}
