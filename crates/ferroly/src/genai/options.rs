//! Generation options for a completion request.

use std::collections::HashMap;

use ferroly::codec::Value;

/// How much effort a reasoning-capable model should spend.
///
/// Sent as `output_config.effort` to Claude and as `reasoning_effort` to
/// OpenAI (where `XHigh` and `Max` are sent as `high`, the highest value that
/// API defines). Ollama has no portable equivalent; use
/// [`Options::custom`] there.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReasoningEffort {
    /// Fastest, least thorough.
    Low,
    /// Balanced.
    Medium,
    /// Thorough.
    High,
    /// Between `High` and `Max`.
    XHigh,
    /// The most the model offers.
    Max,
}

impl ReasoningEffort {
    /// The lower-case wire label (`"low"`, `"medium"`, `"high"`, `"xhigh"`,
    /// `"max"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::XHigh => "xhigh",
            ReasoningEffort::Max => "max",
        }
    }
}

/// How long a prompt-cache entry lives.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheTtl {
    /// The provider's default lifetime (five minutes on Claude).
    #[default]
    Default,
    /// One hour.
    OneHour,
}

/// Prompt-caching controls, for providers where caching is opt-in (Claude).
///
/// OpenAI caches long prompts automatically and Ollama has no prompt cache, so
/// both ignore this setting; cached-token counts are reported in
/// [`Usage`](crate::genai::Usage) either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PromptCache {
    /// Cache the system prompt as its own prefix.
    pub system: bool,
    /// Let the provider cache the conversation up to its last cacheable block.
    pub conversation: bool,
    /// The lifetime of the entries written.
    pub ttl: CacheTtl,
}

impl PromptCache {
    /// Cache the whole conversation prefix automatically — the right default
    /// for multi-turn conversations.
    pub fn auto() -> Self {
        PromptCache {
            system: false,
            conversation: true,
            ttl: CacheTtl::Default,
        }
    }

    /// Cache only the system prompt — for a large, stable system prompt
    /// followed by content that differs on every request.
    pub fn system() -> Self {
        PromptCache {
            system: true,
            conversation: false,
            ttl: CacheTtl::Default,
        }
    }

    /// Sets the entry lifetime.
    pub fn ttl(mut self, ttl: CacheTtl) -> Self {
        self.ttl = ttl;
        self
    }
}

/// Generation options (temperature, token limits, system instructions, …).
///
/// The common settings are typed fields; anything else a provider accepts goes
/// in [`custom`](Options::custom), which is merged into the request body.
/// Construct with a struct literal and `..Default::default()`, the
/// [`Options::builder`], or by setting fields directly.
///
/// A typed option a provider has no equivalent for is not sent to it; the
/// table in `docs/genai.md` lists what each provider receives.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Options {
    /// Maximum number of tokens to generate.
    pub max_tokens: Option<u32>,
    /// Sampling temperature.
    pub temperature: Option<f32>,
    /// Nucleus-sampling probability mass.
    pub top_p: Option<f32>,
    /// Sample only from the `k` most likely tokens (Claude, Ollama).
    pub top_k: Option<u32>,
    /// Sequences that stop generation when produced.
    pub stop: Vec<String>,
    /// A seed for (best-effort) reproducible sampling (OpenAI, Ollama).
    pub seed: Option<i64>,
    /// Penalty for tokens that already appeared at all (OpenAI, Ollama).
    pub presence_penalty: Option<f32>,
    /// Penalty growing with how often a token appeared (OpenAI, Ollama).
    pub frequency_penalty: Option<f32>,
    /// An opaque end-user identifier for the provider's abuse monitoring
    /// (OpenAI `user`, Claude `metadata.user_id`). Never send personal data.
    pub user: Option<String>,
    /// Whether the model may request several tool calls in one turn.
    pub parallel_tool_calls: Option<bool>,
    /// Reasoning effort for models that support it.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Prompt-caching controls.
    pub prompt_cache: Option<PromptCache>,
    /// System / developer instructions.
    pub system_instructions: Option<String>,
    /// Provider-specific options, merged into the top level of the request
    /// body.
    ///
    /// Precedence: a typed option wins over a custom key of the same name;
    /// when both are JSON objects (for example Ollama's `options` or Claude's
    /// `output_config`) the custom entries are added to the typed ones. The
    /// keys a provider owns — `model`, `messages`, `stream` (and `system` on
    /// Claude) — cannot be set this way: a request that tries is rejected
    /// with a configuration error before anything is sent.
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

    /// Sets top-k sampling.
    pub fn top_k(mut self, k: u32) -> Self {
        self.options.top_k = Some(k);
        self
    }

    /// Adds a stop sequence.
    pub fn stop(mut self, sequence: impl Into<String>) -> Self {
        self.options.stop.push(sequence.into());
        self
    }

    /// Replaces the stop sequences.
    pub fn stop_sequences<I, S>(mut self, sequences: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.options.stop = sequences.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the sampling seed.
    pub fn seed(mut self, seed: i64) -> Self {
        self.options.seed = Some(seed);
        self
    }

    /// Sets the presence penalty.
    pub fn presence_penalty(mut self, penalty: f32) -> Self {
        self.options.presence_penalty = Some(penalty);
        self
    }

    /// Sets the frequency penalty.
    pub fn frequency_penalty(mut self, penalty: f32) -> Self {
        self.options.frequency_penalty = Some(penalty);
        self
    }

    /// Sets the opaque end-user identifier.
    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.options.user = Some(user.into());
        self
    }

    /// Allows or forbids parallel tool calls.
    pub fn parallel_tool_calls(mut self, allow: bool) -> Self {
        self.options.parallel_tool_calls = Some(allow);
        self
    }

    /// Sets the reasoning effort.
    pub fn reasoning_effort(mut self, effort: ReasoningEffort) -> Self {
        self.options.reasoning_effort = Some(effort);
        self
    }

    /// Sets the prompt-caching controls.
    pub fn prompt_cache(mut self, cache: PromptCache) -> Self {
        self.options.prompt_cache = Some(cache);
        self
    }

    /// Sets system instructions.
    pub fn system_instructions(mut self, s: impl Into<String>) -> Self {
        self.options.system_instructions = Some(s.into());
        self
    }

    /// Sets a provider-specific option, merged into the request body (see
    /// [`Options::custom`] for the precedence rules).
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
            .top_k(40)
            .stop("END")
            .seed(7)
            .presence_penalty(0.5)
            .frequency_penalty(0.25)
            .user("u-1")
            .parallel_tool_calls(false)
            .reasoning_effort(ReasoningEffort::High)
            .prompt_cache(PromptCache::auto().ttl(CacheTtl::OneHour))
            .system_instructions("be terse")
            .build();
        assert_eq!(opts.max_tokens, Some(256));
        assert_eq!(opts.temperature, Some(0.7));
        assert_eq!(opts.top_p, Some(0.9));
        assert_eq!(opts.top_k, Some(40));
        assert_eq!(opts.stop, vec!["END".to_string()]);
        assert_eq!(opts.seed, Some(7));
        assert_eq!(opts.user.as_deref(), Some("u-1"));
        assert_eq!(opts.parallel_tool_calls, Some(false));
        assert_eq!(opts.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(opts.prompt_cache.unwrap().ttl, CacheTtl::OneHour);
        assert_eq!(opts.system_instructions.as_deref(), Some("be terse"));
    }

    #[test]
    fn custom_key_is_extensible() {
        let opts = Options::builder().custom("logprobs", true).build();
        assert_eq!(
            opts.custom.get("logprobs").and_then(|v| v.as_bool()),
            Some(true)
        );
    }
}
