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
            Ok(CompletionResponse::new(
                request.model,
                Message {
                    role: Role::Assistant,
                    id: None,
                    parts: vec![MessagePart::Text(format!("echo: {last}"))],
                },
            )
            .with_raw_finish_reason("stop"))
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
                    let chunk = CompletionChunk::text_delta(format!("{word} "));
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
        collected.push_str(chunk.unwrap().text());
    }
    assert_eq!(collected.trim(), "one two three");
}

// ---- MockProvider ----------------------------------------------------------

mod mock_provider {
    use std::sync::Arc;

    use ferroly::genai::{
        collect_stream, CompletionChunk, CompletionRequest, CompletionResponse, EmbedRequest,
        Embedder, FinishReason, GenAiError, GenAiProvider, Message, MockProvider, MockReply,
        ToolCallDelta, Usage,
    };

    fn request(text: &str) -> CompletionRequest {
        CompletionRequest::builder("model-x")
            .message(Message::user(text))
            .build()
    }

    #[tokio::test]
    async fn replays_scripted_replies_and_records_requests() {
        let mock = MockProvider::new("mock");
        mock.push_text("one")
            .push_response(
                CompletionResponse::from_text("served-model", "two").with_usage(Usage::new(3, 4)),
            )
            .push_error(GenAiError::Api {
                status: 503,
                message: "busy".into(),
            });
        assert_eq!(mock.remaining(), 3);

        let first = mock.complete(request("a")).await.unwrap();
        assert_eq!(first.text(), "one");
        assert_eq!(first.model, "model-x", "an empty model is the request's");
        assert_eq!(first.finish_reason, Some(FinishReason::Stop));

        let second = mock.complete(request("b")).await.unwrap();
        assert_eq!(second.model, "served-model");
        assert_eq!(second.usage.unwrap().total_tokens, Some(7));

        assert!(matches!(
            mock.complete(request("c")).await,
            Err(GenAiError::Api { status: 503, .. })
        ));

        // The script is empty: an unscripted call fails loudly.
        let err = mock.complete(request("d")).await.unwrap_err();
        assert!(
            matches!(&err, GenAiError::Config(m) if m.contains("no scripted reply")),
            "{err:?}"
        );

        assert_eq!(mock.call_count(), 4);
        let texts: Vec<String> = mock
            .requests()
            .iter()
            .map(|r| r.messages[0].text_content())
            .collect();
        assert_eq!(texts, ["a", "b", "c", "d"]);
        assert_eq!(mock.last_request().unwrap().model, "model-x");
    }

    #[tokio::test]
    async fn streams_and_responses_are_interchangeable() {
        let mock = Arc::new(MockProvider::new("mock"));

        // A scripted stream, consumed as a stream …
        mock.push_stream_text(["Hel", "lo"]);
        let mut stream = mock.complete_stream(request("x")).await.unwrap();
        let mut text = String::new();
        let mut finished = None;
        while let Some(chunk) = stream.recv().await {
            let chunk = chunk.unwrap();
            text.push_str(chunk.text());
            finished = finished.or(chunk.finish_reason());
        }
        assert_eq!(
            (text.as_str(), finished),
            ("Hello", Some(FinishReason::Stop))
        );

        // … or folded into a response by `complete`.
        mock.push_stream(vec![
            Ok(CompletionChunk::text_delta("calling ")),
            Ok(CompletionChunk::ToolCallDelta(ToolCallDelta {
                index: 0,
                id: Some("c1".into()),
                name: Some("lookup".into()),
                arguments: "{\"q\":\"rust\"}".into(),
            })),
            Ok(CompletionChunk::Usage(Usage::new(5, 6))),
            Ok(CompletionChunk::done(FinishReason::ToolCalls)),
        ]);
        let response = mock.complete(request("x")).await.unwrap();
        assert_eq!(response.text(), "calling ");
        assert_eq!(response.tool_calls()[0].1, "lookup");
        assert_eq!(response.finish_reason, Some(FinishReason::ToolCalls));

        // A scripted response, consumed as a stream and folded back.
        mock.push_response(
            CompletionResponse::from_text("m", "whole").with_usage(Usage::new(1, 2)),
        );
        let folded = collect_stream(mock.complete_stream(request("x")).await.unwrap())
            .await
            .unwrap();
        assert_eq!(folded.text(), "whole");
        assert_eq!(folded.usage.unwrap().total_tokens, Some(3));

        // A stream that fails part-way.
        mock.push(MockReply::Stream(vec![
            Ok(CompletionChunk::text_delta("par")),
            Err(GenAiError::Transport("connection reset".into())),
        ]));
        let err = collect_stream(mock.complete_stream(request("x")).await.unwrap())
            .await
            .unwrap_err();
        assert!(matches!(err, GenAiError::Transport(_)));
    }

