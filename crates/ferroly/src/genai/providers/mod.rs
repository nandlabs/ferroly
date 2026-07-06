//! Concrete provider implementations, each gated behind a cargo feature.

#[cfg(any(feature = "openai", feature = "claude", feature = "ollama"))]
pub(crate) mod sse;
#[cfg(any(feature = "openai", feature = "claude"))]
pub(crate) mod util;

#[cfg(feature = "claude")]
mod claude;
#[cfg(feature = "ollama")]
mod ollama;
#[cfg(feature = "openai")]
mod openai;

#[cfg(feature = "claude")]
pub use claude::{ClaudeProvider, ClaudeProviderConfig};
#[cfg(feature = "ollama")]
pub use ollama::OllamaProvider;
#[cfg(feature = "openai")]
pub use openai::OpenAiProvider;

/// Common construction options for the built-in HTTP providers.
#[cfg(any(feature = "openai", feature = "claude", feature = "ollama"))]
#[derive(Debug, Clone, Default)]
pub struct ProviderOptions {
    /// Overrides the provider's default API base URL (no trailing slash).
    pub base_url: Option<String>,
}

#[cfg(any(feature = "openai", feature = "claude", feature = "ollama"))]
impl ProviderOptions {
    /// Creates options with a custom base URL.
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: Some(base_url.into()),
        }
    }
}
