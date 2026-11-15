//! Per-tenant routing policy: which providers and models a tenant may use,
//! and the strategy and priority its calls are routed with.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use super::strategy::RoutingStrategy;
use super::task::Priority;

/// The routing policy of one tenant, applied to every [`Task`](super::Task)
/// whose [`tenant`](super::Task::tenant) names it, by both the completion
/// router ([`RouterBuilder::tenant_policy`](super::RouterBuilder::tenant_policy))
/// and the [`EmbeddingRouter`](super::EmbeddingRouter).
///
/// - **Allowed models.** A policy with an allow-list (any
///   [`allow_provider`](Self::allow_provider) /
///   [`allow_model`](Self::allow_model), or [`deny_all`](Self::deny_all))
///   makes only those models eligible: a candidate passes if its provider is
///   allowed or the exact `provider/model` is. Every other candidate is
///   excluded (with the reason in the decision trace), and a pin to a model
///   outside the list is refused. [`deny_all`](Self::deny_all) on its own is
///   an empty allow-list: nothing is eligible. A policy without an
///   allow-list ([`new`](Self::new)) allows everything.
/// - **Strategy.** Replaces the router's strategy for the tenant's calls.
/// - **Priority.** Replaces the task's [`Priority`].
///
/// A task naming a tenant that has no policy is routed under the router's
/// defaults, unless the router requires a policy
/// ([`RouterBuilder::require_tenant_policy`](super::RouterBuilder::require_tenant_policy)).
///
/// ```
/// use ferroly::genai::router::{Priority, TenantPolicy};
///
/// let policy = TenantPolicy::new()
///     .allow_provider("ollama")
///     .allow_model("claude", "claude-haiku-5-5")
///     .priority(Priority::Cost);
/// assert!(policy.permits("ollama", "llama3"));
/// assert!(policy.permits("claude", "claude-haiku-5-5"));
/// assert!(!policy.permits("claude", "claude-opus-5-5"));
/// assert!(TenantPolicy::new().permits("any", "model"));
/// assert!(!TenantPolicy::deny_all().permits("any", "model"));
/// ```
#[derive(Clone, Default)]
pub struct TenantPolicy {
    /// Whether the policy has an allow-list (possibly empty).
    restricted: bool,
    allowed_providers: Vec<String>,
    allowed_models: Vec<(String, String)>,
    strategy: Option<Arc<dyn RoutingStrategy>>,
    priority: Option<Priority>,
}

impl fmt::Debug for TenantPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TenantPolicy")
            .field("restricted", &self.restricted)
            .field("allowed_providers", &self.allowed_providers)
            .field("allowed_models", &self.allowed_models)
            .field("strategy", &self.strategy.as_ref().map(|s| s.name()))
            .field("priority", &self.priority)
            .finish()
    }
}

impl TenantPolicy {
    /// A policy without an allow-list: every model allowed, router defaults
    /// otherwise.
    pub fn new() -> Self {
        TenantPolicy::default()
    }

    /// A policy with an empty allow-list: no model is eligible until
    /// [`allow_provider`](Self::allow_provider) or
    /// [`allow_model`](Self::allow_model) adds one.
    pub fn deny_all() -> Self {
        TenantPolicy {
            restricted: true,
            ..TenantPolicy::default()
        }
    }

    /// Allows every model of `provider` (and turns the allow-list on).
    pub fn allow_provider(mut self, provider: impl Into<String>) -> Self {
        self.restricted = true;
        self.allowed_providers.push(provider.into());
        self
    }

    /// Allows one `provider/model` (and turns the allow-list on).
    pub fn allow_model(mut self, provider: impl Into<String>, model: impl Into<String>) -> Self {
        self.restricted = true;
        self.allowed_models.push((provider.into(), model.into()));
        self
    }

    /// Routes the tenant's calls with `strategy` instead of the router's.
    pub fn strategy(mut self, strategy: Box<dyn RoutingStrategy>) -> Self {
        self.strategy = Some(Arc::from(strategy));
        self
    }

    /// Routes the tenant's calls with `priority`, whatever the task says.
    pub fn priority(mut self, priority: Priority) -> Self {
        self.priority = Some(priority);
        self
    }

    /// Whether the policy has an allow-list, so that only the models on it
    /// are eligible. `true` for [`deny_all`](Self::deny_all) even with
    /// nothing allowed.
    pub fn is_restricted(&self) -> bool {
        self.restricted
    }

