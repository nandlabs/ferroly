//! The [`ModelRegistry`] — provider catalogs merged with the config overlay.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};

use super::capability::ModelInfo;
use super::config::{parse_capabilities, RoutingConfig};
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
/// rank overrides → capability disable → model disable → custom models.
///
/// Fails if a custom model names a provider that is not in `set`, or if the
/// configuration names an unknown capability.
///
/// Afterwards every model whose **compiled-in** price is older than the
/// configured staleness threshold (180 days by default) and has not been
/// overridden is reported once per process: as a `WARN` record through
/// `ferroly::log` when the `log` feature is enabled, and always through
/// [`stale_cost_warnings`]. Use [`stale_cost_models`] to query a registry
/// directly.
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
            m.cached_input_cost_per_mtok = ov.cached_input_cost_per_mtok;
            m.cache_write_cost_per_mtok = ov.cache_write_cost_per_mtok;
            m.cost_overridden = true;
        }
    }
    // 3. Rank overrides.
    for ov in &cfg.rank_overrides {
        if let Some(m) = merged.get_mut(&key(&ov.provider, &ov.model)) {
            if ov.quality_rank.is_some() {
                m.quality_rank = ov.quality_rank;
            }
            if ov.speed_rank.is_some() {
                m.speed_rank = ov.speed_rank;
            }
        }
    }
    // 4. Capability disable (provider claims it; org declines it).
    for dc in &cfg.disabled_capabilities {
        let strip = parse_capabilities(
            &dc.capabilities,
            &format!("disabled_capabilities for '{}/{}'", dc.provider, dc.model),
        )?;
        if let Some(m) = merged.get_mut(&key(&dc.provider, &dc.model)) {
            m.capabilities.retain(|c| !strip.contains(c));
        }
    }
    // 5. Model disable.
    for dm in &cfg.disabled_models {
        merged.remove(&key(&dm.provider, &dm.model));
    }
    // 6. Custom models — provider must be registered.
    for cm in &cfg.custom_models {
        if !set.contains(&cm.provider) {
            return Err(RouterError::Config(format!(
                "custom model '{}' names unregistered provider '{}'",
                cm.name, cm.provider
            )));
        }
        merged.insert(key(&cm.provider, &cm.name), cm.to_model_info()?);
    }

    let registry = ImmutableRegistry { models: merged };
    if let Some(days) = cfg.staleness_days() {
        for model in stale_cost_models(&registry, days) {
            warn_stale_once(&model, days);
        }
    }
    Ok(Arc::new(registry))
}

/// The models of `registry` whose compiled-in price is older than
/// `max_age_days` and has not been overridden (see
/// [`ModelInfo::cost_is_stale`]).
pub fn stale_cost_models(registry: &dyn ModelRegistry, max_age_days: u32) -> Vec<ModelInfo> {
    registry.filter(&|m| m.cost_is_stale(max_age_days))
}

fn warned() -> &'static Mutex<BTreeSet<String>> {
    static WARNED: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    WARNED.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// Records (and, with the `log` feature, logs) one stale-price warning per
/// model for the lifetime of the process.
fn warn_stale_once(model: &ModelInfo, max_age_days: u32) {
    let id = key(&model.provider, &model.name);
    let first = match warned().lock() {
        Ok(mut set) => set.insert(id.clone()),
        Err(_) => false,
    };
    if !first {
        return;
    }
    #[cfg(feature = "log")]
    {
        use ferroly::codec::Value;
        ferroly::log::warn(
            "genai model-router: compiled-in price is stale; add a cost override",
            &[
                ("provider", Value::from(model.provider.clone())),
                ("model", Value::from(model.name.clone())),
                (
                    "price_as_of",
                    Value::from(model.as_of.map(|d| d.to_string()).unwrap_or_default()),
                ),
                ("max_age_days", Value::from(max_age_days)),
            ],
        );
    }
    #[cfg(not(feature = "log"))]
    let _ = max_age_days;
}

/// The `provider/model` ids for which a stale-price warning has been raised in
/// this process, sorted. This is the queryable form of the warning; it is
/// populated whether or not the `log` feature is enabled.
pub fn stale_cost_warnings() -> Vec<String> {
    match warned().lock() {
        Ok(set) => set.iter().cloned().collect(),
        Err(_) => Vec::new(),
    }
}
