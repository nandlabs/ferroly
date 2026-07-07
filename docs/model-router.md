# `ferroly::genai::router` — model router

A selection layer **above** [`GenAiProvider`](genai.md): describe a `Task` (what
the call needs), and the router picks `(provider, model)` from a merged registry,
clamps options to that model, dispatches the provider's existing `complete` /
`complete_stream`, and **fails over** across models on transient errors.

Part of the `genai` feature — no separate flag.

## Why

Callers currently hard-code a model string in `CompletionRequest.model`. The
router lets you instead say *"a cheap model that supports vision, under $0.02 a
call"* and have the system choose, with cross-provider resilience and a central
place for org policy (cost caps, disabled models).

## At a glance

```rust,no_run
use std::sync::Arc;
use ferroly::genai::{Capability, GenAiProvider, Message, CompletionRequest, OpenAiProvider};
use ferroly::genai::router::{
    build_registry, CapabilityStrategy, ModelRouter, ProviderSet, RouterBuilder,
    RoutingConfig, Task, Priority,
};

# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
let providers = ProviderSet::new(vec![
    Arc::new(OpenAiProvider::new("sk-...", None)) as Arc<dyn GenAiProvider>,
]);
let registry = build_registry(&providers, &RoutingConfig::default())?;
let router = RouterBuilder::new(providers, registry)
    .strategy(Box::new(CapabilityStrategy))
    .build();

let task = Task::new().require(Capability::Chat).priority(Priority::Cost);
let request = CompletionRequest::builder("")   // model is chosen by the router
    .message(Message::user("Summarize this."))
    .build();
let response = router.route(&task, request).await?;
println!("{}", response.text());
# Ok(()) }
```

## Concepts

| Type | Role |
|---|---|
| `ModelInfo` | provider-owned per-model facts: capabilities, token limits, default cost |
| `ProviderSet` | immutable, name-indexed set of providers (no global state) |
| `ModelRegistry` | provider catalogs **merged** with the config overlay; immutable, hot-reloadable |
| `Task` | what the call needs: `required` capabilities, budget, `priority`, `kind`, tags, optional `pin`/`deadline` |
| `RoutingStrategy` | ranks models for a task (`RuleBased`, `Capability`, `Composite`) |
| `Route` / `Candidate` | the plan: a `primary` plus ordered `fallbacks` |
| `RouteDecision` | the trace: chosen, considered, and excluded-with-reason |
| `ModelRouter` | `route` / `route_stream` / `resolve` |
| `EmbeddingRouter` | the same machinery for `Embedder::embed` |

## Capability metadata

Capabilities and limits are **facts owned by the provider** — a static catalog it
returns from the (defaulted) `GenAiProvider::model_catalog()`:

```rust
use ferroly::genai::{Capability, ModelInfo};
let info = ModelInfo::new("openai", "gpt-4o")
    .display_name("GPT-4o")
    .capabilities([Capability::Text, Capability::Chat, Capability::Vision, Capability::ToolUse])
    .limits(128_000, 16_384)
    .cost(2.5, 10.0);
assert!(info.has(Capability::Vision));
assert!((info.est_cost(1000, 500) - (0.0025 + 0.005)).abs() < 1e-9); // blended in+out
```

The built-in OpenAI/Claude/Ollama providers ship representative catalogs. A
provider that doesn't override `model_catalog()` is "capabilities unknown" —
invisible to capability routing but still reachable by an explicit rule or pin.

## Strategies

- **`CapabilityStrategy`** — filters to models satisfying the task's `required`
  capabilities, input limit, and budget; ranks by `Priority` (`Cost` → cheapest
  blended `est_cost`, `Speed` → smallest context, `Quality` → largest). Returns
  the whole ranking, so the tail becomes fallbacks; rejected models are recorded
  in `RouteDecision::excluded` with a reason.
- **`RuleBasedStrategy`** — explicit `(TaskKind → provider/model)` mappings with a
  default; first match is primary, later same-kind rules + the default are the
  fallback tail.

  ```rust
  use ferroly::genai::router::{RuleBasedStrategy, TaskKind};
  let _ = RuleBasedStrategy::new()
      .rule(TaskKind::CodeGen, "openai", "gpt-4o")
      .rule(TaskKind::Summarize, "claude", "claude-3-5-haiku")
      .default_model("openai", "gpt-4o-mini");
  ```
