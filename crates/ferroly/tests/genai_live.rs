#![cfg(all(feature = "openai", feature = "claude", feature = "ollama"))]
//! Opt-in live smoke tests against the real provider APIs.
//!
//! Every other GenAI suite runs against scripted local servers. These tests
//! check that the completion and streaming wire shapes are still accepted by
//! the real services. They are `#[ignore]`d, so a plain `cargo test` never
//! runs them, and each one returns early (passing) unless its credentials are
//! set:
//!
//! | Provider | Required | Optional model override (default) |
//! |---|---|---|
//! | Claude | `ANTHROPIC_API_KEY` | `FERROLY_LIVE_CLAUDE_MODEL` (`claude-haiku-5-5`) |
//! | OpenAI | `OPENAI_API_KEY` | `FERROLY_LIVE_OPENAI_MODEL` (`gpt-6-luna`) |
//! | Ollama | `OLLAMA_URL` or `OLLAMA_HOST` (e.g. `http://127.0.0.1:11434`) | `FERROLY_LIVE_OLLAMA_MODEL` (`llama3`) |
//!
//! ```text
//! ANTHROPIC_API_KEY=… cargo test -p ferroly --features full --test genai_live -- --ignored
//! ```
//!
//! Each test makes one small completion and one small streamed completion,
//! so a run costs a fraction of a cent per hosted provider.

use std::time::Duration;

use ferroly::genai::{
    collect_stream, ClaudeProvider, CompletionChunk, CompletionRequest, CompletionResponse,
    FinishReason, GenAiProvider, Message, OllamaProvider, OpenAiProvider, Options, ProviderOptions,
};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn request(model: &str) -> CompletionRequest {
    CompletionRequest::builder(model)
        .message(Message::user("Reply with the single word: pong"))
        .options(Options::builder().max_tokens(256).build())
        .build()
}

fn options() -> ProviderOptions {
    ProviderOptions::new().response_timeout(Duration::from_secs(60))
}

/// The answer finished normally with some text, or ran out of tokens (a
/// reasoning model may spend a small budget on thinking).
fn check(label: &str, response: &CompletionResponse) {
    let finish = response.finish_reason;
    assert!(
        !response.text().trim().is_empty() || finish == Some(FinishReason::Length),
        "{label}: empty answer (finish {finish:?})"
    );
    assert!(finish.is_some(), "{label}: no finish reason");
    eprintln!(
        "{label}: model={} finish={finish:?} usage={:?} text={:?}",
        response.model,
        response.usage,
        response.text()
    );
}

async fn smoke(label: &str, provider: &dyn GenAiProvider, model: &str) {
    let response = provider
        .complete(request(model))
        .await
        .unwrap_or_else(|e| panic!("{label} complete: {e}"));
    check(&format!("{label} complete"), &response);

    let mut stream = provider
        .complete_stream(request(model))
        .await
        .unwrap_or_else(|e| panic!("{label} stream: {e}"));
    // Peek at the first event, then accumulate the rest.
    let first = stream
        .recv()
        .await
        .expect("the stream sent nothing")
        .unwrap_or_else(|e| panic!("{label} stream: {e}"));
    assert!(
        !matches!(first, CompletionChunk::Done { .. }),
        "{label}: the stream ended at once"
    );
    let rest = collect_stream(stream)
        .await
        .unwrap_or_else(|e| panic!("{label} stream: {e}"));
    assert!(rest.finish_reason.is_some(), "{label}: stream had no end");
}

#[tokio::test]
#[ignore = "live: needs ANTHROPIC_API_KEY"]
async fn claude_live() {
    let Some(key) = env("ANTHROPIC_API_KEY") else {
        eprintln!("ANTHROPIC_API_KEY not set: skipped");
        return;
    };
    let model = env("FERROLY_LIVE_CLAUDE_MODEL").unwrap_or_else(|| "claude-haiku-5-5".into());
    let provider = ClaudeProvider::new(key, Some(options()));
    smoke("claude", &provider, &model).await;
}

#[tokio::test]
#[ignore = "live: needs OPENAI_API_KEY"]
async fn openai_live() {
    let Some(key) = env("OPENAI_API_KEY") else {
        eprintln!("OPENAI_API_KEY not set: skipped");
        return;
    };
    let model = env("FERROLY_LIVE_OPENAI_MODEL").unwrap_or_else(|| "gpt-6-luna".into());
    let provider = OpenAiProvider::new(key, Some(options()));
    smoke("openai", &provider, &model).await;
}

#[tokio::test]
#[ignore = "live: needs OLLAMA_URL or OLLAMA_HOST"]
async fn ollama_live() {
    let Some(url) = env("OLLAMA_URL").or_else(|| env("OLLAMA_HOST")) else {
        eprintln!("OLLAMA_URL / OLLAMA_HOST not set: skipped");
        return;
    };
    let url = if url.starts_with("http://") || url.starts_with("https://") {
        url
    } else {
        format!("http://{url}")
    };
    let model = env("FERROLY_LIVE_OLLAMA_MODEL").unwrap_or_else(|| "llama3".into());
    let opts = ProviderOptions::with_base_url(&url).response_timeout(Duration::from_secs(120));
    let provider = OllamaProvider::new(Some(opts));
    smoke("ollama", &provider, &model).await;
}
