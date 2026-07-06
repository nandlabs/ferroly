//! Generation options for a completion request.

use std::collections::HashMap;

use ferroly::codec::Value;

/// Generation options (temperature, token limits, system instructions, …).
///
/// The common settings are typed fields; provider-specific knobs go in
/// [`custom`](Options::custom), a small string-keyed escape hatch that is empty
/// for most requests. Construct with a struct literal, the [`Options::builder`],
/// or by setting fields directly.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Options {
    /// Maximum number of tokens to generate.
    pub max_tokens: Option<u32>,
    /// Sampling temperature.
    pub temperature: Option<f32>,
    /// Nucleus-sampling probability mass.
    pub top_p: Option<f32>,
    /// System / developer instructions.
    pub system_instructions: Option<String>,
    /// Provider-specific options addressed by key. Empty for most requests.
    pub custom: HashMap<String, Value>,
}

impl Options {
    /// Creates an empty option set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a fluent builder for the common settings.
    pub fn builder() -> OptionsBuilder {
        OptionsBuilder {
            options: Options::new(),
        }
    }
}

/// Fluent builder for [`Options`].
#[derive(Debug, Default)]
#[must_use]
pub struct OptionsBuilder {
    options: Options,
}

impl OptionsBuilder {
    /// Sets the maximum number of tokens to generate.
    pub fn max_tokens(mut self, n: u32) -> Self {
        self.options.max_tokens = Some(n);
        self
    }

    /// Sets the sampling temperature.
    pub fn temperature(mut self, t: f32) -> Self {
        self.options.temperature = Some(t);
        self
    }

    /// Sets the nucleus-sampling top-p.
    pub fn top_p(mut self, p: f32) -> Self {
        self.options.top_p = Some(p);
        self
    }

    /// Sets system instructions.
    pub fn system_instructions(mut self, s: impl Into<String>) -> Self {
        self.options.system_instructions = Some(s.into());
        self
    }

    /// Sets a provider-specific option (the extensibility escape hatch).
    pub fn custom(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.options.custom.insert(key.into(), value.into());
        self
    }

    /// Finalizes the builder.
    pub fn build(self) -> Options {
        self.options
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_builder_round_trips() {
        let opts = Options::builder()
            .max_tokens(256)
            .temperature(0.7)
            .top_p(0.9)
            .system_instructions("be terse")
            .build();
        assert_eq!(opts.max_tokens, Some(256));
        assert_eq!(opts.temperature, Some(0.7));
        assert_eq!(opts.top_p, Some(0.9));
        assert_eq!(opts.system_instructions.as_deref(), Some("be terse"));
    }

    #[test]
    fn custom_key_is_extensible() {
        let opts = Options::builder().custom("seed", 42i64).build();
        assert_eq!(opts.custom.get("seed").and_then(|v| v.as_i64()), Some(42));
    }
}
