//! The [`ComponentManager`] — registration, dependency ordering, and shutdown.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{watch, Notify};

use ferroly::lifecycle::{Component, ComponentState, LifecycleError};

/// Default per-component stop deadline used by
/// [`ComponentManager::start_and_wait`].
pub const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Inner {
    components: HashMap<String, Arc<dyn Component>>,
    /// `deps[id]` = the set of ids that `id` depends on.
    deps: HashMap<String, HashSet<String>>,
    /// Per-component state, published to subscribers via [`watch`].
    /// The sender's current value is the authoritative state.
    watchers: HashMap<String, watch::Sender<ComponentState>>,
}

/// Orchestrates a set of [`Component`]s with dependency-aware start/stop.
///
/// On start, dependencies start before dependents; on stop, the order reverses.
/// Cycles are rejected by [`add_dependency`](ComponentManager::add_dependency).
pub struct ComponentManager {
    inner: Mutex<Inner>,
    shutdown: Notify,
}

impl Default for ComponentManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ComponentManager {
    /// Creates an empty manager.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            shutdown: Notify::new(),
        }
    }

    /// Registers a component, initially in the [`ComponentState::Unknown`] state.
    pub fn register(&self, component: Arc<dyn Component>) {
        let id = component.id().to_string();
        let mut inner = self.inner.lock().unwrap();
        let (tx, _rx) = watch::channel(ComponentState::Unknown);
        inner.watchers.insert(id.clone(), tx);
        inner.deps.entry(id.clone()).or_default();
        inner.components.insert(id, component);
    }

    /// Removes a component and any dependency edges referencing it.
    pub fn unregister(&self, id: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.components.remove(id);
        inner.watchers.remove(id);
        inner.deps.remove(id);
        for set in inner.deps.values_mut() {
            set.remove(id);
        }
    }

    /// Declares that `id` depends on `depends_on` (so `depends_on` starts first).
    ///
    /// Returns [`LifecycleError::CyclicDependency`] if the edge would introduce
    /// a cycle, and [`LifecycleError::ComponentNotFound`] if either id is
    /// unregistered.
    pub fn add_dependency(&self, id: &str, depends_on: &str) -> Result<(), LifecycleError> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.components.contains_key(id) {
            return Err(LifecycleError::ComponentNotFound(id.to_string()));
        }
        if !inner.components.contains_key(depends_on) {
            return Err(LifecycleError::ComponentNotFound(depends_on.to_string()));
        }
        inner
            .deps
            .entry(id.to_string())
            .or_default()
            .insert(depends_on.to_string());

        // Validate acyclicity; revert the edge if a cycle appears.
        if topo_order(&inner.deps).is_err() {
            inner.deps.get_mut(id).unwrap().remove(depends_on);
            return Err(LifecycleError::CyclicDependency(format!(
                "{id} -> {depends_on}"
            )));
        }
        Ok(())
    }

    /// Returns the current state of a component, if registered.
    pub fn get_state(&self, id: &str) -> Option<ComponentState> {
        self.inner
            .lock()
            .unwrap()
            .watchers
            .get(id)
            .map(|tx| *tx.borrow())
    }

    /// Returns the ids of all registered components.
    pub fn list(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .components
            .keys()
            .cloned()
            .collect()
    }

    /// Subscribes to `id`'s state transitions, if registered.
    ///
    /// The returned [`watch::Receiver`] observes the current state immediately
    /// (via [`borrow`](watch::Receiver::borrow)) and each subsequent change
    /// (await [`changed`](watch::Receiver::changed)), an idiomatic channel in
    /// place of observer callbacks.
    pub fn watch(&self, id: &str) -> Option<watch::Receiver<ComponentState>> {
        self.inner
            .lock()
            .unwrap()
            .watchers
            .get(id)
            .map(|tx| tx.subscribe())
    }

    /// Starts a single component after its transitive dependencies.
    pub async fn start(&self, id: &str) -> Result<(), LifecycleError> {
        match self.get_state(id) {
            None => return Err(LifecycleError::ComponentNotFound(id.to_string())),
            Some(ComponentState::Running) => {
                return Err(LifecycleError::ComponentAlreadyStarted(id.to_string()))
            }
            _ => {}
        }
        for target in self.start_order_for(id)? {
            self.start_one(&target).await?;
        }
        Ok(())
    }

    /// Starts all registered components in dependency order.
    pub async fn start_all(&self) -> Result<(), LifecycleError> {
        let order = { topo_order(&self.inner.lock().unwrap().deps)? };
        for id in order {
            self.start_one(&id).await?;
        }
        Ok(())
    }

    /// Starts all registered components with an overall timeout.
    pub async fn start_all_with_timeout(&self, timeout: Duration) -> Result<(), LifecycleError> {
        with_timeout("<all>", timeout, self.start_all()).await
    }

    /// Stops a single component after its dependents.
    pub async fn stop(&self, id: &str) -> Result<(), LifecycleError> {
        if self.get_state(id).is_none() {
            return Err(LifecycleError::ComponentNotFound(id.to_string()));
        }
        // Stop dependents first: reverse of the start closure that reaches `id`.
        let mut order = self.stop_order_for(id)?;
        order.reverse();
        for target in order {
            self.stop_one(&target).await?;
        }
        Ok(())
    }

    /// Stops all registered components in reverse dependency order.
    pub async fn stop_all(&self) -> Result<(), LifecycleError> {
        let mut order = { topo_order(&self.inner.lock().unwrap().deps)? };
        order.reverse();
        for id in order {
            self.stop_one(&id).await?;
        }
        self.shutdown.notify_waiters();
        Ok(())
    }

    /// Stops all components in reverse dependency order, bounding **each**
    /// component's `stop()` by `per_component`. Unlike
    /// [`stop_all`](Self::stop_all), this is best-effort: a component that times
    /// out or errors is marked
    /// [`ComponentState::Error`] and the sweep continues, so one hung component
    /// cannot block shutdown until `SIGKILL`. The first error (if any) is
    /// returned once every component has been attempted.
    pub async fn stop_all_with_timeout(
        &self,
        per_component: Duration,
    ) -> Result<(), LifecycleError> {
        let mut order = { topo_order(&self.inner.lock().unwrap().deps)? };
        order.reverse();
        let mut first_err = None;
        for id in order {
            if let Err(e) = with_timeout(&id, per_component, self.stop_one(&id)).await {
                // On a timeout the dropped stop() left the component mid-stop;
                // reflect that as Error so state stays consistent.
                self.transition(&id, ComponentState::Error);
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        self.shutdown.notify_waiters();
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Blocks until [`stop_all`](ComponentManager::stop_all) completes.
    pub async fn wait(&self) {
        self.shutdown.notified().await;
    }

    /// Starts everything, then blocks until an OS signal (SIGINT/SIGTERM) or a
    /// [`stop_all`](ComponentManager::stop_all) from elsewhere, then stops
    /// everything with a bounded per-component stop ([`DEFAULT_STOP_TIMEOUT`],
    /// 30s) so a hung `stop()` cannot block shutdown until `SIGKILL`.
    pub async fn start_and_wait(&self) -> Result<(), LifecycleError> {
        self.start_all().await?;
        wait_for_signal().await;
        self.stop_all_with_timeout(DEFAULT_STOP_TIMEOUT).await
    }

    // ---- internals -------------------------------------------------------

    async fn start_one(&self, id: &str) -> Result<(), LifecycleError> {
        let comp = {
            let inner = self.inner.lock().unwrap();
            let comp = match inner.components.get(id) {
                Some(c) => c.clone(),
                None => return Err(LifecycleError::ComponentNotFound(id.to_string())),
            };
            // Atomically claim the start: check the state and set `Starting`
            // under a single lock hold, so two concurrent `start`s can't both
            // pass the idempotency guard and double-initialize the component.
            let mut won = false;
            if let Some(tx) = inner.watchers.get(id) {
                tx.send_if_modified(|cur| {
                    if matches!(*cur, ComponentState::Running | ComponentState::Starting) {
                        false // already (being) started — leave it, we didn't win
                    } else {
                        *cur = ComponentState::Starting;
                        won = true;
                        true
                    }
                });
            }
            if !won {
                return Ok(()); // idempotent for dependency starts
            }
            comp
        };

        match comp.start().await {
            Ok(()) => {
                self.transition(id, ComponentState::Running);
                Ok(())
            }
            Err(e) => {
                self.transition(id, ComponentState::Error);
                Err(e)
            }
        }
    }

    async fn stop_one(&self, id: &str) -> Result<(), LifecycleError> {
        let (comp, state) = {
            let inner = self.inner.lock().unwrap();
            let comp = match inner.components.get(id) {
                Some(c) => c.clone(),
                None => return Err(LifecycleError::ComponentNotFound(id.to_string())),
            };
            let state = inner
                .watchers
                .get(id)
                .map(|tx| *tx.borrow())
                .unwrap_or(ComponentState::Unknown);
            (comp, state)
        };

        if state == ComponentState::Stopped || state == ComponentState::Unknown {
            return Ok(()); // nothing running to stop
        }

        self.transition(id, ComponentState::Stopping);
        match comp.stop().await {
            Ok(()) => {
                self.transition(id, ComponentState::Stopped);
                Ok(())
            }
            Err(e) => {
                self.transition(id, ComponentState::Error);
                Err(e)
            }
        }
    }

    /// Updates a component's state, notifying [`watch`](Self::watch) subscribers
    /// only when the state actually changes.
    fn transition(&self, id: &str, new: ComponentState) {
        let inner = self.inner.lock().unwrap();
        if let Some(tx) = inner.watchers.get(id) {
            tx.send_if_modified(|cur| {
                let changed = *cur != new;
                *cur = new;
                changed
            });
        }
    }

    /// The dependency-first order needed to start `id` and its closure.
    fn start_order_for(&self, id: &str) -> Result<Vec<String>, LifecycleError> {
        let inner = self.inner.lock().unwrap();
        let closure = dependency_closure(&inner.deps, id);
        let full = topo_order(&inner.deps)?;
        Ok(full.into_iter().filter(|n| closure.contains(n)).collect())
    }

    /// The dependency-first order of `id` plus everything that depends on it.
    fn stop_order_for(&self, id: &str) -> Result<Vec<String>, LifecycleError> {
        let inner = self.inner.lock().unwrap();
        let dependents = dependent_closure(&inner.deps, id);
        let full = topo_order(&inner.deps)?;
        Ok(full
            .into_iter()
            .filter(|n| dependents.contains(n))
            .collect())
    }
}

async fn with_timeout<F>(id: &str, timeout: Duration, fut: F) -> Result<(), LifecycleError>
where
    F: std::future::Future<Output = Result<(), LifecycleError>>,
{
    match tokio::time::timeout(timeout, fut).await {
        Ok(res) => res,
        Err(_) => Err(LifecycleError::Timeout(id.to_string())),
    }
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Kahn's algorithm producing a dependencies-first ordering, or a cycle error.
fn topo_order(deps: &HashMap<String, HashSet<String>>) -> Result<Vec<String>, LifecycleError> {
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();

    for (id, id_deps) in deps {
        in_degree.entry(id.as_str()).or_insert(0);
        for d in id_deps {
            *in_degree.entry(id.as_str()).or_insert(0) += 1;
            dependents.entry(d.as_str()).or_default().push(id.as_str());
            in_degree.entry(d.as_str()).or_insert(0);
        }
    }

    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(&n, _)| n)
        .collect();
    // Deterministic ordering aids reproducible tests.
    let mut sorted_seed: Vec<&str> = queue.drain(..).collect();
    sorted_seed.sort_unstable();
    queue.extend(sorted_seed);

    let mut order = Vec::new();
    while let Some(n) = queue.pop_front() {
        order.push(n.to_string());
        if let Some(deps_of) = dependents.get(n) {
            let mut ready: Vec<&str> = Vec::new();
            for &m in deps_of {
                let e = in_degree.get_mut(m).unwrap();
                *e -= 1;
                if *e == 0 {
                    ready.push(m);
                }
            }
            ready.sort_unstable();
            queue.extend(ready);
        }
    }

    if order.len() != in_degree.len() {
        let unresolved: Vec<String> = in_degree
            .iter()
            .filter(|(_, &d)| d > 0)
            .map(|(&n, _)| n.to_string())
            .collect();
        return Err(LifecycleError::CyclicDependency(unresolved.join(", ")));
    }
    Ok(order)
}

/// All transitive dependencies of `id`, including `id` itself.
fn dependency_closure(deps: &HashMap<String, HashSet<String>>, id: &str) -> HashSet<String> {
    let mut seen = HashSet::new();
    let mut stack = vec![id.to_string()];
    while let Some(n) = stack.pop() {
        if !seen.insert(n.clone()) {
            continue;
        }
        if let Some(ds) = deps.get(&n) {
            for d in ds {
                stack.push(d.clone());
            }
        }
    }
    seen
}

/// `id` plus everything that transitively depends on `id`.
fn dependent_closure(deps: &HashMap<String, HashSet<String>>, id: &str) -> HashSet<String> {
    let mut seen = HashSet::new();
    let mut stack = vec![id.to_string()];
    while let Some(n) = stack.pop() {
        if !seen.insert(n.clone()) {
            continue;
        }
        for (other, ds) in deps {
            if ds.contains(&n) {
                stack.push(other.clone());
            }
        }
    }
    seen
}
