//! The [`ModelRouter`] trait and its default implementation.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use ferroly::clients::{
    CircuitBreakerConfig, CircuitBreakerRegistry, CircuitState, Permit, Retry, RetryPolicy,
};
use ferroly::genai::{
    BoxFuture, ChunkStream, CompletionChunk, CompletionRequest, CompletionResponse, GenAiError,
    GenAiObserver, GenAiProvider, Usage,
};

use super::budget::Budget;
use super::config::RoutingConfig;
use super::execute::{clamp_options, classify, validate_request};
use super::provider_set::ProviderSet;
use super::registry::{build_registry, ModelRegistry};
use super::route::{
    AttemptOutcome, Candidate, Exclusion, Route, RouteAttempt, RouteDecision, RouteReport,
};
use super::strategy::RoutingStrategy;
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

    /// Routes and runs a streaming completion.
    ///
    /// Fallover happens until the model has started answering: a candidate
    /// that cannot be reached, answers with an error, or whose stream fails
    /// before producing any part of the answer (the opening `Start` / `Usage`
    /// metadata does not count) is replaced by the next one. The returned
    /// future resolves when the first such event has arrived, and the task's
    /// deadline covers that wait.
    /// **Once part of the answer has been handed to the caller there is no
    /// fallover**: a later failure arrives as an `Err` item on the returned
    /// channel (the caller has already consumed part of the answer, and
    /// replaying it on another model would duplicate or contradict it). It
    /// still counts against the model's circuit breaker.
    fn route_stream(
        &self,
        task: &Task,
        request: CompletionRequest,
    ) -> BoxFuture<'_, Result<ChunkStream, RouterError>>;

    /// Returns the plan and full decision trace **without** executing.
    fn resolve(&self, task: &Task) -> Result<(Route, RouteDecision), RouterError>;
}

/// Remembers which model last served each sticky key, up to a capacity (the
/// oldest key is forgotten first).
struct Sticky {
    map: HashMap<String, (String, String)>,
    order: VecDeque<String>,
    capacity: usize,
}

impl Sticky {
    fn get(&self, key: &str) -> Option<(String, String)> {
        self.map.get(key).cloned()
    }

