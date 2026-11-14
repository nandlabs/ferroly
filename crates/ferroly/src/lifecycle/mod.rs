//! Component lifecycle orchestration with dependency ordering, supervision,
//! graceful shutdown and health probes.
//!
//! Register [`Component`]s with a [`ComponentManager`], declare dependencies,
//! and start/stop them in the correct order:
//!
//! ```
//! use std::sync::Arc;
//! use ferroly::lifecycle::{ComponentManager, SimpleComponent};
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), ferroly::lifecycle::LifecycleError> {
//! let mgr = ComponentManager::new();
//! mgr.register(Arc::new(SimpleComponent::new(
//!     "db",
//!     || Box::pin(async { Ok(()) }),
//!     || Box::pin(async { Ok(()) }),
//! )))?;
//! mgr.register(Arc::new(SimpleComponent::new(
//!     "api",
//!     || Box::pin(async { Ok(()) }),
//!     || Box::pin(async { Ok(()) }),
//! )))?;
//! mgr.add_dependency("api", "db")?; // db starts before api
//!
//! mgr.start_all().await?;
//! assert!(mgr.is_ready()); // every component is Running
//! mgr.stop_all().await?;
//! # Ok(())
//! # }
//! ```
//!
//! What the manager guarantees:
//!
//! - **No stuck states.** A start or stop that fails, panics, times out or is
//!   cancelled leaves the component in [`ComponentState::Error`], never in
//!   `Starting`/`Stopping`. A concurrent start of the same component waits for
//!   the real outcome.
//! - **Rollback.** A failed `start_all` stops what it had started, in reverse
//!   order (opt out with [`ManagerOptions::rollback_on_start_failure`]).
//! - **Best-effort shutdown.** `stop_all` attempts every component and reports
//!   all failures together; [`ComponentManager::wait`] is latched.
//! - **Health from state.** [`ComponentManager::health`] and
//!   [`HealthRegistry::register_components`] tie readiness to component state.

#![deny(missing_docs)]

mod component;
mod error;
mod health;
mod manager;
mod state;

pub use component::{BoxFuture, Component, ComponentOptions, RestartPolicy, SimpleComponent};
pub use error::{LifecycleError, LifecycleErrors};
pub use health::{
    CheckOptions, HealthRegistry, HealthReport, HealthStatus, ProbeKind, DEFAULT_CHECK_TIMEOUT,
};
pub use manager::{
    ComponentManager, LifecycleObserver, ManagerOptions, ShutdownHandle, DEFAULT_STOP_TIMEOUT,
};
pub use state::{ComponentState, StateChange};
