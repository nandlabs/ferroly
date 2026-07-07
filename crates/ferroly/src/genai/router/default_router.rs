//! The [`ModelRouter`] trait and its default implementation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use ferroly::clients::{CircuitBreaker, CircuitBreakerConfig, RetryPolicy};
use ferroly::genai::{
    BoxFuture, ChunkStream, CompletionRequest, CompletionResponse, GenAiError, GenAiProvider,
};

use super::execute::{clamp_options, is_retryable};
use super::provider_set::ProviderSet;
use super::registry::ModelRegistry;
use super::route::{Candidate, Route, RouteDecision};
use super::strategy::{default_output_estimate, RoutingStrategy};
use super::strategy_capability::CapabilityStrategy;
use super::{Ranking, RouterError, Task};

/// Selects a model for a task, executes with fallback, and returns the response.
pub trait ModelRouter: Send + Sync {
    /// Routes and runs a non-streaming completion.
    fn route(
        &self,
        task: &Task,
        request: CompletionRequest,
    ) -> BoxFuture<'_, Result<CompletionResponse, RouterError>>;

    /// Routes and runs a streaming completion. Fallover happens only before the
    /// stream is established (pre-first-token); a mid-stream failure surfaces to
    /// the caller through the returned channel.
    fn route_stream(
        &self,
        task: &Task,
        request: CompletionRequest,
    ) -> BoxFuture<'_, Result<ChunkStream, RouterError>>;

    /// Returns the plan and full decision trace **without** executing.
    fn resolve(&self, task: &Task) -> Result<(Route, RouteDecision), RouterError>;
}

/// The default router: strategy selection + option clamping + error-classified,
/// deadline-aware fallback composed with `clients` retry / circuit breaking.
pub struct DefaultRouter {
    providers: ProviderSet,
    registry: RwLock<Arc<dyn ModelRegistry>>,
    strategy: Box<dyn RoutingStrategy>,
    retry: Option<RetryPolicy>,
    breaker_cfg: Option<CircuitBreakerConfig>,
    breakers: Mutex<HashMap<String, Arc<CircuitBreaker>>>,
    fallback_depth: usize,
}

impl DefaultRouter {
    /// Replaces the registry atomically (hot-reload). In-flight routes keep the
    /// snapshot they started with.
    pub fn reload(&self, registry: Arc<dyn ModelRegistry>) {
        *self.registry.write().expect("registry lock") = registry;
    }

    fn snapshot(&self) -> Arc<dyn ModelRegistry> {
        self.registry.read().expect("registry lock").clone()
    }

    /// The circuit breaker for a candidate, created lazily. `None` when no
    /// breaker is configured.
    fn breaker_for(&self, c: &Candidate) -> Option<Arc<CircuitBreaker>> {
        let cfg = self.breaker_cfg.as_ref()?;
        let key = format!("{}/{}", c.provider, c.model);
        let mut map = self.breakers.lock().expect("breaker map lock");
        Some(
            map.entry(key)
                .or_insert_with(|| Arc::new(CircuitBreaker::new(cfg.clone())))
                .clone(),
        )
    }

    /// Remaining time before the deadline, or `Err` if already past it.
    fn remaining(&self, task: &Task) -> Result<Option<Duration>, RouterError> {
        match task.deadline {
            Some(dl) => {
                let now = Instant::now();
                if now >= dl {
                    Err(RouterError::DeadlineExceeded)
                } else {
                    Ok(Some(dl - now))
                }
            }
            None => Ok(None),
        }
    }

    async fn call_with_retry(
        &self,
        provider: &Arc<dyn GenAiProvider>,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, GenAiError> {
        match &self.retry {
            Some(policy) => {
                ferroly::clients::retry(
                    policy,
                    |e: &GenAiError| is_retryable(e, None),
                    || {
                        let p = provider.clone();
                        let r = request.clone();
                        async move { p.complete(r).await }
                    },
                )
                .await
            }
            None => provider.complete(request).await,
        }
    }

    async fn execute(
        &self,
        route: &Route,
        task: &Task,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, RouterError> {
        let reg = self.snapshot();
        let mut last_err: Option<GenAiError> = None;
        let mut tried = 0usize;

        for c in route.attempts() {
            let remaining = self.remaining(task)?;
            let provider = match self.providers.get(&c.provider) {
                Some(p) => p,
                None => continue, // unknown provider → skip to next candidate
            };
            if let Some(b) = self.breaker_for(c) {
                if b.can_execute().is_err() {
                    continue; // breaker open → straight to next model
                }
            }

            let mut req = request.clone();
            req.model = c.model.clone();
            if let Some(info) = reg.get(&c.provider, &c.model) {
                req = clamp_options(req, &info, task);
            }
            tried += 1;

            let call = self.call_with_retry(&provider, req);
            let result = match remaining {
                Some(dur) => match tokio::time::timeout(dur, call).await {
                    Ok(r) => r,
                    Err(_) => {
                        if let Some(b) = self.breaker_for(c) {
                            b.on_execution(false);
                        }
                        return Err(RouterError::DeadlineExceeded);
                    }
                },
                None => call.await,
            };
            if let Some(b) = self.breaker_for(c) {
                b.on_execution(result.is_ok());
            }
            match result {
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
                reason: "no attemptable candidate (providers unregistered)".to_string(),
            }),
        }
    }

    async fn execute_stream(
        &self,
        route: &Route,
        task: &Task,
        request: CompletionRequest,
    ) -> Result<ChunkStream, RouterError> {
        let reg = self.snapshot();
        let mut last_err: Option<GenAiError> = None;
        let mut tried = 0usize;

        for c in route.attempts() {
            let remaining = self.remaining(task)?;
            let provider = match self.providers.get(&c.provider) {
                Some(p) => p,
                None => continue,
            };
            if let Some(b) = self.breaker_for(c) {
                if b.can_execute().is_err() {
                    continue;
                }
            }

            let mut req = request.clone();
            req.model = c.model.clone();
            if let Some(info) = reg.get(&c.provider, &c.model) {
                req = clamp_options(req, &info, task);
            }
            tried += 1;

            let call = provider.complete_stream(req);
            let result = match remaining {
                Some(dur) => match tokio::time::timeout(dur, call).await {
                    Ok(r) => r,
                    Err(_) => return Err(RouterError::DeadlineExceeded),
                },
                None => call.await,
            };
            if let Some(b) = self.breaker_for(c) {
                b.on_execution(result.is_ok());
            }
            match result {
                Ok(stream) => return Ok(stream), // established → surface to caller
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
                reason: "no attemptable candidate (providers unregistered)".to_string(),
            }),
        }
    }

