//! Error type for the GenAI crate.

use ferroly_derive::FerrolyError;

/// Errors raised across message construction, prompt rendering, and provider I/O.
#[derive(Debug, FerrolyError)]
#[non_exhaustive]
pub enum GenAiError {
    /// A prompt template failed to compile or render.
    #[error("template error: {0}")]
    Template(String),

    /// The requested prompt template id was not found in the store.
    #[error("prompt template not found: {0}")]
    TemplateNotFound(String),

    /// A provider was asked for a capability it does not support.
    #[error("provider '{provider}' does not support capability: {capability:?}")]
    Unsupported {
        /// The provider name.
        provider: String,
        /// The unsupported capability.
        capability: ferroly::genai::Capability,
    },

    /// The provider's HTTP transport failed.
    #[error("transport error: {0}")]
    Transport(String),

    /// The provider returned a non-success HTTP status.
    #[error("provider returned status {status}: {message}")]
    Api {
        /// The HTTP status code.
        status: u16,
        /// The provider's error message or body.
        message: String,
    },

    /// The provider's response could not be parsed into the normalized shape.
    #[error("failed to parse provider response: {0}")]
    ResponseParse(String),

    /// Configuration (missing API key, invalid base URL, etc.) was invalid.
    #[error("configuration error: {0}")]
    Config(String),
}
