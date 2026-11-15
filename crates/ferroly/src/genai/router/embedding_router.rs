//! A parallel router for embeddings, sharing the registry and strategies.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use ferroly::genai::{BoxFuture, Capability, EmbedRequest, EmbedResponse, Embedder, GenAiError};

use super::execute::classify;
use super::registry::ModelRegistry;
use super::route::{Candidate, Route};
use super::strategy::RoutingStrategy;
use super::tenant::{merge_tenants, policy_for, TenantPolicy};
use super::{RouterError, RoutingConfig, Task};

/// Routes embedding calls across providers, sharing the model registry with the
/// completion [`ModelRouter`](super::ModelRouter). It filters to
/// `Capability::Embeddings`, ranks with the given strategy, and dispatches to
/// the matching [`Embedder`], failing over on transient errors.
///
/// A task's [`tenant`](Task::tenant) is honoured as by the completion
/// router: the tenant's [`TenantPolicy`] (registered with
/// [`tenant_policy`](Self::tenant_policy) or
/// [`tenant_policies_from`](Self::tenant_policies_from)) narrows the
/// eligible models, primary and fallbacks alike, and may set the strategy
/// and priority.
pub struct EmbeddingRouter {
    embedders: HashMap<String, Arc<dyn Embedder>>,
    registry: RwLock<Arc<dyn ModelRegistry>>,
    strategy: Box<dyn RoutingStrategy>,
    fallback_depth: usize,
    tenants: HashMap<String, TenantPolicy>,
    require_tenant_policy: bool,
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
            tenants: HashMap::new(),
            require_tenant_policy: false,
        }
    }

    /// Sets how many ranked tail entries become fallbacks (default 2).
    pub fn fallback_depth(mut self, depth: usize) -> Self {
        self.fallback_depth = depth;
        self
    }

    /// Registers the routing policy for `tenant` (see [`TenantPolicy`]),
    /// replacing any earlier one of that name.
    pub fn tenant_policy(mut self, tenant: impl Into<String>, policy: TenantPolicy) -> Self {
        self.tenants.insert(tenant.into(), policy);
        self
    }

    /// Applies the `tenants` policies of a routing configuration, laid over
    /// the policies already registered exactly as
    /// [`RouterBuilder::from_config`](super::RouterBuilder::from_config)
    /// does, so one configuration can govern both routers.
    pub fn tenant_policies_from(mut self, config: &RoutingConfig) -> Result<Self, RouterError> {
        self.tenants = merge_tenants(&self.tenants, config.tenant_policies()?);
        Ok(self)
    }

    /// Whether a task naming a tenant that has no policy is refused with
    /// [`RouterError::NoRoute`] (default `false`; see
    /// [`RouterBuilder::require_tenant_policy`](super::RouterBuilder::require_tenant_policy)).
    pub fn require_tenant_policy(mut self, require: bool) -> Self {
        self.require_tenant_policy = require;
        self
    }

    /// Replaces the registry atomically.
    pub fn reload(&self, registry: Arc<dyn ModelRegistry>) {
        *self.registry.write().expect("registry lock") = registry;
    }

    /// Resolves the embedding plan without executing (adds `Embeddings` to the
    /// required capabilities). The task's tenant policy, if any, applies.
    pub fn resolve(&self, task: &Task) -> Result<Route, RouterError> {
        let policy = policy_for(
            &self.tenants,
            task.tenant.as_deref(),
            self.require_tenant_policy,
        )?;
        let mut task = task.clone();
        if !task.required.contains(&Capability::Embeddings) {
            task.required.push(Capability::Embeddings);
        }
        if let Some(priority) = policy.and_then(TenantPolicy::priority_override) {
            task.priority = priority;
        }
        let strategy: &dyn RoutingStrategy = match policy.and_then(|p| p.strategy_override()) {
            Some(strategy) => strategy,
            None => self.strategy.as_ref(),
        };
        let reg = self.registry.read().expect("registry lock").clone();
        let mut ranking = strategy.rank(&task, reg.as_ref());
        // Whatever the strategy ranked, only a model the registry knows to
        // embed (or does not know at all) can serve an embedding call.
        ranking
            .candidates
            .retain(|c| match reg.get(&c.provider, &c.model) {
                Some(info) => info.has(Capability::Embeddings),
                None => true,
            });
        // A tenant policy narrows whatever the strategy ranked, before the
        // fallbacks are taken.
        if let Some(policy) = policy {
            ranking
                .candidates
                .retain(|c| policy.permits(&c.provider, &c.model));
        }
        if ranking.candidates.is_empty() {
            return Err(RouterError::NoRoute {
                reason: match (policy, &task.tenant) {
                    (Some(_), Some(tenant)) => format!(
                        "no embedding model allowed for tenant '{tenant}' satisfied the task \
                         (strategy: {})",
                        strategy.name()
                    ),
                    _ => format!(
                        "no embedding model satisfied the task (strategy: {})",
                        strategy.name()
                    ),
                },
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
                        // Embedding is idempotent, so a timeout is failed over
                        // like any other transient error.
                        if !classify(&e, true, true).fallover {
                            return Err(RouterError::GenAi(e));
                        }
                        last_err = Some(e);
                    }
                }
            }
            match last_err {
                Some(e) => Err(RouterError::AllRoutesFailed {
                    attempts: tried,
                    last: e,
                }),
                None => Err(RouterError::NoRoute {
                    reason: "no candidate could be attempted: no embedder is registered for the \
                             ranked provider(s)"
                        .to_string(),
                }),
            }
        })
    }
}
