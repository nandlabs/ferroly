//! An immutable, name-indexed set of providers.

use std::collections::HashMap;
use std::sync::Arc;

use ferroly::genai::GenAiProvider;

/// An immutable set of providers keyed by [`GenAiProvider::name`]. Explicit and
/// injectable — no package-global registry.
#[derive(Clone)]
pub struct ProviderSet {
    by_name: HashMap<String, Arc<dyn GenAiProvider>>,
}

impl ProviderSet {
    /// Indexes `providers` by name (last wins on a duplicate name).
    pub fn new(providers: Vec<Arc<dyn GenAiProvider>>) -> Self {
        let mut by_name = HashMap::with_capacity(providers.len());
        for p in providers {
            by_name.insert(p.name().to_string(), p);
        }
        ProviderSet { by_name }
    }

    /// The provider registered under `name`, if any.
    pub fn get(&self, name: &str) -> Option<Arc<dyn GenAiProvider>> {
        self.by_name.get(name).cloned()
    }

    /// Whether a provider named `name` is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// All providers, stable-sorted by name.
    pub fn all(&self) -> Vec<Arc<dyn GenAiProvider>> {
        let mut v: Vec<Arc<dyn GenAiProvider>> = self.by_name.values().cloned().collect();
        v.sort_by(|a, b| a.name().cmp(b.name()));
        v
    }
}
