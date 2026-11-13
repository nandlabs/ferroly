//! Model router: resolve tasks to a `(provider, model)` choice without any live
//! API call (`resolve` is side-effect-free), then build a router from a YAML
//! policy and run a routed call against a scripted provider.
//!
//! Run with:
//! ```sh
//! cargo run -p ferroly --features openai --example model_router
//! ```

use std::sync::Arc;

use ferroly::genai::router::{
    build_registry, stale_cost_models, CapabilityStrategy, ModelInfo, ModelRouter, Priority,
    ProviderSet, RouterBuilder, RoutingConfig, Task, TaskKind,
};
use ferroly::genai::{
    Capability, CompletionRequest, CompletionResponse, GenAiProvider, Message, MockProvider,
    OpenAiProvider, Usage,
};

/// An operator policy: route by rule first, then by capability; keep sensitive
/// work on the local model; refresh a price that the compiled-in catalog may
/// have got wrong by now.
const POLICY: &str = r#"
genai:
  routing:
    default_strategy: composite
    fallback_depth: 1
    rules:
      -
        kind: codegen
        provider: openai
        model: gpt-4o
      -
        tags:
          - sensitive
        provider: local
        model: house-model
    cost_overrides:
      -
        provider: openai
        model: gpt-4o-mini
        input_cost_per_mtok: 0.15
        output_cost_per_mtok: 0.60
"#;

fn describe(router: &dyn ModelRouter, label: &str, task: &Task) {
    match router.resolve(task) {
        Ok((route, decision)) => {
            println!(
                "{label}: -> {}/{} (via {}, est ${:.4})",
                route.primary.provider,
                route.primary.model,
                decision.strategy,
                route.primary.est_cost,
            );
            for f in &route.fallbacks {
                println!("    fallback: {}/{}", f.provider, f.model);
            }
            for e in &decision.excluded {
                println!("    excluded: {}/{} ({})", e.provider, e.model, e.reason);
            }
        }
        Err(e) => println!("{label}: no route ({e})"),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Providers advertise their model catalogs; constructing them makes no
    // network call, and `resolve` below doesn't either.
    let openai = Arc::new(OpenAiProvider::new("sk-demo", None));
    // A scripted stand-in for a self-hosted model, so the example can run a
    // routed call offline.
    let local = Arc::new(MockProvider::new("local").with_catalog(
        vec![ModelInfo::new("local", "house-model")
            .capabilities([Capability::Text, Capability::Chat])
            .limits(32_000, 4_096)
            .cost(0.20, 0.60)
            .ranks(35, 70)],
    ));
    let providers = ProviderSet::new(vec![
        openai as Arc<dyn GenAiProvider>,
        local.clone() as Arc<dyn GenAiProvider>,
    ]);

    // 1. Capability routing over the compiled-in catalogs.
    let registry = build_registry(&providers, &RoutingConfig::default())?;
    for model in stale_cost_models(registry.as_ref(), 180) {
        println!(
            "note: the compiled-in price of {}/{} dates from {} — override it",
            model.provider,
            model.name,
            model.as_of.map(|d| d.to_string()).unwrap_or_default(),
        );
    }
    let router = RouterBuilder::new(providers.clone(), registry)
        .strategy(Box::new(CapabilityStrategy))
        .build();
    describe(&router, "cheapest", &Task::new().priority(Priority::Cost));
    describe(&router, "best", &Task::new().priority(Priority::Quality));
    describe(
        &router,
        "fastest with vision",
        &Task::new()
            .require(Capability::Vision)
            .priority(Priority::Speed),
    );

    // 2. The same providers under the YAML policy.
    let config = RoutingConfig::from_yaml(POLICY)?;
    let router = RouterBuilder::from_config(providers, &config)?.build();
    describe(&router, "codegen", &Task::new().kind(TaskKind::CodeGen));
    let sensitive = Task::new().kind(TaskKind::Summarize).tag("sensitive");
    describe(&router, "sensitive summary", &sensitive);

    // 3. A routed call: the rule sends it to the local (scripted) model, and
    //    the report compares the estimate with what the call actually used.
    local.push_response(
        CompletionResponse::from_text("house-model", "A short summary.")
            .with_usage(Usage::new(1_200, 40)),
    );
    let request = CompletionRequest::builder("")
        .message(Message::user("Summarize the incident report."))
        .build();
    let (response, report) = router
        .route_with_report(&sensitive.tokens(1_000, 100), request)
        .await?;
    let served = report.served_by.expect("a model served the call");
    println!(
        "answer from {}/{}: {:?} (estimated ${:.6}, actual ${:.6})",
        served.provider,
        served.model,
        response.text(),
        report.estimated_cost_usd,
        report.actual_cost_usd.unwrap_or(0.0),
    );
    Ok(())
}
