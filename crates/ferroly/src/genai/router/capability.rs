//! Per-model capability + cost metadata.
//!
//! [`ModelInfo`] is the authoritative, provider-owned description of a single
//! model: what it can do, its token limits, a quality and a speed rank, and a
//! default cost. Providers supply it via
//! [`GenAiProvider::model_catalog`](crate::genai::GenAiProvider::model_catalog);
//! operators may override cost, ranks and policy via configuration.
//!
//! The catalogs compiled into the built-in providers are **defaults to
//! override**. Model ids, limits and prices change faster than a library is
//! released: every entry records the date it was correct on
//! ([`ModelInfo::as_of`]), and the registry warns when an entry with a
//! non-zero price is old and has not been overridden.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use ferroly::genai::{Capability, Usage};

/// A calendar date (UTC) on which a catalog entry was known to be correct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AsOf {
    /// The year, e.g. `2026`.
    pub year: u16,
    /// The month, `1..=12`.
    pub month: u8,
    /// The day of the month, `1..=31`.
    pub day: u8,
}

impl AsOf {
    /// A date. Out-of-range months and days are clamped into range.
    pub const fn new(year: u16, month: u8, day: u8) -> Self {
        let month = if month < 1 {
            1
        } else if month > 12 {
            12
        } else {
            month
        };
        let day = if day < 1 {
            1
        } else if day > 31 {
            31
        } else {
            day
        };
        AsOf { year, month, day }
    }

    /// Parses `YYYY-MM-DD`.
    pub fn parse(text: &str) -> Option<AsOf> {
        let mut parts = text.trim().splitn(3, '-');
        let year: u16 = parts.next()?.parse().ok()?;
        let month: u8 = parts.next()?.parse().ok()?;
        let day: u8 = parts.next()?.parse().ok()?;
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        Some(AsOf { year, month, day })
    }

    /// Days since 1970-01-01 (proleptic Gregorian calendar).
    pub fn days_since_epoch(&self) -> i64 {
        // Howard Hinnant's `days_from_civil`.
        let y = i64::from(self.year) - i64::from(self.month <= 2);
        let era = y.div_euclid(400);
        let yoe = y.rem_euclid(400);
        let m = i64::from(self.month);
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(self.day) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    /// How many days before `now` this date lies (negative for a future date).
    pub fn age_days_at(&self, now: SystemTime) -> i64 {
        let today = now
            .duration_since(UNIX_EPOCH)
            .map(|d| (d.as_secs() / 86_400) as i64)
            .unwrap_or(0);
        today - self.days_since_epoch()
    }
}

impl std::fmt::Display for AsOf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// Authoritative description of one model offered by a provider.
///
/// Capabilities and token limits are provider-owned facts; cost and ranks are
/// compiled-in defaults that configuration may override.
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
    /// USD per million input tokens served from the prompt cache. `None`
    /// prices them like ordinary input (an over-estimate).
    pub cached_input_cost_per_mtok: Option<f64>,
    /// USD per million input tokens written to the prompt cache. `None`
    /// prices them like ordinary input.
    pub cache_write_cost_per_mtok: Option<f64>,
    /// How capable the model is relative to others, `0..=100`, higher is
    /// better. Drives `Priority::Quality`. `None` when unknown.
    pub quality_rank: Option<u8>,
    /// How fast the model answers relative to others, `0..=100`, higher is
    /// faster. Drives `Priority::Speed`. `None` when unknown.
    pub speed_rank: Option<u8>,
    /// The date the limits and prices of this entry were correct on.
    pub as_of: Option<AsOf>,
    /// Whether the price was set by the operator (a cost override or a custom
    /// model) instead of coming from a compiled-in catalog.
    pub cost_overridden: bool,
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
            cached_input_cost_per_mtok: None,
            cache_write_cost_per_mtok: None,
            quality_rank: None,
            speed_rank: None,
            as_of: None,
            cost_overridden: false,
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

