//! Routing configuration (YAML overlay).
//!
//! Parsing is strict: an unknown key, a wrong type, an unknown capability,
//! task kind or strategy name is an error. A configuration that parses is
//! applied in full by [`RouterBuilder::from_config`](super::RouterBuilder::from_config)
//! — nothing in it is silently ignored.

use ferroly::codec::Value;

use super::capability::{parse_capability, ModelInfo};
use super::error::RouterError;
use super::strategy::{RoutingStrategy, RuleBasedStrategy};
use super::strategy_capability::CapabilityStrategy;
use super::strategy_composite::CompositeStrategy;
use super::task::{Priority, TaskKind};
use super::tenant::TenantPolicy;
use ferroly::genai::Capability;

/// A routing rule: tasks of `kind` and/or carrying `tags` go to
/// `provider/model`.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleConfig {
    /// The task kind this rule matches (e.g. `codegen`); any kind when absent.
    pub kind: Option<String>,
    /// Tags a task must all carry for the rule to match.
    pub tags: Vec<String>,
    /// The provider to route to.
    pub provider: String,
    /// The model to route to.
    pub model: String,
}

/// A per-model cost override.
#[derive(Debug, Clone, PartialEq)]
pub struct CostOverride {
    /// Provider name.
    pub provider: String,
    /// Model name.
    pub model: String,
    /// USD per million input tokens.
    pub input_cost_per_mtok: f64,
    /// USD per million output tokens.
    pub output_cost_per_mtok: f64,
    /// USD per million cached input tokens (optional).
    pub cached_input_cost_per_mtok: Option<f64>,
    /// USD per million input tokens written to the cache (optional).
    pub cache_write_cost_per_mtok: Option<f64>,
}

/// A per-model override of the quality and/or speed rank.
#[derive(Debug, Clone, PartialEq)]
pub struct RankOverride {
    /// Provider name.
    pub provider: String,
    /// Model name.
    pub model: String,
    /// The quality rank, `0..=100`.
    pub quality_rank: Option<u8>,
    /// The speed rank, `0..=100`.
    pub speed_rank: Option<u8>,
}

/// A `(provider, model)` reference.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRef {
    /// Provider name.
    pub provider: String,
    /// Model name.
    pub model: String,
}

/// Capabilities to strip from a model (org policy).
#[derive(Debug, Clone, PartialEq)]
pub struct DisabledCaps {
    /// Provider name.
    pub provider: String,
    /// Model name.
    pub model: String,
    /// Capability names to remove (e.g. `[vision]`).
    pub capabilities: Vec<String>,
}

/// A model defined wholly in config (self-hosted / fine-tuned / newly
/// released). Its `provider` must be registered.
#[derive(Debug, Clone, PartialEq)]
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
    /// The quality rank, `0..=100` (optional).
    pub quality_rank: Option<u8>,
    /// The speed rank, `0..=100` (optional).
    pub speed_rank: Option<u8>,
}

impl CustomModel {
    /// Builds the [`ModelInfo`] this custom entry describes. Fails on an
    /// unknown capability name.
    pub fn to_model_info(&self) -> Result<ModelInfo, RouterError> {
        let caps = parse_capabilities(
            &self.capabilities,
            &format!("custom model '{}/{}'", self.provider, self.name),
        )?;
        let mut info = ModelInfo::new(&self.provider, &self.name)
            .capabilities(caps)
            .limits(self.max_input_tokens, self.max_output_tokens)
            .cost(self.input_cost_per_mtok, self.output_cost_per_mtok);
        info.quality_rank = self.quality_rank;
        info.speed_rank = self.speed_rank;
        info.cost_overridden = true;
        Ok(info)
    }
}

