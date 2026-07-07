//! Model router: resolve tasks to a `(provider, model)` choice without any live
//! API call (`resolve` is side-effect-free).
//!
//! Run with:
//! ```sh
//! cargo run -p ferroly --features openai --example model_router
//! ```

use std::sync::Arc;

use ferroly::genai::router::{
    build_registry, CapabilityStrategy, ModelRouter, Priority, ProviderSet, RouterBuilder,
    RoutingConfig, Task,
};
use ferroly::genai::{Capability, GenAiProvider, OpenAiProvider};

fn main() {
    // Providers advertise their model catalogs; constructing them makes no
    // network call, and `resolve` below doesn't either.
    let providers = ProviderSet::new(vec![
        Arc::new(OpenAiProvider::new("sk-demo", None)) as Arc<dyn GenAiProvider>
    ]);
    let registry = build_registry(&providers, &RoutingConfig::default()).expect("registry");
    let router = RouterBuilder::new(providers, registry)
        .strategy(Box::new(CapabilityStrategy))
        .build();

    let tasks = [
        (
            "cheapest chat",
            Task::new()
                .require(Capability::Chat)
                .priority(Priority::Cost),
        ),
        (
            "best for vision",
            Task::new()
                .require(Capability::Vision)
                .priority(Priority::Quality),
        ),
    ];

    for (label, task) in tasks {
        match router.resolve(&task) {
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
            }
            Err(e) => println!("{label}: no route ({e})"),
        }
    }
}
