//! Async tests in a crate whose only dependency is `ferroly` (feature `rt`).

use ferroly::rt::{self, sync::mpsc, time::Duration};

#[ferroly::rt::test]
async fn bare_attribute() {
    let (tx, mut rx) = mpsc::channel::<u32>(1);
    rt::spawn(async move {
        tx.send(7).await.ok();
    });
    assert_eq!(rx.recv().await, Some(7));
}

#[ferroly::rt::test(flavor = "multi_thread", worker_threads = 2)]
async fn attribute_with_options() {
    let handles: Vec<_> = (0..4).map(|i| rt::spawn(async move { i * 2 })).collect();
    let mut total = 0;
    for handle in handles {
        total += handle.await.unwrap();
    }
    assert_eq!(total, 12);
}

#[ferroly::rt::test(crate = "ferroly::rt::__tokio")]
async fn attribute_with_explicit_crate_path() {
    rt::yield_now().await;
}

#[ferroly::rt::test]
#[should_panic(expected = "boom")]
async fn attribute_should_panic() {
    rt::yield_now().await;
    panic!("boom");
}

ferroly::rt::async_test! {
    async fn declarative_macro() {
        let slept = rt::time::timeout(
            Duration::from_millis(5),
            rt::time::sleep(Duration::from_secs(60)),
        )
        .await;
        assert!(slept.is_err());
    }

    async fn declarative_macro_with_result() -> Result<(), std::num::ParseIntError> {
        let n: u8 = "9".parse()?;
        assert_eq!(n, 9);
        Ok(())
    }

    async fn reexported_macros() {
        let picked = rt::select! {
            v = async { 1u8 } => v,
            _ = rt::time::sleep(Duration::from_secs(60)) => 0,
        };
        assert_eq!(picked, 1);
        let (a, b) = rt::join!(async { 2 }, async { 3 });
        assert_eq!(a + b, 5);
    }
}
