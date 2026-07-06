//! A three-state circuit breaker (closed / open / half-open).

use std::sync::Mutex;
use std::time::Duration;

// `tokio::time::Instant` respects a paused test clock (`tokio::time::pause`),
// making the cooldown deterministically testable; in production it tracks real
// time exactly like `std::time::Instant`.
use tokio::time::Instant;

use ferroly::clients::error::CircuitOpenError;

/// The state of a [`CircuitBreaker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Requests flow normally; failures are counted.
    Closed,
    /// Requests are rejected until the cooldown timeout elapses.
    Open,
    /// A limited number of trial requests probe whether the dependency recovered.
    HalfOpen,
}

/// Tuning for a [`CircuitBreaker`].
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Consecutive failures in `Closed` that trip the breaker to `Open`.
    pub failure_threshold: u32,
    /// Successful trials in `HalfOpen` that close the breaker.
    pub success_threshold: u32,
    /// Maximum concurrent trial requests permitted in `HalfOpen`.
    pub max_half_open: u32,
    /// How long the breaker stays `Open` before allowing trials.
    pub timeout: Duration,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            success_threshold: 2,
            max_half_open: 1,
            timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug)]
struct State {
    circuit: CircuitState,
    failures: u32,
    successes: u32,
    half_open_in_flight: u32,
    opened_at: Option<Instant>,
}

/// A concurrency-safe circuit breaker.
///
/// Call [`can_execute`](CircuitBreaker::can_execute) before an operation and report the
/// outcome with [`on_execution`](CircuitBreaker::on_execution):
///
/// ```
/// use ferroly::clients::{CircuitBreaker, CircuitBreakerConfig};
///
/// let cb = CircuitBreaker::new(CircuitBreakerConfig::default());
/// if cb.can_execute().is_ok() {
///     // perform the call, then:
///     cb.on_execution(true);
/// }
/// ```
#[derive(Debug)]
pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    state: Mutex<State>,
}

impl CircuitBreaker {
    /// Creates a breaker in the closed state.
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            state: Mutex::new(State {
                circuit: CircuitState::Closed,
                failures: 0,
                successes: 0,
                half_open_in_flight: 0,
                opened_at: None,
            }),
        }
    }

    /// Returns the current circuit state.
    pub fn state(&self) -> CircuitState {
        self.state.lock().unwrap().circuit
    }

    /// Checks whether an operation may proceed, transitioning `Open` → `HalfOpen`
    /// once the cooldown elapses. Increments the in-flight trial count when it
    /// admits a half-open probe.
    pub fn can_execute(&self) -> Result<(), CircuitOpenError> {
        let mut s = self.state.lock().unwrap();
        match s.circuit {
            CircuitState::Closed => Ok(()),
            CircuitState::Open => {
                let elapsed = s.opened_at.map(|t| t.elapsed()).unwrap_or_default();
                if elapsed >= self.config.timeout {
                    s.circuit = CircuitState::HalfOpen;
                    s.successes = 0;
                    s.half_open_in_flight = 1;
                    Ok(())
                } else {
                    Err(CircuitOpenError)
                }
            }
            CircuitState::HalfOpen => {
                if s.half_open_in_flight < self.config.max_half_open {
                    s.half_open_in_flight += 1;
                    Ok(())
                } else {
                    Err(CircuitOpenError)
                }
            }
        }
    }

    /// Reports the outcome of an operation admitted by [`can_execute`](CircuitBreaker::can_execute).
    pub fn on_execution(&self, success: bool) {
        let mut s = self.state.lock().unwrap();
        match s.circuit {
            CircuitState::Closed => {
                if success {
                    s.failures = 0;
                } else {
                    s.failures += 1;
                    if s.failures >= self.config.failure_threshold {
                        self.trip(&mut s);
                    }
                }
            }
            CircuitState::HalfOpen => {
                s.half_open_in_flight = s.half_open_in_flight.saturating_sub(1);
                if success {
                    s.successes += 1;
                    if s.successes >= self.config.success_threshold {
                        s.circuit = CircuitState::Closed;
                        s.failures = 0;
                        s.successes = 0;
                        s.opened_at = None;
                    }
                } else {
                    self.trip(&mut s);
                }
            }
            CircuitState::Open => {
                // A stray report while open; if it failed, refresh the cooldown.
                if !success {
                    s.opened_at = Some(Instant::now());
                }
            }
        }
    }

    fn trip(&self, s: &mut State) {
        s.circuit = CircuitState::Open;
        s.opened_at = Some(Instant::now());
        s.failures = 0;
        s.successes = 0;
        s.half_open_in_flight = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            failure_threshold: 3,
            success_threshold: 2,
            max_half_open: 1,
            timeout: Duration::from_millis(20),
        }
    }

    #[test]
    fn trips_open_after_threshold_failures() {
        let cb = CircuitBreaker::new(cfg());
        assert_eq!(cb.state(), CircuitState::Closed);
        for _ in 0..3 {
            assert!(cb.can_execute().is_ok());
            cb.on_execution(false);
        }
        assert_eq!(cb.state(), CircuitState::Open);
        assert!(cb.can_execute().is_err());
    }

    // `start_paused` freezes the tokio clock; `advance` moves it deterministically
    // past the cooldown, so these no longer race a wall-clock sleep.
    #[tokio::test(start_paused = true)]
    async fn recovers_through_half_open() {
        let cb = CircuitBreaker::new(cfg());
        for _ in 0..3 {
            let _ = cb.can_execute();
            cb.on_execution(false);
        }
        assert_eq!(cb.state(), CircuitState::Open);

        tokio::time::advance(Duration::from_millis(21)).await; // past the 20ms cooldown
                                                               // First probe admitted -> HalfOpen.
        assert!(cb.can_execute().is_ok());
        assert_eq!(cb.state(), CircuitState::HalfOpen);
        cb.on_execution(true);
        // Second success closes it.
        assert!(cb.can_execute().is_ok());
        cb.on_execution(true);
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[tokio::test(start_paused = true)]
    async fn half_open_failure_reopens() {
        let cb = CircuitBreaker::new(cfg());
        for _ in 0..3 {
            let _ = cb.can_execute();
            cb.on_execution(false);
        }
        tokio::time::advance(Duration::from_millis(21)).await;
        assert!(cb.can_execute().is_ok());
        cb.on_execution(false);
        assert_eq!(cb.state(), CircuitState::Open);
    }
}