    /// Sets the per-million-token prices of cached input: tokens read from the
    /// cache, and tokens written to it.
    pub fn cache_cost(mut self, read_per_mtok: Option<f64>, write_per_mtok: Option<f64>) -> Self {
        self.cached_input_cost_per_mtok = read_per_mtok;
        self.cache_write_cost_per_mtok = write_per_mtok;
        self
    }

    /// Sets the quality and speed ranks (`0..=100`, higher is better / faster).
    pub fn ranks(mut self, quality: u8, speed: u8) -> Self {
        self.quality_rank = Some(quality.min(100));
        self.speed_rank = Some(speed.min(100));
        self
    }

    /// Sets the quality rank.
    pub fn quality_rank(mut self, rank: u8) -> Self {
        self.quality_rank = Some(rank.min(100));
        self
    }

    /// Sets the speed rank.
    pub fn speed_rank(mut self, rank: u8) -> Self {
        self.speed_rank = Some(rank.min(100));
        self
    }

    /// Records the date this entry was correct on.
    pub fn as_of(mut self, date: AsOf) -> Self {
        self.as_of = Some(date);
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

    /// Whether this model can answer a completion request: it advertises
    /// `Chat` or `Text`. An embedding-only model does not.
    pub fn is_generative(&self) -> bool {
        self.has(Capability::Chat) || self.has(Capability::Text)
    }

    /// An estimated USD cost for a call with the given token counts — blended
    /// input + output (output usually dominates).
    pub fn est_cost(&self, in_tokens: u32, out_tokens: u32) -> f64 {
        in_tokens as f64 / 1e6 * self.input_cost_per_mtok
            + out_tokens as f64 / 1e6 * self.output_cost_per_mtok
    }

    /// The USD cost of a call from the usage the provider reported, at this
    /// entry's prices. Cached prompt tokens are billed at the cache-read
    /// price and cache writes at the cache-write price (both fall back to the
    /// input price when unset). `None` when the usage carries no token counts.
    ///
    /// ```
    /// use ferroly::genai::{ModelInfo, Usage};
    ///
    /// let model = ModelInfo::new("p", "m").cost(2.0, 10.0).cache_cost(Some(0.2), None);
    /// let usage = Usage {
    ///     prompt_tokens: Some(1_000_000),   // of which …
    ///     cached_tokens: Some(900_000),     // … 900k came from the cache
    ///     completion_tokens: Some(100_000),
    ///     ..Usage::default()
    /// };
    /// // 100k fresh input @ $2 + 900k cached @ $0.20 + 100k output @ $10
    /// let cost = model.cost_for(&usage).unwrap();
    /// assert!((cost - (0.2 + 0.18 + 1.0)).abs() < 1e-9);
    /// ```
    pub fn cost_for(&self, usage: &Usage) -> Option<f64> {
        if usage.prompt_tokens.is_none() && usage.completion_tokens.is_none() {
            return None;
        }
        let prompt = u64::from(usage.prompt_tokens.unwrap_or(0));
        let cached = u64::from(usage.cached_tokens.unwrap_or(0)).min(prompt);
        let written = u64::from(usage.cache_write_tokens.unwrap_or(0)).min(prompt - cached);
        let fresh = prompt - cached - written;
        let completion = u64::from(usage.completion_tokens.unwrap_or(0));
        let per = |tokens: u64, price: f64| tokens as f64 / 1e6 * price;
        Some(
            per(fresh, self.input_cost_per_mtok)
                + per(
                    cached,
                    self.cached_input_cost_per_mtok
                        .unwrap_or(self.input_cost_per_mtok),
                )
                + per(
                    written,
                    self.cache_write_cost_per_mtok
                        .unwrap_or(self.input_cost_per_mtok),
                )
                + per(completion, self.output_cost_per_mtok),
        )
    }

    /// Whether this entry's compiled-in price should no longer be trusted: it
    /// has a non-zero price, a known date older than `max_age_days` at `now`,
    /// and no operator override.
    pub fn cost_is_stale_at(&self, max_age_days: u32, now: SystemTime) -> bool {
        if self.cost_overridden {
            return false;
        }
        if self.input_cost_per_mtok == 0.0 && self.output_cost_per_mtok == 0.0 {
            return false;
        }
        match self.as_of {
            Some(date) => date.age_days_at(now) > i64::from(max_age_days),
            None => false,
        }
    }

    /// [`cost_is_stale_at`](Self::cost_is_stale_at) evaluated now.
    pub fn cost_is_stale(&self, max_age_days: u32) -> bool {
        self.cost_is_stale_at(max_age_days, SystemTime::now())
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
    use std::time::Duration;

    #[test]
    fn has_and_est_cost() {
        let m = ModelInfo::new("openai", "gpt-4o")
            .capabilities([Capability::Chat, Capability::Vision])
            .limits(128_000, 16_384)
            .cost(2.5, 10.0);
        assert!(m.has(Capability::Vision));
        assert!(!m.has(Capability::Embeddings));
        assert!(m.is_generative());
        // 1000 in @ 2.5/Mtok + 500 out @ 10/Mtok
        let cost = m.est_cost(1000, 500);
        assert!((cost - (0.0025 + 0.005)).abs() < 1e-9);
        let embed = ModelInfo::new("openai", "embed").capabilities([Capability::Embeddings]);
        assert!(!embed.is_generative());
    }

    #[test]
    fn as_of_epoch_days_and_parsing() {
        assert_eq!(AsOf::new(1970, 1, 1).days_since_epoch(), 0);
        assert_eq!(AsOf::new(1970, 1, 2).days_since_epoch(), 1);
        assert_eq!(AsOf::new(2000, 3, 1).days_since_epoch(), 11_017);
        assert_eq!(AsOf::new(2024, 2, 29).days_since_epoch(), 19_782);
        assert_eq!(AsOf::parse("2026-11-03"), Some(AsOf::new(2026, 11, 3)));
        assert_eq!(AsOf::parse("2026-13-03"), None);
        assert_eq!(AsOf::parse("soon"), None);
        assert_eq!(AsOf::new(2026, 1, 5).to_string(), "2026-01-05");
    }

    #[test]
    fn staleness_needs_a_price_a_date_and_no_override() {
        let now = UNIX_EPOCH + Duration::from_secs(86_400 * 20_000); // 2024-10-04
        let old = AsOf::new(2023, 1, 1);
        let priced = ModelInfo::new("p", "m").cost(1.0, 2.0).as_of(old);
        assert!(priced.cost_is_stale_at(180, now));
        assert!(!priced.cost_is_stale_at(10_000, now));
        // Free (local) models have no price to go stale.
        assert!(!ModelInfo::new("p", "m")
            .as_of(old)
            .cost_is_stale_at(180, now));
        // No date: nothing to judge.
        assert!(!ModelInfo::new("p", "m")
            .cost(1.0, 2.0)
            .cost_is_stale_at(180, now));
        // An operator override is trusted.
        let mut overridden = priced.clone();
        overridden.cost_overridden = true;
        assert!(!overridden.cost_is_stale_at(180, now));
    }

    #[test]
    fn cost_for_handles_missing_and_cached_tokens() {
        let m = ModelInfo::new("p", "m").cost(3.0, 15.0);
        assert_eq!(m.cost_for(&Usage::default()), None);
        let cost = m.cost_for(&Usage::new(1_000_000, 1_000_000)).unwrap();
        assert!((cost - 18.0).abs() < 1e-9);
        // Without a cache price, cached tokens cost like fresh input.
        let usage = Usage {
            prompt_tokens: Some(1_000_000),
            cached_tokens: Some(500_000),
            ..Usage::default()
        };
        assert!((m.cost_for(&usage).unwrap() - 3.0).abs() < 1e-9);
    }
}
