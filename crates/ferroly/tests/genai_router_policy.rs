#![cfg(feature = "genai")]
//! Model-router policy tests: candidate selection, circuit breaking, error
//! classification, budgets, sticky routing, configuration and reporting.
//!
//! Providers are the public `MockProvider` (scripted, in-memory) — no network.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferroly::clients::{
    CircuitBreaker, CircuitBreakerConfig, CircuitBreakerRegistry, CircuitState, RetryPolicy,
};
use ferroly::genai::router::{
    build_registry, stale_cost_models, stale_cost_warnings, AsOf, AttemptOutcome, Budget,
    BudgetStore, CapabilityStrategy, DefaultRouter, InMemoryBudgetStore, ModelInfo, ModelRouter,
    Priority, ProviderSet, RouteReport, RouterBuilder, RouterError, RoutingConfig,
    RuleBasedStrategy, Task, TaskKind,
};
use ferroly::genai::{
    collect_stream, BoxFuture, Capability, ChunkStream, CompletionChunk, CompletionRequest,
    CompletionResponse, FinishReason, GenAiError, GenAiObserver, GenAiProvider, Message,
    MockProvider, RateLimitInfo, TimeoutPhase, ToolDefinition, Usage,
};

fn chat(name: &str, cost_in: f64, cost_out: f64) -> ModelInfo {
    ModelInfo::new("", name)
        .capabilities([Capability::Text, Capability::Chat, Capability::ToolUse])
        .limits(100_000, 4_000)
        .cost(cost_in, cost_out)
}

fn embedding(name: &str) -> ModelInfo {
    ModelInfo::new("", name)
        .capabilities([Capability::Embeddings])
        .limits(8_000, 0)
        .cost(0.01, 0.0)
}

fn mock(name: &str, models: Vec<ModelInfo>) -> Arc<MockProvider> {
    Arc::new(MockProvider::new(name).with_catalog(models))
}

fn builder(providers: &[&Arc<MockProvider>]) -> RouterBuilder {
    let set = ProviderSet::new(
        providers
            .iter()
            .map(|p| (*p).clone() as Arc<dyn GenAiProvider>)
            .collect(),
    );
    let registry = build_registry(&set, &RoutingConfig::default()).unwrap();
    RouterBuilder::new(set, registry)
}

fn request() -> CompletionRequest {
    CompletionRequest::builder("")
        .message(Message::user("hi"))
        .build()
}

fn api(status: u16) -> GenAiError {
    GenAiError::Api {
        status,
        message: format!("status {status}"),
    }
}

fn breaker(timeout_ms: u64) -> CircuitBreakerConfig {
    CircuitBreakerConfig {
        failure_threshold: 1,
        success_threshold: 1,
        max_half_open: 1,
        timeout: Duration::from_millis(timeout_ms),
    }
}

/// Breakers that leave `Open` after 60 ms but would hold on to an unreported
/// half-open trial for a full minute: if a cancelled call or an abandoned
/// stream did not release its slot itself, the model would stay unavailable
/// for the rest of the test.
fn slow_reclaim_breakers() -> Arc<CircuitBreakerRegistry> {
    Arc::new(CircuitBreakerRegistry::with_factory(|_key| {
        CircuitBreaker::builder(breaker(60))
            .half_open_timeout(Duration::from_secs(60))
            .build()
    }))
}

/// Collects the route reports a router hands to its observer.
#[derive(Default)]
struct Reports(Mutex<Vec<RouteReport>>);

impl GenAiObserver for Reports {
    fn on_route(&self, report: &RouteReport) {
        self.0.lock().unwrap().push(report.clone());
    }
}

