#![cfg(feature = "lifecycle")]
use std::sync::{Arc, Mutex};

use ferroly::lifecycle::{
    BoxFuture, Component, ComponentManager, ComponentState, LifecycleError, SimpleComponent,
};

/// Records the global order in which components start/stop.
struct Recorder {
    id: String,
    log: Arc<Mutex<Vec<String>>>,
}

impl Component for Recorder {
    fn id(&self) -> &str {
        &self.id
    }
    fn start(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            self.log.lock().unwrap().push(format!("start:{}", self.id));
            Ok(())
        })
    }
    fn stop(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            self.log.lock().unwrap().push(format!("stop:{}", self.id));
            Ok(())
        })
    }
}

#[tokio::test]
async fn starts_dependencies_first_and_stops_in_reverse() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mgr = ComponentManager::new();

    for id in ["db", "cache", "api"] {
        mgr.register(Arc::new(Recorder {
            id: id.into(),
            log: log.clone(),
        }));
    }
    mgr.add_dependency("api", "db").unwrap();
    mgr.add_dependency("api", "cache").unwrap();

    mgr.start_all().await.unwrap();
    mgr.stop_all().await.unwrap();

    let events = log.lock().unwrap().clone();
    let start_api = events.iter().position(|e| e == "start:api").unwrap();
    let start_db = events.iter().position(|e| e == "start:db").unwrap();
    let start_cache = events.iter().position(|e| e == "start:cache").unwrap();
    assert!(start_db < start_api, "db must start before api");
    assert!(start_cache < start_api, "cache must start before api");

    let stop_api = events.iter().position(|e| e == "stop:api").unwrap();
    let stop_db = events.iter().position(|e| e == "stop:db").unwrap();
    assert!(stop_api < stop_db, "api must stop before db");
}

#[tokio::test]
async fn detects_cyclic_dependency() {
    let mgr = ComponentManager::new();
    mgr.register(Arc::new(SimpleComponent::new(
        "a",
        || Box::pin(async { Ok(()) }),
        || Box::pin(async { Ok(()) }),
    )));
    mgr.register(Arc::new(SimpleComponent::new(
        "b",
        || Box::pin(async { Ok(()) }),
        || Box::pin(async { Ok(()) }),
    )));
    mgr.add_dependency("a", "b").unwrap();
    let err = mgr.add_dependency("b", "a").unwrap_err();
    assert!(matches!(err, LifecycleError::CyclicDependency(_)));
}

#[tokio::test]
async fn tracks_state_and_publishes_to_watch() {
    let mgr = Arc::new(ComponentManager::new());
    mgr.register(Arc::new(SimpleComponent::new(
        "svc",
        || Box::pin(async { Ok(()) }),
        || Box::pin(async { Ok(()) }),
    )));

    // A watch subscriber sees the current state immediately and each publish.
    let mut rx = mgr.watch("svc").unwrap();
    assert_eq!(*rx.borrow(), ComponentState::Unknown);
    assert_eq!(mgr.get_state("svc"), Some(ComponentState::Unknown));

    mgr.start("svc").await.unwrap();
    assert_eq!(mgr.get_state("svc"), Some(ComponentState::Running));
    // `transition` publishes synchronously, so the latest value is Running.
    assert_eq!(*rx.borrow_and_update(), ComponentState::Running);

    mgr.stop("svc").await.unwrap();
    assert_eq!(mgr.get_state("svc"), Some(ComponentState::Stopped));
    assert_eq!(*rx.borrow(), ComponentState::Stopped);
    assert!(rx.has_changed().unwrap());
}

#[tokio::test]
async fn double_start_errors() {
    let mgr = ComponentManager::new();
    mgr.register(Arc::new(SimpleComponent::new(
        "once",
        || Box::pin(async { Ok(()) }),
        || Box::pin(async { Ok(()) }),
    )));
    mgr.start("once").await.unwrap();
    let err = mgr.start("once").await.unwrap_err();
    assert!(matches!(err, LifecycleError::ComponentAlreadyStarted(_)));
}

