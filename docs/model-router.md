# `ferroly::genai::router` — model router

[← Docs index](README.md) · [← Project README](../README.md)

A selection layer **above** [`GenAiProvider`](genai.md): describe a `Task` (what
the call needs), and the router picks `(provider, model)` from a merged registry,
clamps options to that model, dispatches the provider's existing `complete` /
`complete_stream`, and **fails over** across models when — and only when — that
can help.

Part of the `genai` feature — no separate flag.

## Why

Callers otherwise hard-code a model string in `CompletionRequest.model`. The
router lets you instead say *"a cheap model that supports vision, under $0.02 a
call"* and have the system choose, with cross-provider resilience and a central
place for org policy: cost caps, cumulative budgets, disabled models, routing
rules.

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

let task = Task::new().require(Capability::Vision).priority(Priority::Cost);
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
| `ModelInfo` | per-model facts: capabilities, token limits, quality and speed rank, prices and the date they were correct on |
| `ProviderSet` | immutable, name-indexed set of providers (no global state) |
| `ModelRegistry` | provider catalogs **merged** with the config overlay; immutable, hot-reloadable |
| `Task` | what the call needs: `required` capabilities, per-call budget, `priority`, `kind`, `tags`, optional `pin`, `deadline`, `budget_key`, `sticky_key` |
| `RoutingStrategy` | ranks models for a task (`RuleBased`, `Capability`, `Composite`) |
| `Route` / `Candidate` | the plan: a `primary` plus ordered `fallbacks` |
| `RouteDecision` | the trace: chosen, considered, and excluded-with-reason |
| `RouteReport` | what executing the plan did: every attempt, who served, estimated vs actual cost |
| `ModelRouter` | `route` / `route_stream` / `resolve` |
| `Budget` / `BudgetStore` | cumulative spend limits per key |
| `EmbeddingRouter` | the same machinery for `Embedder::embed` |

## Capability metadata

Capabilities and limits are facts supplied by the provider — a catalog it
returns from the (defaulted) `GenAiProvider::model_catalog()`:

```rust
use ferroly::genai::router::AsOf;
use ferroly::genai::{Capability, ModelInfo, Usage};

let info = ModelInfo::new("openai", "gpt-4o")
    .display_name("GPT-4o")
    .capabilities([Capability::Text, Capability::Chat, Capability::Vision, Capability::ToolUse])
    .limits(128_000, 16_384)
    .cost(2.5, 10.0)                  // USD per million input / output tokens
    .cache_cost(Some(1.25), None)     // cached input / cache-write prices
    .ranks(70, 60)                    // quality, speed: 0..=100, higher is better / faster
    .as_of(AsOf::new(2026, 11, 1));   // when these numbers were correct

assert!(info.has(Capability::Vision));
assert!(info.is_generative());        // Chat or Text: can serve a completion
assert!((info.est_cost(1000, 500) - (0.0025 + 0.005)).abs() < 1e-9);
assert!(info.cost_for(&Usage::new(1000, 500)).is_some()); // from real usage
```

`ModelInfo` fields: `name`, `provider`, `display_name`, `capabilities`,
`max_input_tokens`, `max_output_tokens`, `input_cost_per_mtok`,
`output_cost_per_mtok`, `cached_input_cost_per_mtok`,
`cache_write_cost_per_mtok`, `quality_rank`, `speed_rank`, `as_of`,
`cost_overridden`, `metadata`.

