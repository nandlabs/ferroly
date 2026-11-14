//! [`CancellationToken`] — cooperative cancellation shared between tasks.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use tokio::sync::watch;

/// A cloneable flag that tasks can wait on to learn they should stop.
///
/// Cancellation is **cooperative**: [`cancel`](CancellationToken::cancel) does
/// not interrupt anything by itself; tasks observe it by awaiting
/// [`cancelled`](CancellationToken::cancelled) (typically in a `select!`) or
/// polling [`is_cancelled`](CancellationToken::is_cancelled). Once cancelled, a
/// token stays cancelled — a waiter that arrives late returns immediately.
///
/// Clones share one flag. A [`child_token`](CancellationToken::child_token) is
/// cancelled when its parent is, but cancelling the child leaves the parent
/// (and its siblings) running — useful for "stop this request" inside "stop
/// the whole server".
///
/// ```
/// use ferroly::rt::{self, CancellationToken};
///
/// # async fn demo() {
/// let token = CancellationToken::new();
/// let worker = rt::spawn({
///     let token = token.clone();
///     async move {
///         rt::select! {
///             _ = token.cancelled() => "stopped",
///             _ = rt::time::sleep(rt::time::Duration::from_secs(3600)) => "finished",
///         }
///     }
/// });
/// token.cancel();
/// assert_eq!(worker.await.unwrap(), "stopped");
/// # }
/// ```
#[derive(Clone)]
pub struct CancellationToken {
    node: Arc<Node>,
}

struct Node {
    /// The latched flag; `watch` wakes every waiter when it flips.
    cancelled: watch::Sender<bool>,
    /// Child tokens to cancel along with this one.
    children: Mutex<Vec<Weak<Node>>>,
}

impl Node {
    fn new(cancelled: bool) -> Arc<Node> {
        Arc::new(Node {
            cancelled: watch::channel(cancelled).0,
            children: Mutex::new(Vec::new()),
        })
    }

    fn is_cancelled(&self) -> bool {
        *self.cancelled.borrow()
    }

    /// The child list. A poisoned lock is recovered: the list is only ever
    /// pushed to, pruned or drained, so it is valid whatever panicked.
    fn children(&self) -> MutexGuard<'_, Vec<Weak<Node>>> {
        self.children.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl CancellationToken {
    /// Creates a token that is not cancelled.
    pub fn new() -> Self {
        Self {
            node: Node::new(false),
        }
    }

    /// Creates a token that is cancelled when this one is.
    ///
    /// Cancelling the child does not cancel this token. A child of an
    /// already-cancelled token starts out cancelled.
    pub fn child_token(&self) -> CancellationToken {
        let mut children = self.node.children();
        // Checked under the child-list lock, which `cancel` also holds while
        // it flips the flag, so a child is either registered before the drain
        // or sees the flag already set.
        if self.node.is_cancelled() {
            return CancellationToken {
                node: Node::new(true),
            };
        }
        let child = Node::new(false);
        // Drop entries for children that no longer exist, but only when the
        // list is about to grow, so the cost stays amortized-constant.
        if children.len() == children.capacity() {
            children.retain(|c| c.strong_count() > 0);
        }
        children.push(Arc::downgrade(&child));
        CancellationToken { node: child }
    }

    /// Cancels this token, all its clones, and every descendant token.
    /// Calling it again is a no-op.
    pub fn cancel(&self) {
        // Iterative so a long chain of child tokens cannot overflow the stack.
        let mut pending = vec![self.node.clone()];
        while let Some(node) = pending.pop() {
            let children = {
                let mut children = node.children();
                if node.is_cancelled() {
                    continue;
                }
                node.cancelled.send_replace(true);
                std::mem::take(&mut *children)
            };
            pending.extend(children.iter().filter_map(Weak::upgrade));
        }
    }

    /// Whether the token has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.node.is_cancelled()
    }

    /// Completes once the token is cancelled (immediately if it already is).
    ///
    /// Safe to drop and re-create at any time — for instance as a `select!`
    /// branch inside a loop.
    pub async fn cancelled(&self) {
        wait_cancelled(&self.node).await
    }

    /// Like [`cancelled`](CancellationToken::cancelled), but owns the token so
    /// the future is `'static` (for spawning or storing).
    pub async fn cancelled_owned(self) {
        wait_cancelled(&self.node).await
    }

    /// Runs `future` to completion unless the token is cancelled first.
    ///
    /// Returns `Some(output)` if the future finished, or `None` if cancellation
    /// won — in which case the future is dropped at whatever `.await` it had
    /// reached. If the token is already cancelled, the future is not polled.
    pub async fn run_until_cancelled<F: Future>(&self, future: F) -> Option<F::Output> {
        tokio::select! {
            biased;
            _ = self.cancelled() => None,
            output = future => Some(output),
        }
    }

    /// Returns a guard that cancels the token when dropped — handy for
    /// cancelling a group of tasks when their owner goes out of scope or
    /// unwinds.
    pub fn drop_guard(self) -> DropGuard {
        DropGuard { token: Some(self) }
    }
}

async fn wait_cancelled(node: &Node) {
    let mut changes = node.cancelled.subscribe();
    loop {
        // The flag is read under the same lock that `changed` compares
        // versions against, so a cancel between the check and the await
        // cannot be missed.
        if *changes.borrow_and_update() {
            return;
        }
        if changes.changed().await.is_err() {
            // The sender lives in `node`, which the caller keeps alive, so
            // this is unreachable; never report a cancellation that did not
            // happen.
            std::future::pending::<()>().await;
        }
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancellationToken")
            .field("is_cancelled", &self.is_cancelled())
            .finish()
    }
}

