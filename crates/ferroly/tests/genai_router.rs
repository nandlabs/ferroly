#![cfg(feature = "genai")]
//! Model-router tests driven by a scripted stub provider — no live API calls.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ferroly::genai::router::{
    build_registry, CapabilityStrategy, CompositeStrategy, EmbeddingRouter, ModelInfo, ModelRouter,
    Priority, ProviderSet, RouteDecision, RouterBuilder, RoutingConfig, RuleBasedStrategy, Task,
    TaskKind,
};
use ferroly::genai::{
    BoxFuture, Capability, ChunkStream, CompletionChunk, CompletionRequest, CompletionResponse,
    EmbedRequest, EmbedResponse, Embedder, GenAiError, GenAiProvider, Message, Role,
};

/// How a stub model responds to a call.
#[derive(Clone, Copy)]
enum Outcome {
    Ok,
    Transient, // 503 -> retryable, triggers fallover
    Permanent, // 400 -> not retryable, stops
}

/// A scripted provider: a fixed catalog + per-model outcome + a call log.
struct Stub {
    name: String,
    catalog: Vec<ModelInfo>,
    outcomes: Mutex<HashMap<String, Outcome>>,
    calls: Arc<Mutex<Vec<String>>>,
}

impl Stub {
    fn new(name: &str, catalog: Vec<ModelInfo>) -> Arc<Stub> {
        Arc::new(Stub {
            name: name.to_string(),
            catalog,
            outcomes: Mutex::new(HashMap::new()),
            calls: Arc::new(Mutex::new(Vec::new())),
        })
    }
    fn set(&self, model: &str, outcome: Outcome) {
        self.outcomes
            .lock()
            .unwrap()
            .insert(model.to_string(), outcome);
    }
    fn outcome(&self, model: &str) -> Outcome {
        self.outcomes
            .lock()
            .unwrap()
            .get(model)
            .copied()
            .unwrap_or(Outcome::Ok)
    }
}

fn err_for(o: Outcome) -> GenAiError {
    match o {
        Outcome::Transient => GenAiError::Api {
            status: 503,
            message: "busy".into(),
        },
        Outcome::Permanent => GenAiError::Api {
            status: 400,
            message: "bad request".into(),
        },
        Outcome::Ok => unreachable!(),
    }
}

impl GenAiProvider for Stub {
    fn name(&self) -> &str {
        &self.name
    }
    fn complete(
        &self,
        request: CompletionRequest,
    ) -> BoxFuture<'_, Result<CompletionResponse, GenAiError>> {
        let model = request.model.clone();
        self.calls.lock().unwrap().push(model.clone());
        let outcome = self.outcome(&model);
        Box::pin(async move {
            match outcome {
                Outcome::Ok => Ok(CompletionResponse {
                    model,
                    message: Message::text(Role::Assistant, "0", "ok"),
                    finish_reason: Some("stop".into()),
                    usage: None,
                }),
                other => Err(err_for(other)),
            }
        })
    }
    fn complete_stream(
        &self,
        request: CompletionRequest,
    ) -> BoxFuture<'_, Result<ChunkStream, GenAiError>> {
        let model = request.model.clone();
        self.calls.lock().unwrap().push(model.clone());
        let outcome = self.outcome(&model);
        Box::pin(async move {
            match outcome {
                Outcome::Ok => {
                    let (tx, rx) = tokio::sync::mpsc::channel(1);
                    let _ = tx
                        .send(Ok(CompletionChunk {
                            delta: "hi".into(),
                            finish_reason: Some("stop".into()),
                            ..Default::default()
                        }))
                        .await;
                    Ok(rx)
                }
                other => Err(err_for(other)),
            }
        })
    }
    fn supports(&self, _capability: Capability) -> bool {
        true
    }
    fn model_catalog(&self) -> Vec<ModelInfo> {
        self.catalog.clone()
    }
}

impl Embedder for Stub {
    fn embed(&self, request: EmbedRequest) -> BoxFuture<'_, Result<EmbedResponse, GenAiError>> {
        let model = request.model.clone();
        self.calls.lock().unwrap().push(model.clone());
        let outcome = self.outcome(&model);
        Box::pin(async move {
            match outcome {
                Outcome::Ok => Ok(EmbedResponse {
                    model,
                    embeddings: vec![vec![0.1, 0.2, 0.3]],
                    usage: None,
                }),
                other => Err(err_for(other)),
            }
        })
    }
}

fn chat(provider: &str, name: &str, in_max: u32, out_max: u32, cin: f64, cout: f64) -> ModelInfo {
    ModelInfo::new(provider, name)
        .capabilities([Capability::Text, Capability::Chat])
        .limits(in_max, out_max)
        .cost(cin, cout)
}

fn user_req() -> CompletionRequest {
    CompletionRequest::builder("")
        .message(Message::user("hi"))
        .build()
}

// ---- registry merge ------------------------------------------------------

