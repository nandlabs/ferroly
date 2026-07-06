//! Error type for parameter extraction.

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