    fn resolve_inner(&self, task: &Task) -> Result<(Route, RouteDecision), RouterError> {
        let reg = self.snapshot();

        if let Some(pin) = &task.pin {
            let est_cost = reg
                .get(&pin.provider, &pin.model)
                .map(|m| {
                    let out = if task.est_output_tokens > 0 {
                        task.est_output_tokens
                    } else {
                        default_output_estimate(task.kind)
                    };
                    m.est_cost(task.est_input_tokens, out)
                })
                .unwrap_or(0.0);
            let chosen = Candidate {
                provider: pin.provider.clone(),
                model: pin.model.clone(),
                score: 0.0,
                est_cost,
            };
            let decision = RouteDecision {
                strategy: "pin".to_string(),
                chosen: chosen.clone(),
                fallbacks: Vec::new(),
                considered: vec![chosen.clone()],
                excluded: Vec::new(),
                pinned: true,
            };
            return Ok((
                Route {
                    primary: chosen,
                    fallbacks: Vec::new(),
                },
                decision,
            ));
        }

        let Ranking {
            candidates,
            excluded,
        } = self.strategy.rank(task, reg.as_ref());
        if candidates.is_empty() {
            return Err(RouterError::NoRoute {
                reason: format!(
                    "no model satisfied the task (strategy: {})",
                    self.strategy.name()
                ),
            });
        }

        let considered = candidates.clone();
        let mut iter = candidates.into_iter();
        let primary = iter.next().expect("non-empty");
        let fallbacks: Vec<Candidate> = iter.take(self.fallback_depth).collect();

        let decision = RouteDecision {
            strategy: self.strategy.name().to_string(),
            chosen: primary.clone(),
            fallbacks: fallbacks.clone(),
            considered,
            excluded,
            pinned: false,
        };
        Ok((Route { primary, fallbacks }, decision))
    }

    /// Emits the decision to the structured log.
    fn log_decision(&self, decision: &RouteDecision) {
        super::observe::log_decision(decision);
    }
}

impl ModelRouter for DefaultRouter {
    fn route(
        &self,
        task: &Task,
        request: CompletionRequest,
    ) -> BoxFuture<'_, Result<CompletionResponse, RouterError>> {
        let resolved = self.resolve_inner(task);
        let task = task.clone();
        Box::pin(async move {
            let (route, decision) = resolved?;
            self.log_decision(&decision);
            self.execute(&route, &task, request).await
        })
    }

    fn route_stream(
        &self,
        task: &Task,
        request: CompletionRequest,
    ) -> BoxFuture<'_, Result<ChunkStream, RouterError>> {
        let resolved = self.resolve_inner(task);
        let task = task.clone();
        Box::pin(async move {
            let (route, decision) = resolved?;
            self.log_decision(&decision);
            self.execute_stream(&route, &task, request).await
        })
    }

    fn resolve(&self, task: &Task) -> Result<(Route, RouteDecision), RouterError> {
        self.resolve_inner(task)
    }
}

/// Builds a [`DefaultRouter`].
pub struct RouterBuilder {
    providers: ProviderSet,
    registry: Arc<dyn ModelRegistry>,
    strategy: Option<Box<dyn RoutingStrategy>>,
    retry: Option<RetryPolicy>,
    breaker_cfg: Option<CircuitBreakerConfig>,
    fallback_depth: usize,
}

impl RouterBuilder {
    /// Starts a builder over an explicit provider set and a built registry.
    pub fn new(providers: ProviderSet, registry: Arc<dyn ModelRegistry>) -> Self {
        RouterBuilder {
            providers,
            registry,
            strategy: None,
            retry: None,
            breaker_cfg: None,
            fallback_depth: 2,
        }
    }

    /// Sets the routing strategy (default: [`CapabilityStrategy`]).
    pub fn strategy(mut self, strategy: Box<dyn RoutingStrategy>) -> Self {
        self.strategy = Some(strategy);
        self
    }

    /// Sets the same-model retry policy.
    pub fn retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry = Some(policy);
        self
    }

    /// Enables per-`(provider, model)` circuit breaking with this config.
    pub fn circuit_breaker(mut self, config: CircuitBreakerConfig) -> Self {
        self.breaker_cfg = Some(config);
        self
    }

    /// Sets how many ranked tail entries become fallbacks (default 2).
    pub fn fallback_depth(mut self, depth: usize) -> Self {
        self.fallback_depth = depth;
        self
    }

    /// Builds the router.
    pub fn build(self) -> DefaultRouter {
        DefaultRouter {
            providers: self.providers,
            registry: RwLock::new(self.registry),
            strategy: self
                .strategy
                .unwrap_or_else(|| Box::new(CapabilityStrategy)),
            retry: self.retry,
            breaker_cfg: self.breaker_cfg,
            breakers: Mutex::new(HashMap::new()),
            fallback_depth: self.fallback_depth,
        }
    }
}
