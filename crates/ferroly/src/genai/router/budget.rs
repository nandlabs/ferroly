//! Cumulative budget tracking for the router.
//!
//! [`Task::max_cost_usd`](super::Task::max_cost_usd) bounds the *estimated*
//! cost of one call. A [`Budget`] bounds what a key — a tenant, a user, an API
//! key — spends **over time**: the router refuses a call when the key's
//! recorded spend has reached its limit, and records each call's actual cost
//! afterwards.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferroly::genai::BoxFuture;

/// Where cumulative spend is kept. Implement it over a shared database to
/// enforce one budget across several processes; [`InMemoryBudgetStore`] is the
/// per-process default.
pub trait BudgetStore: Send + Sync {
    /// What `key` has spent in the current period, in USD.
    fn spent<'a>(&'a self, key: &'a str) -> BoxFuture<'a, f64>;

    /// Adds `usd` to what `key` has spent and returns the new total.
    fn record<'a>(&'a self, key: &'a str, usd: f64) -> BoxFuture<'a, f64>;
}

/// A [`BudgetStore`] kept in process memory.
///
/// Without a period the totals only grow (until [`reset`](Self::reset)); with
/// [`with_period`](Self::with_period) each key's total starts again from zero
/// once that long has passed since its first spend of the current period — a
/// simple fixed window, e.g. 30 days for a monthly cap.
#[derive(Debug, Default)]
pub struct InMemoryBudgetStore {
    spent: Mutex<HashMap<String, (f64, Instant)>>,
    period: Option<Duration>,
}

impl InMemoryBudgetStore {
    /// A store whose totals never reset by themselves.
    pub fn new() -> Self {
        InMemoryBudgetStore::default()
    }

    /// A store whose per-key totals reset every `period`.
    pub fn with_period(period: Duration) -> Self {
        InMemoryBudgetStore {
            spent: Mutex::new(HashMap::new()),
            period: Some(period),
        }
    }

    /// Clears the spend of one key.
    pub fn reset(&self, key: &str) {
        if let Ok(mut map) = self.spent.lock() {
            map.remove(key);
        }
    }

    /// Clears every key.
    pub fn reset_all(&self) {
        if let Ok(mut map) = self.spent.lock() {
            map.clear();
        }
    }

    /// The current total of every key, sorted by key.
    pub fn snapshot(&self) -> Vec<(String, f64)> {
        let mut out: Vec<(String, f64)> = match self.spent.lock() {
            Ok(map) => map
                .iter()
                .filter(|(_, (_, start))| !self.expired(*start))
                .map(|(k, (v, _))| (k.clone(), *v))
                .collect(),
            Err(_) => Vec::new(),
        };
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn expired(&self, start: Instant) -> bool {
        self.period.is_some_and(|p| start.elapsed() >= p)
    }

    fn spent_now(&self, key: &str) -> f64 {
        match self.spent.lock() {
            Ok(map) => match map.get(key) {
                Some((total, start)) if !self.expired(*start) => *total,
                _ => 0.0,
            },
            Err(_) => 0.0,
        }
    }

    fn record_now(&self, key: &str, usd: f64) -> f64 {
        let usd = if usd.is_finite() && usd > 0.0 {
            usd
        } else {
            0.0
        };
        let Ok(mut map) = self.spent.lock() else {
            return usd;
        };
        let entry = map
            .entry(key.to_string())
            .or_insert_with(|| (0.0, Instant::now()));
        if self.expired(entry.1) {
            *entry = (0.0, Instant::now());
        }
        entry.0 += usd;
        entry.0
    }
}

impl BudgetStore for InMemoryBudgetStore {
    fn spent<'a>(&'a self, key: &'a str) -> BoxFuture<'a, f64> {
        Box::pin(std::future::ready(self.spent_now(key)))
    }

    fn record<'a>(&'a self, key: &'a str, usd: f64) -> BoxFuture<'a, f64> {
        Box::pin(std::future::ready(self.record_now(key, usd)))
    }
}

/// A cumulative spending policy: a store plus per-key limits.
///
/// ```
/// use std::sync::Arc;
/// use ferroly::genai::router::{Budget, InMemoryBudgetStore};
///
/// let budget = Budget::new(Arc::new(InMemoryBudgetStore::new()))
///     .default_limit_usd(5.0)          // every key …
///     .limit_usd("tenant-gold", 250.0); // … except this one
/// assert_eq!(budget.limit_for("tenant-gold"), Some(250.0));
/// assert_eq!(budget.limit_for("anyone-else"), Some(5.0));
/// ```
#[derive(Clone)]
pub struct Budget {
    store: Arc<dyn BudgetStore>,
    default_limit: Option<f64>,
    limits: HashMap<String, f64>,
}

impl std::fmt::Debug for Budget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Budget")
            .field("default_limit", &self.default_limit)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl Budget {
    /// A budget over `store` with no limits yet (spend is recorded but never
    /// refused).
    pub fn new(store: Arc<dyn BudgetStore>) -> Self {
        Budget {
            store,
            default_limit: None,
            limits: HashMap::new(),
        }
    }

    /// A budget over a fresh [`InMemoryBudgetStore`].
    pub fn in_memory() -> Self {
        Budget::new(Arc::new(InMemoryBudgetStore::new()))
    }

    /// The limit, in USD, for keys without one of their own.
    pub fn default_limit_usd(mut self, usd: f64) -> Self {
        self.default_limit = Some(usd);
        self
    }

    /// The limit, in USD, for one key.
    pub fn limit_usd(mut self, key: impl Into<String>, usd: f64) -> Self {
        self.limits.insert(key.into(), usd);
        self
    }

    /// The limit that applies to `key`, if any.
    pub fn limit_for(&self, key: &str) -> Option<f64> {
        self.limits.get(key).copied().or(self.default_limit)
    }

    /// The store behind this budget.
    pub fn store(&self) -> &Arc<dyn BudgetStore> {
        &self.store
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_store_accumulates_and_resets() {
        let store = InMemoryBudgetStore::new();
        assert_eq!(store.spent("a").await, 0.0);
        assert_eq!(store.record("a", 0.5).await, 0.5);
        assert_eq!(store.record("a", 0.25).await, 0.75);
        // Garbage amounts never reduce or poison the total.
        assert_eq!(store.record("a", f64::NAN).await, 0.75);
        assert_eq!(store.record("a", -3.0).await, 0.75);
        assert_eq!(store.spent("b").await, 0.0);
        assert_eq!(store.snapshot(), vec![("a".to_string(), 0.75)]);
        store.reset("a");
        assert_eq!(store.spent("a").await, 0.0);
    }

    #[tokio::test]
    async fn period_starts_a_new_window() {
        let store = InMemoryBudgetStore::with_period(Duration::from_millis(400));
        store.record("a", 1.0).await;
        assert_eq!(store.spent("a").await, 1.0);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(store.spent("a").await, 0.0);
        assert_eq!(store.record("a", 0.1).await, 0.1);
    }
}
