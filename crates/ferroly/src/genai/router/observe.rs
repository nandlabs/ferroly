//! Decision logging. Emits a compact structured record of each routing decision
//! via `ferroly::log` when the `log` feature is enabled; otherwise a no-op.

use super::route::RouteDecision;

/// Logs a routing decision at debug level (no-op without the `log` feature).
pub(crate) fn log_decision(decision: &RouteDecision) {
    #[cfg(feature = "log")]
    {
        use ferroly::codec::Value;
        ferroly::log::debug(
            "genai model-router decision",
            &[
                ("strategy", Value::from(decision.strategy.clone())),
                ("provider", Value::from(decision.chosen.provider.clone())),
                ("model", Value::from(decision.chosen.model.clone())),
                ("est_cost_usd", Value::from(decision.chosen.est_cost)),
                ("fallbacks", Value::from(decision.fallbacks.len() as i64)),
                ("considered", Value::from(decision.considered.len() as i64)),
                ("excluded", Value::from(decision.excluded.len() as i64)),
                ("pinned", Value::from(decision.pinned)),
            ],
        );
    }
    #[cfg(not(feature = "log"))]
    let _ = decision;
}