    #[tokio::test]
    async fn default_text_and_embeddings() {
        let mock = MockProvider::new("mock").with_default_text("always");
        assert_eq!(mock.complete(request("x")).await.unwrap().text(), "always");
        assert_eq!(mock.complete(request("y")).await.unwrap().text(), "always");

        // Unscripted embeddings are deterministic pseudo-vectors.
        let a = mock
            .embed(EmbedRequest::new("e", vec!["x".into(), "y".into()]).dimensions(4))
            .await
            .unwrap();
        let b = mock
            .embed(EmbedRequest::single("e", "x").dimensions(4))
            .await
            .unwrap();
        assert_eq!(a.embeddings.len(), 2);
        assert_eq!(a.embeddings[0].len(), 4);
        assert_eq!(a.embeddings[0], b.embeddings[0]);
        assert_ne!(a.embeddings[0], a.embeddings[1]);

        // Scripted vectors and errors.
        mock.push_embeddings(vec![vec![1.0, 0.0]]);
        mock.push_embed_error(GenAiError::Api {
            status: 500,
            message: "x".into(),
        });
        assert_eq!(
            mock.embed(EmbedRequest::single("e", "z"))
                .await
                .unwrap()
                .embeddings,
            vec![vec![1.0, 0.0]]
        );
        assert!(mock.embed(EmbedRequest::single("e", "z")).await.is_err());
        assert_eq!(mock.embed_requests().len(), 4);

        // Batching splits the input and keeps the order.
        let inputs: Vec<String> = (0..5).map(|i| format!("text-{i}")).collect();
        let whole = mock
            .embed(EmbedRequest::new("e", inputs.clone()))
            .await
            .unwrap();
        let before = mock.embed_requests().len();
        let batched = mock
            .embed_batched(EmbedRequest::new("e", inputs), 2)
            .await
            .unwrap();
        assert_eq!(batched.embeddings, whole.embeddings);
        assert_eq!(
            mock.embed_requests().len() - before,
            3,
            "5 inputs in batches of 2"
        );
    }
}

// ---- observers --------------------------------------------------------------

#[cfg(feature = "metrics")]
#[tokio::test]
async fn metrics_observer_counts_routed_calls() {
    use ferroly::genai::router::{
        build_registry, ModelInfo, ModelRouter, ProviderSet, RouterBuilder, RoutingConfig, Task,
    };
    use ferroly::genai::{MetricsObserver, MockProvider};
    use ferroly::metrics::Registry;

    let metrics = Registry::new();
    let mock = Arc::new(
        MockProvider::new("mock").with_catalog(vec![ModelInfo::new("", "m")
            .capabilities([Capability::Chat])
            .cost(1.0, 2.0)]),
    );
    let set = ProviderSet::new(vec![mock.clone() as Arc<dyn GenAiProvider>]);
    let registry = build_registry(&set, &RoutingConfig::default()).unwrap();
    let router = RouterBuilder::new(set, registry)
        .observer(Arc::new(MetricsObserver::new(&metrics)))
        .build();

    let ask = || CompletionRequest::new("", vec![Message::user("hi")]);
    mock.push_text("ok");
    router.route(&Task::new(), ask()).await.unwrap();
    mock.push_error(GenAiError::Api {
        status: 400,
        message: "bad".into(),
    });
    let _ = router.route(&Task::new(), ask()).await;

    let text = metrics.encode();
    for outcome in ["ok", "api"] {
        let line = format!(
            r#"genai_route_decisions_total{{model="m",outcome="{outcome}",provider="mock",strategy="capability"}} 1"#
        );
        assert!(text.contains(&line), "missing {line} in:\n{text}");
    }
}

// ---- RAG helper --------------------------------------------------------------

#[cfg(feature = "vectorstore")]
#[tokio::test]
async fn rag_index_embeds_upserts_and_queries_by_text() {
    use ferroly::codec::Value;
    use ferroly::genai::rag::{RagDocument, RagIndex};
    use ferroly::genai::MockProvider;
    use ferroly::vectorstore::{Filter, MemoryStore};

    let embedder = Arc::new(MockProvider::new("mock"));
    let store = Arc::new(MemoryStore::new());
    let index = RagIndex::new(embedder.clone(), store.clone(), "embed-model")
        .dimensions(16)
        .batch_size(2);

    let tenant = |name: &str| Value::Object(vec![("tenant".to_string(), Value::from(name))]);
    let stored = index
        .upsert(vec![
            RagDocument::new("a", "Rust is a systems programming language.")
                .with_metadata(tenant("x")),
            RagDocument::new("b", "Paris is the capital of France.").with_metadata(tenant("x")),
            RagDocument::new("c", "The mitochondria is the powerhouse of the cell.")
                .with_metadata(tenant("y")),
        ])
        .await
        .unwrap();
    assert_eq!(stored, 3);
    assert_eq!(store.len(), 3);
    // Three texts in batches of two: two embedding calls, same model and size.
    let calls = embedder.embed_requests();
    assert_eq!(calls.len(), 2);
    assert!(calls
        .iter()
        .all(|c| c.model == "embed-model" && c.dimensions == Some(16)));

    // The mock embeds identical text identically, so an exact query finds it.
    let hits = index
        .query("Paris is the capital of France.", 2)
        .await
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].id, "b");
    assert_eq!(
        hits[0].text.as_deref(),
        Some("Paris is the capital of France.")
    );
    assert_eq!(hits[0].metadata.get("tenant").unwrap().as_str(), Some("x"));
    assert!(hits[0].score > hits[1].score);

    // Metadata filters scope the search (for example to one tenant).
    let hits = index
        .query_filtered(
            "Paris is the capital of France.",
            5,
            Filter::eq("tenant", "y"),
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "c");

    // Nothing is stored when embedding fails.
    embedder.push_embed_error(GenAiError::Api {
        status: 500,
        message: "down".into(),
    });
    assert!(index
        .upsert(vec![RagDocument::new("d", "new")])
        .await
        .is_err());
    assert_eq!(store.len(), 3);
}