/// Parses capability names, failing on the first unknown one.
pub(crate) fn parse_capabilities(
    names: &[String],
    context: &str,
) -> Result<Vec<Capability>, RouterError> {
    names
        .iter()
        .map(|name| {
            parse_capability(name).ok_or_else(|| {
                RouterError::Config(format!("{context}: unknown capability '{name}'"))
            })
        })
        .collect()
}

/// The routing policy of one tenant (`routing.tenants[]`), applied to tasks
/// whose [`tenant`](super::Task::tenant) is `name`. See
/// [`TenantPolicy`] for the semantics.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TenantConfig {
    /// The tenant name, matched against [`Task::tenant`](super::Task::tenant).
    pub name: String,
    /// Providers whose every model the tenant may use. `None` (the key is
    /// absent) sets no allow-list; `Some`, even empty, turns it on, so
    /// `allowed_providers: []` without `allowed_models` allows nothing.
    pub allowed_providers: Option<Vec<String>>,
    /// Individual models the tenant may use; `None` and `Some` as for
    /// `allowed_providers`.
    pub allowed_models: Option<Vec<ModelRef>>,
    /// The strategy for the tenant's calls (`capability`, `rule_based` or
    /// `composite`, built over this config's `rules` and `default_model`).
    pub strategy: Option<String>,
    /// The priority for the tenant's calls (`quality`, `speed` or `cost`).
    pub priority: Option<String>,
}

impl TenantConfig {
    /// Builds the [`TenantPolicy`] this entry describes; `config` supplies the
    /// rules a rule-based strategy uses.
    pub fn to_policy(&self, config: &RoutingConfig) -> Result<TenantPolicy, RouterError> {
        let ctx = || format!("tenant '{}'", self.name);
        let mut policy = if self.allowed_providers.is_some() || self.allowed_models.is_some() {
            TenantPolicy::deny_all()
        } else {
            TenantPolicy::new()
        };
        for provider in self.allowed_providers.iter().flatten() {
            policy = policy.allow_provider(provider);
        }
        for m in self.allowed_models.iter().flatten() {
            policy = policy.allow_model(&m.provider, &m.model);
        }
        if let Some(name) = &self.strategy {
            policy = policy.strategy(config.build_strategy(name, &ctx())?);
        }
        if let Some(name) = &self.priority {
            let priority = Priority::parse(name).ok_or_else(|| {
                RouterError::Config(format!(
                    "{}: unknown priority '{name}' (expected quality, speed or cost)",
                    ctx()
                ))
            })?;
            policy = policy.priority(priority);
        }
        Ok(policy)
    }
}

/// The `genai.routing` configuration overlay. Every field is optional; an empty
/// document yields an empty config.
///
/// ```yaml
/// genai:
///   routing:
///     default_strategy: composite
///     fallback_depth: 2
///     rules:
///       - kind: codegen
///         provider: claude
///         model: claude-opus-5-5
///       - tags: [sensitive]
///         provider: ollama
///         model: llama3
///     default_model:
///       provider: openai
///       model: gpt-6.1-terra
///     tenants:
///       - name: acme
///         allowed_providers: [ollama]
///         allowed_models:
///           - provider: claude
///             model: claude-haiku-5-5
///         strategy: capability
///         priority: cost
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RoutingConfig {
    /// The strategy to build: `capability`, `rule_based` or `composite`
    /// (rules first, capability ranking when no rule matches). When absent:
    /// `composite` if there are rules or a default model, else `capability`.
    pub default_strategy: Option<String>,
    /// How many ranked tail entries become fallbacks.
    pub fallback_depth: Option<usize>,
    /// Rule-based mappings, in priority order.
    pub rules: Vec<RuleConfig>,
    /// The rule-based strategy's final fallback.
    pub default_model: Option<ModelRef>,
    /// Per-model cost overrides.
    pub cost_overrides: Vec<CostOverride>,
    /// Per-model rank overrides.
    pub rank_overrides: Vec<RankOverride>,
    /// Models to remove entirely.
    pub disabled_models: Vec<ModelRef>,
    /// Capabilities to strip per model.
    pub disabled_capabilities: Vec<DisabledCaps>,
    /// Config-defined custom models.
    pub custom_models: Vec<CustomModel>,
    /// After how many days a compiled-in price is reported as stale
    /// (default 180). `0` disables the check.
    pub cost_staleness_days: Option<u32>,
    /// Per-tenant routing policies.
    pub tenants: Vec<TenantConfig>,
}