/// Cancels its [`CancellationToken`] when dropped. Created by
/// [`CancellationToken::drop_guard`].
#[derive(Debug)]
pub struct DropGuard {
    token: Option<CancellationToken>,
}

impl DropGuard {
    /// Returns the token without cancelling it.
    pub fn disarm(mut self) -> CancellationToken {
        self.token
            .take()
            .expect("DropGuard holds its token until disarmed")
    }
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        if let Some(token) = &self.token {
            token.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    /// A generous bound: these futures should resolve at once.
    const SOON: Duration = Duration::from_secs(5);

    async fn is_pending<F: Future>(future: F) -> bool {
        timeout(Duration::from_millis(20), future).await.is_err()
    }

    #[tokio::test]
    async fn cancel_wakes_waiters_and_latches() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
        assert!(is_pending(token.cancelled()).await);

        let waiter = tokio::spawn(token.clone().cancelled_owned());
        tokio::task::yield_now().await;
        token.cancel();
        timeout(SOON, waiter).await.unwrap().unwrap();

        // Latched: later waiters (and clones) return immediately.
        assert!(token.is_cancelled());
        timeout(SOON, token.cancelled()).await.unwrap();
        timeout(SOON, token.clone().cancelled()).await.unwrap();
        token.cancel(); // idempotent
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn many_waiters_are_all_woken() {
        let token = CancellationToken::new();
        let waiters: Vec<_> = (0..64)
            .map(|_| tokio::spawn(token.clone().cancelled_owned()))
            .collect();
        tokio::task::yield_now().await;
        token.cancel();
        for waiter in waiters {
            timeout(SOON, waiter).await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn parent_cancels_children_but_not_the_reverse() {
        let parent = CancellationToken::new();
        let child_a = parent.child_token();
        let child_b = parent.child_token();
        let grandchild = child_a.child_token();

        child_a.cancel();
        assert!(child_a.is_cancelled());
        assert!(grandchild.is_cancelled());
        assert!(!parent.is_cancelled());
        assert!(!child_b.is_cancelled());

        let waiter = tokio::spawn(child_b.clone().cancelled_owned());
        tokio::task::yield_now().await;
        parent.cancel();
        timeout(SOON, waiter).await.unwrap().unwrap();
        assert!(child_b.is_cancelled());
    }

    #[tokio::test]
    async fn child_of_cancelled_parent_starts_cancelled() {
        let parent = CancellationToken::new();
        parent.cancel();
        let child = parent.child_token();
        assert!(child.is_cancelled());
        timeout(SOON, child.cancelled()).await.unwrap();
    }

    #[tokio::test]
    async fn deep_chain_cancels_without_overflowing_the_stack() {
        let root = CancellationToken::new();
        let mut chain = vec![root.clone()];
        for _ in 0..50_000 {
            let next = chain.last().unwrap().child_token();
            chain.push(next);
        }
        root.cancel();
        assert!(chain.last().unwrap().is_cancelled());
    }

    #[test]
    fn dropped_children_are_pruned() {
        let parent = CancellationToken::new();
        for _ in 0..10_000 {
            drop(parent.child_token());
        }
        // Without pruning the list would hold 10 000 dead entries.
        assert!(parent.node.children().len() < 64);
        let live = parent.child_token();
        parent.cancel();
        assert!(live.is_cancelled());
    }

    #[tokio::test]
    async fn concurrent_child_creation_and_cancel_never_loses_a_child() {
        for _ in 0..200 {
            let parent = CancellationToken::new();
            let maker = {
                let parent = parent.clone();
                tokio::spawn(
                    async move { (0..16).map(|_| parent.child_token()).collect::<Vec<_>>() },
                )
            };
            let canceller = {
                let parent = parent.clone();
                tokio::spawn(async move { parent.cancel() })
            };
            let children = maker.await.unwrap();
            canceller.await.unwrap();
            assert!(children.iter().all(CancellationToken::is_cancelled));
        }
    }

    #[tokio::test]
    async fn run_until_cancelled_returns_output_or_none() {
        let token = CancellationToken::new();
        assert_eq!(token.run_until_cancelled(async { 7 }).await, Some(7));

        let stopper = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            stopper.cancel();
        });
        let out = timeout(
            SOON,
            token.run_until_cancelled(std::future::pending::<u8>()),
        )
        .await
        .unwrap();
        assert_eq!(out, None);

        // Already cancelled: the future is never polled.
        let out = token
            .run_until_cancelled(async { unreachable!("must not be polled") })
            .await;
        assert_eq!(out, None::<()>);
    }

    #[tokio::test]
    async fn drop_guard_cancels_unless_disarmed() {
        let token = CancellationToken::new();
        drop(token.clone().drop_guard());
        assert!(token.is_cancelled());

        let token = CancellationToken::new();
        let back = token.clone().drop_guard().disarm();
        assert!(!token.is_cancelled());
        assert!(!back.is_cancelled());
    }

    #[test]
    fn token_is_send_sync_and_its_futures_are_send() {
        fn assert_send_sync<T: Send + Sync>() {}
        fn assert_send<T: Send>(_: &T) {}
        assert_send_sync::<CancellationToken>();
        assert_send_sync::<DropGuard>();
        let token = CancellationToken::new();
        assert_send(&token.cancelled());
        assert_send(&token.clone().cancelled_owned());
    }
}
