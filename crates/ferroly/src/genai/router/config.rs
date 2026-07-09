//! Routing configuration (YAML overlay), decoded via `codec`.

use ferroly::codec::{Decode, Value};

use super::capability::{parse_capability, ModelInfo};
use super::error::RouterError;

/// A `(TaskKind → provider/model)` rule.
#[derive(Debug, Clone, Decode)]
pub struct RuleConfig {
    /// The task kind this rule matches (e.g. `codegen`).
    pub kind: String,
    /// The provider to route to.
    pub provider: String,
    /// The model to route to.
    pub model: String,
}

/// A per-model cost override.
#[derive(Debug, Clone, Decode)]
pub struct CostOverride {
    /// Provider name.
    pub provider: String,
    /// Model name.
    pub model: String,
    /// USD per million input tokens.
    pub input_cost_per_mtok: f64,
    /// USD per million output tokens.
    pub output_cost_per_mtok: f64,
}

/// A `(provider, model)` reference.
#[derive(Debug, Clone, Decode)]
pub struct ModelRef {
    /// Provider name.
    pub provider: String,
    /// Model name.
    pub model: String,
}

/// Capabilities to strip from a model (org policy).
#[derive(Debug, Clone, Decode)]
pub struct DisabledCaps {
    /// Provider name.
    pub provider: String,
    /// Model name.
    pub model: String,
    /// Capability names to remove (e.g. `[vision]`).
    pub capabilities: Vec<String>,
}

/// A model defined wholly in config (self-hosted / fine-tuned). Its `provider`
/// must be registered.
#[derive(Debug, Clone, Decode)]
pub struct CustomModel {
    /// The serving provider (must be registered).
    pub provider: String,
    /// The model name.
    pub name: String,
    /// Capability names.
    pub capabilities: Vec<String>,
    /// Max input tokens.
    pub max_input_tokens: u32,
    /// Max output tokens.
    pub max_output_tokens: u32,
    /// USD per million input tokens.
    pub input_cost_per_mtok: f64,
    /// USD per million output tokens.
    pub output_cost_per_mtok: f64,
}

impl CustomModel {
    /// Builds the [`ModelInfo`] this custom entry describes.
    pub fn to_model_info(&self) -> ModelInfo {
        ModelInfo::new(&self.provider, &self.name)
            .capabilities(self.capabilities.iter().filter_map(|s| parse_capability(s)))
            .limits(self.max_input_tokens, self.max_output_tokens)
            .cost(self.input_cost_per_mtok, self.output_cost_per_mtok)
    }
}

/// The `genai.routing` configuration overlay. Every field is optional; an empty
/// document yields an empty config.
#[derive(Debug, Clone, Default)]
pub struct RoutingConfig {
    /// The default strategy name (`rule_based` | `capability` | `composite`).
    pub default_strategy: Option<String>,
    /// How many ranked tail entries become fallbacks.
    pub fallback_depth: Option<usize>,
    /// Rule-based `(kind → model)` mappings.
    pub rules: Vec<RuleConfig>,
    /// Per-model cost overrides.
    pub cost_overrides: Vec<CostOverride>,
    /// Models to remove entirely.
    pub disabled_models: Vec<ModelRef>,
    /// Capabilities to strip per model.
    pub disabled_capabilities: Vec<DisabledCaps>,
    /// Config-defined custom models.
    pub custom_models: Vec<CustomModel>,
}

impl RoutingConfig {
    /// Parses a YAML document, reading the `genai.routing` section (an absent
    /// section yields the default, empty config).
    pub fn from_yaml(input: &str) -> Result<RoutingConfig, RouterError> {
        let root = ferroly::codec::yaml::from_str(input)
            .map_err(|e| RouterError::Config(e.to_string()))?;
        match root.get("genai").and_then(|g| g.get("routing")) {
            Some(routing) => RoutingConfig::from_value(routing),
            None => Ok(RoutingConfig::default()),
        }
    }

    /// Builds a config from an already-parsed `routing` [`Value`].
    pub fn from_value(v: &Value) -> Result<RoutingConfig, RouterError> {
        let mut cfg = RoutingConfig::default();
        if let Some(s) = v.get("default_strategy").and_then(Value::as_str) {
            cfg.default_strategy = Some(s.to_string());
        }
        if let Some(n) = v.get("fallback_depth").and_then(Value::as_i64) {
            cfg.fallback_depth = Some(n.max(0) as usize);
        }
        cfg.rules = decode_array(v, "rules")?;
        cfg.cost_overrides = decode_array(v, "cost_overrides")?;
        cfg.disabled_models = decode_array(v, "disabled_models")?;
        cfg.disabled_capabilities = decode_array(v, "disabled_capabilities")?;
        cfg.custom_models = decode_array(v, "custom_models")?;
        Ok(cfg)
    }
}

/// Decodes an optional array field into `Vec<T>` (missing → empty).
fn decode_array<T: Decode>(v: &Value, key: &str) -> Result<Vec<T>, RouterError> {
    match v.get(key) {
        Some(arr) => Vec::<T>::decode(arr).map_err(|e| RouterError::Config(format!("{key}: {e}"))),
        None => Ok(Vec::new()),
    }
}
