//! A scripted, in-memory provider for tests.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use ferroly::genai::provider::{BoxFuture, ChunkStream, GenAiProvider};
use ferroly::genai::response::response_to_chunks;
use ferroly::genai::{
    Capability, CompletionChunk, CompletionRequest, CompletionResponse, EmbedRequest,
    EmbedResponse, Embedder, FinishReason, GenAiError, ModelInfo, StreamAccumulator,
};

/// One scripted reply of a [`MockProvider`].
#[derive(Debug)]
pub enum MockReply {
    /// A complete response. Served as is by `complete`, and split into chunks
    /// by `complete_stream`.
    Response(CompletionResponse),
    /// The items of a stream, in order — including, if wanted, an `Err` in the
    /// middle. Served as is by `complete_stream`, and folded into a response
    /// by `complete` (the first `Err` becomes the call's error).
    Stream(Vec<Result<CompletionChunk, GenAiError>>),
    /// The call fails with this error (for a stream: before it is
    /// established).
    Error(GenAiError),
}

/// A [`GenAiProvider`] (and [`Embedder`]) that replays scripted replies and
/// records every request — for testing code that talks to a model without a
/// network or an API key.
///
/// Replies are served in the order they were pushed, one per call, whichever
/// of `complete` / `complete_stream` makes the call. When the script is
/// empty, the provider answers with its default text if one is set, and
/// otherwise fails with a configuration error naming the provider — so a test
/// that makes more calls than it scripted fails loudly.
///
/// ```
/// use ferroly::genai::{
///     CompletionRequest, GenAiError, GenAiProvider, Message, MockProvider,
/// };
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() {
/// let mock = MockProvider::new("mock");
/// mock.push_text("first answer");
/// mock.push_error(GenAiError::Api { status: 503, message: "busy".into() });
///
/// let request = CompletionRequest::builder("any-model")
///     .message(Message::user("hello"))
///     .build();
/// assert_eq!(mock.complete(request.clone()).await.unwrap().text(), "first answer");
/// assert!(mock.complete(request).await.is_err());
///
/// // Every request was recorded.
/// assert_eq!(mock.call_count(), 2);
/// assert_eq!(mock.requests()[0].messages[0].text_content(), "hello");
/// # }
/// ```
#[derive(Debug)]
pub struct MockProvider {
    name: String,
    catalog: Vec<ModelInfo>,
    default_text: Option<String>,
    delay: Mutex<Option<Duration>>,
    replies: Mutex<VecDeque<MockReply>>,
    requests: Mutex<Vec<CompletionRequest>>,
    embed_replies: Mutex<VecDeque<Result<Vec<Vec<f32>>, GenAiError>>>,
    embed_requests: Mutex<Vec<EmbedRequest>>,
}

impl MockProvider {
    /// A provider registered under `name`, with an empty script and catalog.
    pub fn new(name: impl Into<String>) -> Self {
        MockProvider {
            name: name.into(),
            catalog: Vec::new(),
            default_text: None,
            delay: Mutex::new(None),
            replies: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            embed_replies: Mutex::new(VecDeque::new()),
            embed_requests: Mutex::new(Vec::new()),
        }
    }

    /// Sets the model catalog advertised to a router. Entries are re-labelled
    /// with this provider's name.
    pub fn with_catalog(mut self, models: Vec<ModelInfo>) -> Self {
        self.catalog = models
            .into_iter()
            .map(|mut m| {
                m.provider = self.name.clone();
                m
            })
            .collect();
        self
    }

    /// Answers with `text` whenever the script is empty, instead of failing.
    pub fn with_default_text(mut self, text: impl Into<String>) -> Self {
        self.default_text = Some(text.into());
        self
    }

    /// Waits this long before answering each call — to exercise timeouts,
    /// deadlines and cancellation.
    pub fn with_delay(self, delay: Duration) -> Self {
        self.set_delay(Some(delay));
        self
    }

    /// Changes (or removes) the per-call delay of a provider that is already
    /// in use.
    pub fn set_delay(&self, delay: Option<Duration>) {
        if let Ok(mut current) = self.delay.lock() {
            *current = delay;
        }
    }

    /// Queues a reply.
    pub fn push(&self, reply: MockReply) -> &Self {
        if let Ok(mut replies) = self.replies.lock() {
            replies.push_back(reply);
        }
        self
    }

    /// Queues a complete response. An empty `model` is filled in with the
    /// request's model when the reply is served.
    pub fn push_response(&self, response: CompletionResponse) -> &Self {
        self.push(MockReply::Response(response))
    }

    /// Queues a plain-text answer that finishes normally.
    pub fn push_text(&self, text: impl Into<String>) -> &Self {
        self.push(MockReply::Response(CompletionResponse::from_text("", text)))
    }

    /// Queues a failure.
    pub fn push_error(&self, error: GenAiError) -> &Self {
        self.push(MockReply::Error(error))
    }

    /// Queues a stream given item by item.
    pub fn push_stream(&self, items: Vec<Result<CompletionChunk, GenAiError>>) -> &Self {
        self.push(MockReply::Stream(items))
    }

    /// Queues a stream of text deltas followed by a normal finish.
    pub fn push_stream_text<I, S>(&self, deltas: I) -> &Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut items: Vec<Result<CompletionChunk, GenAiError>> = deltas
            .into_iter()
            .map(|d| Ok(CompletionChunk::TextDelta(d.into())))
            .collect();
        items.push(Ok(CompletionChunk::done(FinishReason::Stop)));
        self.push(MockReply::Stream(items))
    }

    /// Queues the vectors returned by the next `embed` call.
    pub fn push_embeddings(&self, vectors: Vec<Vec<f32>>) -> &Self {
        if let Ok(mut replies) = self.embed_replies.lock() {
            replies.push_back(Ok(vectors));
        }
        self
    }

    /// Queues a failure for the next `embed` call.
    pub fn push_embed_error(&self, error: GenAiError) -> &Self {
        if let Ok(mut replies) = self.embed_replies.lock() {
            replies.push_back(Err(error));
        }
        self
    }

    /// Every completion request received so far, in order.
    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().map(|r| r.clone()).unwrap_or_default()
    }

    /// The most recent completion request.
    pub fn last_request(&self) -> Option<CompletionRequest> {
        self.requests.lock().ok().and_then(|r| r.last().cloned())
    }

    /// How many completion calls (streaming or not) were made.
    pub fn call_count(&self) -> usize {
        self.requests.lock().map(|r| r.len()).unwrap_or(0)
    }

    /// Every embeddings request received so far, in order.
    pub fn embed_requests(&self) -> Vec<EmbedRequest> {
        self.embed_requests
            .lock()
            .map(|r| r.clone())
            .unwrap_or_default()
    }

    /// How many scripted completion replies are still queued.
    pub fn remaining(&self) -> usize {
        self.replies.lock().map(|r| r.len()).unwrap_or(0)
    }

    fn next_reply(&self, request: &CompletionRequest) -> Result<MockReply, GenAiError> {
        if let Ok(mut requests) = self.requests.lock() {
            requests.push(request.clone());
        }
        let scripted = self.replies.lock().ok().and_then(|mut r| r.pop_front());
        match (scripted, &self.default_text) {
            (Some(reply), _) => Ok(reply),
            (None, Some(text)) => Ok(MockReply::Response(CompletionResponse::from_text(
                "",
                text.clone(),
            ))),
            (None, None) => Err(GenAiError::Config(format!(
                "MockProvider '{}': no scripted reply left for this call",
                self.name
            ))),
        }
    }

    async fn pause(&self) {
        let delay = self.delay.lock().ok().and_then(|d| *d);
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
    }
}