#[test]
fn registry_merge_overrides_disables_and_custom() {
    let stub = Stub::new(
        "openai",
        vec![
            chat("openai", "big", 128_000, 8_000, 5.0, 15.0),
            chat("openai", "small", 8_000, 2_000, 0.5, 1.5),
        ],
    );
    let providers = ProviderSet::new(vec![stub as Arc<dyn GenAiProvider>]);

    let cfg = RoutingConfig::from_yaml(
        r#"
genai:
  routing:
    cost_overrides:
      -
        provider: openai
        model: big
        input_cost_per_mtok: 1.0
        output_cost_per_mtok: 2.0
    disabled_models:
      -
        provider: openai
        model: small
    custom_models:
      -
        provider: openai
        name: tuned
        capabilities:
          - text
          - chat
          - json_mode
        max_input_tokens: 32000
        max_output_tokens: 4096
        input_cost_per_mtok: 0.1
        output_cost_per_mtok: 0.2
"#,
    )
    .unwrap();

    let reg = build_registry(&providers, &cfg).unwrap();
    // small was disabled.
    assert!(reg.get("openai", "small").is_none());
    // big's cost was overridden.
    let big = reg.get("openai", "big").unwrap();
    assert_eq!(big.input_cost_per_mtok, 1.0);
    assert_eq!(big.output_cost_per_mtok, 2.0);
    // custom model added.
    let tuned = reg.get("openai", "tuned").unwrap();
    assert!(tuned.has(Capability::JsonMode));
    assert_eq!(reg.all().len(), 2); // big + tuned
}

#[test]
fn registry_rejects_phantom_custom_model() {
    let stub = Stub::new("openai", vec![chat("openai", "big", 100, 100, 1.0, 1.0)]);
    let providers = ProviderSet::new(vec![stub as Arc<dyn GenAiProvider>]);
    let cfg = RoutingConfig::from_yaml(
        r#"
genai:
  routing:
    custom_models:
      -
        provider: nope
        name: x
        capabilities:
          - text
        max_input_tokens: 1
        max_output_tokens: 1
        input_cost_per_mtok: 0.0
        output_cost_per_mtok: 0.0
"#,
    )
    .unwrap();
    assert!(build_registry(&providers, &cfg).is_err());
}

// ---- strategies ----------------------------------------------------------

fn two_model_router(
    strategy: Box<dyn ferroly::genai::router::RoutingStrategy>,
) -> ferroly::genai::router::DefaultRouter {
    let stub = Stub::new(
        "openai",
        vec![
            chat("openai", "cheap", 8_000, 2_000, 0.5, 1.5),
            chat("openai", "premium", 128_000, 8_000, 5.0, 15.0),
        ],
    );
    let providers = ProviderSet::new(vec![stub as Arc<dyn GenAiProvider>]);
    let reg = build_registry(&providers, &RoutingConfig::default()).unwrap();
    RouterBuilder::new(providers, reg)
        .strategy(strategy)
        .build()
}

#[test]
fn capability_ranks_by_priority() {
    let router = two_model_router(Box::new(CapabilityStrategy));

    let cost = Task::new()
        .require(Capability::Chat)
        .priority(Priority::Cost);
    assert_eq!(router.resolve(&cost).unwrap().0.primary.model, "cheap");

    let quality = Task::new()
        .require(Capability::Chat)
        .priority(Priority::Quality);
    assert_eq!(router.resolve(&quality).unwrap().0.primary.model, "premium");
}

#[test]
fn capability_excludes_over_budget_and_over_limit() {
    let router = two_model_router(Box::new(CapabilityStrategy));
    // Tiny budget rules out premium; cheap survives.
    let task = Task::new()
        .require(Capability::Chat)
        .tokens(1000, 1000)
        .max_cost_usd(0.01);
    let (route, decision) = router.resolve(&task).unwrap();
    assert_eq!(route.primary.model, "cheap");
    assert!(decision.excluded.iter().any(|e| e.model == "premium"));
}

#[test]
fn capability_abstains_without_a_match() {
    let router = two_model_router(Box::new(CapabilityStrategy));
    let task = Task::new().require(Capability::Vision); // no model has vision
    assert!(router.resolve(&task).is_err());
}

#[test]
fn rule_based_and_composite() {
    let rules = RuleBasedStrategy::new()
        .rule(TaskKind::CodeGen, "openai", "premium")
        .default_model("openai", "cheap");
    let router = two_model_router(Box::new(rules));

    let codegen = Task::new().kind(TaskKind::CodeGen);
    let (route, decision) = router.resolve(&codegen).unwrap();
    assert_eq!(route.primary.model, "premium");
    assert_eq!(decision.strategy, "rule_based");
    // default is the fallback tail.
    assert!(route.fallbacks.iter().any(|c| c.model == "cheap"));

    // Composite: rule-based abstains (no kind, no default) -> capability wins.
    let composite = CompositeStrategy::new(vec![
        Box::new(RuleBasedStrategy::new()), // abstains
        Box::new(CapabilityStrategy),
    ]);
    let router = two_model_router(Box::new(composite));
    let task = Task::new()
        .require(Capability::Chat)
        .priority(Priority::Cost);
    let (_r, d) = router.resolve(&task).unwrap();
    assert_eq!(d.strategy, "composite");
}

