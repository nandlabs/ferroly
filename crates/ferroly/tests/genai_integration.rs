#![cfg(feature = "genai")]
//! End-to-end tests of the provider trait and streaming — using an in-memory
//! mock provider so no network access is required.

use std::sync::Arc;

use ferroly::genai::{
    BoxFuture, Capability, ChunkStream, CompletionChunk, CompletionRequest, CompletionResponse,
    GenAiError, GenAiProvider, Message, MessagePart, Role,
};

struct EchoProvider;

impl GenAiProvider for EchoProvider {
    fn name(&self) -> &str {
        "echo"
    }

    fn supports(&self, capability: Capability) -> bool {
        matches!(capability, Capability::Streaming)
    }

    fn complete(
        &self,
        request: CompletionRequest,
    ) -> BoxFuture<'_, Result<CompletionResponse, GenAiError>> {
        Box::pin(async move {
            let last = request
                .messages
                .last()
                .map(|m| m.text_content())
                .unwrap_or_default();
            Ok(CompletionResponse {
                model: request.model,
                message: Message {
                    role: Role::Assistant,
                    id: None,
                    parts: vec![MessagePart::Text(format!("echo: {last}"))],
                },
                finish_reason: Some("stop".into()),
                usage: None,
            })
        })
    }

    fn complete_stream(
        &self,
        request: CompletionRequest,
    ) -> BoxFuture<'_, Result<ChunkStream, GenAiError>> {
        Box::pin(async move {
            let last = request
                .messages
                .last()
                .map(|m| m.text_content())
                .unwrap_or_default();
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            tokio::spawn(async move {
                for word in last.split_whitespace() {
                    let chunk = CompletionChunk {
                        delta: format!("{word} "),
                        finish_reason: None,
                        usage: None,
                    };
                    if tx.send(Ok(chunk)).await.is_err() {
                        return;
                    }
                }
            });
            Ok(rx)
        })
    }
}

#[tokio::test]
async fn provider_completes_behind_dyn() {
    // Application code depends on the trait, not a vendor — hold it as a
    // trait object without any registry indirection.
    let provider: Arc<dyn GenAiProvider> = Arc::new(EchoProvider);
    assert_eq!(provider.name(), "echo");

    let request = CompletionRequest::builder("echo-1")
        .message(Message::user("hello world"))
        .build();
    let response = provider.complete(request).await.unwrap();
    assert_eq!(response.text(), "echo: hello world");
    assert!(provider.supports(Capability::Streaming));
    assert!(!provider.supports(Capability::Vision));
}

#[tokio::test]
async fn streaming_yields_chunks() {
    let provider = EchoProvider;
    let request = CompletionRequest::builder("echo-1")
        .message(Message::user("one two three"))
        .build();

    let mut stream = provider.complete_stream(request).await.unwrap();
    let mut collected = String::new();
    while let Some(chunk) = stream.recv().await {
        collected.push_str(&chunk.unwrap().delta);
    }
    assert_eq!(collected.trim(), "one two three");
}