/// The age, in days, beyond which a compiled-in price is reported as stale.
pub const DEFAULT_COST_STALENESS_DAYS: u32 = 180;

const STRATEGIES: &[&str] = &["capability", "rule_based", "composite"];

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
        if v.is_null() {
            return Ok(RoutingConfig::default());
        }
        let mut o = Obj::new(v, "routing")?;
        let cfg = RoutingConfig {
            default_strategy: o.opt_str("default_strategy")?,
            fallback_depth: o.opt_u32("fallback_depth")?.map(|n| n as usize),
            rules: o.list("rules", |item, ctx| {
                let mut r = Obj::new(item, ctx)?;
                let rule = RuleConfig {
                    kind: r.opt_str("kind")?,
                    tags: r.str_list("tags")?,
                    provider: r.str("provider")?,
                    model: r.str("model")?,
                };
                r.finish()?;
                Ok(rule)
            })?,
            default_model: match o.take("default_model") {
                Some(item) => Some(model_ref(item, "routing.default_model")?),
                None => None,
            },
            cost_overrides: o.list("cost_overrides", |item, ctx| {
                let mut c = Obj::new(item, ctx)?;
                let ov = CostOverride {
                    provider: c.str("provider")?,
                    model: c.str("model")?,
                    input_cost_per_mtok: c.f64("input_cost_per_mtok")?,
                    output_cost_per_mtok: c.f64("output_cost_per_mtok")?,
                    cached_input_cost_per_mtok: c.opt_f64("cached_input_cost_per_mtok")?,
                    cache_write_cost_per_mtok: c.opt_f64("cache_write_cost_per_mtok")?,
                };
                c.finish()?;
                Ok(ov)
            })?,
            rank_overrides: o.list("rank_overrides", |item, ctx| {
                let mut c = Obj::new(item, ctx)?;
                let ov = RankOverride {
                    provider: c.str("provider")?,
                    model: c.str("model")?,
                    quality_rank: c.opt_rank("quality_rank")?,
                    speed_rank: c.opt_rank("speed_rank")?,
                };
                c.finish()?;
                Ok(ov)
            })?,
            disabled_models: o.list("disabled_models", model_ref)?,
            disabled_capabilities: o.list("disabled_capabilities", |item, ctx| {
                let mut c = Obj::new(item, ctx)?;
                let dc = DisabledCaps {
                    provider: c.str("provider")?,
                    model: c.str("model")?,
                    capabilities: c.str_list("capabilities")?,
                };
                c.finish()?;
                Ok(dc)
            })?,
            custom_models: o.list("custom_models", |item, ctx| {
                let mut c = Obj::new(item, ctx)?;
                let cm = CustomModel {
                    provider: c.str("provider")?,
                    name: c.str("name")?,
                    capabilities: c.str_list("capabilities")?,
                    max_input_tokens: c.u32("max_input_tokens")?,
                    max_output_tokens: c.u32("max_output_tokens")?,
                    input_cost_per_mtok: c.f64("input_cost_per_mtok")?,
                    output_cost_per_mtok: c.f64("output_cost_per_mtok")?,
                    quality_rank: c.opt_rank("quality_rank")?,
                    speed_rank: c.opt_rank("speed_rank")?,
                };
                c.finish()?;
                Ok(cm)
            })?,
            cost_staleness_days: o.opt_u32("cost_staleness_days")?,
            tenants: o.list("tenants", |item, ctx| {
                let mut t = Obj::new(item, ctx)?;
                let tenant = TenantConfig {
                    name: t.str("name")?,
                    allowed_providers: t.opt_str_list("allowed_providers")?,
                    allowed_models: t.opt_list("allowed_models", model_ref)?,
                    strategy: t.opt_str("strategy")?,
                    priority: t.opt_str("priority")?,
                };
                t.finish()?;
                Ok(tenant)
            })?,
        };
        o.finish()?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Checks everything that can be checked without the provider set: the
    /// strategy name, rule kinds, capability names, and that the rules and
    /// the strategy agree.
    pub fn validate(&self) -> Result<(), RouterError> {
        if let Some(name) = &self.default_strategy {
            if !STRATEGIES.contains(&name.as_str()) {
                return Err(RouterError::Config(format!(
                    "unknown default_strategy '{name}' (expected one of: {})",
                    STRATEGIES.join(", ")
                )));
            }
        }
        for rule in &self.rules {
            if let Some(kind) = &rule.kind {
                if TaskKind::parse(kind).is_none() {
                    return Err(RouterError::Config(format!(
                        "rule for {}/{}: unknown task kind '{kind}'",
                        rule.provider, rule.model
                    )));
                }
            }
        }
        for dc in &self.disabled_capabilities {
            parse_capabilities(
                &dc.capabilities,
                &format!("disabled_capabilities for '{}/{}'", dc.provider, dc.model),
            )?;
        }
        for cm in &self.custom_models {
            cm.to_model_info()?;
        }
        let mut names: Vec<&str> = Vec::new();
        for tenant in &self.tenants {
            if tenant.name.trim().is_empty() {
                return Err(RouterError::Config("a tenant has an empty name".into()));
            }
            if names.contains(&tenant.name.as_str()) {
                return Err(RouterError::Config(format!(
                    "tenant '{}' is configured twice",
                    tenant.name
                )));
            }
            names.push(&tenant.name);
            tenant.to_policy(self)?;
        }
        let has_rules = !self.rules.is_empty() || self.default_model.is_some();
        match self.default_strategy.as_deref() {
            Some("capability") if has_rules => Err(RouterError::Config(
                "rules / default_model are configured but default_strategy is 'capability', \
                 which would ignore them; use 'composite' or 'rule_based'"
                    .to_string(),
            )),
            Some("rule_based") if !has_rules => Err(RouterError::Config(
                "default_strategy is 'rule_based' but no rules or default_model are configured"
                    .to_string(),
            )),
            _ => Ok(()),
        }
    }

    /// Builds the routing strategy this configuration describes.
    pub fn strategy(&self) -> Result<Box<dyn RoutingStrategy>, RouterError> {
        self.validate()?;
        let has_rules = !self.rules.is_empty() || self.default_model.is_some();
        let name = match self.default_strategy.as_deref() {
            Some(name) => name,
            None if has_rules => "composite",
            None => "capability",
        };
        self.build_strategy(name, "default_strategy")
    }

    /// The per-tenant policies this configuration describes, by tenant name.
    pub fn tenant_policies(&self) -> Result<Vec<(String, TenantPolicy)>, RouterError> {
        self.tenants
            .iter()
            .map(|t| Ok((t.name.clone(), t.to_policy(self)?)))
            .collect()
    }

    /// Builds the strategy `name` over this config's rules.
    fn build_strategy(
        &self,
        name: &str,
        context: &str,
    ) -> Result<Box<dyn RoutingStrategy>, RouterError> {
        if !STRATEGIES.contains(&name) {
            return Err(RouterError::Config(format!(
                "{context}: unknown strategy '{name}' (expected one of: {})",
                STRATEGIES.join(", ")
            )));
        }
        let has_rules = !self.rules.is_empty() || self.default_model.is_some();
        if name == "rule_based" && !has_rules {
            return Err(RouterError::Config(format!(
                "{context}: strategy 'rule_based' needs rules or a default_model"
            )));
        }
        let rule_based = || -> Result<RuleBasedStrategy, RouterError> {
            let mut strategy = RuleBasedStrategy::new();
            for rule in &self.rules {
                let kind = match &rule.kind {
                    Some(kind) => Some(TaskKind::parse(kind).ok_or_else(|| {
                        RouterError::Config(format!("unknown task kind '{kind}'"))
                    })?),
                    None => None,
                };
                strategy = strategy.rule_for(kind, rule.tags.clone(), &rule.provider, &rule.model);
            }
            if let Some(default) = &self.default_model {
                strategy = strategy.default_model(&default.provider, &default.model);
            }
            Ok(strategy)
        };
        Ok(match name {
            "capability" => Box::new(CapabilityStrategy),
            "rule_based" => Box::new(rule_based()?),
            _ => Box::new(CompositeStrategy::new(vec![
                Box::new(rule_based()?),
                Box::new(CapabilityStrategy),
            ])),
        })
    }

    /// The staleness threshold in days (`None` when the check is disabled).
    pub fn staleness_days(&self) -> Option<u32> {
        match self.cost_staleness_days {
            Some(0) => None,
            Some(days) => Some(days),
            None => Some(DEFAULT_COST_STALENESS_DAYS),
        }
    }
}

