//! Error types for the router.

use ferroly_derive::FerrolyError;

/// Errors raised while extracting a typed route/query parameter.
#[derive(Debug, FerrolyError)]
#[non_exhaustive]
pub enum ParamError {
    /// The named parameter was not present.
    #[error("missing parameter: {0}")]
    Missing(String),

    /// The parameter was present but could not be parsed into the target type.
    #[error("invalid parameter '{name}': {reason}")]
    Invalid {
        /// The parameter name.
        name: String,
        /// Why parsing failed.
        reason: String,
    },
}

/// A problem with the routes or configuration registered on a
/// [`Router`](super::Router), reported by
/// [`Router::validate`](super::Router::validate) and
/// [`Router::try_into_handler`](super::Router::try_into_handler).
///
/// Registration itself never panics and never fails: the builder methods stay
/// chainable, problems are collected, and these two methods hand them back.
#[derive(Debug, Clone, PartialEq, Eq, FerrolyError)]
#[non_exhaustive]
pub enum RouteError {
    /// A route pattern or a group/mount prefix is malformed. The route (or
    /// the whole group) was **not** registered.
    #[error("invalid route pattern '{pattern}': {reason}")]
    InvalidPattern {
        /// The pattern as written.
        pattern: String,
        /// What is wrong with it.
        reason: String,
    },

    /// Two routes match exactly the same paths for the same method. When the
    /// router is finalized anyway (with `into_handler`), the one registered
    /// first wins.
    #[error("conflicting routes for {method}: '{first}' and '{second}'")]
    Conflict {
        /// The method both routes are registered for.
        method: String,
        /// The template of the route registered first.
        first: String,
        /// The template of the route registered second.
        second: String,
    },

    /// A middleware or helper was configured with values that cannot work
    /// (for example a JWT secret that is too short to be accepted).
    #[error("invalid router configuration: {0}")]
    Config(String),
}