// ---- execution: fallback, pin, resolve==executed --------------------------

#[tokio::test]
async fn falls_over_on_transient_then_succeeds() {
    let stub = Stub::new(
        "openai",
        vec![
            chat("openai", "cheap", 8_000, 2_000, 0.5, 1.5),
            chat("openai", "premium", 128_000, 8_000, 5.0, 15.0),
        ],
    );
    let calls = stub.calls.clone();
    stub.set("cheap", Outcome::Transient); // primary (cost) fails transiently
    let providers = ProviderSet::new(vec![stub as Arc<dyn GenAiProvider>]);
    let reg = build_registry(&providers, &RoutingConfig::default()).unwrap();
    let router = RouterBuilder::new(providers, reg)
        .strategy(Box::new(CapabilityStrategy))
        .build();

    let task = Task::new()
        .require(Capability::Chat)
        .priority(Priority::Cost);
    let resp = router.route(&task, user_req()).await.unwrap();
    assert_eq!(resp.model, "premium"); // fell over to the fallback
    let log = calls.lock().unwrap();
    assert_eq!(
        log.as_slice(),
        &["cheap".to_string(), "premium".to_string()]
    );
}

#[tokio::test]
async fn permanent_error_stops_without_fallback() {
    let stub = Stub::new(
        "openai",
        vec![
            chat("openai", "cheap", 8_000, 2_000, 0.5, 1.5),
            chat("openai", "premium", 128_000, 8_000, 5.0, 15.0),
        ],
    );
    let calls = stub.calls.clone();
    stub.set("cheap", Outcome::Permanent);
    let providers = ProviderSet::new(vec![stub as Arc<dyn GenAiProvider>]);
    let reg = build_registry(&providers, &RoutingConfig::default()).unwrap();
    let router = RouterBuilder::new(providers, reg)
        .strategy(Box::new(CapabilityStrategy))
        .build();

    let task = Task::new()
        .require(Capability::Chat)
        .priority(Priority::Cost);
    assert!(router.route(&task, user_req()).await.is_err());
    // Only the primary was attempted — no fallback on a permanent error.
    assert_eq!(calls.lock().unwrap().as_slice(), &["cheap".to_string()]);
}

#[tokio::test]
async fn pin_bypasses_routing() {
    let stub = Stub::new(
        "openai",
        vec![chat("openai", "premium", 128_000, 8_000, 5.0, 15.0)],
    );
    let calls = stub.calls.clone();
    let providers = ProviderSet::new(vec![stub as Arc<dyn GenAiProvider>]);
    let reg = build_registry(&providers, &RoutingConfig::default()).unwrap();
    let router = RouterBuilder::new(providers, reg).build();

    let task = Task::new().pin("openai", "premium");
    let (_route, decision): (_, RouteDecision) = router.resolve(&task).unwrap();
    assert!(decision.pinned);
    let resp = router.route(&task, user_req()).await.unwrap();
    assert_eq!(resp.model, "premium");
    assert_eq!(calls.lock().unwrap().as_slice(), &["premium".to_string()]);
}

#[test]
fn resolve_matches_the_executed_plan() {
    let router = two_model_router(Box::new(CapabilityStrategy));
    let task = Task::new()
        .require(Capability::Chat)
        .priority(Priority::Cost);
    let (route, decision) = router.resolve(&task).unwrap();
    assert_eq!(decision.chosen, route.primary);
    assert_eq!(decision.fallbacks, route.fallbacks);
    assert_eq!(route.primary.model, "cheap");
}

// ---- embeddings ----------------------------------------------------------

#[tokio::test]
async fn embedding_router_routes_and_falls_over() {
    let stub = Stub::new(
        "openai",
        vec![
            ModelInfo::new("openai", "embed-a")
                .capabilities([Capability::Embeddings])
                .limits(8000, 0)
                .cost(0.02, 0.0),
            ModelInfo::new("openai", "embed-b")
                .capabilities([Capability::Embeddings])
                .limits(8000, 0)
                .cost(0.10, 0.0),
        ],
    );
    stub.set("embed-a", Outcome::Transient); // cheapest fails -> fall over
    let calls = stub.calls.clone();
    let embedder: Arc<dyn Embedder> = stub.clone();
    let providers = ProviderSet::new(vec![stub as Arc<dyn GenAiProvider>]);
    let reg = build_registry(&providers, &RoutingConfig::default()).unwrap();

    let router = EmbeddingRouter::new(
        vec![("openai".to_string(), embedder)],
        reg,
        Box::new(CapabilityStrategy),
    );
    let task = Task::new().priority(Priority::Cost);
    let resp = router
        .embed(&task, EmbedRequest::single("", "hello"))
        .await
        .unwrap();
    assert_eq!(resp.model, "embed-b");
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        &["embed-a".to_string(), "embed-b".to_string()]
    );
}