fn model_ref(item: &Value, ctx: &str) -> Result<ModelRef, RouterError> {
    let mut m = Obj::new(item, ctx)?;
    let r = ModelRef {
        provider: m.str("provider")?,
        model: m.str("model")?,
    };
    m.finish()?;
    Ok(r)
}

/// A strict reader over one configuration object: every key must be consumed.
struct Obj<'a> {
    ctx: String,
    entries: &'a [(String, Value)],
    seen: Vec<&'a str>,
}

impl<'a> Obj<'a> {
    fn new(v: &'a Value, ctx: &str) -> Result<Self, RouterError> {
        match v.as_object() {
            Some(entries) => Ok(Obj {
                ctx: ctx.to_string(),
                entries,
                seen: Vec::new(),
            }),
            None => Err(RouterError::Config(format!("{ctx}: expected a mapping"))),
        }
    }

    fn err(&self, key: &str, what: &str) -> RouterError {
        RouterError::Config(format!("{}.{key}: {what}", self.ctx))
    }

    /// The value of `key` (null counts as absent), marking the key as known.
    fn take(&mut self, key: &'a str) -> Option<&'a Value> {
        self.seen.push(key);
        self.entries
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
            .filter(|v| !v.is_null())
    }

    fn opt_str(&mut self, key: &'a str) -> Result<Option<String>, RouterError> {
        match self.take(key) {
            None => Ok(None),
            Some(Value::Str(text)) => Ok(Some(text.clone())),
            Some(_) => Err(self.err(key, "expected a string")),
        }
    }

    fn str(&mut self, key: &'a str) -> Result<String, RouterError> {
        self.opt_str(key)?
            .ok_or_else(|| self.err(key, "missing required key"))
    }

    fn opt_f64(&mut self, key: &'a str) -> Result<Option<f64>, RouterError> {
        match self.take(key) {
            None => Ok(None),
            Some(v) => match v.as_f64() {
                Some(n) if n.is_finite() && n >= 0.0 => Ok(Some(n)),
                _ => Err(self.err(key, "expected a non-negative number")),
            },
        }
    }

    fn f64(&mut self, key: &'a str) -> Result<f64, RouterError> {
        self.opt_f64(key)?
            .ok_or_else(|| self.err(key, "missing required key"))
    }

    fn opt_u32(&mut self, key: &'a str) -> Result<Option<u32>, RouterError> {
        match self.take(key) {
            None => Ok(None),
            Some(v) => match v.as_u64().and_then(|n| u32::try_from(n).ok()) {
                Some(n) => Ok(Some(n)),
                None => Err(self.err(key, "expected a non-negative integer")),
            },
        }
    }

    fn u32(&mut self, key: &'a str) -> Result<u32, RouterError> {
        self.opt_u32(key)?
            .ok_or_else(|| self.err(key, "missing required key"))
    }

    fn opt_rank(&mut self, key: &'a str) -> Result<Option<u8>, RouterError> {
        match self.opt_u32(key)? {
            None => Ok(None),
            Some(n) if n <= 100 => Ok(Some(n as u8)),
            Some(_) => Err(self.err(key, "expected a rank between 0 and 100")),
        }
    }

    fn str_list(&mut self, key: &'a str) -> Result<Vec<String>, RouterError> {
        Ok(self.opt_str_list(key)?.unwrap_or_default())
    }

    /// Like [`str_list`](Self::str_list), telling an absent key (`None`)
    /// from an empty list.
    fn opt_str_list(&mut self, key: &'a str) -> Result<Option<Vec<String>>, RouterError> {
        match self.take(key) {
            None => Ok(None),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| match item {
                    Value::Str(text) => Ok(text.clone()),
                    _ => Err(self.err(key, "expected a list of strings")),
                })
                .collect::<Result<_, _>>()
                .map(Some),
            Some(_) => Err(self.err(key, "expected a list of strings")),
        }
    }

