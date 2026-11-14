//! Head sampling: the decision, made when a span starts, whether its trace is
//! recorded and exported.

use std::sync::RwLock;

use super::context::Context;
use super::id::TraceId;

/// Decides which traces are recorded.
///
/// The decision is made once per span, when it starts, and travels with the
/// trace: locally through the span's [`Context`], and to other services as the
/// `sampled` flag of the `traceparent` header. An unsampled span still has a
/// valid context (so the decision keeps propagating) but records nothing and
/// is never exported.
///
/// ```
/// use ferroly::obs::{set_sampler, Sampler};
///
/// // Keep 10% of the traces that start here; follow the caller's decision
/// // for requests that arrive with a `traceparent`.
/// set_sampler(Sampler::parent_based(Sampler::TraceIdRatio(0.1)));
/// # set_sampler(Sampler::default());
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum Sampler {
    /// Record every trace.
    AlwaysOn,
    /// Record nothing.
    AlwaysOff,
    /// Record the given fraction of traces (`0.0..=1.0`). The decision is a
    /// pure function of the trace id, so every span of a trace — in every
    /// service using the same ratio — agrees.
    TraceIdRatio(f64),
    /// Follow the parent's decision when there is a parent (local or remote);
    /// use the wrapped sampler for root spans.
    ParentBased(Box<Sampler>),
}

impl Default for Sampler {
    /// `ParentBased(AlwaysOn)`: record everything, unless an upstream service
    /// decided not to sample the trace.
    fn default() -> Self {
        Sampler::ParentBased(Box::new(Sampler::AlwaysOn))
    }
}

impl Sampler {
    /// `ParentBased(root)` without the `Box`.
    pub fn parent_based(root: Sampler) -> Sampler {
        Sampler::ParentBased(Box::new(root))
    }

    /// Whether a span of trace `trace_id`, with the given parent, is sampled.
    pub fn should_sample(&self, parent: Option<&Context>, trace_id: TraceId) -> bool {
        match self {
            Sampler::AlwaysOn => true,
            Sampler::AlwaysOff => false,
            Sampler::TraceIdRatio(ratio) => ratio_admits(*ratio, trace_id),
            Sampler::ParentBased(root) => match parent {
                Some(parent) => parent.is_sampled(),
                None => root.should_sample(None, trace_id),
            },
        }
    }
}

/// The OpenTelemetry trace-id-ratio rule: compare the low 63 bits of the trace
/// id with `ratio * 2^63`.
fn ratio_admits(ratio: f64, trace_id: TraceId) -> bool {
    if ratio.is_nan() || ratio <= 0.0 {
        return false;
    }
    if ratio >= 1.0 {
        return true;
    }
    let bytes = trace_id.to_bytes();
    let mut low = [0u8; 8];
    low.copy_from_slice(&bytes[8..]);
    let value = u64::from_be_bytes(low) >> 1;
    let bound = (ratio * (1u64 << 63) as f64) as u64;
    value < bound
}

static SAMPLER: RwLock<Option<Sampler>> = RwLock::new(None);

/// Installs the process-wide [`Sampler`] (default: [`Sampler::default`]).
pub fn set_sampler(sampler: Sampler) {
    *SAMPLER.write().unwrap_or_else(|e| e.into_inner()) = Some(sampler);
}

/// The process-wide [`Sampler`] currently in force.
pub fn sampler() -> Sampler {
    SAMPLER
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default()
}

/// The global sampler's decision, without cloning it.
pub(super) fn should_sample(parent: Option<&Context>, trace_id: TraceId) -> bool {
    match SAMPLER.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
        Some(sampler) => sampler.should_sample(parent, trace_id),
        // The default, `ParentBased(AlwaysOn)`.
        None => parent.map_or(true, Context::is_sampled),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::obs::SpanId;

    fn ctx(sampled: bool) -> Context {
        Context::new_remote(TraceId::random(), SpanId::random(), sampled)
    }

    #[test]
    fn fixed_samplers_ignore_the_parent() {
        let id = TraceId::random();
        for parent in [None, Some(ctx(true)), Some(ctx(false))] {
            assert!(Sampler::AlwaysOn.should_sample(parent.as_ref(), id));
            assert!(!Sampler::AlwaysOff.should_sample(parent.as_ref(), id));
        }
    }

    #[test]
    fn parent_based_follows_the_parent_and_delegates_for_roots() {
        let id = TraceId::random();
        let on = Sampler::parent_based(Sampler::AlwaysOn);
        let off = Sampler::parent_based(Sampler::AlwaysOff);
        assert!(on.should_sample(None, id));
        assert!(!off.should_sample(None, id));
        assert!(off.should_sample(Some(&ctx(true)), id), "parent sampled");
        assert!(!on.should_sample(Some(&ctx(false)), id), "parent unsampled");
        assert_eq!(Sampler::default(), on);
    }

    #[test]
    fn ratio_is_deterministic_per_trace_and_roughly_proportional() {
        let low = |n: u64| {
            let mut bytes = [0xabu8; 16];
            bytes[8..].copy_from_slice(&n.to_be_bytes());
            TraceId::from_bytes(bytes).unwrap()
        };
        let half = Sampler::TraceIdRatio(0.5);
        assert!(half.should_sample(None, low(0)));
        assert!(half.should_sample(None, low((1 << 63) - 2)));
        assert!(!half.should_sample(None, low(1 << 63)));
        assert!(!half.should_sample(None, low(u64::MAX)));

        assert!(!Sampler::TraceIdRatio(0.0).should_sample(None, low(0)));
        assert!(!Sampler::TraceIdRatio(-1.0).should_sample(None, low(0)));
        assert!(!Sampler::TraceIdRatio(f64::NAN).should_sample(None, low(0)));
        assert!(Sampler::TraceIdRatio(1.0).should_sample(None, low(u64::MAX)));
        assert!(Sampler::TraceIdRatio(7.0).should_sample(None, low(u64::MAX)));

        let quarter = Sampler::TraceIdRatio(0.25);
        let n = 20_000;
        let kept = (0..n)
            .filter(|_| {
                let id = TraceId::random();
                let first = quarter.should_sample(None, id);
                assert_eq!(first, quarter.should_sample(None, id), "stable per trace");
                first
            })
            .count();
        let fraction = kept as f64 / n as f64;
        assert!((0.22..0.28).contains(&fraction), "kept {fraction}");
    }
}