impl Reports {
    fn last(&self) -> RouteReport {
        self.0.lock().unwrap().last().cloned().expect("a report")
    }
    async fn wait_for(&self, n: usize) -> RouteReport {
        for _ in 0..200 {
            if self.0.lock().unwrap().len() >= n {
                return self.last();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("no route report #{n} arrived");
    }
}

// ---- G1: candidate selection ---------------------------------------------

/// G1: a completion is never routed to an embedding-only model, whatever the
/// priority — even though it is the cheapest and smallest entry.
#[test]
fn completions_never_select_an_embedding_model() {
    let p = mock(
        "p",
        vec![chat("chat-model", 5.0, 15.0), embedding("embed-model")],
    );
    let router = builder(&[&p]).build();
    for priority in [Priority::Cost, Priority::Speed, Priority::Quality] {
        // No required capability at all: the chat/text requirement is implicit.
        let (route, decision) = router.resolve(&Task::new().priority(priority)).unwrap();
        assert_eq!(route.primary.model, "chat-model", "{priority:?}");
        assert!(route.fallbacks.is_empty(), "{priority:?}");
        let excluded = decision
            .excluded
            .iter()
            .find(|e| e.model == "embed-model")
            .expect("the embedding model is listed as excluded");
        assert!(excluded.reason.contains("not a chat/text model"));
    }

    // A rule that names the embedding model cannot smuggle it in either.
    let rules = RuleBasedStrategy::new().rule(TaskKind::Chat, "p", "embed-model");
    let router = builder(&[&p]).strategy(Box::new(rules)).build();
    let err = router
        .resolve(&Task::new().kind(TaskKind::Chat))
        .unwrap_err();
    assert!(matches!(err, RouterError::NoRoute { .. }), "{err:?}");
}

/// G1: `Quality` and `Speed` use the explicit ranks, with deterministic
/// tie-breaks; context size only orders models that have no rank.
#[test]
fn quality_and_speed_use_explicit_ranks() {
    // Same context size everywhere, so the old heuristic could not tell these
    // apart (and broke the tie alphabetically toward the smaller model).
    let p = mock(
        "p",
        vec![
            chat("a-small", 1.0, 2.0).ranks(40, 90),
            chat("b-large", 10.0, 30.0).ranks(95, 30),
            chat("c-medium", 3.0, 9.0).ranks(70, 60),
            chat("d-medium-twin", 4.0, 9.0).ranks(70, 60),
            chat("e-unranked", 0.5, 1.0),
        ],
    );
    let router = builder(&[&p]).fallback_depth(10).build();
    let order = |priority| -> Vec<String> {
        let (_, decision) = router
            .resolve(&Task::new().priority(priority).tokens(1000, 1000))
            .unwrap();
        decision.considered.into_iter().map(|c| c.model).collect()
    };
    assert_eq!(
        order(Priority::Quality),
        // Rank first; equal ranks → the cheaper one; unranked last.
        [
            "b-large",
            "c-medium",
            "d-medium-twin",
            "a-small",
            "e-unranked"
        ]
    );
    assert_eq!(
        order(Priority::Speed),
        [
            "a-small",
            "c-medium",
            "d-medium-twin",
            "b-large",
            "e-unranked"
        ]
    );
    assert_eq!(
        order(Priority::Cost),
        [
            "e-unranked",
            "a-small",
            "c-medium",
            "d-medium-twin",
            "b-large"
        ]
    );
}

// ---- G6: breaker permits ---------------------------------------------------

/// G6: a cancelled call that held the half-open trial slot releases it, so
/// the model is not stuck unavailable.
#[tokio::test]
async fn cancelled_call_does_not_leak_the_half_open_slot() {
    let p = mock("p", vec![chat("m", 1.0, 2.0)]);
    let router = builder(&[&p])
        .circuit_breakers(slow_reclaim_breakers())
        .build();
    let task = Task::new();

    p.push_error(api(503));
    assert!(router.route(&task, request()).await.is_err());
    assert_eq!(
        router.breaker_states(),
        vec![("p/m".to_string(), CircuitState::Open)]
    );

    tokio::time::sleep(Duration::from_millis(90)).await;

    // The half-open trial hangs and the caller gives up on it.
    p.set_delay(Some(Duration::from_secs(30)));
    p.push_text("never delivered");
    let cancelled = tokio::time::timeout(Duration::from_millis(60), router.route(&task, request()));
    assert!(cancelled.await.is_err(), "the caller timed out");
    assert_eq!(p.call_count(), 2, "the trial call did start");

    // The slot was released with the dropped future: the next call is
    // admitted as a trial and closes the breaker.
    p.set_delay(None);
    p.push_text("recovered");
    let response = router.route(&task, request()).await.unwrap();
    assert_eq!(response.text(), "recovered");
    assert_eq!(
        router.breaker_states(),
        vec![("p/m".to_string(), CircuitState::Closed)]
    );
}

/// A provider whose stream delivers one chunk and then stays open forever.
struct EndlessStream {
    calls: Mutex<usize>,
    endless: Mutex<bool>,
}

impl GenAiProvider for EndlessStream {
    fn name(&self) -> &str {
        "endless"
    }
    fn supports(&self, _c: Capability) -> bool {
        true
    }
    fn model_catalog(&self) -> Vec<ModelInfo> {
        let mut m = chat("m", 1.0, 2.0);
        m.provider = "endless".into();
        vec![m]
    }
    fn complete(
        &self,
        _request: CompletionRequest,
    ) -> BoxFuture<'_, Result<CompletionResponse, GenAiError>> {
        *self.calls.lock().unwrap() += 1;
        Box::pin(async move { Err(api(503)) })
    }
    fn complete_stream(
        &self,
        _request: CompletionRequest,
    ) -> BoxFuture<'_, Result<ChunkStream, GenAiError>> {
        *self.calls.lock().unwrap() += 1;
        let endless = *self.endless.lock().unwrap();
        Box::pin(async move {
            let (tx, rx) = tokio::sync::mpsc::channel(4);
            tx.send(Ok(CompletionChunk::text_delta("first")))
                .await
                .unwrap();
            if endless {
                tokio::spawn(async move {
                    // Keep the stream open until the consumer goes away.
                    tx.closed().await;
                });
            } else {
                tx.send(Ok(CompletionChunk::done(FinishReason::Stop)))
                    .await
                    .unwrap();
            }
            Ok(rx)
        })
    }
}

