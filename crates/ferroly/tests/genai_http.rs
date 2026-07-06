#![cfg(feature = "genai")]
//! Drives the GenAI providers against a local canned HTTP server so the
//! transport, response parsing, and streaming (`sse::pump`) paths are exercised
//! without any network access.

#![allow(dead_code)]

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Spawns a one-shot server returning `response` verbatim; yields the base URL.
async fn spawn_canned(response: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(&response).await;
            let _ = sock.flush().await;
        }
    });
    format!("http://{addr}")
}

/// A `200 OK` response with the given body.
async fn ok_body(content_type: &str, body: &str) -> String {
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    spawn_canned(resp.into_bytes()).await
}

/// A `401` error response.
async fn error_401(body: &str) -> String {
    let resp = format!(
        "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    spawn_canned(resp.into_bytes()).await
}

// ---- OpenAI ----------------------------------------------------------------

#[cfg(feature = "openai")]
mod openai {
    use super::*;
    use ferroly::genai::{
        CompletionRequest, GenAiError, GenAiProvider, Message, OpenAiProvider, ProviderOptions,
    };

    fn provider(base: String) -> OpenAiProvider {
        OpenAiProvider::new("sk-test", Some(ProviderOptions::with_base_url(base)))
    }

    fn request() -> CompletionRequest {
        CompletionRequest::builder("gpt-4o")
            .message(Message::user("hi"))
            .build()
    }

    #[tokio::test]
    async fn complete() {
        let base = ok_body(
            "application/json",
            r#"{"model":"gpt-4o","choices":[{"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}"#,
        )
        .await;
        let resp = provider(base).complete(request()).await.unwrap();
        assert_eq!(resp.text(), "hello");
        assert_eq!(resp.usage.unwrap().total_tokens, Some(4));
    }

    #[tokio::test]
    async fn streams() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"He\"}}]}\n\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n\
                   data: [DONE]\n\n";
        let base = ok_body("text/event-stream", sse).await;
        let mut stream = provider(base).complete_stream(request()).await.unwrap();
        let mut out = String::new();
        while let Some(chunk) = stream.recv().await {
            out.push_str(&chunk.unwrap().delta);
        }
        assert_eq!(out, "Hello");
    }

    #[tokio::test]
    async fn error_status() {
        let base = error_401("nope").await;
        let err = provider(base).complete(request()).await.unwrap_err();
        assert!(matches!(err, GenAiError::Api { status: 401, .. }));
    }

    #[tokio::test]
    async fn embeds() {
        use ferroly::genai::{EmbedRequest, Embedder};
        let base = ok_body(
            "application/json",
            r#"{"model":"text-embedding-3-small","data":[{"embedding":[0.1,0.2,0.3],"index":0},{"embedding":[0.4,0.5,0.6],"index":1}],"usage":{"prompt_tokens":6,"total_tokens":6}}"#,
        )
        .await;
        let resp = provider(base)
            .embed(EmbedRequest::new(
                "text-embedding-3-small",
                vec!["a".into(), "b".into()],
            ))
            .await
            .unwrap();
        assert_eq!(resp.embeddings.len(), 2);
        assert_eq!(resp.embeddings[0], vec![0.1, 0.2, 0.3]);
        assert_eq!(resp.usage.unwrap().total_tokens, Some(6));
    }
}

// ---- Claude ----------------------------------------------------------------

#[cfg(feature = "claude")]
mod claude {
    use super::*;
    use ferroly::genai::{
        ClaudeProvider, CompletionRequest, GenAiProvider, Message, ProviderOptions,
    };

    fn provider(base: String) -> ClaudeProvider {
        ClaudeProvider::new("k", Some(ProviderOptions::with_base_url(base)))
    }

    fn request() -> CompletionRequest {
        CompletionRequest::builder("claude-sonnet-5")
            .message(Message::user("hi"))
            .build()
    }

    #[tokio::test]
    async fn complete() {
        let base = ok_body(
            "application/json",
            r#"{"model":"claude-sonnet-5","content":[{"type":"text","text":"hi there"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}"#,
        )
        .await;
        let resp = provider(base).complete(request()).await.unwrap();
        assert_eq!(resp.text(), "hi there");
        assert_eq!(resp.usage.unwrap().total_tokens, Some(15));
    }

    #[tokio::test]
    async fn streams() {
        let sse = "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n\
                   data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n\
                   data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n";
        let base = ok_body("text/event-stream", sse).await;
        let mut stream = provider(base).complete_stream(request()).await.unwrap();
        let mut out = String::new();
        while let Some(chunk) = stream.recv().await {
            out.push_str(&chunk.unwrap().delta);
        }
        assert_eq!(out, "Hello");
    }
}

// ---- Ollama ----------------------------------------------------------------

#[cfg(feature = "ollama")]
mod ollama {
    use super::*;
    use ferroly::genai::{
        CompletionRequest, GenAiProvider, Message, OllamaProvider, ProviderOptions,
    };

    fn provider(base: String) -> OllamaProvider {
        OllamaProvider::new(Some(ProviderOptions::with_base_url(base)))
    }

    fn request() -> CompletionRequest {
        CompletionRequest::builder("llama3")
            .message(Message::user("hi"))
            .build()
    }

    #[tokio::test]
    async fn complete() {
        let base = ok_body(
            "application/json",
            r#"{"model":"llama3","message":{"role":"assistant","content":"hello"},"done":true,"prompt_eval_count":8,"eval_count":3}"#,
        )
        .await;
        let resp = provider(base).complete(request()).await.unwrap();
        assert_eq!(resp.text(), "hello");
        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
    }

    #[tokio::test]
    async fn streams() {
        let ndjson = "{\"message\":{\"content\":\"He\"},\"done\":false}\n\
                      {\"message\":{\"content\":\"llo\"},\"done\":false}\n\
                      {\"message\":{\"content\":\"\"},\"done\":true}\n";
        let base = ok_body("application/x-ndjson", ndjson).await;
        let mut stream = provider(base).complete_stream(request()).await.unwrap();
        let mut out = String::new();
        while let Some(chunk) = stream.recv().await {
            out.push_str(&chunk.unwrap().delta);
        }
        assert_eq!(out, "Hello");
    }
}