    fn list<T>(
        &mut self,
        key: &'a str,
        parse: impl Fn(&'a Value, &str) -> Result<T, RouterError>,
    ) -> Result<Vec<T>, RouterError> {
        Ok(self.opt_list(key, parse)?.unwrap_or_default())
    }

    /// Like [`list`](Self::list), telling an absent key (`None`) from an
    /// empty list.
    fn opt_list<T>(
        &mut self,
        key: &'a str,
        parse: impl Fn(&'a Value, &str) -> Result<T, RouterError>,
    ) -> Result<Option<Vec<T>>, RouterError> {
        match self.take(key) {
            None => Ok(None),
            Some(Value::Array(items)) => items
                .iter()
                .enumerate()
                .map(|(i, item)| parse(item, &format!("{}.{key}[{i}]", self.ctx)))
                .collect::<Result<_, _>>()
                .map(Some),
            Some(_) => Err(self.err(key, "expected a list")),
        }
    }

    /// Fails if the object has a key that was never asked for.
    fn finish(self) -> Result<(), RouterError> {
        for (key, _) in self.entries {
            if !self.seen.contains(&key.as_str()) {
                return Err(RouterError::Config(format!(
                    "{}: unknown key '{key}'",
                    self.ctx
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(routing_yaml: &str) -> Result<RoutingConfig, RouterError> {
        RoutingConfig::from_yaml(&format!("genai:\n  routing:\n{routing_yaml}"))
    }

    #[test]
    fn empty_and_absent_sections_are_defaults() {
        assert_eq!(
            RoutingConfig::from_yaml("other: 1\n").unwrap(),
            RoutingConfig::default()
        );
        let cfg = RoutingConfig::default();
        assert_eq!(cfg.strategy().unwrap().name(), "capability");
        assert_eq!(cfg.staleness_days(), Some(DEFAULT_COST_STALENESS_DAYS));
    }

    #[test]
    fn unknown_keys_and_names_are_errors() {
        for (yaml, needle) in [
            ("    default_strategy: fastest\n", "unknown default_strategy"),
            ("    defualt_strategy: capability\n", "unknown key 'defualt_strategy'"),
            ("    fallback_depth: -1\n", "non-negative integer"),
            (
                "    rules:\n      -\n        kind: poetry\n        provider: p\n        model: m\n",
                "unknown task kind 'poetry'",
            ),
            (
                "    rules:\n      -\n        kind: chat\n        provider: p\n",
                "rules[0].model: missing required key",
            ),
            (
                "    rules:\n      -\n        kind: chat\n        provider: p\n        model: m\n        weight: 3\n",
                "unknown key 'weight'",
            ),
            (
                "    disabled_capabilities:\n      -\n        provider: p\n        model: m\n        capabilities:\n          - telepathy\n",
                "unknown capability 'telepathy'",
            ),
            (
                "    default_strategy: capability\n    rules:\n      -\n        kind: chat\n        provider: p\n        model: m\n",
                "would ignore them",
            ),
            ("    default_strategy: rule_based\n", "no rules"),
        ] {
            let err = parse(yaml).unwrap_err().to_string();
            assert!(err.contains(needle), "{yaml:?} gave {err:?}");
        }
    }

    #[test]
    fn rules_select_the_composite_strategy() {
        let cfg = parse(
            "    fallback_depth: 1\n    rules:\n      -\n        kind: codegen\n        provider: p\n        model: big\n      -\n        tags:\n          - sensitive\n        provider: local\n        model: small\n    default_model:\n      provider: p\n      model: small\n",
        )
        .unwrap();
        assert_eq!(cfg.fallback_depth, Some(1));
        assert_eq!(cfg.rules.len(), 2);
        assert_eq!(cfg.rules[1].tags, vec!["sensitive".to_string()]);
        assert_eq!(cfg.strategy().unwrap().name(), "composite");
        let rb = RoutingConfig {
            default_strategy: Some("rule_based".into()),
            ..cfg
        };
        assert_eq!(rb.strategy().unwrap().name(), "rule_based");
    }

    #[test]
    fn tenants_parse_and_validate() {
        let cfg = parse(
            "    tenants:\n      -\n        name: acme\n        allowed_providers:\n          - ollama\n        allowed_models:\n          -\n            provider: claude\n            model: claude-haiku-5-5\n        strategy: capability\n        priority: cost\n",
        )
        .unwrap();
        assert_eq!(cfg.tenants.len(), 1);
        let policies = cfg.tenant_policies().unwrap();
        let (name, policy) = &policies[0];
        assert_eq!(name, "acme");
        assert!(policy.permits("ollama", "anything"));
        assert!(policy.permits("claude", "claude-haiku-5-5"));
        assert!(!policy.permits("claude", "claude-opus-5-5"));
        assert_eq!(policy.priority_override(), Some(Priority::Cost));
        assert_eq!(policy.strategy_override().unwrap().name(), "capability");

        // An empty allow-list allows nothing; an absent one allows all.
        let cfg = parse(
            "    tenants:\n      -\n        name: none\n        allowed_providers: []\n      -\n        name: open\n        priority: cost\n",
        )
        .unwrap();
        assert_eq!(cfg.tenants[0].allowed_providers, Some(Vec::new()));
        assert_eq!(cfg.tenants[0].allowed_models, None);
        let policies = cfg.tenant_policies().unwrap();
        assert!(policies[0].1.is_restricted());
        assert!(!policies[0].1.permits("ollama", "anything"));
        assert!(!policies[1].1.is_restricted());
        assert!(policies[1].1.permits("ollama", "anything"));

        for (yaml, needle) in [
            (
                "    tenants:\n      -\n        name: a\n        priority: fastest\n",
                "unknown priority 'fastest'",
            ),
            (
                "    tenants:\n      -\n        name: a\n        strategy: magic\n",
                "unknown strategy 'magic'",
            ),
            (
                "    tenants:\n      -\n        name: a\n        strategy: rule_based\n",
                "needs rules",
            ),
            (
                "    tenants:\n      -\n        name: a\n      -\n        name: a\n",
                "configured twice",
            ),
            (
                "    tenants:\n      -\n        name: a\n        budget: 3\n",
                "unknown key 'budget'",
            ),
        ] {
            let err = parse(yaml).unwrap_err().to_string();
            assert!(err.contains(needle), "{yaml:?} gave {err:?}");
        }
    }
}
