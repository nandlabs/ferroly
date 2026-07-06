//! Component lifecycle orchestration with dependency ordering.
//!
//! Register [`Component`]s with a [`ComponentManager`], declare dependencies,
//! and start/stop them in the correct order:
//!
//! ```
//! use std::sync::Arc;
//! use ferroly::lifecycle::{ComponentManager, SimpleComponent};
//!
//! # #[tokio::main]
//! # async fn main() {
//! let mgr = ComponentManager::new();
//! mgr.register(Arc::new(SimpleComponent::new(
//!     "db",
//!     || Box::pin(async { Ok(()) }),
//!     || Box::pin(async { Ok(()) }),
//! )));
//! mgr.register(Arc::new(SimpleComponent::new(
//!     "api",
//!     || Box::pin(async { Ok(()) }),
//!     || Box::pin(async { Ok(()) }),
//! )));
//! mgr.add_dependency("api", "db").unwrap(); // db starts before api
//!
//! mgr.start_all().await.unwrap();
//! mgr.stop_all().await.unwrap();
//! # }
//! ```

#![deny(missing_docs)]

mod component;
mod error;
mod health;
mod manager;
mod state;

pub use component::{BoxFuture, Component, SimpleComponent};
pub use error::LifecycleError;
pub use health::{HealthRegistry, HealthStatus};
pub use manager::{ComponentManager, DEFAULT_STOP_TIMEOUT};
pub use state::ComponentState;