- **`CompositeStrategy`** — tries sub-strategies in order; the first non-empty
  ranking wins (e.g. a sensitive-tag policy, then capability, then rules).

## Resilience

Two independent axes, both deadline-aware:

- **Same-model retry** — `RouterBuilder::retry_policy(..)` wraps each model's call
  in `clients::RetryPolicy` backoff for transient errors.
- **Cross-model fallover** — on a *retryable* error (429, 5xx, network) the router
  moves to the next candidate. A **permanent** error (4xx, bad schema, or a
  `content_filter` finish reason) stops immediately — a different model would
  fail identically. Optional per-`(provider, model)` circuit breaking via
  `RouterBuilder::circuit_breaker(..)`.

Options are **clamped** to the chosen model before dispatch (`max_tokens` capped;
a JSON response format set when the task requires `JsonMode`).

Streaming (`route_stream`) falls over only **before the stream is established**
(pre-first-token); a mid-stream failure surfaces to the caller through the
channel.

## Decision trace

`resolve` returns the plan and a `RouteDecision` **without executing** — the
backbone of fast, offline tests and a "why this model?" answer in production
(also logged via `ferroly::log` when the `log` feature is on):

```rust,no_run
# use ferroly::genai::router::*;
# use ferroly::genai::Capability;
# fn demo(router: &DefaultRouter) {
let task = Task::new().require(Capability::Chat).priority(Priority::Cost);
let (route, decision) = router.resolve(&task).unwrap();
println!("chose {}/{} via {}", route.primary.provider, route.primary.model, decision.strategy);
for ex in &decision.excluded {
    println!("  excluded {}/{}: {}", ex.provider, ex.model, ex.reason);
}
# }
```

## Configuration (YAML)

Load an operator policy overlay with `RoutingConfig::from_yaml` (read from the
`genai.routing` section) and merge it with the provider catalogs via
`build_registry`. The merge order is fixed: base → cost overrides → capability
disable → model disable → custom models (a custom model naming an unregistered
provider fails the build).

```yaml
genai:
  routing:
    cost_overrides:
      -
        provider: openai
        model: gpt-4o
        input_cost_per_mtok: 2.5
        output_cost_per_mtok: 10.0
    disabled_models:
      -
        provider: ollama
        model: llama2
    disabled_capabilities:
      -
        provider: openai
        model: gpt-4o-mini
        capabilities:
          - vision
    custom_models:
      -
        provider: ollama
        name: my-finetune
        capabilities:
          - text
          - chat
        max_input_tokens: 32000
        max_output_tokens: 4096
        input_cost_per_mtok: 0.0
        output_cost_per_mtok: 0.0
```

> **YAML note.** ferroly's YAML codec is block-style only: write list items with
> the `-` on its own line followed by the indented mapping (as above), and use
> block sequences (`- text`) rather than flow style (`[text]`). Compact
> `- key: value` items and flow collections are a tracked follow-up.

## Embeddings

`EmbeddingRouter` shares the registry (filtered to `Capability::Embeddings`) and
strategies, dispatching to `Embedder::embed` with the same fallback engine:

```rust,no_run
# use std::sync::Arc;
# use ferroly::genai::{Embedder, EmbedRequest};
# use ferroly::genai::router::*;
# async fn demo(registry: Arc<dyn ModelRegistry>, embedders: Vec<(String, Arc<dyn Embedder>)>) {
let router = EmbeddingRouter::new(embedders, registry, Box::new(CapabilityStrategy));
let task = Task::new().priority(Priority::Cost);
let resp = router.embed(&task, EmbedRequest::single("", "hello")).await.unwrap();
# let _ = resp;
# }
```

## Hot-reload

The registry is immutable; `DefaultRouter::reload(new_registry)` swaps it
atomically. In-flight routes keep the snapshot they started with.

## Limitations

- **In-process selection, not a proxy/load-balancer.** It chooses a model per
  call; it does not multiplex or balance traffic.
- **No ML prompt classification in core** — route on declared capabilities/tags;
  a custom `RoutingStrategy` can add smarter selection.
- **Cost is an estimate** — blended input+output from `ModelInfo` (compiled-in or
  config-overridden), used for ranking and budget filtering, not billing.
- **Streaming fallover is pre-first-token only.**

## See also

- [genai](genai.md) — the provider interface the router sits above.
- [clients](clients.md) — the retry / circuit-breaker primitives it composes.
