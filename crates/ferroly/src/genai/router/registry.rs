//! The [`ModelRegistry`] — provider catalogs merged with the config overlay.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::capability::{parse_capability, ModelInfo};
use super::config::RoutingConfig;
use super::error::RouterError;
use super::provider_set::ProviderSet;

/// A queryable, immutable view of the available models.
pub trait ModelRegistry: Send + Sync {
    /// All known models.
    fn all(&self) -> Vec<ModelInfo>;
    /// The model registered as `(provider, model)`, if any.
    fn get(&self, provider: &str, model: &str) -> Option<ModelInfo>;
    /// All models matching `pred`.
    fn filter(&self, pred: &dyn Fn(&ModelInfo) -> bool) -> Vec<ModelInfo>;
}

fn key(provider: &str, model: &str) -> String {
    format!("{provider}/{model}")
}

/// The immutable concrete registry.
struct ImmutableRegistry {
    models: BTreeMap<String, ModelInfo>,
}

impl ModelRegistry for ImmutableRegistry {
    fn all(&self) -> Vec<ModelInfo> {
        self.models.values().cloned().collect()
    }
    fn get(&self, provider: &str, model: &str) -> Option<ModelInfo> {
        self.models.get(&key(provider, model)).cloned()
    }
    fn filter(&self, pred: &dyn Fn(&ModelInfo) -> bool) -> Vec<ModelInfo> {
        self.models.values().filter(|m| pred(m)).cloned().collect()
    }
}

/// Builds an immutable registry from the provider catalogs (base, authoritative)
/// merged with the config overlay, in the fixed order: base → cost overrides →
/// capability disable → model disable → custom models. Fails if a custom model
/// names a provider that is not in `set`.
pub fn build_registry(
    set: &ProviderSet,
    cfg: &RoutingConfig,
) -> Result<Arc<dyn ModelRegistry>, RouterError> {
    let mut merged: BTreeMap<String, ModelInfo> = BTreeMap::new();

    // 1. Base — provider-owned catalogs.
    for p in set.all() {
        for m in p.model_catalog() {
            merged.insert(key(&m.provider, &m.name), m);
        }
    }
    // 2. Cost overrides.
    for ov in &cfg.cost_overrides {
        if let Some(m) = merged.get_mut(&key(&ov.provider, &ov.model)) {
            m.input_cost_per_mtok = ov.input_cost_per_mtok;
            m.output_cost_per_mtok = ov.output_cost_per_mtok;
        }
    }
    // 3. Capability disable (provider claims it; org declines it).
    for dc in &cfg.disabled_capabilities {
        if let Some(m) = merged.get_mut(&key(&dc.provider, &dc.model)) {
            let strip: Vec<_> = dc
                .capabilities
                .iter()
                .filter_map(|s| parse_capability(s))
                .collect();
            m.capabilities.retain(|c| !strip.contains(c));
        }
    }
    // 4. Model disable.
    for dm in &cfg.disabled_models {
        merged.remove(&key(&dm.provider, &dm.model));
    }
    // 5. Custom models — provider must be registered.
    for cm in &cfg.custom_models {
        if !set.contains(&cm.provider) {
            return Err(RouterError::Config(format!(
                "custom model '{}' names unregistered provider '{}'",
                cm.name, cm.provider
            )));
        }
        merged.insert(key(&cm.provider, &cm.name), cm.to_model_info());
    }

    Ok(Arc::new(ImmutableRegistry { models: merged }))
}