fn ok_component(id: &str) -> Arc<SimpleComponent> {
    Arc::new(SimpleComponent::new(
        id,
        || Box::pin(async { Ok(()) }),
        || Box::pin(async { Ok(()) }),
    ))
}

#[tokio::test]
async fn unregister_list_and_missing() {
    let mgr = ComponentManager::new();
    mgr.register(ok_component("a"));
    mgr.register(ok_component("b"));

    let mut ids = mgr.list();
    ids.sort();
    assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(mgr.get_state("a"), Some(ComponentState::Unknown));

    mgr.unregister("a");
    assert_eq!(mgr.get_state("a"), None);

    assert!(matches!(
        mgr.start("ghost").await,
        Err(LifecycleError::ComponentNotFound(_))
    ));
    assert!(matches!(
        mgr.stop("ghost").await,
        Err(LifecycleError::ComponentNotFound(_))
    ));
}

#[tokio::test]
async fn start_failure_sets_error_state() {
    let mgr = ComponentManager::new();
    mgr.register(Arc::new(SimpleComponent::new(
        "bad",
        || {
            Box::pin(async {
                Err(LifecycleError::failure(
                    "bad",
                    std::io::Error::other("boom"),
                ))
            })
        },
        || Box::pin(async { Ok(()) }),
    )));
    assert!(mgr.start("bad").await.is_err());
    assert_eq!(mgr.get_state("bad"), Some(ComponentState::Error));
}

#[test]
fn error_display_and_state_predicates() {
    let errors = [
        LifecycleError::ComponentNotFound("a".into()),
        LifecycleError::ComponentAlreadyStarted("a".into()),
        LifecycleError::CyclicDependency("a".into()),
        LifecycleError::Timeout("a".into()),
        LifecycleError::failure("a", std::io::Error::other("boom")),
    ];
    for e in &errors {
        assert!(!e.to_string().is_empty());
    }
    // #[source] is threaded through by the derive.
    let f = LifecycleError::failure("a", std::io::Error::other("inner"));
    assert!(std::error::Error::source(&f).is_some());

    assert!(ComponentState::Running.is_running());
    assert!(!ComponentState::Stopped.is_running());
    assert!(!ComponentState::Starting.is_running());
}

#[tokio::test]
async fn start_all_with_timeout_ok_and_elapsed() {
    use std::time::Duration;
    let mgr = ComponentManager::new();
    mgr.register(ok_component("g"));
    mgr.start_all_with_timeout(Duration::from_secs(5))
        .await
        .unwrap();
    mgr.stop_all().await.unwrap();

    let slow = ComponentManager::new();
    slow.register(Arc::new(SimpleComponent::new(
        "slow",
        || {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(())
            })
        },
        || Box::pin(async { Ok(()) }),
    )));
    let err = slow
        .start_all_with_timeout(Duration::from_millis(20))
        .await
        .unwrap_err();
    assert!(matches!(err, LifecycleError::Timeout(_)));
}

#[tokio::test]
async fn stop_all_with_timeout_bounds_a_hung_stop() {
    use std::time::Duration;

    let mgr = ComponentManager::new();
    // A component whose stop() never returns must not block shutdown forever.
    mgr.register(Arc::new(SimpleComponent::new(
        "hangs",
        || Box::pin(async { Ok(()) }),
        || {
            Box::pin(async {
                std::future::pending::<()>().await;
                Ok(())
            })
        },
    )));
    // A well-behaved component should still be stopped by the sweep.
    mgr.register(ok_component("clean"));

    mgr.start_all().await.unwrap();

    let err = mgr
        .stop_all_with_timeout(Duration::from_millis(50))
        .await
        .unwrap_err();
    assert!(matches!(err, LifecycleError::Timeout(_)));

    // The hung component is marked Error; the clean one still reached Stopped.
    assert_eq!(mgr.get_state("hangs"), Some(ComponentState::Error));
    assert_eq!(mgr.get_state("clean"), Some(ComponentState::Stopped));
}
