//! [`shutdown_signal`] — wait for the OS to ask the process to stop.

/// Which request ended [`shutdown_signal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShutdownSignal {
    /// An interactive interrupt: Ctrl-C (`SIGINT` on Unix).
    Interrupt,
    /// A termination request: `SIGTERM` (Unix only) — what `kill`, systemd,
    /// Docker and Kubernetes send before resorting to `SIGKILL`.
    Terminate,
}

/// Completes when the process is asked to shut down: Ctrl-C on every platform,
/// or `SIGTERM` on Unix.
///
/// Handlers are installed when the future is first polled, and only signals
/// delivered after that are seen — so start awaiting it early (for example in
/// a `select!` next to the server future). Once a handler is installed, the
/// signal's default action (killing the process) no longer happens: the
/// program is responsible for exiting. A second Ctrl-C will therefore not kill
/// a process that is stuck in its shutdown path unless you await this again
/// and exit.
///
/// Returns an error only if a handler could not be installed.
///
/// On Windows only Ctrl-C is observed; console-close and service-stop events
/// are not.
///
/// ```no_run
/// use ferroly::rt::{self, CancellationToken, TaskTracker, time::Duration};
///
/// # async fn serve(_stop: CancellationToken) {}
/// # async fn demo() -> std::io::Result<()> {
/// let stop = CancellationToken::new();
/// let tasks = TaskTracker::new();
/// tasks.spawn(serve(stop.clone()));
///
/// let signal = rt::shutdown_signal().await?;
/// eprintln!("received {signal:?}, shutting down");
/// stop.cancel();
/// if let Err(e) = tasks.shutdown(Duration::from_secs(30)).await {
///     eprintln!("{e}");
/// }
/// # Ok(())
/// # }
/// ```
pub async fn shutdown_signal() -> std::io::Result<ShutdownSignal> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = interrupt.recv() => Ok(ShutdownSignal::Interrupt),
            _ = terminate.recv() => Ok(ShutdownSignal::Terminate),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        Ok(ShutdownSignal::Interrupt)
    }
}