    fn put(&mut self, key: &str, provider: &str, model: &str) {
        if self.capacity == 0 {
            return;
        }
        if !self.map.contains_key(key) {
            self.order.push_back(key.to_string());
            while self.order.len() > self.capacity {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
        self.map
            .insert(key.to_string(), (provider.to_string(), model.to_string()));
    }
}

/// The pieces of the router that outlive a call — a spawned stream forwarder
/// needs them after `route_stream` has returned.
#[derive(Clone)]
struct Shared {
    observer: Option<Arc<dyn GenAiObserver>>,
    budget: Option<Budget>,
    sticky: Arc<Mutex<Sticky>>,
}

/// The default router: strategy selection + option clamping + error-classified,
/// deadline-aware fallback composed with `clients` retry / circuit breaking.
pub struct DefaultRouter {
    providers: ProviderSet,
    registry: RwLock<Arc<dyn ModelRegistry>>,
    strategy: Box<dyn RoutingStrategy>,
    retry: Option<Retry>,
    breakers: Option<Arc<CircuitBreakerRegistry>>,
    fallback_depth: usize,
    fallback_on_timeout: bool,
    shared: Shared,
}

/// Everything known about a routed call once it is over.
struct Finished {
    decision: RouteDecision,
    attempts: Vec<RouteAttempt>,
    served_by: Option<Candidate>,
    usage: Option<Usage>,
    started: Instant,
    error_kind: Option<&'static str>,
    task: Task,
}

impl Shared {
    /// Computes the actual cost, charges the budget, remembers the sticky
    /// model and notifies the observer.
    async fn finish(&self, registry: &dyn ModelRegistry, done: Finished) -> RouteReport {
        let Finished {
            decision,
            attempts,
            served_by,
            usage,
            started,
            error_kind,
            task,
        } = done;
        let estimated_cost_usd = served_by
            .as_ref()
            .map_or(decision.chosen.est_cost, |c| c.est_cost);
        let actual_cost_usd = match (&served_by, &usage) {
            (Some(c), Some(u)) => registry
                .get(&c.provider, &c.model)
                .and_then(|m| m.cost_for(u)),
            _ => None,
        };
        if let Some(served) = &served_by {
            if let (Some(budget), Some(key)) = (&self.budget, &task.budget_key) {
                // Charge what the call cost; fall back to the estimate when
                // the provider reported no usage, so spend is never missed.
                budget
                    .store()
                    .record(key, actual_cost_usd.unwrap_or(estimated_cost_usd))
                    .await;
            }
            if let Some(key) = &task.sticky_key {
                if let Ok(mut sticky) = self.sticky.lock() {
                    sticky.put(key, &served.provider, &served.model);
                }
            }
        }
        let report = RouteReport {
            decision,
            attempts,
            served_by,
            estimated_cost_usd,
            actual_cost_usd,
            usage,
            latency: started.elapsed(),
            error_kind,
            budget_key: task.budget_key,
        };
        match &self.observer {
            Some(observer) => observer.on_route(&report),
            None => {
                // Without an observer the decision is still written to the
                // structured log (at debug level) when logging is compiled in.
                #[cfg(feature = "log")]
                ferroly::genai::observe::log_route(&report, false);
            }
        }
        report
    }
}

/// The result of trying the candidates of a route.
enum Attempted<T> {
    /// A candidate produced `T`; its breaker permit is still open.
    Served {
        candidate: Candidate,
        value: T,
        permit: Option<Permit>,
    },
    Failed(RouterError),
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

    /// The state of every circuit breaker created so far, as
    /// `(provider/model, state)` sorted by key. Empty when circuit breaking is
    /// not enabled.
    pub fn breaker_states(&self) -> Vec<(String, CircuitState)> {
        self.breakers
            .as_ref()
            .map(|b| b.states())
            .unwrap_or_default()
    }

    /// Remaining time before the deadline, or `Err` if already past it.
    fn remaining(task: &Task) -> Result<Option<Duration>, RouterError> {
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

    /// Runs `op` under the same-model retry policy, if one is configured.
    async fn with_retry<T, F, Fut>(&self, op: F) -> Result<T, GenAiError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, GenAiError>>,
    {
        match &self.retry {
            Some(retry) => {
                let on_timeout = self.fallback_on_timeout;
                retry
                    .run_with(
                        |e: &GenAiError, _attempt| classify(e, false, on_timeout).retry,
                        op,
                    )
                    .await
            }
            None => {
                let mut op = op;
                op().await
            }
        }
    }

    /// Applies the task's cumulative budget to the plan: refuses the call
    /// when the key is spent, and drops candidates the remaining budget
    /// cannot cover.
    async fn apply_budget(&self, task: &Task, route: Route) -> Result<Route, RouterError> {
        let (Some(budget), Some(key)) = (&self.shared.budget, &task.budget_key) else {
            return Ok(route);
        };
        let Some(limit_usd) = budget.limit_for(key) else {
            return Ok(route);
        };
        let spent_usd = budget.store().spent(key).await;
        let exceeded = || RouterError::BudgetExceeded {
            key: key.clone(),
            limit_usd,
            spent_usd,
        };
        if spent_usd >= limit_usd {
            return Err(exceeded());
        }
        let remaining = limit_usd - spent_usd;
        let mut affordable = std::iter::once(route.primary)
            .chain(route.fallbacks)
            .filter(|c| c.est_cost <= remaining);
        let primary = affordable.next().ok_or_else(exceeded)?;
        Ok(Route {
            primary,
            fallbacks: affordable.collect(),
        })
    }

    /// Tries the candidates of `route` in order with `call`, handling circuit
    /// breakers, the deadline, same-model retry and error-classified
    /// fallover. `attempts` receives one entry per candidate.
    async fn attempt<T, C, Fut>(
        &self,
        route: &Route,
        task: &Task,
        request: &CompletionRequest,
        attempts: &mut Vec<RouteAttempt>,
        call: C,
    ) -> Attempted<T>
    where
        C: Fn(Arc<dyn GenAiProvider>, CompletionRequest) -> Fut,
        Fut: std::future::Future<Output = Result<T, GenAiError>>,
    {
        let reg = self.snapshot();
        let mut last_err: Option<GenAiError> = None;
        let mut tried = 0usize;
        let record = |attempts: &mut Vec<RouteAttempt>,
                      c: &Candidate,
                      outcome: AttemptOutcome,
                      latency: Duration,
                      error: Option<String>| {
            attempts.push(RouteAttempt {
                provider: c.provider.clone(),
                model: c.model.clone(),
                outcome,
                latency,
                error,
            });
        };

        for c in route.attempts() {
            let remaining = match Self::remaining(task) {
                Ok(r) => r,
                Err(e) => return Attempted::Failed(e),
            };
            let Some(provider) = self.providers.get(&c.provider) else {
                record(
                    attempts,
                    c,
                    AttemptOutcome::SkippedUnknownProvider,
                    Duration::ZERO,
                    None,
                );
                continue;
            };
            // An RAII permit: if this future is dropped (caller cancelled) or
            // returns early, the permit is released without counting, so a
            // half-open trial slot can never be leaked.
            let permit = match &self.breakers {
                Some(breakers) => match breakers.get(&c.key()).try_acquire() {
                    Ok(permit) => Some(permit),
                    Err(_) => {
                        record(
                            attempts,
                            c,
                            AttemptOutcome::SkippedCircuitOpen,
                            Duration::ZERO,
                            None,
                        );
                        continue;
                    }
                },
                None => None,
            };

            let mut req = request.clone();
            req.model = c.model.clone();
            if let Some(info) = reg.get(&c.provider, &c.model) {
                req = clamp_options(req, &info, task);
            }
            tried += 1;
            let started = Instant::now();

            let run = self.with_retry(|| call(provider.clone(), req.clone()));
            let result = match remaining {
                Some(limit) => match tokio::time::timeout(limit, run).await {
                    Ok(r) => r,
                    Err(_) => {
                        // The caller's deadline, not evidence about the model:
                        // the permit is dropped uncounted.
                        record(
                            attempts,
                            c,
                            AttemptOutcome::DeadlineExceeded,
                            started.elapsed(),
                            None,
                        );
                        return Attempted::Failed(RouterError::DeadlineExceeded);
                    }
                },
                None => run.await,
            };
            match result {
                Ok(value) => {
                    record(
                        attempts,
                        c,
                        AttemptOutcome::Succeeded,
                        started.elapsed(),
                        None,
                    );
                    return Attempted::Served {
                        candidate: c.clone(),
                        value,
                        permit,
                    };
                }
                Err(e) => {
                    let disposition = classify(&e, false, self.fallback_on_timeout);
                    if let Some(permit) = permit {
                        permit.record(disposition.outcome);
                    }
                    let outcome = if disposition.fallover {
                        AttemptOutcome::FailedOver
                    } else {
                        AttemptOutcome::Failed
                    };
                    record(attempts, c, outcome, started.elapsed(), Some(e.to_string()));
                    if !disposition.fallover {
                        return Attempted::Failed(RouterError::GenAi(e));
                    }
                    last_err = Some(e);
                }
            }
        }

        Attempted::Failed(match last_err {
            Some(last) => RouterError::AllRoutesFailed {
                attempts: tried,
                last,
            },
            None => {
                let open: Vec<String> = attempts
                    .iter()
                    .filter(|a| a.outcome == AttemptOutcome::SkippedCircuitOpen)
                    .map(|a| format!("{}/{}", a.provider, a.model))
                    .collect();
                if open.is_empty() {
                    let unknown: Vec<String> = attempts
                        .iter()
                        .map(|a| a.provider.clone())
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect();
                    RouterError::NoRoute {
                        reason: format!(
                            "no candidate could be attempted: provider(s) not registered: {}",
                            unknown.join(", ")
                        ),
                    }
                } else {
                    RouterError::CircuitOpen {
                        models: open.join(", "),
                    }
                }
            }
        })
    }

    /// Routes and runs a non-streaming completion, returning the response
    /// together with the [`RouteReport`] — which candidates were tried, which
    /// one served the call, and the estimated versus actual cost.
    pub async fn route_with_report(
        &self,
        task: &Task,
        request: CompletionRequest,
    ) -> Result<(CompletionResponse, RouteReport), RouterError> {
        let started = Instant::now();
        validate_request(task, &request)?;
        let (route, decision) = self.resolve_inner(task)?;
        let reg = self.snapshot();
        let mut attempts = Vec::new();

        let attempted = match self.apply_budget(task, route).await {
            Ok(route) => {
                self.attempt(
                    &route,
                    task,
                    &request,
                    &mut attempts,
                    |provider, req| async move { provider.complete(req).await },
                )
                .await
            }
            Err(e) => Attempted::Failed(e),
        };
        match attempted {
            Attempted::Served {
                candidate,
                value,
                permit,
            } => {
                if let Some(permit) = permit {
                    permit.success();
                }
                let report = self
                    .shared
                    .finish(
                        reg.as_ref(),
                        Finished {
                            decision,
                            attempts,
                            served_by: Some(candidate),
                            usage: value.usage.clone(),
                            started,
                            error_kind: None,
                            task: task.clone(),
                        },
                    )
                    .await;
                Ok((value, report))
            }
            Attempted::Failed(e) => {
                self.shared
                    .finish(
                        reg.as_ref(),
                        Finished {
                            decision,
                            attempts,
                            served_by: None,
                            usage: None,
                            started,
                            error_kind: Some(e.kind_label()),
                            task: task.clone(),
                        },
                    )
                    .await;
                Err(e)
            }
        }
    }

    async fn stream_inner(
        &self,
        task: &Task,
        request: CompletionRequest,
    ) -> Result<ChunkStream, RouterError> {
        let started = Instant::now();
        validate_request(task, &request)?;
        let (route, decision) = self.resolve_inner(task)?;
        let reg = self.snapshot();
        let mut attempts = Vec::new();

        let attempted = match self.apply_budget(task, route).await {
            Ok(route) => {
                // "Established" means the model started answering. The opening
                // metadata (`Start`, `Usage`) is held back until the first
                // event that carries part of the answer, so a stream that
                // fails before producing any of it — typically an overload
                // reported right after the stream opened — is still failed
                // over: the caller has seen nothing yet.
                self.attempt(
                    &route,
                    task,
                    &request,
                    &mut attempts,
                    |provider, req| async move {
                        let mut stream = provider.complete_stream(req).await?;
                        let mut opening: Vec<CompletionChunk> = Vec::new();
                        loop {
                            match stream.recv().await {
                                Some(Ok(chunk)) => {
                                    let metadata = matches!(
                                        chunk,
                                        CompletionChunk::Start { .. } | CompletionChunk::Usage(_)
                                    );
                                    opening.push(chunk);
                                    if !metadata {
                                        return Ok((opening, stream));
                                    }
                                }
                                Some(Err(e)) => return Err(e),
                                None if opening.is_empty() => {
                                    return Err(GenAiError::Transport(
                                        "the provider closed the stream without sending anything"
                                            .into(),
                                    ))
                                }
                                // Only metadata, then a clean end: an empty
                                // answer, which is the provider's to give.
                                None => return Ok((opening, stream)),
                            }
                        }
                    },
                )
                .await
            }
            Err(e) => Attempted::Failed(e),
        };
        let (candidate, first, mut inner, permit) = match attempted {
            Attempted::Served {
                candidate,
                value: (first, inner),
                permit,
            } => (candidate, first, inner, permit),
            Attempted::Failed(e) => {
                self.shared
                    .finish(
                        reg.as_ref(),
                        Finished {
                            decision,
                            attempts,
                            served_by: None,
                            usage: None,
                            started,
                            error_kind: Some(e.kind_label()),
                            task: task.clone(),
                        },
                    )
                    .await;
                return Err(e);
            }
        };

        // Forward the rest of the stream. The task owns the breaker permit for
        // as long as the stream lives: it resolves it when the stream ends,
        // and simply drops it (uncounted) if the caller abandons the stream.
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let shared = self.shared.clone();
        let task = task.clone();
        let on_timeout = self.fallback_on_timeout;
        tokio::spawn(async move {
            let mut usage: Option<Usage> = None;
            let mut permit = permit;
            let mut error_kind: Option<&'static str> = None;
            let note = |chunk: &CompletionChunk, usage: &mut Option<Usage>| {
                if let CompletionChunk::Usage(u) = chunk {
                    match usage {
                        Some(total) => total.merge(u),
                        None => *usage = Some(u.clone()),
                    }
                }
            };
            let mut abandoned = false;
            for chunk in first {
                note(&chunk, &mut usage);
                if tx.send(Ok(chunk)).await.is_err() {
                    abandoned = true;
                    break;
                }
            }
            while !abandoned {
                let item = tokio::select! {
                    biased;
                    _ = tx.closed() => {
                        abandoned = true;
                        continue;
                    }
                    item = inner.recv() => item,
                };
                match item {
                    Some(Ok(chunk)) => {
                        note(&chunk, &mut usage);
                        abandoned = tx.send(Ok(chunk)).await.is_err();
                    }
                    Some(Err(e)) => {
                        if let Some(permit) = permit.take() {
                            permit.record(classify(&e, false, on_timeout).outcome);
                        }
                        error_kind = Some(e.kind_label());
                        let _ = tx.send(Err(e)).await;
                        break;
                    }
                    None => {
                        if let Some(permit) = permit.take() {
                            permit.success();
                        }
                        break;
                    }
                }
            }
            if abandoned {
                error_kind = Some("cancelled");
            }
            drop(permit); // an abandoned stream releases its permit uncounted
            drop(inner); // stops the provider's reader
            if let Some(u) = &mut usage {
                u.fill_total();
            }
            shared
                .finish(
                    reg.as_ref(),
                    Finished {
                        decision,
                        attempts,
                        served_by: Some(candidate),
                        usage,
                        started,
                        error_kind,
                        task,
                    },
                )
                .await;
        });
        Ok(rx)
    }

    fn resolve_inner(&self, task: &Task) -> Result<(Route, RouteDecision), RouterError> {
        let reg = self.snapshot();

        if let Some(pin) = &task.pin {
            let est_cost = reg
                .get(&pin.provider, &pin.model)
                .map(|m| m.est_cost(task.est_input_tokens, task.output_estimate()))
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
                sticky: false,
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
            mut excluded,
        } = self.strategy.rank(task, reg.as_ref());

        // A completion can only be served by a chat/text model. Whatever the
        // strategy ranked, a model the registry knows to be embedding-only is
        // never a candidate (the embedding router applies the mirror rule).
        // Models the registry does not know are left to the strategy.
        let mut candidates: Vec<Candidate> = candidates
            .into_iter()
            .filter(|c| match reg.get(&c.provider, &c.model) {
                Some(info) if !info.is_generative() => {
                    excluded.push(Exclusion {
                        provider: c.provider.clone(),
                        model: c.model.clone(),
                        reason: "not a chat/text model (cannot serve a completion)".to_string(),
                    });
                    false
                }
                _ => true,
            })
            .collect();
        if candidates.is_empty() {
            return Err(RouterError::NoRoute {
                reason: format!(
                    "no model satisfied the task (strategy: {})",
                    self.strategy.name()
                ),
            });
        }

        // Sticky routing: prefer the model that last served this key, as long
        // as it is still among the eligible candidates.
        let mut sticky = false;
        if let Some(key) = &task.sticky_key {
            let last = self.shared.sticky.lock().ok().and_then(|s| s.get(key));
            if let Some((provider, model)) = last {
                if let Some(pos) = candidates
                    .iter()
                    .position(|c| c.provider == provider && c.model == model)
                {
                    let preferred = candidates.remove(pos);
                    candidates.insert(0, preferred);
                    sticky = true;
                }
            }
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
            sticky,
        };
        Ok((Route { primary, fallbacks }, decision))
    }
}

impl ModelRouter for DefaultRouter {
    fn route(
        &self,
        task: &Task,
        request: CompletionRequest,
    ) -> BoxFuture<'_, Result<CompletionResponse, RouterError>> {
        let task = task.clone();
        Box::pin(async move {
            self.route_with_report(&task, request)
                .await
                .map(|(response, _report)| response)
        })
    }

    fn route_stream(
        &self,
        task: &Task,
        request: CompletionRequest,
    ) -> BoxFuture<'_, Result<ChunkStream, RouterError>> {
        let task = task.clone();
        Box::pin(async move { self.stream_inner(&task, request).await })
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
    retry: Option<Retry>,
    breakers: Option<Arc<CircuitBreakerRegistry>>,
    fallback_depth: usize,
    fallback_on_timeout: bool,
    observer: Option<Arc<dyn GenAiObserver>>,
    budget: Option<Budget>,
    sticky_capacity: usize,
}

impl RouterBuilder {
    /// Starts a builder over an explicit provider set and a built registry.
    pub fn new(providers: ProviderSet, registry: Arc<dyn ModelRegistry>) -> Self {
        RouterBuilder {
            providers,
            registry,
            strategy: None,
            retry: None,
            breakers: None,
            fallback_depth: 2,
            fallback_on_timeout: false,
            observer: None,
            budget: None,
            sticky_capacity: 10_000,
        }
    }

    /// Starts a builder from a routing configuration, applying **all** of it:
    /// the registry overlay (cost / rank overrides, disabled models and
    /// capabilities, custom models), the strategy named by `default_strategy`
    /// with its `rules` and `default_model`, and `fallback_depth`.
    pub fn from_config(
        providers: ProviderSet,
        config: &RoutingConfig,
    ) -> Result<Self, RouterError> {
        let registry = build_registry(&providers, config)?;
        let strategy = config.strategy()?;
        let mut builder = RouterBuilder::new(providers, registry).strategy(strategy);
        if let Some(depth) = config.fallback_depth {
            builder = builder.fallback_depth(depth);
        }
        Ok(builder)
    }

    /// Sets the routing strategy (default: [`CapabilityStrategy`]).
    pub fn strategy(mut self, strategy: Box<dyn RoutingStrategy>) -> Self {
        self.strategy = Some(strategy);
        self
    }

    /// Sets the same-model retry schedule. Only failures that are safe to
    /// repeat are retried, and a provider's `Retry-After` is honoured.
    pub fn retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry = Some(Retry::new(policy));
        self
    }

    /// Sets the same-model retry behaviour with jitter, a time limit or a
    /// shared retry budget.
    pub fn retry(mut self, retry: impl Into<Retry>) -> Self {
        self.retry = Some(retry.into());
        self
    }

    /// Enables per-`(provider, model)` circuit breaking with this config.
    pub fn circuit_breaker(mut self, config: CircuitBreakerConfig) -> Self {
        self.breakers = Some(Arc::new(CircuitBreakerRegistry::new(config)));
        self
    }

    /// Enables circuit breaking with a caller-built registry — to share
    /// breakers between routers, configure them per key, or watch their state.
    /// Keys are `provider/model`.
    pub fn circuit_breakers(mut self, registry: Arc<CircuitBreakerRegistry>) -> Self {
        self.breakers = Some(registry);
        self
    }

    /// Sets how many ranked tail entries become fallbacks (default 2).
    pub fn fallback_depth(mut self, depth: usize) -> Self {
        self.fallback_depth = depth;
        self
    }

    /// Whether a completion that timed out **after the request was sent** may
    /// be tried on the next model (default `false`). The first model may
    /// still have produced — and billed — its answer, so turning this on
    /// trades possible double spend for availability.
    pub fn fallback_on_timeout(mut self, fallback: bool) -> Self {
        self.fallback_on_timeout = fallback;
        self
    }

    /// Installs an observer that receives a
    /// [`RouteReport`] for every routed call.
    pub fn observer(mut self, observer: Arc<dyn GenAiObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Enables cumulative budget tracking for tasks that carry a
    /// [`budget_key`](Task::budget_key).
    pub fn budget(mut self, budget: Budget) -> Self {
        self.budget = Some(budget);
        self
    }

    /// How many sticky keys are remembered (default 10 000; `0` disables
    /// sticky routing).
    pub fn sticky_capacity(mut self, capacity: usize) -> Self {
        self.sticky_capacity = capacity;
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
            breakers: self.breakers,
            fallback_depth: self.fallback_depth,
            fallback_on_timeout: self.fallback_on_timeout,
            shared: Shared {
                observer: self.observer,
                budget: self.budget,
                sticky: Arc::new(Mutex::new(Sticky {
                    map: HashMap::new(),
                    order: VecDeque::new(),
                    capacity: self.sticky_capacity,
                })),
            },
        }
    }
}