The catalogs compiled into the built-in providers are **defaults to override**
— see [genai: model catalogs](genai.md#model-catalogs) and
[Stale prices](#stale-prices). A provider that doesn't override
`model_catalog()` is "capabilities unknown": invisible to capability routing
but still reachable by an explicit rule or pin.

## Which models can be chosen

A completion is only ever routed to a model that can generate text: **a model
the registry knows and that advertises neither `Chat` nor `Text` — an
embedding model — is never a candidate**, whatever the strategy ranked and
whatever the priority. It is listed in `RouteDecision::excluded` with the
reason `not a chat/text model`. You do not have to add `Capability::Chat` to
every task. (The embedding router applies the mirror rule: only `Embeddings`
models.) Models the registry does not know are left to the strategy — that is
how an explicit rule reaches an uncatalogued model. A `pin` bypasses routing
entirely.

## Strategies

### `CapabilityStrategy`

Filters to models satisfying the task's `required` capabilities, input limit
and per-call budget (`Task::max_cost_usd` against the estimated cost), then
ranks by `Priority`. It returns the whole ranking, so the tail becomes
fallbacks; rejected models are recorded in `RouteDecision::excluded` with a
reason.

| Priority | Ranks by | Ties broken by |
|---|---|---|
| `Quality` (default) | `quality_rank`, highest first | lower estimated cost, then `provider/model` name |
| `Speed` | `speed_rank`, highest first | lower estimated cost, then name |
| `Cost` | estimated cost, lowest first | higher `quality_rank`, then name |

The ranks are explicit fields of `ModelInfo`, set by the provider's catalog and
overridable in configuration. A model **without** the relevant rank sorts after
every ranked model; only among unranked models is context size used as a rough
stand-in (larger for `Quality`, smaller for `Speed`). The ordering is fully
deterministic.

### `RuleBasedStrategy`

Explicit rules. A rule matches a task when the task has the rule's `TaskKind`
(if the rule names one) **and** carries every tag the rule requires
(`Task::tags`). Matching rules, in the order they were added, become the
primary and its fallbacks; the default model, if set, is the final fallback.
With no match and no default the strategy abstains.

```rust
use ferroly::genai::router::{RuleBasedStrategy, Task, TaskKind};

let rules = RuleBasedStrategy::new()
    // Tagged tasks stay on the local model, whatever their kind.
    .tag_rule(["sensitive"], "ollama", "llama3")
    .rule(TaskKind::CodeGen, "claude", "claude-opus-5-5")
    .rule(TaskKind::Summarize, "claude", "claude-haiku-5-5")
    .default_model("openai", "gpt-4o-mini");

let task = Task::new().kind(TaskKind::Summarize).tag("sensitive");
# let _ = (rules, task);
```

`rule(kind, provider, model)`, `tag_rule(tags, provider, model)` and the general
`rule_for(Option<kind>, tags, provider, model)`.

### `CompositeStrategy`

Tries sub-strategies in order; the first non-empty ranking wins — typically
rules first, capability ranking when no rule matches.

## Resilience

### What happens to a failed attempt

Every provider error is classified once
([`GenAiError::error_class`](genai.md#error-handling)), and the router acts on
three independent questions:

| Error | Retry the same model | Fall over to the next | Circuit breaker |
|---|---|---|---|
| transport failure, connect timeout, `5xx` | yes | yes | failure |
| rate limited (`429`), overloaded (`529`/`503`) | yes — after the provider's `Retry-After` | yes | failure |
| timeout **after the request was sent** | no | no (unless `fallback_on_timeout`) | failure |
| unparseable response | no | yes | failure |
| context length exceeded, unsupported feature | no | yes — another model may fit | ignored |
| auth (`401`/`403`), validation (`400`/`404`/`422`), content filter | no | **no — routing stops** | **ignored** |

- **Same-model retry** — `RouterBuilder::retry_policy(RetryPolicy)` or
  `retry(Retry)` (jitter, time limit, shared retry budget). Off by default.
  A provider's `Retry-After` is honoured (bounded by `Retry::max_retry_after`).
  Configure retry in **one** place: if the providers also retry
  (`ProviderOptions::retry`), the attempts multiply.
- **Cross-model fallover** — when the table says so, the router moves to the
  next candidate. If every attempted candidate failed that way, the result is
  `RouterError::AllRoutesFailed` carrying the last error.
- **A caller error stops routing.** A different model would fail identically,
  so the error is returned as `RouterError::GenAi`.
- **Timeouts.** A completion is not idempotent: a response that timed out may
  still have been generated and billed. It is therefore neither retried nor
  failed over by default. `RouterBuilder::fallback_on_timeout(true)` opts in to
  trying the next model. A connect timeout — nothing was sent — is always
  retried and failed over.

Options are **clamped** to the chosen model before dispatch (`max_tokens`
capped to the model's limit; a JSON response format set when the task requires
`JsonMode`).

### Early validation

A task that requires `Capability::ToolUse` must come with tools. A request
without any fails immediately with `RouterError::InvalidRequest` — before a
model is chosen and before any provider is called — instead of paying for a
tool-capable model on a call that cannot use it.

### Circuit breaking

`RouterBuilder::circuit_breaker(config)` gives every `provider/model` its own
breaker from a [`CircuitBreakerRegistry`](clients.md#circuitbreakerregistry--one-breaker-per-key);
`circuit_breakers(Arc<CircuitBreakerRegistry>)` takes one you built — to share
breakers between routers, configure them per key, or watch state changes.
`DefaultRouter::breaker_states()` lists the current states.

- **Permits are RAII.** Each attempt holds a breaker
  [`Permit`](clients.md#the-permit-api-preferred). If the routed call is
  cancelled, runs past the task's deadline, or — for a stream — the caller
  drops the receiver, the permit is released without counting. A half-open
  trial slot can therefore never be leaked, and a model cannot get stuck
  unavailable.
- **Caller errors never open a breaker.** `400`/`401`/`403`/`404`/`422`,
  content-filter and context-length errors are recorded as *ignored*: a burst
  of bad requests from one caller does not take a model away from everyone
  else.
- **A stream holds its permit until it ends**: success when it completes,
  failure when it fails mid-way, nothing when it is abandoned.
- **When every candidate is skipped because its breaker is open**, the error is
  `RouterError::CircuitOpen { models }`, naming them — not a misleading "no
  route".

```rust,no_run
# use ferroly::genai::router::*;
# use ferroly::clients::{CircuitBreakerConfig, RetryPolicy};
# fn demo(providers: ProviderSet, registry: std::sync::Arc<dyn ModelRegistry>) {
let router = RouterBuilder::new(providers, registry)
    .retry_policy(RetryPolicy::default())
    .circuit_breaker(CircuitBreakerConfig::default())
    .fallback_depth(2)
    .build();
# let _ = router;
# }
```

### Deadlines

`Task::deadline(instant)` bounds the whole routed call, all attempts included.
When it passes, the in-flight attempt is cancelled and the result is
`RouterError::DeadlineExceeded`. The caller's deadline is not evidence about
the model, so it does not count against the breaker; configure the provider's
[timeouts](genai.md#provideroptions) to detect slow models.

### Streaming and mid-stream failure

`route_stream` falls over until the model has **started answering**: a
candidate that cannot be reached, answers with an error, or whose stream fails
before producing any part of the answer is replaced by the next one. The
opening metadata of a stream (`Start`, the input `Usage`) does not count as
part of the answer: the router holds it back until the first event that does —
text, reasoning, a tool call, or the end of the answer — so that, for example,
an `overloaded` error event right after the stream opened is still failed over
and the caller never sees a half-opened stream from the failed model. The
future returned by `route_stream` therefore resolves when that first event has
arrived, and the task's deadline covers the wait.

**Once part of the answer has been handed to the caller there is no fallover.**
The caller has already consumed part of the answer, and replaying the request
on another model would duplicate or contradict it. A later failure arrives as
an `Err` item on the returned channel, and it counts against the model's
breaker. If you need a complete answer or nothing, collect the stream
(`collect_stream`) or use `route`, and retry at your level.

## Cost, budgets and reports

### Per-call budget

`Task::max_cost_usd` excludes models whose **estimated** cost for the task
(`Task::tokens(input, output)`; the output estimate defaults per `TaskKind`)
exceeds it.

### Cumulative budgets

A `Budget` bounds what a key — a tenant, a user, an API-key id — spends **over
time**:

```rust,no_run
# use std::sync::Arc;
# use ferroly::genai::router::*;
# fn demo(providers: ProviderSet, registry: Arc<dyn ModelRegistry>) {
let budget = Budget::new(Arc::new(InMemoryBudgetStore::with_period(
        std::time::Duration::from_secs(30 * 24 * 3600), // a 30-day window
    )))
    .default_limit_usd(5.0)            // every key …
    .limit_usd("tenant-gold", 250.0);  // … except this one

let router = RouterBuilder::new(providers, registry).budget(budget).build();
let task = Task::new().budget_key("tenant-a");
# let _ = (router, task);
# }
```

For a task with a `budget_key` that has a limit:

1. If the key's recorded spend has reached the limit, the call is refused with
   `RouterError::BudgetExceeded { key, limit_usd, spent_usd }` — nothing is
   sent.
2. Candidates whose estimated cost exceeds what is left are dropped; if none
   remains, the call is refused the same way.
3. After a successful call, its **actual** cost — computed from the usage the
   provider reported, at the registry's prices — is recorded (the estimate is
   used when the provider reported no usage). For a stream this happens when
   the stream ends.

Spend is charged after the call, so concurrent calls can overshoot a limit by
what is in flight; treat the limit as a soft cap.

`BudgetStore` is the pluggable part:

```rust
pub trait BudgetStore: Send + Sync {
    fn spent<'a>(&'a self, key: &'a str) -> BoxFuture<'a, f64>;
    fn record<'a>(&'a self, key: &'a str, usd: f64) -> BoxFuture<'a, f64>;
}
```

`InMemoryBudgetStore` is per-process (`new()`; `with_period(d)` for a fixed
window per key; `reset`, `reset_all`, `snapshot`). Implement the trait over a
shared database to enforce one budget across several instances.

### Reports: estimated vs actual

`DefaultRouter::route_with_report` returns the response together with a
`RouteReport`, and the same report is handed to the router's observer for every
routed call (streamed calls included, when the stream ends):

```rust,no_run
# use ferroly::genai::router::*;
# use ferroly::genai::CompletionRequest;
# async fn demo(router: &DefaultRouter, task: Task, request: CompletionRequest) -> Result<(), RouterError> {
let (response, report) = router.route_with_report(&task, request).await?;
let served = report.served_by.as_ref().unwrap();
println!(
    "{}/{} answered; estimated ${:.4}, actual ${:.4}",
    served.provider, served.model,
    report.estimated_cost_usd,
    report.actual_cost_usd.unwrap_or(0.0),
);
for attempt in &report.attempts {
    println!("  {}/{}: {:?} in {:?}", attempt.provider, attempt.model, attempt.outcome, attempt.latency);
}
# let _ = response; Ok(()) }
```

`RouteReport { decision, attempts, served_by, estimated_cost_usd,
actual_cost_usd, usage, latency, error_kind, budget_key }`. Each `RouteAttempt`
has an `AttemptOutcome`: `Succeeded`, `FailedOver`, `Failed`,
`SkippedCircuitOpen`, `SkippedUnknownProvider` or `DeadlineExceeded`.

Costs are computed from catalog prices (compiled-in or overridden). They are
for ranking, budgeting and monitoring — not an invoice.

## Sticky routing

`Task::sticky_key(key)` keeps calls with the same key on the model that last
**served** that key, as long as it is still among the eligible candidates —
useful for provider-side prompt caches and for consistent behaviour within a
conversation. The remembered model is moved to the front of the ranking
(`RouteDecision::sticky` is then `true`) and the others stay as fallbacks.
Stickiness never overrides eligibility. The router remembers up to
`RouterBuilder::sticky_capacity` keys (default 10 000, oldest forgotten first;
`0` disables it), per process.

## Decision trace and observability

`resolve` returns the plan and a `RouteDecision` **without executing** — the
backbone of fast, offline tests and a "why this model?" answer in production:

```rust,no_run
# use ferroly::genai::router::*;
# use ferroly::genai::Capability;
# fn demo(router: &DefaultRouter) {
let task = Task::new().require(Capability::Vision).priority(Priority::Cost);
let (route, decision) = router.resolve(&task).unwrap();
println!("chose {}/{} via {}", route.primary.provider, route.primary.model, decision.strategy);
for ex in &decision.excluded {
    println!("  excluded {}/{}: {}", ex.provider, ex.model, ex.reason);
}
# }
```

`RouterBuilder::observer(Arc<dyn GenAiObserver>)` installs an
[observer](genai.md#observability) whose `on_route` receives every
`RouteReport`. The built-in adapters use it: `MetricsObserver` counts decisions
and fallbacks, `ObsObserver` records a `genai.route` span, `LogObserver` writes
a structured record. Without an observer, and with the `log` feature on, each
decision is still logged at debug level.

## Configuration (YAML)

Load an operator policy with `RoutingConfig::from_yaml` (read from the
`genai.routing` section) and build the router from it with
**`RouterBuilder::from_config`**, which applies all of it: the registry overlay,
the strategy and the fallback depth.

```rust,no_run
# use ferroly::genai::router::*;
# fn demo(providers: ProviderSet, yaml: &str) -> Result<(), RouterError> {
let config = RoutingConfig::from_yaml(yaml)?;
let router = RouterBuilder::from_config(providers, &config)?.build();
# let _ = router; Ok(()) }
```

```yaml
genai:
  routing:
    default_strategy: composite
    fallback_depth: 2
    rules:
      -
        kind: codegen
        provider: claude
        model: claude-opus-5-5
      -
        tags:
          - sensitive
        provider: ollama
        model: llama3
    default_model:
      provider: openai
      model: gpt-4o-mini
    cost_overrides:
      -
        provider: openai
        model: gpt-4o
        input_cost_per_mtok: 2.5
        output_cost_per_mtok: 10.0
        cached_input_cost_per_mtok: 1.25
    rank_overrides:
      -
        provider: openai
        model: gpt-4o-mini
        quality_rank: 50
        speed_rank: 90
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
        quality_rank: 40
        speed_rank: 70
    cost_staleness_days: 180
```

| Key | Meaning |
|---|---|
| `default_strategy` | `capability`, `rule_based`, or `composite` (rules first, then capability ranking). When absent: `composite` if there are rules or a default model, else `capability`. |
| `fallback_depth` | how many ranked tail entries become fallbacks (default 2) |
| `rules` | rule-based mappings in priority order; each has `provider`, `model`, and `kind` and/or `tags` |
| `default_model` | the rule-based strategy's final fallback |
| `cost_overrides` | replace a model's prices (marks them as operator-supplied) |
| `rank_overrides` | replace a model's `quality_rank` and/or `speed_rank` |
| `disabled_models` | remove models |
| `disabled_capabilities` | strip capabilities from a model |
| `custom_models` | add models (the `provider` must be registered) |
| `cost_staleness_days` | age after which a compiled-in price is reported (default 180; `0` disables) |

The registry merge order is fixed: base catalogs → cost overrides → rank
overrides → capability disable → model disable → custom models.

**Parsing is strict; nothing is silently ignored.** These are all errors
(`RouterError::Config`, naming the offending key):

- an unknown key anywhere in the `routing` section, or a value of the wrong type;
- an unknown `default_strategy`, an unknown task `kind`, an unknown capability
  name (in `disabled_capabilities` or `custom_models`);
- `rules` or `default_model` together with `default_strategy: capability`
  (they would be ignored), or `default_strategy: rule_based` with no rules;
- a custom model naming an unregistered provider (at registry build).

`RoutingConfig::validate()` runs the checks that need no provider set;
`RoutingConfig::strategy()` builds the configured strategy on its own, and
`build_registry(&providers, &config)` the registry.

> **YAML note.** ferroly's YAML codec is block-style: write list items with the
> `-` on its own line followed by the indented mapping (as above), and use block
> sequences (`- text`) rather than flow style (`[text]`).

### Stale prices

Compiled-in prices age. When a registry is built, every model that

- has a non-zero price,
- carries an `as_of` date older than `cost_staleness_days` (180 by default), and
- has **not** been given a cost override (custom models count as overridden)

is reported **once per process**: as a `WARN` record through `ferroly::log`
when the `log` feature is enabled, and always in
`router::stale_cost_warnings()` (the list of `provider/model` ids warned about
so far). `router::stale_cost_models(&registry, max_age_days)` queries a
registry directly, and `ModelInfo::cost_is_stale(days)` a single entry. The fix
is a `cost_overrides` entry with the current price.

## Embeddings

`EmbeddingRouter` shares the registry (restricted to `Capability::Embeddings`)
and strategies, dispatching to `Embedder::embed` with the same fallback engine.
Embedding is idempotent, so a timeout is failed over like any other transient
error.

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

## Errors

```rust
#[non_exhaustive]
pub enum RouterError {
    NoRoute { reason: String },                        // nothing satisfied the task
    AllRoutesFailed { attempts: usize, last: GenAiError },
    UnknownProvider(String),
    DeadlineExceeded,
    GenAi(GenAiError),                                 // a failure fallover cannot fix
    Config(String),
    CircuitOpen { models: String },                    // every candidate's breaker is open
    BudgetExceeded { key: String, limit_usd: f64, spent_usd: f64 },
    InvalidRequest(String),                            // e.g. ToolUse required, no tools
}
```

`RouterError::kind_label()` gives a stable label for metrics and logs.

## Limitations

- **In-process selection, not a proxy/load-balancer.** It chooses a model per
  call; it does not multiplex or balance traffic. Breakers, budgets
  (`InMemoryBudgetStore`) and sticky keys are per process.
- **No ML prompt classification in core** — route on declared capabilities,
  kinds and tags; a custom `RoutingStrategy` can add smarter selection.
- **Ranks are static judgments**, not measurements: nothing observes latency or
  answer quality at run time. Override them for your workload.
- **Cost is computed from catalog prices** — for ranking, budgets and
  monitoring, not billing. Tiered prices (for example a higher rate for very
  long prompts) are not modelled.
- **Budgets are a soft cap** (spend is recorded after the call).
- **No fallover once a stream has started delivering the answer** (see
  [above](#streaming-and-mid-stream-failure)).
- **The embedding router** has no breakers, budgets or sticky keys.

## See also

- [genai](genai.md) — the provider interface the router sits above.
- [clients](clients.md) — the retry / circuit-breaker primitives it composes.
- [metrics](metrics.md), [obs](obs.md), [log](log.md) — where routing reports go.