/// A deterministic stand-in embedding: the same text always gives the same
/// unit-length vector, and different texts (almost always) different ones.
fn pseudo_embedding(text: &str, dimensions: usize) -> Vec<f32> {
    let mut state: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        state ^= u64::from(byte);
        state = state.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut out = Vec::with_capacity(dimensions);
    for _ in 0..dimensions {
        // xorshift64*
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let word = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
        out.push(((word >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0);
        if state == 0 {
            state = 0x9e37_79b9_7f4a_7c15;
        }
    }
    let norm = out.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in &mut out {
            *v /= norm;
        }
    }
    out
}

impl GenAiProvider for MockProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "Scripted in-memory provider for tests"
    }

    fn supports(&self, _capability: Capability) -> bool {
        true
    }

    fn model_catalog(&self) -> Vec<ModelInfo> {
        self.catalog.clone()
    }

    fn complete(
        &self,
        request: CompletionRequest,
    ) -> BoxFuture<'_, Result<CompletionResponse, GenAiError>> {
        Box::pin(async move {
            let reply = self.next_reply(&request);
            self.pause().await;
            match reply? {
                MockReply::Response(mut response) => {
                    if response.model.is_empty() {
                        response.model = request.model;
                    }
                    Ok(response)
                }
                MockReply::Stream(items) => {
                    let mut acc = StreamAccumulator::for_model(request.model);
                    for item in items {
                        acc.push(item?);
                    }
                    acc.finish()
                }
                MockReply::Error(e) => Err(e),
            }
        })
    }

    fn complete_stream(
        &self,
        request: CompletionRequest,
    ) -> BoxFuture<'_, Result<ChunkStream, GenAiError>> {
        Box::pin(async move {
            let reply = self.next_reply(&request);
            self.pause().await;
            let items: Vec<Result<CompletionChunk, GenAiError>> = match reply? {
                MockReply::Response(mut response) => {
                    if response.model.is_empty() {
                        response.model = request.model;
                    }
                    response_to_chunks(&response).into_iter().map(Ok).collect()
                }
                MockReply::Stream(items) => items,
                MockReply::Error(e) => return Err(e),
            };
            let (tx, rx) = tokio::sync::mpsc::channel(items.len().max(1));
            for item in items {
                // The channel holds every item, so this never blocks.
                let _ = tx.try_send(item);
            }
            Ok(rx)
        })
    }
}

impl Embedder for MockProvider {
    /// Returns the next scripted vectors, or — when none are queued — a
    /// deterministic pseudo-embedding per input (`dimensions` components,
    /// 8 by default), so retrieval code can be exercised without scripting.
    fn embed(&self, request: EmbedRequest) -> BoxFuture<'_, Result<EmbedResponse, GenAiError>> {
        Box::pin(async move {
            if let Ok(mut requests) = self.embed_requests.lock() {
                requests.push(request.clone());
            }
            self.pause().await;
            let scripted = self
                .embed_replies
                .lock()
                .ok()
                .and_then(|mut r| r.pop_front());
            let embeddings = match scripted {
                Some(reply) => reply?,
                None => {
                    let dimensions = request.dimensions.unwrap_or(8) as usize;
                    request
                        .input
                        .iter()
                        .map(|text| pseudo_embedding(text, dimensions))
                        .collect()
                }
            };
            Ok(EmbedResponse {
                model: request.model,
                embeddings,
                usage: None,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pseudo_embeddings_are_deterministic_unit_vectors() {
        let a = pseudo_embedding("hello", 8);
        assert_eq!(a, pseudo_embedding("hello", 8));
        assert_ne!(a, pseudo_embedding("world", 8));
        let norm: f32 = a.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4);
        assert_eq!(pseudo_embedding("x", 3).len(), 3);
    }
}
