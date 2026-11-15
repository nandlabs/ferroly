#![cfg(all(feature = "openai", feature = "claude", feature = "ollama"))]
//! The model router over the **real** compiled-in catalogs of the OpenAI,
//! Claude and Ollama providers (the other router suites use stub catalogs).
//!
//! Only `resolve` is called: nothing is sent anywhere.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ferroly::genai::router::{
    build_registry, ModelRegistry, ModelRouter, Priority, ProviderSet, RouterBuilder,
    RoutingConfig, Task,
};
use ferroly::genai::{Capability, ClaudeProvider, GenAiProvider, OllamaProvider, OpenAiProvider};

fn providers() -> ProviderSet {
    ProviderSet::new(vec![
        Arc::new(OpenAiProvider::new("sk-test", None)) as Arc<dyn GenAiProvider>,
        Arc::new(ClaudeProvider::new("sk-ant-test", None)) as Arc<dyn GenAiProvider>,
        Arc::new(OllamaProvider::new(None)) as Arc<dyn GenAiProvider>,
    ])
}

fn registry() -> Arc<dyn ModelRegistry> {
    build_registry(&providers(), &RoutingConfig::default()).unwrap()
}

/// 2026-11-15, the date the OpenAI catalog was refreshed.
fn refresh_date() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(20_772 * 86_400)
}

/// For every priority, with and without `ToolUse`: a route exists, no
/// embedding model is ever a candidate, and a tool task only gets models that
/// advertise tool use.
#[test]
fn every_priority_routes_to_generative_models_only() {
    let registry = registry();
    let router = RouterBuilder::new(providers(), registry.clone())
        .fallback_depth(50)
        .build();
    for priority in [Priority::Quality, Priority::Speed, Priority::Cost] {
        for tools in [false, true] {
            let mut task = Task::new().priority(priority);
            if tools {
                task = task.require(Capability::ToolUse);
            }
            let (route, decision) = router
                .resolve(&task)
                .unwrap_or_else(|e| panic!("{priority:?}, tools={tools}: {e}"));
            assert!(!decision.considered.is_empty());
            let chosen = std::iter::once(&route.primary).chain(&route.fallbacks);
            for c in decision.considered.iter().chain(chosen) {
                let info = registry
                    .get(&c.provider, &c.model)
                    .unwrap_or_else(|| panic!("{}/{} is not in the registry", c.provider, c.model));
                assert!(
                    info.is_generative() && !info.has(Capability::Embeddings),
                    "{priority:?}, tools={tools}: embedding model {}/{} was a candidate",
                    c.provider,
                    c.model
                );
                if tools {
                    assert!(
                        info.has(Capability::ToolUse),
                        "{}/{} cannot use tools",
                        c.provider,
                        c.model
                    );
                }
            }
            // Every generative model is considered (up to the tool filter).
            let eligible = registry
                .filter(&|m| m.is_generative() && (!tools || m.has(Capability::ToolUse)))
                .len();
            assert_eq!(
                decision.considered.len(),
                eligible,
                "{priority:?}, tools={tools}"
            );
        }
    }
}

/// The picks follow the documented ordering over the real prices and ranks.
#[test]
fn picks_follow_the_catalog_prices_and_ranks() {
    let router = RouterBuilder::new(providers(), registry()).build();
    let pick = |task: Task| {
        let (route, _) = router.resolve(&task).unwrap();
        format!("{}/{}", route.primary.provider, route.primary.model)
    };
    // A local model costs nothing.
    assert_eq!(pick(Task::new().priority(Priority::Cost)), "ollama/llama3");
    // The local catalog entry has no tool use: the cheapest tool-capable
    // hosted model wins.
    assert_eq!(
        pick(
            Task::new()
                .priority(Priority::Cost)
                .require(Capability::ToolUse)
        ),
        "claude/claude-haiku-5-5"
    );
    // Highest quality rank (100) is shared; the cheaper one wins the tie.
    assert_eq!(
        pick(Task::new().priority(Priority::Quality)),
        "claude/claude-fable-5-2"
    );
    // A vision + JSON task can only be served by OpenAI here (the Claude
    // provider has no schema-less JSON mode, the local model no vision).
    let vision_json = pick(
        Task::new()
            .priority(Priority::Cost)
            .require(Capability::Vision)
            .require(Capability::JsonMode),
    );
    assert!(vision_json.starts_with("openai/"), "{vision_json}");
}

/// G11: the compiled-in hosted catalogs are current on the refresh date, and
/// the OpenAI one no longer lists deprecated models.
#[test]
fn hosted_catalog_prices_are_current() {
    let openai = OpenAiProvider::new("sk-test", None).model_catalog();
    let claude = ClaudeProvider::new("sk-ant-test", None).model_catalog();
    for m in openai.iter().chain(&claude) {
        assert!(m.as_of.is_some(), "{} has no as-of date", m.name);
        assert!(
            !m.cost_is_stale_at(180, refresh_date()),
            "{} is stale on the refresh date",
            m.name
        );
    }
    for gone in ["gpt-4o", "gpt-4o-mini", "gpt-4.1", "o3", "o4-mini"] {
        assert!(
            openai.iter().all(|m| m.name != gone),
            "{gone} is deprecated or shut down"
        );
    }
    assert!(openai.iter().any(|m| m.has(Capability::Embeddings)));
}
