//! Per-model capability + cost metadata.
//!
//! [`ModelInfo`] is the authoritative, provider-owned description of a single
//! model: what it can do, its token limits, and a default cost. Providers supply
//! it via [`GenAiProvider::model_catalog`](crate::genai::GenAiProvider::model_catalog);
//! operators may override cost/policy via configuration.

use std::collections::BTreeMap;

use ferroly::genai::Capability;

/// Authoritative description of one model offered by a provider.
///
/// Capabilities and token limits are provider-owned facts; cost is a compiled-in
/// default that configuration may override.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    /// The model identifier passed to the provider (e.g. `gpt-4o`).
    pub name: String,
    /// The name the serving provider is registered under (e.g. `openai`).
    pub provider: String,
    /// A human-readable label.
    pub display_name: String,
    /// The capabilities this model supports.
    pub capabilities: Vec<Capability>,
    /// Maximum accepted input (prompt) tokens.
    pub max_input_tokens: u32,
    /// Maximum generated output tokens.
    pub max_output_tokens: u32,
    /// USD per million input tokens.
    pub input_cost_per_mtok: f64,
    /// USD per million output tokens.
    pub output_cost_per_mtok: f64,
    /// Free-form provider metadata.
    pub metadata: BTreeMap<String, String>,
}

impl ModelInfo {
    /// Starts a `ModelInfo` for `(provider, name)` with empty capabilities,
    /// zero limits, and zero cost. Chain the setters to fill it in.
    pub fn new(provider: impl Into<String>, name: impl Into<String>) -> Self {
        let name = name.into();
        ModelInfo {
            display_name: name.clone(),
            name,
            provider: provider.into(),
            capabilities: Vec::new(),
            max_input_tokens: 0,
            max_output_tokens: 0,
            input_cost_per_mtok: 0.0,
            output_cost_per_mtok: 0.0,
            metadata: BTreeMap::new(),
        }
    }

    /// Sets the human-readable label.
    pub fn display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = name.into();
        self
    }

    /// Sets the supported capabilities.
    pub fn capabilities(mut self, caps: impl IntoIterator<Item = Capability>) -> Self {
        self.capabilities = caps.into_iter().collect();
        self
    }

    /// Sets the input and output token limits.
    pub fn limits(mut self, max_input: u32, max_output: u32) -> Self {
        self.max_input_tokens = max_input;
        self.max_output_tokens = max_output;
        self
    }

    /// Sets the per-million-token input and output cost (USD).
    pub fn cost(mut self, input_per_mtok: f64, output_per_mtok: f64) -> Self {
        self.input_cost_per_mtok = input_per_mtok;
        self.output_cost_per_mtok = output_per_mtok;
        self
    }

    /// Adds a metadata entry.
    pub fn meta(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Whether this model advertises capability `c`.
    pub fn has(&self, c: Capability) -> bool {
        self.capabilities.contains(&c)
    }

    /// An estimated USD cost for a call with the given token counts — blended
    /// input + output (output usually dominates).
    pub fn est_cost(&self, in_tokens: u32, out_tokens: u32) -> f64 {
        in_tokens as f64 / 1e6 * self.input_cost_per_mtok
            + out_tokens as f64 / 1e6 * self.output_cost_per_mtok
    }
}

/// Parses a capability name (as used in YAML config) into a [`Capability`].
/// Accepts a few common spellings; returns `None` for anything unrecognized.
pub fn parse_capability(s: &str) -> Option<Capability> {
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "text" => Capability::Text,
        "chat" => Capability::Chat,
        "streaming" | "stream" => Capability::Streaming,
        "vision" | "image" => Capability::Vision,
        "audio" => Capability::Audio,
        "tool_calling" | "tool_use" | "tooluse" | "tools" => Capability::ToolUse,
        "json_mode" | "jsonmode" | "json" => Capability::JsonMode,
        "embeddings" | "embedding" => Capability::Embeddings,
        "reasoning" | "reason" => Capability::Reasoning,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_and_est_cost() {
        let m = ModelInfo::new("openai", "gpt-4o")
            .capabilities([Capability::Chat, Capability::Vision])
            .limits(128_000, 16_384)
            .cost(2.5, 10.0);
        assert!(m.has(Capability::Vision));
        assert!(!m.has(Capability::Embeddings));
        // 1000 in @ 2.5/Mtok + 500 out @ 10/Mtok
        let cost = m.est_cost(1000, 500);
        assert!((cost - (0.0025 + 0.005)).abs() < 1e-9);
    }
}