/// G6: an abandoned stream that held the half-open trial slot releases it.
#[tokio::test]
async fn abandoned_stream_does_not_leak_the_half_open_slot() {
    let provider = Arc::new(EndlessStream {
        calls: Mutex::new(0),
        endless: Mutex::new(true),
    });
    let set = ProviderSet::new(vec![provider.clone() as Arc<dyn GenAiProvider>]);
    let registry = build_registry(&set, &RoutingConfig::default()).unwrap();
    let router = RouterBuilder::new(set, registry)
        .circuit_breakers(slow_reclaim_breakers())
        .build();
    let task = Task::new();

    // Open the breaker (the non-streaming call always fails with 503).
    assert!(router.route(&task, request()).await.is_err());
    assert_eq!(router.breaker_states()[0].1, CircuitState::Open);
    tokio::time::sleep(Duration::from_millis(90)).await;

    // The half-open trial is a stream the caller walks away from.
    let mut stream = router.route_stream(&task, request()).await.unwrap();
    assert_eq!(stream.recv().await.unwrap().unwrap().text(), "first");
    drop(stream);

    // The trial slot is freed as soon as the forwarding task notices that the
    // receiver is gone — long before the breaker's 60 s reclaim. The next
    // stream is then admitted and, by finishing, closes the breaker.
    *provider.endless.lock().unwrap() = false;
    let mut admitted = None;
    for _ in 0..300 {
        match router.route_stream(&task, request()).await {
            Ok(stream) => {
                admitted = Some(stream);
                break;
            }
            Err(RouterError::CircuitOpen { .. }) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
    let response = collect_stream(admitted.expect("the trial slot was released"))
        .await
        .unwrap();
    assert_eq!(response.text(), "first");
    for _ in 0..100 {
        if router.breaker_states()[0].1 == CircuitState::Closed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(router.breaker_states()[0].1, CircuitState::Closed);
}

// ---- G7: caller errors and open breakers ----------------------------------

/// G7: caller errors never open the breaker — however many of them arrive —
/// and they stop routing instead of falling over.
#[tokio::test]
async fn caller_errors_do_not_open_the_breaker() {
    let a = mock("a", vec![chat("m", 1.0, 2.0)]);
    let b = mock("b", vec![chat("m", 2.0, 4.0)]);
    let router = builder(&[&a, &b]).circuit_breaker(breaker(10_000)).build();
    let task = Task::new().priority(Priority::Cost);

    let caller_errors = || -> Vec<GenAiError> {
        vec![
            api(400),
            api(404),
            api(422),
            GenAiError::Auth {
                status: 401,
                message: "bad key".into(),
            },
            GenAiError::Auth {
                status: 403,
                message: "forbidden".into(),
            },
            GenAiError::ContentFilter {
                message: "refused".into(),
            },
        ]
    };
    // Twice over: far more than the failure threshold of 1.
    for _ in 0..2 {
        for error in caller_errors() {
            a.push_error(error);
            let err = router.route(&task, request()).await.unwrap_err();
            assert!(matches!(err, RouterError::GenAi(_)), "{err:?}");
        }
    }
    assert_eq!(a.call_count(), 12);
    assert_eq!(b.call_count(), 0, "a caller error is not failed over");
    assert_eq!(
        router.breaker_states(),
        vec![("a/m".to_string(), CircuitState::Closed)],
        "the model is still available to everyone else"
    );

    // And the model still serves the next well-formed request.
    a.push_text("fine");
    assert_eq!(router.route(&task, request()).await.unwrap().text(), "fine");
}

/// G7: when every candidate is skipped because its breaker is open, the error
/// says exactly that.
#[tokio::test]
async fn all_breakers_open_is_reported_as_circuit_open() {
    let a = mock("a", vec![chat("m", 1.0, 2.0)]);
    let b = mock("b", vec![chat("m", 2.0, 4.0)]);
    let reports = Arc::new(Reports::default());
    let router = builder(&[&a, &b])
        .circuit_breaker(breaker(10_000))
        .observer(reports.clone())
        .build();
    let task = Task::new().priority(Priority::Cost);

    // One provider failure each opens both breakers (threshold 1).
    a.push_error(api(503));
    b.push_error(api(500));
    let err = router.route(&task, request()).await.unwrap_err();
    assert!(
        matches!(err, RouterError::AllRoutesFailed { attempts: 2, .. }),
        "{err:?}"
    );

    let err = router.route(&task, request()).await.unwrap_err();
    match &err {
        RouterError::CircuitOpen { models } => assert_eq!(models, "a/m, b/m"),
        other => panic!("expected CircuitOpen, got {other:?}"),
    }
    assert!(err.to_string().contains("circuit open"));
    assert_eq!(a.call_count() + b.call_count(), 2, "nothing more was sent");

    let report = reports.last();
    assert_eq!(report.error_kind, Some("circuit_open"));
    assert!(report
        .attempts
        .iter()
        .all(|a| a.outcome == AttemptOutcome::SkippedCircuitOpen));
}

// ---- error classification in routing --------------------------------------

/// Throttling honours `Retry-After` on the same model, then falls over.
#[tokio::test]
async fn rate_limit_waits_then_falls_over() {
    let a = mock("a", vec![chat("m", 1.0, 2.0)]);
    let b = mock("b", vec![chat("m", 2.0, 4.0)]);
    let reports = Arc::new(Reports::default());
    let router = builder(&[&a, &b])
        .retry_policy(RetryPolicy::fixed(1, Duration::from_millis(1)))
        .observer(reports.clone())
        .build();
    let limited = || GenAiError::RateLimited {
        message: "slow down".into(),
        retry_after: Some(Duration::from_millis(70)),
        limits: RateLimitInfo::default(),
    };
    a.push_error(limited());
    a.push_error(limited());
    b.push_text("from b");

    let started = Instant::now();
    let response = router
        .route(&Task::new().priority(Priority::Cost), request())
        .await
        .unwrap();
    assert_eq!(response.text(), "from b");
    assert!(
        started.elapsed() >= Duration::from_millis(70),
        "the retry waited as long as the provider asked"
    );
    assert_eq!(a.call_count(), 2, "one retry on the same model");
    let report = reports.last();
    assert_eq!(report.attempts[0].outcome, AttemptOutcome::FailedOver);
    assert_eq!(report.attempts[1].outcome, AttemptOutcome::Succeeded);
    assert_eq!(report.served_by.unwrap().provider, "b");
}

/// A completion that timed out after being sent is not repeated on another
/// model unless the router is told to; a context-length error does fall over.
#[tokio::test]
async fn timeouts_and_context_errors() {
    let a = mock("a", vec![chat("m", 1.0, 2.0)]);
    let b = mock("b", vec![chat("m", 2.0, 4.0)]);
    let timeout = || GenAiError::Timeout {
        phase: TimeoutPhase::Response,
    };
    let task = Task::new().priority(Priority::Cost);

    let router = builder(&[&a, &b])
        .retry_policy(RetryPolicy::fixed(3, Duration::from_millis(1)))
        .build();
    a.push_error(timeout());
    let err = router.route(&task, request()).await.unwrap_err();
    assert!(
        matches!(err, RouterError::GenAi(GenAiError::Timeout { .. })),
        "{err:?}"
    );
    assert_eq!(
        (a.call_count(), b.call_count()),
        (1, 0),
        "not re-sent anywhere"
    );

    let router = builder(&[&a, &b]).fallback_on_timeout(true).build();
    a.push_error(timeout());
    b.push_text("from b");
    assert_eq!(
        router.route(&task, request()).await.unwrap().text(),
        "from b"
    );

    // Nothing was sent on a connect timeout: always safe to move on.
    let router = builder(&[&a, &b]).build();
    a.push_error(GenAiError::Timeout {
        phase: TimeoutPhase::Connect,
    });
    b.push_text("after connect timeout");
    assert_eq!(
        router.route(&task, request()).await.unwrap().text(),
        "after connect timeout"
    );

    // Another model may have room for a prompt this one cannot fit.
    a.push_error(GenAiError::ContextLengthExceeded {
        message: "too long".into(),
    });
    b.push_text("fits here");
    assert_eq!(
        router.route(&task, request()).await.unwrap().text(),
        "fits here"
    );
}

/// Pending B1: a task that requires tool use but comes without tools is a
/// caller error raised before any provider is called.
#[tokio::test]
async fn tool_use_without_tools_is_rejected_early() {
    let p = mock("p", vec![chat("m", 1.0, 2.0)]);
    let router = builder(&[&p]).build();
    let task = Task::new().require(Capability::ToolUse);

    let err = router.route(&task, request()).await.unwrap_err();
    assert!(matches!(err, RouterError::InvalidRequest(_)), "{err:?}");
    let err = router.route_stream(&task, request()).await.unwrap_err();
    assert!(matches!(err, RouterError::InvalidRequest(_)), "{err:?}");
    assert_eq!(p.call_count(), 0);

    p.push_text("with tools");
    let with_tools = CompletionRequest::builder("")
        .message(Message::user("hi"))
        .tool(ToolDefinition::new("t", "d", ferroly::codec::Value::Null))
        .build();
    assert_eq!(
        router.route(&task, with_tools).await.unwrap().text(),
        "with tools"
    );
}

/// Pending B2: `model_info_for` finds a catalog entry by model name.
#[test]
fn model_info_for_looks_up_the_catalog() {
    let p = mock("p", vec![chat("m1", 1.0, 2.0), chat("m2", 3.0, 4.0)]);
    let info = p.model_info_for("m2").unwrap();
    assert_eq!(
        (info.provider.as_str(), info.input_cost_per_mtok),
        ("p", 3.0)
    );
    assert!(p.model_info_for("nope").is_none());
}

// ---- streaming -------------------------------------------------------------

/// A stream that fails before delivering anything is failed over; once a
/// chunk has reached the caller, a failure is surfaced and counted instead.
#[tokio::test]
async fn stream_fallover_stops_at_the_first_chunk() {
    let a = mock("a", vec![chat("m", 1.0, 2.0)]);
    let b = mock("b", vec![chat("m", 2.0, 4.0)]);
    let reports = Arc::new(Reports::default());
    let router = builder(&[&a, &b])
        .circuit_breaker(breaker(10_000))
        .observer(reports.clone())
        .build();
    let task = Task::new().priority(Priority::Cost);

    // 1. The first event of a's stream is an error: b takes over.
    a.push_stream(vec![Err(GenAiError::Overloaded {
        status: 529,
        message: "Overloaded".into(),
        retry_after: None,
    })]);
    b.push_stream(vec![
        Ok(CompletionChunk::text_delta("from ")),
        Ok(CompletionChunk::text_delta("b")),
        Ok(CompletionChunk::Usage(Usage::new(1_000_000, 500_000))),
        Ok(CompletionChunk::done(FinishReason::Stop)),
    ]);
    let response = collect_stream(router.route_stream(&task, request()).await.unwrap())
        .await
        .unwrap();
    assert_eq!(response.text(), "from b");

    let report = reports.wait_for(1).await;
    assert_eq!(report.attempts[0].outcome, AttemptOutcome::FailedOver);
    assert_eq!(report.served_by.as_ref().unwrap().provider, "b");
    // Actual cost comes from the usage the stream reported: 1M in @ $2 + 0.5M out @ $4.
    assert!((report.actual_cost_usd.unwrap() - 4.0).abs() < 1e-9);
    assert!(report.error_kind.is_none());
    // a's failure opened its breaker (threshold 1): the next call skips it.

    // 2. b fails mid-stream, after the caller already has a chunk: no
    //    fallover, the error is delivered, and it counts against b.
    b.push_stream(vec![
        Ok(CompletionChunk::text_delta("partial")),
        Err(api(502)),
    ]);
    let mut stream = router.route_stream(&task, request()).await.unwrap();
    assert_eq!(stream.recv().await.unwrap().unwrap().text(), "partial");
    assert!(matches!(
        stream.recv().await.unwrap(),
        Err(GenAiError::Api { status: 502, .. })
    ));
    assert!(stream.recv().await.is_none());
    let report = reports.wait_for(2).await;
    assert_eq!(report.error_kind, Some("api"));
    assert_eq!(
        report.attempts[0].outcome,
        AttemptOutcome::SkippedCircuitOpen
    );
    for _ in 0..100 {
        if router
            .breaker_states()
            .iter()
            .all(|(_, s)| *s == CircuitState::Open)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        router.breaker_states(),
        vec![
            ("a/m".to_string(), CircuitState::Open),
            ("b/m".to_string(), CircuitState::Open)
        ]
    );
}

// ---- cost, budgets, sticky routing ----------------------------------------

/// The report carries the estimated and the actual cost, and cumulative
/// budgets are enforced per key.
#[tokio::test]
async fn budgets_track_actual_spend_per_key() {
    // $1 per million input tokens, $2 per million output tokens.
    let p = mock("p", vec![chat("m", 1.0, 2.0)]);
    let store = Arc::new(InMemoryBudgetStore::new());
    let budget = Budget::new(store.clone())
        .default_limit_usd(5.0)
        .limit_usd("big-tenant", 100.0);
    let reports = Arc::new(Reports::default());
    let router: DefaultRouter = builder(&[&p])
        .budget(budget)
        .observer(reports.clone())
        .build();
    let respond = |prompt: u32, completion: u32| {
        p.push_response(
            CompletionResponse::from_text("m", "ok").with_usage(Usage::new(prompt, completion)),
        );
    };

    // Estimated from the task (100k in / 50k out) …
    let task = Task::new().tokens(100_000, 50_000).budget_key("tenant-a");
    respond(2_000_000, 1_000_000); // … but the call actually used far more.
    let (_, report) = router.route_with_report(&task, request()).await.unwrap();
    assert!((report.estimated_cost_usd - 0.2).abs() < 1e-9);
    assert!((report.actual_cost_usd.unwrap() - 4.0).abs() < 1e-9);
    assert_eq!(report.budget_key.as_deref(), Some("tenant-a"));
    assert!((store.spent("tenant-a").await - 4.0).abs() < 1e-9);

    // $1 left: a call estimated at $0.20 still fits …
    respond(1_000_000, 500_000); // costs $2
    router.route(&task, request()).await.unwrap();
    assert!((store.spent("tenant-a").await - 6.0).abs() < 1e-9);

    // … but now the key is over its $5 limit: refused without a call.
    let calls = p.call_count();
    let err = router.route(&task, request()).await.unwrap_err();
    match &err {
        RouterError::BudgetExceeded {
            key,
            limit_usd,
            spent_usd,
        } => {
            assert_eq!(key, "tenant-a");
            assert_eq!(*limit_usd, 5.0);
            assert!((spent_usd - 6.0).abs() < 1e-9);
        }
        other => panic!("expected BudgetExceeded, got {other:?}"),
    }
    assert_eq!(p.call_count(), calls);
    assert_eq!(reports.last().error_kind, Some("budget_exceeded"));

    // Other keys are unaffected; a task without a key is not tracked.
    respond(10, 10);
    router
        .route(&Task::new().budget_key("big-tenant"), request())
        .await
        .unwrap();
    respond(10, 10);
    router.route(&Task::new(), request()).await.unwrap();
    assert_eq!(store.snapshot().len(), 2);

    // A call whose estimate alone exceeds what is left is refused up front.
    let pricey = Task::new()
        .tokens(90_000, 60_000_000)
        .budget_key("big-tenant");
    assert!(matches!(
        router.route(&pricey, request()).await.unwrap_err(),
        RouterError::BudgetExceeded { .. }
    ));
}

/// Sticky routing keeps a key on the model that last served it.
#[tokio::test]
async fn sticky_key_stays_on_the_serving_model() {
    let a = mock("a", vec![chat("m", 1.0, 2.0)]);
    let b = mock("b", vec![chat("m", 2.0, 4.0)]);
    let router = builder(&[&a, &b]).build();
    let cheap_first = Task::new().priority(Priority::Cost);
    let sticky = cheap_first.clone().sticky_key("conversation-1");

    // The cheaper model fails once, so b serves the conversation.
    a.push_error(api(503));
    b.push_text("b-1");
    assert_eq!(
        router.route(&sticky, request()).await.unwrap().text(),
        "b-1"
    );

    // a is healthy again and still ranks first — but the conversation stays
    // on b, while other traffic goes to a.
    let (route, decision) = router.resolve(&sticky).unwrap();
    assert_eq!(route.primary.provider, "b");
    assert!(decision.sticky);
    assert_eq!(route.fallbacks[0].provider, "a");
    b.push_text("b-2");
    assert_eq!(
        router.route(&sticky, request()).await.unwrap().text(),
        "b-2"
    );

    let (route, decision) = router.resolve(&cheap_first).unwrap();
    assert_eq!(route.primary.provider, "a");
    assert!(!decision.sticky);

    // Stickiness never overrides eligibility.
    let needs_vision = sticky.clone().require(Capability::Vision);
    assert!(router.resolve(&needs_vision).is_err());
}

/// The deadline covers the whole routed call.
#[tokio::test]
async fn deadline_is_enforced() {
    let p = mock("p", vec![chat("m", 1.0, 2.0)]);
    let router = builder(&[&p]).circuit_breaker(breaker(10_000)).build();
    p.set_delay(Some(Duration::from_secs(30)));
    p.push_text("too late");
    let task = Task::new().deadline(Instant::now() + Duration::from_millis(60));
    let err = router.route(&task, request()).await.unwrap_err();
    assert!(matches!(err, RouterError::DeadlineExceeded), "{err:?}");
    // The caller's deadline says nothing about the model's health.
    assert_eq!(router.breaker_states()[0].1, CircuitState::Closed);
}

// ---- B21: configuration is applied ----------------------------------------

const CONFIG: &str = r#"
genai:
  routing:
    default_strategy: composite
    fallback_depth: 1
    rules:
      -
        kind: codegen
        provider: p
        model: big
      -
        tags:
          - sensitive
        provider: p
        model: local
    default_model:
      provider: p
      model: small
    rank_overrides:
      -
        provider: p
        model: small
        quality_rank: 99
    disabled_capabilities:
      -
        provider: p
        model: big
        capabilities:
          - tool_use
"#;

/// B21: `default_strategy`, `rules`, `default_model` and `fallback_depth`
/// from the configuration decide how the router routes.
#[tokio::test]
async fn router_built_from_config_applies_all_of_it() {
    let p = mock(
        "p",
        vec![
            chat("big", 10.0, 30.0).ranks(90, 20),
            chat("small", 1.0, 2.0).ranks(40, 90),
            chat("local", 0.0, 0.0).ranks(20, 50),
        ],
    );
    let set = ProviderSet::new(vec![p.clone() as Arc<dyn GenAiProvider>]);
    let config = RoutingConfig::from_yaml(CONFIG).unwrap();
    let router = RouterBuilder::from_config(set, &config).unwrap().build();

    // rules: kind → model.
    let (route, decision) = router
        .resolve(&Task::new().kind(TaskKind::CodeGen))
        .unwrap();
    assert_eq!(decision.strategy, "composite");
    assert_eq!(route.primary.model, "big");
    // fallback_depth: 1 — only one fallback, the default model.
    assert_eq!(route.fallbacks.len(), 1);
    assert_eq!(route.fallbacks[0].model, "small");

    // rules: tags → model (Task.tags take part in rule matching).
    let (route, _) = router
        .resolve(&Task::new().kind(TaskKind::Chat).tag("sensitive"))
        .unwrap();
    assert_eq!(route.primary.model, "local");
    // A sensitive codegen task matches both rules, in rule order.
    let (route, _) = router
        .resolve(&Task::new().kind(TaskKind::CodeGen).tag("sensitive"))
        .unwrap();
    assert_eq!(
        (
            route.primary.model.as_str(),
            route.fallbacks[0].model.as_str()
        ),
        ("big", "local")
    );

    // No rule matches: the default model.
    let (route, _) = router.resolve(&Task::new()).unwrap();
    assert_eq!(route.primary.model, "small");

    // And the call really goes where the rules say.
    p.push_text("done");
    let response = router
        .route(&Task::new().kind(TaskKind::CodeGen), request())
        .await
        .unwrap();
    assert_eq!(response.text(), "done");
    assert_eq!(p.last_request().unwrap().model, "big");

    // The registry overlay was applied too.
    let capability_router = RouterBuilder::from_config(
        ProviderSet::new(vec![p.clone() as Arc<dyn GenAiProvider>]),
        &RoutingConfig {
            default_strategy: Some("capability".into()),
            rules: Vec::new(),
            default_model: None,
            ..config.clone()
        },
    )
    .unwrap()
    .build();
    // rank override: `small` now outranks `big` on quality.
    let (route, _) = capability_router
        .resolve(&Task::new().priority(Priority::Quality))
        .unwrap();
    assert_eq!(route.primary.model, "small");
    // disabled capability: `big` no longer offers tool use.
    let (_, decision) = capability_router
        .resolve(&Task::new().require(Capability::ToolUse))
        .unwrap();
    assert!(decision.excluded.iter().any(|e| e.model == "big"));
}

/// B21: a key that cannot be honoured is an error, never silently ignored.
#[test]
fn config_errors_are_reported() {
    let p = mock("p", vec![chat("m", 1.0, 2.0)]);
    let set = ProviderSet::new(vec![p as Arc<dyn GenAiProvider>]);
    let routing = |body: &str| format!("genai:\n  routing:\n{body}");

    // An unknown capability in disabled_capabilities.
    let yaml = routing(
        "    disabled_capabilities:\n      -\n        provider: p\n        model: m\n        capabilities:\n          - teleport\n",
    );
    let err = RoutingConfig::from_yaml(&yaml).unwrap_err().to_string();
    assert!(err.contains("unknown capability 'teleport'"), "{err}");

    // The same mistake in a config built in code is caught by the registry.
    let config = RoutingConfig {
        disabled_capabilities: vec![ferroly::genai::router::DisabledCaps {
            provider: "p".into(),
            model: "m".into(),
            capabilities: vec!["teleport".into()],
        }],
        ..RoutingConfig::default()
    };
    let err = build_registry(&set, &config)
        .err()
        .expect("an error")
        .to_string();
    assert!(err.contains("unknown capability 'teleport'"), "{err}");

    for (body, needle) in [
        (
            "    default_strategy: smartest\n",
            "unknown default_strategy",
        ),
        ("    fallback_depht: 3\n", "unknown key 'fallback_depht'"),
        (
            "    rules:\n      -\n        kind: haiku\n        provider: p\n        model: m\n",
            "unknown task kind",
        ),
    ] {
        let err = RoutingConfig::from_yaml(&routing(body))
            .unwrap_err()
            .to_string();
        assert!(err.contains(needle), "{body:?} gave {err}");
    }
}

// ---- G11 / pending B3: stale prices ----------------------------------------

/// A compiled-in price older than the threshold is flagged once, unless the
/// operator has overridden it.
#[test]
fn stale_catalog_prices_are_flagged_unless_overridden() {
    let old = AsOf::new(2020, 1, 1);
    let p = mock(
        "stale-test",
        vec![
            chat("old-price", 1.0, 2.0).as_of(old),
            chat("old-but-overridden", 1.0, 2.0).as_of(old),
            chat("old-but-free", 0.0, 0.0).as_of(old),
            chat("undated", 1.0, 2.0),
        ],
    );
    let set = ProviderSet::new(vec![p as Arc<dyn GenAiProvider>]);
    let config = RoutingConfig::from_yaml(
        "genai:\n  routing:\n    cost_overrides:\n      -\n        provider: stale-test\n        model: old-but-overridden\n        input_cost_per_mtok: 1.5\n        output_cost_per_mtok: 2.5\n",
    )
    .unwrap();
    let registry = build_registry(&set, &config).unwrap();

    let stale: Vec<String> = stale_cost_models(registry.as_ref(), 180)
        .into_iter()
        .map(|m| m.name)
        .collect();
    assert_eq!(stale, ["old-price"]);
    assert!(
        registry
            .get("stale-test", "old-but-overridden")
            .unwrap()
            .cost_overridden
    );

    // The warning was raised (and is queryable without the `log` feature).
    let warnings = stale_cost_warnings();
    assert!(warnings.contains(&"stale-test/old-price".to_string()));
    assert!(!warnings.iter().any(|w| w.contains("old-but-overridden")));
    // Building again does not warn twice.
    build_registry(&set, &config).unwrap();
    let again = stale_cost_warnings();
    assert_eq!(
        again
            .iter()
            .filter(|w| w.as_str() == "stale-test/old-price")
            .count(),
        1
    );

    // The check can be switched off.
    let quiet = RoutingConfig {
        cost_staleness_days: Some(0),
        ..RoutingConfig::default()
    };
    assert_eq!(quiet.staleness_days(), None);
}

/// Routing still works for the simple cases the capability strategy covers.
#[tokio::test]
async fn route_report_lists_every_attempt() {
    let a = mock("a", vec![chat("m", 1.0, 2.0)]);
    let b = mock("b", vec![chat("m", 2.0, 4.0)]);
    let router = builder(&[&a, &b])
        .strategy(Box::new(CapabilityStrategy))
        .build();
    a.push_error(api(503));
    b.push_response(CompletionResponse::from_text("m", "ok").with_usage(Usage::new(100, 50)));
    let task = Task::new().priority(Priority::Cost).tokens(100, 50);
    let (response, report) = router.route_with_report(&task, request()).await.unwrap();
    assert_eq!(response.text(), "ok");
    assert_eq!(report.attempts.len(), 2);
    assert_eq!(report.attempts[0].provider, "a");
    assert_eq!(report.attempts[0].outcome, AttemptOutcome::FailedOver);
    assert!(report.attempts[0].error.as_ref().unwrap().contains("503"));
    assert_eq!(report.attempts[1].outcome, AttemptOutcome::Succeeded);
    assert_eq!(report.decision.chosen.provider, "a");
    assert_eq!(report.served_by.as_ref().unwrap().provider, "b");
    // Estimate and actual agree when the estimate was exact.
    assert!((report.estimated_cost_usd - report.actual_cost_usd.unwrap()).abs() < 1e-12);
    assert_eq!(report.usage.unwrap().total_tokens, Some(150));
}

/// Real providers open a stream with metadata (`Start`, input `Usage`) before
/// any content. A failure that follows only such metadata is still failed
/// over — the caller has seen none of the answer — and the opening events of
/// the model that does answer are delivered in order.
#[tokio::test]
async fn stream_failing_after_only_metadata_is_failed_over() {
    let a = mock("a", vec![chat("m", 1.0, 2.0)]);
    let b = mock("b", vec![chat("m", 2.0, 4.0)]);
    let router = builder(&[&a, &b]).build();
    let task = Task::new().priority(Priority::Cost);

    a.push_stream(vec![
        Ok(CompletionChunk::Start {
            id: Some("msg_a".into()),
            model: Some("m".into()),
        }),
        Ok(CompletionChunk::Usage(Usage {
            prompt_tokens: Some(12),
            ..Usage::default()
        })),
        Err(GenAiError::Overloaded {
            status: 529,
            message: "Overloaded".into(),
            retry_after: None,
        }),
    ]);
    b.push_stream(vec![
        Ok(CompletionChunk::Start {
            id: Some("msg_b".into()),
            model: Some("m".into()),
        }),
        Ok(CompletionChunk::text_delta("answer")),
        Ok(CompletionChunk::done(FinishReason::Stop)),
    ]);

    let mut stream = router.route_stream(&task, request()).await.unwrap();
    let mut events = Vec::new();
    while let Some(item) = stream.recv().await {
        events.push(item.unwrap());
    }
    // Nothing of a's stream leaked through; b's arrives complete and in order.
    assert!(matches!(
        &events[0],
        CompletionChunk::Start { id: Some(id), .. } if id == "msg_b"
    ));
    assert_eq!(events[1].text(), "answer");
    assert!(events[2].is_done());
    assert_eq!(events.len(), 3);
    assert_eq!((a.call_count(), b.call_count()), (1, 1));

    // A stream that opens and ends without content is an (empty) answer, not
    // a failure.
    a.push_stream(vec![Ok(CompletionChunk::Start {
        id: None,
        model: Some("m".into()),
    })]);
    let empty = collect_stream(router.route_stream(&task, request()).await.unwrap())
        .await
        .unwrap();
    assert_eq!(empty.text(), "");
    assert_eq!(b.call_count(), 1, "no fallover for an empty answer");
}