    /// Whether the tenant may use `provider/model`.
    pub fn permits(&self, provider: &str, model: &str) -> bool {
        !self.restricted
            || self.allowed_providers.iter().any(|p| p == provider)
            || self
                .allowed_models
                .iter()
                .any(|(p, m)| p == provider && m == model)
    }

    /// The allowed providers.
    pub fn allowed_providers(&self) -> &[String] {
        &self.allowed_providers
    }

    /// The allowed `(provider, model)` pairs.
    pub fn allowed_models(&self) -> &[(String, String)] {
        &self.allowed_models
    }

    /// The priority the policy imposes, if any.
    pub fn priority_override(&self) -> Option<Priority> {
        self.priority
    }

    /// The strategy the policy imposes, if any.
    pub fn strategy_override(&self) -> Option<&dyn RoutingStrategy> {
        self.strategy.as_deref()
    }

    pub(crate) fn strategy_arc(&self) -> Option<Arc<dyn RoutingStrategy>> {
        self.strategy.clone()
    }

    /// `self` laid over `base`: each part `self` sets (the allow-list, the
    /// strategy, the priority) replaces `base`'s, and each part it leaves
    /// unset keeps `base`'s. A configured tenant is laid over a
    /// code-registered one of the same name this way, so a configuration
    /// entry that only sets a priority keeps the code's allow-list.
    pub(crate) fn over(self, base: &TenantPolicy) -> TenantPolicy {
        let (restricted, allowed_providers, allowed_models) = if self.restricted {
            (true, self.allowed_providers, self.allowed_models)
        } else {
            (
                base.restricted,
                base.allowed_providers.clone(),
                base.allowed_models.clone(),
            )
        };
        TenantPolicy {
            restricted,
            allowed_providers,
            allowed_models,
            strategy: self.strategy.or_else(|| base.strategy.clone()),
            priority: self.priority.or(base.priority),
        }
    }
}

/// The policies in force: the code-registered ones with the configured ones
/// laid over them ([`TenantPolicy::over`]).
pub(crate) fn merge_tenants(
    code: &HashMap<String, TenantPolicy>,
    configured: Vec<(String, TenantPolicy)>,
) -> HashMap<String, TenantPolicy> {
    let mut tenants = code.clone();
    for (name, policy) in configured {
        let merged = match code.get(&name) {
            Some(base) => policy.over(base),
            None => policy,
        };
        tenants.insert(name, merged);
    }
    tenants
}

/// The policy a task is routed under, or why it may not be routed: a task
/// naming a tenant without a policy is refused when `require` is set.
pub(crate) fn policy_for<'a>(
    tenants: &'a HashMap<String, TenantPolicy>,
    tenant: Option<&str>,
    require: bool,
) -> Result<Option<&'a TenantPolicy>, super::RouterError> {
    let Some(tenant) = tenant else {
        return Ok(None);
    };
    match tenants.get(tenant) {
        Some(policy) => Ok(Some(policy)),
        None if require => Err(super::RouterError::NoRoute {
            reason: format!("tenant '{tenant}' has no routing policy and the router requires one"),
        }),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_all_is_an_empty_allow_list() {
        let none = TenantPolicy::deny_all();
        assert!(none.is_restricted());
        assert!(!none.permits("ollama", "llama3"));
        let one = TenantPolicy::deny_all().allow_provider("ollama");
        assert!(one.permits("ollama", "llama3"));
        assert!(!one.permits("claude", "x"));
        assert!(!TenantPolicy::new().priority(Priority::Cost).is_restricted());
    }

    #[test]
    fn a_policy_laid_over_another_keeps_what_it_leaves_unset() {
        let code = TenantPolicy::new()
            .allow_provider("ollama")
            .priority(Priority::Speed);
        let only_priority = TenantPolicy::new().priority(Priority::Cost).over(&code);
        assert!(only_priority.permits("ollama", "x"));
        assert!(!only_priority.permits("claude", "x"));
        assert_eq!(only_priority.priority_override(), Some(Priority::Cost));

        let new_list = TenantPolicy::new().allow_provider("claude").over(&code);
        assert!(new_list.permits("claude", "x"));
        assert!(!new_list.permits("ollama", "x"));
        assert_eq!(new_list.priority_override(), Some(Priority::Speed));

        assert!(!TenantPolicy::deny_all().over(&code).permits("ollama", "x"));
    }
}
