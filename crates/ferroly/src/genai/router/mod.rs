//! Model router — capability/cost-based selection over `GenAiProvider`s with
//! automatic, error-classified fallback.
//!
//! The router sits **above** providers: describe a [`Task`] (required
//! capabilities, budget, priority), and the router picks `(provider, model)`
//! from a merged [`ModelRegistry`], clamps options to that model, dispatches the
//! provider's existing `complete` / `complete_stream`, and fails over across
//! models on transient errors — composing with `clients` retry / circuit
//! breaking.
//!
//! ```
//! use std::sync::Arc;
//! use ferroly::genai::{Capability, CompletionRequest, Message};
//! use ferroly::genai::router::{
//!     build_registry, CapabilityStrategy, ModelRouter, ProviderSet, RouterBuilder,
//!     RoutingConfig, Task, Priority,
//! };
//! # use ferroly::genai::router::{ModelInfo};
//! # use ferroly::genai::{GenAiProvider, BoxFuture, CompletionResponse, ChunkStream, GenAiError};
//! # struct Stub;
//! # impl GenAiProvider for Stub {
//! #   fn name(&self) -> &str { "stub" }
//! #   fn complete(&self, _r: CompletionRequest) -> BoxFuture<'_, Result<CompletionResponse, GenAiError>> { unimplemented!() }
//! #   fn complete_stream(&self, _r: CompletionRequest) -> BoxFuture<'_, Result<ChunkStream, GenAiError>> { unimplemented!() }
//! #   fn supports(&self, _c: Capability) -> bool { true }
//! #   fn model_catalog(&self) -> Vec<ModelInfo> {
//! #     vec![ModelInfo::new("stub", "small").capabilities([Capability::Chat]).limits(8000, 2000).cost(0.5, 1.5),
//! #          ModelInfo::new("stub", "big").capabilities([Capability::Chat, Capability::Vision]).limits(128000, 8000).cost(5.0, 15.0)]
//! #   }
//! # }
//! let providers = ProviderSet::new(vec![Arc::new(Stub) as Arc<dyn GenAiProvider>]);
//! let registry = build_registry(&providers, &RoutingConfig::default()).unwrap();
//! let router = RouterBuilder::new(providers, registry)
//!     .strategy(Box::new(CapabilityStrategy))
//!     .build();
//!
//! // Resolve (no live call) — cheapest chat model that fits.
//! let task = Task::new().require(Capability::Chat).priority(Priority::Cost);
//! let (route, decision) = router.resolve(&task).unwrap();
//! assert_eq!(route.primary.model, "small");
//! assert_eq!(decision.strategy, "capability");
//! ```

#![deny(missing_docs)]

mod capability;
mod config;
mod default_router;
mod embedding_router;
mod error;
mod execute;
mod observe;
mod provider_set;
mod registry;
mod route;
mod strategy;
mod strategy_capability;
mod strategy_composite;
mod task;

pub use capability::{parse_capability, ModelInfo};
pub use config::{CostOverride, CustomModel, DisabledCaps, ModelRef, RoutingConfig, RuleConfig};
pub use default_router::{DefaultRouter, ModelRouter, RouterBuilder};
pub use embedding_router::EmbeddingRouter;
pub use error::RouterError;
pub use provider_set::ProviderSet;
pub use registry::{build_registry, ModelRegistry};
pub use route::{Candidate, Exclusion, Ranking, Route, RouteDecision};
pub use strategy::{default_output_estimate, satisfies, RoutingStrategy, RuleBasedStrategy};
pub use strategy_capability::CapabilityStrategy;
pub use strategy_composite::CompositeStrategy;
pub use task::{Pin, Priority, Task, TaskKind};
