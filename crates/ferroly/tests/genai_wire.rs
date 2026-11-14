#![cfg(feature = "genai")]
//! Wire-shape tests for the GenAI providers.
//!
//! Every test drives a provider against the scripted local server in
//! `genai_support` and asserts on the **exact request body** the provider sent
//! and on what it parsed from the scripted response. Nothing here talks to a
//! real provider.

#![allow(dead_code, unused_imports)]

mod genai_support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::codec::Value;
use ferroly::genai::{
    collect_stream, ChunkStream, CompletionChunk, CompletionResponse, FinishReason, GenAiError,
    GenAiObserver, Message, MessagePart, Operation, PayloadDirection, RequestInfo, RequestOutcome,
    Role, TimeoutPhase, ToolDefinition,
};
use genai_support::{j, Reply, Server};

/// The weather tool used across the tool-calling tests.
fn weather_tool() -> ToolDefinition {
    ToolDefinition::new(
        "get_weather",
        "Get weather",
        j(r#"{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}"#),
    )
}

/// A conversation in which the assistant made two parallel tool calls and
/// both results came back.
fn tool_conversation() -> Vec<Message> {
    let mut call = Message::assistant("Let me check.");
    call.add_tool_call("call_1", "get_weather", j(r#"{"city":"Paris"}"#))
        .add_tool_call("call_2", "get_weather", j(r#"{"city":"Rome"}"#));
    vec![
        Message::user("Weather in Paris and Rome?"),
        call,
        Message::tool_result("call_1", "18C"),
        Message::tool_result("call_2", j(r#"{"temp":24}"#)),
    ]
}

/// Drains a stream into its items.
async fn drain(mut stream: ChunkStream) -> Vec<Result<CompletionChunk, GenAiError>> {
    let mut items = Vec::new();
    while let Some(item) = stream.recv().await {
        items.push(item);
    }
    items
}

/// An observer that records what it is told.
#[derive(Default)]
struct Recorder {
    starts: Mutex<Vec<RequestInfo>>,
    ends: Mutex<Vec<(RequestInfo, RequestOutcome)>>,
    payloads: Mutex<Vec<(PayloadDirection, Value)>>,
    want_payloads: bool,
}

impl GenAiObserver for Recorder {
    fn on_request_start(&self, info: &RequestInfo) {
        self.starts.lock().unwrap().push(info.clone());
    }
    fn on_request_end(&self, info: &RequestInfo, outcome: &RequestOutcome) {
        self.ends
            .lock()
            .unwrap()
            .push((info.clone(), outcome.clone()));
    }
    fn request_headers(&self, _info: &RequestInfo) -> Vec<(String, String)> {
        vec![("x-observed".to_string(), "yes".to_string())]
    }
    fn wants_payloads(&self) -> bool {
        self.want_payloads
    }
    fn on_payload(&self, _info: &RequestInfo, direction: PayloadDirection, body: &Value) {
        self.payloads
            .lock()
            .unwrap()
            .push((direction, body.clone()));
    }
}

// ---- OpenAI ----------------------------------------------------------------

#[cfg(feature = "openai")]
mod openai {
    use super::*;
    use ferroly::clients::{ApiKeyAuth, RetryPolicy};
    use ferroly::genai::{
        CompletionRequest, EmbedRequest, Embedder, GenAiProvider, OpenAiConfig, OpenAiProvider,
        Options, ProviderOptions, ReasoningEffort, ResponseFormat, ToolChoice,
    };

    const KEY: &str = "sk-test-0123456789abcdef";

    fn provider(server: &Server) -> OpenAiProvider {
        OpenAiProvider::new(KEY, Some(ProviderOptions::with_base_url(&server.url)))
    }

    const OK: &str = r#"{"id":"chatcmpl-1","model":"gpt-4o","choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1000,"completion_tokens":500,"total_tokens":1500}}"#;

    fn simple() -> CompletionRequest {
        CompletionRequest::builder("gpt-4o")
            .message(Message::user("hi"))
            .build()
    }

    /// P0-11: assistant tool calls and tool results are encoded in the Chat
    /// Completions shape, and parallel tool calls are parsed from the answer.
    #[tokio::test]
    async fn tool_calls_and_results_round_trip() {
        let server = Server::one(Reply::json(
            200,
            r#"{"model":"gpt-4o","choices":[{"message":{"role":"assistant","content":null,"tool_calls":[
                {"id":"call_a","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Oslo\"}"}},
                {"id":"call_b","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Bergen\"}"}}
            ]},"finish_reason":"tool_calls"}]}"#,
        ))
        .await;
        let request = CompletionRequest::builder("gpt-4o")
            .messages(tool_conversation())
            .tool(weather_tool())
            .tool_choice(ToolChoice::Auto)
            .options(Options::builder().parallel_tool_calls(true).build())
            .build();
        let response = provider(&server).complete(request).await.unwrap();

        let sent = server.request(0);
        assert_eq!(sent.method, "POST");
        assert_eq!(sent.path, "/v1/chat/completions");
        assert_eq!(
            sent.body,
            concat!(
                r#"{"model":"gpt-4o","messages":["#,
                r#"{"role":"user","content":"Weather in Paris and Rome?"},"#,
                r#"{"role":"assistant","content":"Let me check.","tool_calls":["#,
                r#"{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}},"#,
                r#"{"id":"call_2","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Rome\"}"}}]},"#,
                r#"{"role":"tool","tool_call_id":"call_1","content":"18C"},"#,
                r#"{"role":"tool","tool_call_id":"call_2","content":"{\"temp\":24}"}],"#,
                r#""stream":false,"#,
                r#""tools":[{"type":"function","function":{"name":"get_weather","description":"Get weather","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}],"#,
                r#""parallel_tool_calls":true,"tool_choice":"auto"}"#
            )
        );

        assert_eq!(response.finish_reason, Some(FinishReason::ToolCalls));
        let calls = response.tool_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!((calls[0].0, calls[0].1), ("call_a", "get_weather"));
        assert_eq!(calls[0].2.get("city").unwrap().as_str(), Some("Oslo"));
        assert_eq!(calls[1].0, "call_b");
        assert_eq!(calls[1].2.get("city").unwrap().as_str(), Some("Bergen"));
    }

    /// An assistant turn that is only tool calls has `content: null`, and
    /// several results in one tool message become one message each.
    #[tokio::test]
    async fn tool_call_without_text_has_null_content() {
        let server = Server::one(Reply::json(200, OK)).await;
        let mut results = Message::tool_result("c1", "one");
        results.add_tool_result("c2", "two");
        let mut calls = Message::tool_call("c1", "t", j("{}"));
        calls.add_tool_call("c2", "t", j("{}"));
        let request = CompletionRequest::builder("gpt-4o")
            .messages(vec![Message::user("go"), calls, results])
            .build();
        provider(&server).complete(request).await.unwrap();
        assert_eq!(
            server.request(0).body,
            concat!(
                r#"{"model":"gpt-4o","messages":[{"role":"user","content":"go"},"#,
                r#"{"role":"assistant","content":null,"tool_calls":["#,
                r#"{"id":"c1","type":"function","function":{"name":"t","arguments":"{}"}},"#,
                r#"{"id":"c2","type":"function","function":{"name":"t","arguments":"{}"}}]},"#,
                r#"{"role":"tool","tool_call_id":"c1","content":"one"},"#,
                r#"{"role":"tool","tool_call_id":"c2","content":"two"}],"stream":false}"#
            )
        );
    }

    /// G2: a stream carries text, parallel tool-call fragments, usage and the
    /// finish reason, and folds into a complete response.
    #[tokio::test]
    async fn stream_carries_text_tool_calls_usage_and_finish() {
        let server = Server::one(Reply::sse(&[
            "data: {\"id\":\"chatcmpl-9\",\"model\":\"gpt-4o-2024\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Check\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ing.\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_2\",\"type\":\"function\",\"function\":{\"name\":\"get_time\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"Paris\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":9,\"total_tokens\":29,\"prompt_tokens_details\":{\"cached_tokens\":16}}}\n\n",
            "data: [DONE]\n\n",
        ]))
        .await;
        let request = CompletionRequest::builder("gpt-4o")
            .message(Message::user("weather and time?"))
            .tool(weather_tool())
            .build();
        let stream = provider(&server).complete_stream(request).await.unwrap();
        let chunks: Vec<CompletionChunk> = drain(stream)
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();

        // The request asked for usage in the stream.
        let body = server.request(0).json();
        assert_eq!(body.get("stream").unwrap().as_bool(), Some(true));
        assert_eq!(
            body.get("stream_options").unwrap(),
            &j(r#"{"include_usage":true}"#)
        );

        // Simple consumers still get the text by ignoring everything else.
        let text: String = chunks.iter().map(|c| c.text()).collect();
        assert_eq!(text, "Checking.");
        assert!(matches!(
            &chunks[0],
            CompletionChunk::Start { id: Some(id), model: Some(model) }
                if id == "chatcmpl-9" && model == "gpt-4o-2024"
        ));
        assert_eq!(chunks.iter().filter(|c| c.tool_call().is_some()).count(), 4);
        assert!(chunks.iter().any(|c| c.usage().is_some()));
        assert_eq!(
            chunks.iter().find_map(|c| c.finish_reason()),
            Some(FinishReason::ToolCalls)
        );

        let mut acc = ferroly::genai::StreamAccumulator::new();
        for chunk in chunks {
            acc.push(chunk);
        }
        let response = acc.finish().unwrap();
        assert_eq!(response.model, "gpt-4o-2024");
        assert_eq!(response.text(), "Checking.");
        let calls = response.tool_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!((calls[0].0, calls[0].1), ("call_1", "get_weather"));
        assert_eq!(calls[0].2.get("city").unwrap().as_str(), Some("Paris"));
        assert_eq!((calls[1].0, calls[1].1), ("call_2", "get_time"));
        let usage = response.usage.unwrap();
        assert_eq!(usage.prompt_tokens, Some(20));
        assert_eq!(usage.completion_tokens, Some(9));
        assert_eq!(usage.cached_tokens, Some(16));
        assert_eq!(response.raw_finish_reason.as_deref(), Some("tool_calls"));
    }

    /// G2: a failure reported inside the stream is an `Err`, not a quiet end.
    #[tokio::test]
    async fn stream_error_event_and_truncated_stream_are_errors() {
        let server = Server::start(vec![
            Reply::sse(&[
                "data: {\"choices\":[{\"delta\":{\"content\":\"par\"}}]}\n\n",
                "data: {\"error\":{\"message\":\"The server had an error\",\"type\":\"server_error\"}}\n\n",
            ]),
            // The connection ends without a finish reason or `[DONE]`.
            Reply::sse(&["data: {\"choices\":[{\"delta\":{\"content\":\"par\"}}]}\n\n"]),
        ])
        .await;
        let p = provider(&server);

        let items = drain(p.complete_stream(simple()).await.unwrap()).await;
        assert_eq!(items[0].as_ref().unwrap().text(), "par");
        let err = items.last().unwrap().as_ref().unwrap_err();
        assert!(
            matches!(err, GenAiError::Api { status: 500, message } if message.contains("server had an error")),
            "{err:?}"
        );

        let items = drain(p.complete_stream(simple()).await.unwrap()).await;
        assert!(matches!(
            items.last().unwrap(),
            Err(GenAiError::Transport(m)) if m.contains("before the provider signalled completion")
        ));
        // collect_stream surfaces the error instead of a partial response.
        let server = Server::one(Reply::sse(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"par\"}}]}\n\n",
        ]))
        .await;
        let stream = provider(&server).complete_stream(simple()).await.unwrap();
        assert!(collect_stream(stream).await.is_err());
    }

    /// G3: multi-byte characters split across network chunks, CRLF line
    /// endings, comment lines, and a `data:` field continued on a second line
    /// are all reassembled.
    #[tokio::test]
    async fn sse_framing_survives_arbitrary_chunk_boundaries() {
        let event = "data: {\"choices\":[{\"delta\":{\"content\":\"h\u{e9}llo \u{4e16}\u{754c} \u{1f600}\"}}]}\r\n\r\n";
        let bytes = event.as_bytes();
        // Cut inside "é" (2 bytes), inside "世" (3 bytes) and inside the emoji
        // (4 bytes).
        let e_acute = event.find('\u{e9}').unwrap() + 1;
        let cjk = event.find('\u{4e16}').unwrap() + 2;
        let emoji = event.find('\u{1f600}').unwrap() + 3;
        let server = Server::one(Reply::chunked(
            "text/event-stream",
            &[
                b": keep-alive comment\r\n\r\n",
                &bytes[..e_acute],
                &bytes[e_acute..cjk],
                &bytes[cjk..emoji],
                &bytes[emoji..],
                // One JSON document spread over two `data:` lines.
                b"data: {\"choices\":[{\"delta\":\ndata: {\"content\":\"!\"}}]}\n\n",
                // A line split in the middle of the field name.
                b"da",
                b"ta: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            ],
        ))
        .await;
        let response = collect_stream(provider(&server).complete_stream(simple()).await.unwrap())
            .await
            .unwrap();
        assert_eq!(response.text(), "h\u{e9}llo \u{4e16}\u{754c} \u{1f600}!");
        assert_eq!(response.finish_reason, Some(FinishReason::Stop));
    }

    /// G12 + options coverage: the token limit goes out as
    /// `max_completion_tokens`, and every typed option lands in its field.
    #[tokio::test]
    async fn typed_options_are_sent() {
        let server = Server::start(vec![Reply::json(200, OK), Reply::json(200, OK)]).await;
        let options = Options::builder()
            .max_tokens(256)
            .temperature(0.2)
            .top_p(0.9)
            .top_k(40) // no Chat Completions equivalent: not sent
            .stop("END")
            .stop("STOP")
            .seed(7)
            .presence_penalty(0.5)
            .frequency_penalty(0.25)
            .user("user-123")
            .reasoning_effort(ReasoningEffort::XHigh)
            .system_instructions("Be terse.")
            .build();
        let request = CompletionRequest::builder("gpt-4o")
            .message(Message::user("hi"))
            .options(options)
            .build();
        provider(&server).complete(request.clone()).await.unwrap();
        assert_eq!(
            server.request(0).body,
            concat!(
                r#"{"model":"gpt-4o","messages":[{"role":"system","content":"Be terse."},{"role":"user","content":"hi"}],"#,
                r#""stream":false,"temperature":0.2,"top_p":0.9,"max_completion_tokens":256,"#,
                r#""stop":["END","STOP"],"seed":7,"presence_penalty":0.5,"frequency_penalty":0.25,"#,
                r#""user":"user-123","reasoning_effort":"high"}"#
            )
        );

        // OpenAI-compatible servers that only know the old name.
        let legacy = OpenAiProvider::with_config(
            OpenAiConfig::new(KEY).legacy_max_tokens(true),
            Some(ProviderOptions::with_base_url(&server.url)),
        );
        legacy.complete(request).await.unwrap();
        let body = server.request(1).json();
        assert_eq!(body.get("max_tokens").unwrap().as_u64(), Some(256));
        assert!(body.get("max_completion_tokens").is_none());
    }

    /// B21: `Options::custom` reaches the request body; a typed option wins;
    /// provider-owned keys are rejected before anything is sent.
    #[tokio::test]
    async fn custom_options_are_merged_into_the_body() {
        let server = Server::one(Reply::json(200, OK)).await;
        let options = Options::builder()
            .seed(7)
            .custom("seed", 99i64) // loses to the typed option
            .custom("logprobs", true)
            .custom("top_logprobs", 3i64)
            .custom("service_tier", "flex")
            .build();
        let request = CompletionRequest::builder("gpt-4o")
            .message(Message::user("hi"))
            .options(options)
            .build();
        let p = provider(&server);
        p.complete(request).await.unwrap();
        assert_eq!(
            server.request(0).body,
            concat!(
                r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"stream":false,"seed":7,"#,
                r#""logprobs":true,"service_tier":"flex","top_logprobs":3}"#
            )
        );

        for owned in ["model", "messages", "stream"] {
            let request = CompletionRequest::builder("gpt-4o")
                .message(Message::user("hi"))
                .options(Options::builder().custom(owned, "x").build())
                .build();
            let err = p.complete(request).await.unwrap_err();
            assert!(matches!(err, GenAiError::Config(_)), "{owned}: {err:?}");
        }
        assert_eq!(server.hits(), 1, "rejected requests were never sent");
    }

    /// Structured output: `strict` is sent when requested, and the answer is
    /// decoded even when the model wraps it in a code fence.
    #[tokio::test]
    async fn structured_output_strict_schema_and_fenced_answer() {
        #[derive(ferroly::codec::Decode, Debug, PartialEq)]
        struct City {
            name: String,
            population: u32,
        }
        let schema = j(
            r#"{"type":"object","properties":{"name":{"type":"string"},"population":{"type":"integer"}},"required":["name","population"],"additionalProperties":false}"#,
        );
        let fenced = r#"{"model":"gpt-4o","choices":[{"message":{"role":"assistant","content":"```json\n{\"name\":\"Paris\",\"population\":2100000}\n```"},"finish_reason":"stop"}]}"#;
        let server = Server::start(vec![Reply::json(200, fenced), Reply::json(200, OK)]).await;
        let p = provider(&server);

        let request = CompletionRequest::builder("gpt-4o")
            .message(Message::user("Describe Paris"))
            .response_format(ResponseFormat::strict_schema("city", schema.clone()))
            .build();
        let response = p.complete(request).await.unwrap();
        assert_eq!(
            server.request(0).json().get("response_format").unwrap(),
            &j(&format!(
                r#"{{"type":"json_schema","json_schema":{{"name":"city","schema":{},"strict":true}}}}"#,
                ferroly::codec::json::to_string(&schema)
            ))
        );
        assert_eq!(
            response.decode::<City>().unwrap(),
            City {
                name: "Paris".into(),
                population: 2_100_000
            }
        );

        // The shorthand is not strict.
        let request = CompletionRequest::builder("gpt-4o")
            .message(Message::user("x"))
            .response_format(ResponseFormat::JsonSchema(schema))
            .build();
        let plain = p.complete(request).await.unwrap();
        let format = server.request(1).json();
        let json_schema = format
            .get("response_format")
            .unwrap()
            .get("json_schema")
            .unwrap();
        assert_eq!(json_schema.get("name").unwrap().as_str(), Some("response"));
        assert!(json_schema.get("strict").is_none());
        // "ok" is not JSON: a clear parse error, not a panic or a default.
        assert!(matches!(
            plain.decode::<City>(),
            Err(GenAiError::ResponseParse(_))
        ));
    }

    /// The error taxonomy: status, error type and headers pick the variant.
    #[tokio::test]
    async fn provider_errors_map_to_specific_variants() {
        let long_message = "x".repeat(10_000);
        let big =
            format!(r#"{{"error":{{"message":"{long_message}","type":"invalid_request_error"}}}}"#);
        let echo = format!(
            r#"{{"error":{{"message":"Incorrect API key provided: {KEY}","type":"invalid_request_error","code":"invalid_api_key"}}}}"#
        );
        let server = Server::start(vec![
            Reply::with_headers(
                429,
                &[
                    ("Retry-After", "7"),
                    ("x-ratelimit-remaining-requests", "0"),
                    ("x-ratelimit-reset-requests", "7s"),
                ],
                "application/json",
                r#"{"error":{"message":"Rate limit reached","type":"requests","code":"rate_limit_exceeded"}}"#,
            ),
            Reply::json(401, &echo),
            Reply::json(
                400,
                r#"{"error":{"message":"This model's maximum context length is 128000 tokens.","type":"invalid_request_error","code":"context_length_exceeded"}}"#,
            ),
            Reply::json(
                400,
                r#"{"error":{"message":"Your request was rejected by the safety system.","type":"invalid_request_error","code":"content_policy_violation"}}"#,
            ),
            Reply::with_headers(503, &[("retry-after-ms", "250")], "text/plain", "upstream overloaded"),
            Reply::json(400, &big),
            Reply::json(
                429,
                r#"{"error":{"message":"You exceeded your current quota","type":"insufficient_quota","code":"insufficient_quota"}}"#,
            ),
        ])
        .await;
        let p = provider(&server);
        let next = || p.complete(simple());

        match next().await.unwrap_err() {
            GenAiError::RateLimited {
                message,
                retry_after,
                limits,
            } => {
                assert!(message.contains("Rate limit reached"));
                assert_eq!(retry_after, Some(Duration::from_secs(7)));
                assert_eq!(limits.get("x-ratelimit-remaining-requests"), Some("0"));
                assert_eq!(limits.get("X-RateLimit-Reset-Requests"), Some("7s"));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
        match next().await.unwrap_err() {
            GenAiError::Auth { status, message } => {
                assert_eq!(status, 401);
                // The key the server echoed back never reaches the error.
                assert!(!message.contains(KEY), "{message}");
                assert!(message.contains("[REDACTED]"), "{message}");
            }
            other => panic!("expected Auth, got {other:?}"),
        }
        assert!(matches!(
            next().await.unwrap_err(),
            GenAiError::ContextLengthExceeded { .. }
        ));
        assert!(matches!(
            next().await.unwrap_err(),
            GenAiError::ContentFilter { .. }
        ));
        match next().await.unwrap_err() {
            GenAiError::Overloaded {
                status,
                message,
                retry_after,
            } => {
                assert_eq!(status, 503);
                assert_eq!(message, "upstream overloaded");
                assert_eq!(retry_after, Some(Duration::from_millis(250)));
            }
            other => panic!("expected Overloaded, got {other:?}"),
        }
        match next().await.unwrap_err() {
            GenAiError::Api { status, message } => {
                assert_eq!(status, 400);
                assert!(message.len() < 2_200, "body was capped: {}", message.len());
                assert!(message.ends_with("[truncated]"));
            }
            other => panic!("expected Api, got {other:?}"),
        }
        // Out of quota is a 429 that waiting cannot fix: not retryable.
        let quota = next().await.unwrap_err();
        assert!(
            matches!(quota, GenAiError::Auth { status: 429, .. }),
            "{quota:?}"
        );
        assert!(!quota.is_transient());
    }

    /// G4: a response timeout is its own error and is never re-sent, even
    /// with retries configured — the completion may already have been billed.
    #[tokio::test]
    async fn response_timeout_is_distinct_and_not_retried() {
        let server = Server::start(vec![Reply::hang(), Reply::json(200, OK)]).await;
        let opts = ProviderOptions::with_base_url(&server.url)
            .response_timeout(Duration::from_millis(300))
            .retry(RetryPolicy::fixed(3, Duration::from_millis(1)));
        let p = OpenAiProvider::new(KEY, Some(opts));
        let err = p.complete(simple()).await.unwrap_err();
        assert!(
            matches!(
                err,
                GenAiError::Timeout {
                    phase: TimeoutPhase::Response
                }
            ),
            "{err:?}"
        );
        assert!(err.is_timeout() && !err.is_transient());
        assert_eq!(server.hits(), 1, "the timed-out completion was not re-sent");
    }

    /// With retries configured, throttling is retried after the server's
    /// delay and transient failures after the backoff.
    #[tokio::test]
    async fn retry_honours_retry_after() {
        let server = Server::start(vec![
            Reply::with_headers(
                429,
                &[("retry-after-ms", "60")],
                "application/json",
                r#"{"error":{"message":"slow down","type":"requests"}}"#,
            ),
            Reply::json(500, r#"{"error":{"message":"oops","type":"server_error"}}"#),
            Reply::json(200, OK),
        ])
        .await;
        let opts = ProviderOptions::with_base_url(&server.url)
            .retry(RetryPolicy::fixed(3, Duration::from_millis(1)));
        let started = std::time::Instant::now();
        let response = OpenAiProvider::new(KEY, Some(opts))
            .complete(simple())
            .await
            .unwrap();
        assert_eq!(response.text(), "ok");
        assert_eq!(server.hits(), 3);
        assert!(
            started.elapsed() >= Duration::from_millis(60),
            "waited the server-requested delay"
        );
    }

    /// G5: a silent stream trips the idle timeout, and dropping the receiver
    /// stops the reader and closes the connection.
    #[tokio::test]
    async fn stream_idle_timeout_and_cancellation() {
        let first = "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n";
        let server = Server::start(vec![
            Reply::chunked_then_hang("text/event-stream", &[first.as_bytes()]),
            Reply::chunked_then_hang("text/event-stream", &[first.as_bytes()]),
        ])
        .await;

        // Idle timeout.
        let opts = ProviderOptions::with_base_url(&server.url)
            .stream_idle_timeout(Duration::from_millis(500));
        let p = OpenAiProvider::new(KEY, Some(opts));
        let items = drain(p.complete_stream(simple()).await.unwrap()).await;
        assert_eq!(items[0].as_ref().unwrap().text(), "a");
        assert!(
            matches!(
                items.last().unwrap(),
                Err(GenAiError::Timeout {
                    phase: TimeoutPhase::StreamIdle
                })
            ),
            "{:?}",
            items.last()
        );
        assert!(server.wait_closed(1, Duration::from_secs(5)).await);

        // Receiver dropped: no idle timeout involved (the default is minutes).
        let p = provider(&server);
        let mut stream = p.complete_stream(simple()).await.unwrap();
        assert_eq!(stream.recv().await.unwrap().unwrap().text(), "a");
        drop(stream);
        assert!(
            server.wait_closed(2, Duration::from_secs(5)).await,
            "the reader task closed its connection when the receiver went away"
        );
    }

    /// Custom auth, extra headers, observer headers; and Azure's path, query
    /// and `api-key` header.
    #[tokio::test]
    async fn auth_headers_and_azure_endpoint() {
        let server = Server::start(vec![
            Reply::json(200, OK),
            Reply::json(200, OK),
            Reply::json(200, OK),
        ])
        .await;

        // Built-in bearer key.
        provider(&server).complete(simple()).await.unwrap();
        let sent = server.request(0);
        assert_eq!(
            sent.header("authorization"),
            Some(&*format!("Bearer {KEY}"))
        );
        assert_eq!(sent.header("content-type"), Some("application/json"));

        // A custom auth provider replaces it; extra headers ride along.
        let opts = ProviderOptions::with_base_url(&server.url)
            .auth(Arc::new(ApiKeyAuth::new("x-gateway-key", "gw-secret")))
            .header("OpenAI-Organization", "org-1")
            .observer(Arc::new(Recorder::default()));
        OpenAiProvider::new(KEY, Some(opts))
            .complete(simple())
            .await
            .unwrap();
        let sent = server.request(1);
        assert_eq!(sent.header("x-gateway-key"), Some("gw-secret"));
        assert_eq!(sent.header("authorization"), None);
        assert_eq!(sent.header("openai-organization"), Some("org-1"));
        assert_eq!(sent.header("x-observed"), Some("yes"));

        // Azure OpenAI.
        let azure = OpenAiProvider::with_config(
            OpenAiConfig::azure(&server.url, "gpt4o-prod", "2024-10-21", "azure-key"),
            None,
        );
        assert_eq!(azure.name(), "azure-openai");
        azure.complete(simple()).await.unwrap();
        let sent = server.request(2);
        assert_eq!(
            sent.path,
            "/openai/deployments/gpt4o-prod/chat/completions?api-version=2024-10-21"
        );
        assert_eq!(sent.header("api-key"), Some("azure-key"));
        assert_eq!(sent.header("authorization"), None);
    }

    /// Embeddings: `dimensions` is sent, large inputs are split into batches,
    /// and vectors come back in input order whatever order the server used.
    #[tokio::test]
    async fn embeddings_are_batched_in_order() {
        let server = Server::start(vec![
            // Deliberately out of order: index 1 before index 0.
            Reply::json(200, r#"{"model":"e","data":[{"index":1,"embedding":[2.0]},{"index":0,"embedding":[1.0]}],"usage":{"prompt_tokens":4,"total_tokens":4}}"#),
            Reply::json(200, r#"{"model":"e","data":[{"index":0,"embedding":[3.0]},{"index":1,"embedding":[4.0]}],"usage":{"prompt_tokens":4,"total_tokens":4}}"#),
            Reply::json(200, r#"{"model":"e","data":[{"index":0,"embedding":[5]}],"usage":{"prompt_tokens":2,"total_tokens":2}}"#),
        ])
        .await;
        let opts = ProviderOptions::with_base_url(&server.url).embed_batch_size(2);
        let p = OpenAiProvider::new(KEY, Some(opts));
        let inputs: Vec<String> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let response = p
            .embed(EmbedRequest::new("e", inputs).dimensions(1).user("u-1"))
            .await
            .unwrap();
        assert_eq!(
            response.embeddings,
            vec![vec![1.0], vec![2.0], vec![3.0], vec![4.0], vec![5.0]]
        );
        assert_eq!(response.usage.unwrap().prompt_tokens, Some(10));
        assert_eq!(server.hits(), 3);
        assert_eq!(server.request(0).path, "/v1/embeddings");
        assert_eq!(
            server.request(0).body,
            r#"{"model":"e","input":["a","b"],"dimensions":1,"user":"u-1"}"#
        );
        assert_eq!(
            server.request(2).body,
            r#"{"model":"e","input":["e"],"dimensions":1,"user":"u-1"}"#
        );
    }

    /// A batch that comes back short is an error, not silently shifted data.
    #[tokio::test]
    async fn short_embedding_batch_is_an_error() {
        let server = Server::one(Reply::json(
            200,
            r#"{"model":"e","data":[{"index":0,"embedding":[1.0]}]}"#,
        ))
        .await;
        let err = provider(&server)
            .embed(EmbedRequest::new("e", vec!["a".into(), "b".into()]))
            .await
            .unwrap_err();
        assert!(matches!(err, GenAiError::ResponseParse(_)), "{err:?}");
    }

    /// The observer sees start and end, usage, the cost from catalog prices,
    /// and — only when it asks — the payloads. Credentials never show up.
    #[tokio::test]
    async fn observer_gets_usage_cost_and_payloads() {
        let server = Server::start(vec![
            Reply::json(200, OK),
            Reply::json(401, r#"{"error":{"message":"bad key"}}"#),
            Reply::sse(&[
                "data: {\"choices\":[{\"delta\":{\"content\":\"a\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":2000000,\"completion_tokens\":1000000}}\n\n",
                "data: [DONE]\n\n",
            ]),
        ])
        .await;
        let recorder = Arc::new(Recorder {
            want_payloads: true,
            ..Recorder::default()
        });
        let opts = ProviderOptions::with_base_url(&server.url).observer(recorder.clone());
        let p = OpenAiProvider::new(KEY, Some(opts));

        p.complete(simple()).await.unwrap();
        p.complete(simple()).await.unwrap_err();
        collect_stream(p.complete_stream(simple()).await.unwrap())
            .await
            .unwrap();
        // The stream's end is reported by its reader task.
        for _ in 0..500 {
            if recorder.ends.lock().unwrap().len() == 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let ends = recorder.ends.lock().unwrap().clone();
        assert_eq!(recorder.starts.lock().unwrap().len(), 3);
        assert_eq!(ends.len(), 3);

        let (info, ok) = &ends[0];
        assert_eq!(
            (info.provider.as_str(), info.model.as_str()),
            ("openai", "gpt-4o")
        );
        assert_eq!(info.operation, Operation::Chat);
        assert_eq!(ok.outcome_label(), "ok");
        assert_eq!(ok.finish_reason, Some(FinishReason::Stop));
        assert_eq!(ok.usage.as_ref().unwrap().total_tokens, Some(1500));
        // 1000 in @ $2.50/M + 500 out @ $10/M
        assert!((ok.cost_usd.unwrap() - 0.0075).abs() < 1e-9);

        let (_, failed) = &ends[1];
        assert_eq!(failed.error_kind, Some("auth"));
        assert_eq!(failed.status, Some(401));

        let (info, streamed) = &ends[2];
        assert_eq!(info.operation, Operation::ChatStream);
        assert!(streamed.time_to_first_chunk.is_some());
        // 2M in @ $2.50/M + 1M out @ $10/M
        assert!((streamed.cost_usd.unwrap() - 15.0).abs() < 1e-9);

        let payloads = recorder.payloads.lock().unwrap();
        assert_eq!(payloads[0].0, PayloadDirection::Request);
        assert_eq!(payloads[1].0, PayloadDirection::Response);
        let all = format!("{payloads:?}");
        assert!(!all.contains(KEY), "payloads never include credentials");
    }

    #[test]
    fn debug_output_never_contains_the_key() {
        let p = OpenAiProvider::new(KEY, None);
        let config = OpenAiConfig::new(KEY);
        let opts = ProviderOptions::new()
            .header("x-secret-header", "hunter2")
            .auth(Arc::new(ApiKeyAuth::new("k", KEY)));
        let debug = format!("{p:?} {config:?} {opts:?}");
        assert!(!debug.contains(KEY), "{debug}");
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(debug.contains("x-secret-header"));
    }
}

// ---- Claude ----------------------------------------------------------------

#[cfg(feature = "claude")]
mod claude {
    use super::*;
    use ferroly::clients::BearerAuth;
    use ferroly::genai::{
        CacheTtl, Capability, ClaudeProvider, ClaudeProviderConfig, CompletionRequest,
        GenAiProvider, Options, PromptCache, ProviderOptions, ReasoningEffort, ResponseFormat,
        StreamAccumulator, ToolChoice,
    };

    const KEY: &str = "sk-ant-test-0123456789";
    const MODEL: &str = "claude-opus-5-5";

    fn provider(server: &Server) -> ClaudeProvider {
        ClaudeProvider::new(KEY, Some(ProviderOptions::with_base_url(&server.url)))
    }

    const OK: &str = r#"{"id":"msg_1","model":"claude-opus-5-5","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}"#;

    fn simple() -> CompletionRequest {
        CompletionRequest::builder(MODEL)
            .message(Message::user("hi"))
            .build()
    }

    /// Claude's tool encoding stays correct: `tool_use` blocks on the
    /// assistant turn and all `tool_result` blocks in one user message.
    #[tokio::test]
    async fn tool_calls_and_results_round_trip() {
        let server = Server::one(Reply::json(
            200,
            r#"{"id":"msg_2","model":"claude-opus-5-5","content":[
                {"type":"text","text":"Checking both."},
                {"type":"tool_use","id":"toolu_a","name":"get_weather","input":{"city":"Oslo"}},
                {"type":"tool_use","id":"toolu_b","name":"get_weather","input":{"city":"Bergen"}}
            ],"stop_reason":"tool_use","usage":{"input_tokens":30,"output_tokens":20}}"#,
        ))
        .await;
        let request = CompletionRequest::builder(MODEL)
            .messages(tool_conversation())
            .tool(weather_tool().strict(true))
            .tool_choice(ToolChoice::Auto)
            .options(Options::builder().max_tokens(1024).build())
            .build();
        let response = provider(&server).complete(request).await.unwrap();

        let sent = server.request(0);
        assert_eq!(sent.path, "/v1/messages");
        assert_eq!(sent.header("x-api-key"), Some(KEY));
        assert_eq!(sent.header("anthropic-version"), Some("2023-06-01"));
        assert_eq!(
            sent.body,
            concat!(
                r#"{"model":"claude-opus-5-5","max_tokens":1024,"messages":["#,
                r#"{"role":"user","content":[{"type":"text","text":"Weather in Paris and Rome?"}]},"#,
                r#"{"role":"assistant","content":[{"type":"text","text":"Let me check."},"#,
                r#"{"type":"tool_use","id":"call_1","name":"get_weather","input":{"city":"Paris"}},"#,
                r#"{"type":"tool_use","id":"call_2","name":"get_weather","input":{"city":"Rome"}}]},"#,
                r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"18C"},"#,
                r#"{"type":"tool_result","tool_use_id":"call_2","content":"{\"temp\":24}"}]}],"#,
                r#""stream":false,"#,
                r#""tools":[{"name":"get_weather","description":"Get weather","strict":true,"input_schema":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}],"#,
                r#""tool_choice":{"type":"auto"}}"#
            )
        );

        assert_eq!(response.finish_reason, Some(FinishReason::ToolCalls));
        assert_eq!(response.raw_finish_reason.as_deref(), Some("tool_use"));
        assert_eq!(response.text(), "Checking both.");
        let calls = response.tool_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!((calls[0].0, calls[0].1), ("toolu_a", "get_weather"));
        assert_eq!(calls[1].2.get("city").unwrap().as_str(), Some("Bergen"));
    }

    /// G8: `Message::system` and `system_instructions` are merged (in that
    /// documented order: instructions first), and a missing `max_tokens` uses
    /// the model's catalog limit instead of a tiny silent default.
    #[tokio::test]
    async fn system_sources_are_merged_and_max_tokens_defaults_from_catalog() {
        let server = Server::start(vec![Reply::json(200, OK), Reply::json(200, OK)]).await;
        let request = CompletionRequest::builder(MODEL)
            .message(Message::system("You answer in French."))
            .message(Message::user("hi"))
            .message(Message::system("Keep it short."))
            .options(
                Options::builder()
                    .system_instructions("You are Bob.")
                    .build(),
            )
            .build();
        let p = provider(&server);
        p.complete(request).await.unwrap();
        assert_eq!(
            server.request(0).body,
            concat!(
                r#"{"model":"claude-opus-5-5","max_tokens":16000,"#,
                r#""messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],"stream":false,"#,
                r#""system":"You are Bob.\n\nYou answer in French.\n\nKeep it short."}"#
            )
        );

        // A model outside the catalog gets the documented conservative value,
        // and the provider-level default overrides both.
        let unknown = CompletionRequest::builder("claude-custom-finetune")
            .message(Message::user("hi"))
            .build();
        p.complete(unknown).await.unwrap();
        assert_eq!(
            server.request(1).json().get("max_tokens").unwrap().as_u64(),
            Some(4096)
        );
    }

    /// G8: truncation by the token limit is surfaced as `FinishReason::Length`.
    #[tokio::test]
    async fn truncation_is_surfaced() {
        let server = Server::one(Reply::json(
            200,
            r#"{"model":"claude-opus-5-5","content":[{"type":"text","text":"{\"partial\":"}],"stop_reason":"max_tokens","usage":{"input_tokens":1,"output_tokens":16000}}"#,
        ))
        .await;
        let response = provider(&server).complete(simple()).await.unwrap();
        assert_eq!(response.finish_reason, Some(FinishReason::Length));
        assert!(response.is_truncated());
        let err = response.decode_value().unwrap_err().to_string();
        assert!(err.contains("truncated by the output-token limit"), "{err}");
    }

    /// G8: structured output uses `output_config.format`; schema-less JSON
    /// mode does not exist on the Messages API and is refused, not ignored.
    #[tokio::test]
    async fn structured_output_uses_output_config_and_json_mode_is_unsupported() {
        let server = Server::one(Reply::json(200, OK)).await;
        let schema = j(
            r#"{"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}"#,
        );
        let p = provider(&server);
        let request = CompletionRequest::builder(MODEL)
            .message(Message::user("hi"))
            .response_format(ResponseFormat::JsonSchema(schema.clone()))
            .options(
                Options::builder()
                    .max_tokens(100)
                    .reasoning_effort(ReasoningEffort::Low)
                    .build(),
            )
            .build();
        p.complete(request).await.unwrap();
        assert_eq!(
            server.request(0).json().get("output_config").unwrap(),
            &j(&format!(
                r#"{{"format":{{"type":"json_schema","schema":{}}},"effort":"low"}}"#,
                ferroly::codec::json::to_string(&schema)
            ))
        );

        let request = CompletionRequest::builder(MODEL)
            .message(Message::user("hi"))
            .response_format(ResponseFormat::Json)
            .build();
        let err = p.complete(request).await.unwrap_err();
        assert!(
            matches!(
                &err,
                GenAiError::Unsupported { provider, capability: Capability::JsonMode } if provider == "claude"
            ),
            "{err:?}"
        );
        assert_eq!(server.hits(), 1, "the unsupported request was never sent");
    }

    /// Options coverage: stop sequences, top_k, user id, prompt caching,
    /// serial tool calls and custom keys, in the Messages API's field names.
    #[tokio::test]
    async fn typed_and_custom_options_are_sent() {
        let server = Server::start(vec![
            Reply::json(200, OK),
            Reply::json(200, OK),
            Reply::json(200, OK),
        ])
        .await;
        let options = Options::builder()
            .max_tokens(500)
            .temperature(0.3)
            .top_p(0.8)
            .top_k(20)
            .stop("END")
            .user("user-9")
            .seed(1) // no Messages API equivalent: not sent
            .parallel_tool_calls(false)
            .prompt_cache(PromptCache {
                system: true,
                conversation: true,
                ttl: CacheTtl::OneHour,
            })
            .system_instructions("Stable system prompt.")
            .custom(
                "thinking",
                j(r#"{"type":"adaptive","display":"summarized"}"#),
            )
            .custom(
                "output_config",
                j(r#"{"task_budget":{"type":"tokens","total":20000}}"#),
            )
            .build();
        let request = CompletionRequest::builder(MODEL)
            .message(Message::user("hi"))
            .tool(weather_tool())
            .options(options.clone())
            .build();
        let p = provider(&server);
        p.complete(request.clone()).await.unwrap();
        assert_eq!(
            server.request(0).body,
            concat!(
                r#"{"model":"claude-opus-5-5","max_tokens":500,"#,
                r#""messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],"stream":false,"#,
                r#""system":[{"type":"text","text":"Stable system prompt.","cache_control":{"type":"ephemeral","ttl":"1h"}}],"#,
                r#""cache_control":{"type":"ephemeral","ttl":"1h"},"#,
                r#""temperature":0.3,"top_p":0.8,"top_k":20,"stop_sequences":["END"],"#,
                r#""metadata":{"user_id":"user-9"},"#,
                r#""tools":[{"name":"get_weather","description":"Get weather","input_schema":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}],"#,
                r#""tool_choice":{"type":"auto","disable_parallel_tool_use":true},"#,
                r#""output_config":{"task_budget":{"type":"tokens","total":20000}},"#,
                r#""thinking":{"type":"adaptive","display":"summarized"}}"#
            )
        );

        // Sampling parameters can be left out for models that reject them.
        let quiet = ClaudeProvider::with_config(
            ClaudeProviderConfig::new(KEY)
                .omit_sampling_params(true)
                .api_version("2026-10-21")
                .beta("beta-one")
                .beta("beta-two")
                .default_max_tokens(2000),
            Some(ProviderOptions::with_base_url(&server.url)),
        );
        let mut request = request;
        request.options.max_tokens = None;
        quiet.complete(request).await.unwrap();
        let sent = server.request(1);
        let body = sent.json();
        for dropped in ["temperature", "top_p", "top_k"] {
            assert!(body.get(dropped).is_none(), "{dropped} was sent");
        }
        assert_eq!(body.get("max_tokens").unwrap().as_u64(), Some(2000));
        assert_eq!(sent.header("anthropic-version"), Some("2026-10-21"));
        assert_eq!(sent.header("anthropic-beta"), Some("beta-one,beta-two"));

        // `system` is built by the provider and cannot be smuggled in.
        let request = CompletionRequest::builder(MODEL)
            .message(Message::user("hi"))
            .options(Options::builder().custom("system", "override").build())
            .build();
        assert!(matches!(
            p.complete(request).await.unwrap_err(),
            GenAiError::Config(_)
        ));
        assert_eq!(server.hits(), 2);
    }

    /// G2: every stream event type is handled — text, thinking with its
    /// signature, tool input fragments, usage, stop reason — and the thinking
    /// block is preserved so the next request can send it back unchanged.
    #[tokio::test]
    async fn stream_handles_all_event_types() {
        let server = Server::start(vec![
            Reply::sse(&[
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_9\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-5-5\",\"content\":[],\"usage\":{\"input_tokens\":25,\"cache_read_input_tokens\":100,\"output_tokens\":1}}}\n\n",
                "event: ping\ndata: {\"type\":\"ping\"}\n\n",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Need the weather.\"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-abc\"}}\n\n",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Let me \"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"check.\"}}\n\n",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"get_weather\",\"input\":{}}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\": \\\"Pa\"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"ris\\\"}\"}}\n\n",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":42}}\n\n",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            ]),
            Reply::json(200, OK),
        ])
        .await;
        let p = provider(&server);
        let first_request = CompletionRequest::builder(MODEL)
            .message(Message::user("Weather in Paris?"))
            .tool(weather_tool())
            .options(Options::builder().max_tokens(1000).build())
            .build();
        let stream = p.complete_stream(first_request.clone()).await.unwrap();
        let chunks: Vec<CompletionChunk> = drain(stream)
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();

        let body = server.request(0).json();
        assert_eq!(body.get("stream").unwrap().as_bool(), Some(true));

        let text: String = chunks.iter().map(|c| c.text()).collect();
        assert_eq!(text, "Let me check.");
        assert!(chunks
            .iter()
            .any(|c| matches!(c, CompletionChunk::ReasoningDelta(t) if t == "Need the weather.")));

        let mut acc = StreamAccumulator::new();
        for chunk in chunks {
            acc.push(chunk);
        }
        let response = acc.finish().unwrap();
        assert_eq!(response.id.as_deref(), Some("msg_9"));
        assert_eq!(response.finish_reason, Some(FinishReason::ToolCalls));
        assert_eq!(response.raw_finish_reason.as_deref(), Some("tool_use"));
        assert_eq!(response.reasoning.as_deref(), Some("Need the weather."));
        let calls = response.tool_calls();
        assert_eq!((calls[0].0, calls[0].1), ("toolu_1", "get_weather"));
        assert_eq!(calls[0].2.get("city").unwrap().as_str(), Some("Paris"));
        let usage = response.usage.clone().unwrap();
        // 25 fresh + 100 cached input tokens; output from message_delta.
        assert_eq!(usage.prompt_tokens, Some(125));
        assert_eq!(usage.cached_tokens, Some(100));
        assert_eq!(usage.completion_tokens, Some(42));
        assert_eq!(usage.total_tokens, Some(167));

        // Second turn: the assistant message goes back exactly as produced —
        // thinking block (with signature) first, then text, then tool_use.
        let mut messages = first_request.messages.clone();
        messages.push(response.message);
        messages.push(Message::tool_result("toolu_1", "18C"));
        let second = CompletionRequest::builder(MODEL)
            .messages(messages)
            .tool(weather_tool())
            .options(Options::builder().max_tokens(1000).build())
            .build();
        p.complete(second).await.unwrap();
        let sent = server.request(1).json();
        let assistant = sent.get("messages").unwrap().get_index(1).unwrap();
        assert_eq!(
            assistant,
            &j(concat!(
                r#"{"role":"assistant","content":["#,
                r#"{"type":"thinking","thinking":"Need the weather.","signature":"sig-abc"},"#,
                r#"{"type":"text","text":"Let me check."},"#,
                r#"{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{"city":"Paris"}}]}"#
            ))
        );
    }

    /// G2: an `error` event in the stream is an `Err` of the right kind.
    #[tokio::test]
    async fn stream_error_event_is_an_error() {
        let server = Server::one(Reply::sse(&[
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":1}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n",
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
        ]))
        .await;
        let items = drain(provider(&server).complete_stream(simple()).await.unwrap()).await;
        let text: String = items
            .iter()
            .filter_map(|i| i.as_ref().ok())
            .map(|c| c.text())
            .collect();
        assert_eq!(text, "Hel");
        let err = items.last().unwrap().as_ref().unwrap_err();
        assert!(
            matches!(err, GenAiError::Overloaded { status: 529, message, .. } if message.contains("Overloaded")),
            "{err:?}"
        );
        assert!(err.is_transient());
    }

    /// Error bodies map to the specific variants.
    #[tokio::test]
    async fn provider_errors_map_to_specific_variants() {
        let server = Server::start(vec![
            Reply::json(529, r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#),
            Reply::with_headers(
                429,
                &[("retry-after", "3"), ("anthropic-ratelimit-requests-remaining", "0")],
                "application/json",
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"Too many requests"}}"#,
            ),
            Reply::json(401, r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#),
            Reply::json(400, r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 1200000 tokens > 1000000 maximum"}}"#),
            Reply::json(400, r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages: roles must alternate"}}"#),
        ])
        .await;
        let p = provider(&server);
        assert!(matches!(
            p.complete(simple()).await.unwrap_err(),
            GenAiError::Overloaded { status: 529, .. }
        ));
        match p.complete(simple()).await.unwrap_err() {
            GenAiError::RateLimited {
                retry_after,
                limits,
                ..
            } => {
                assert_eq!(retry_after, Some(Duration::from_secs(3)));
                assert_eq!(
                    limits.get("anthropic-ratelimit-requests-remaining"),
                    Some("0")
                );
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
        assert!(matches!(
            p.complete(simple()).await.unwrap_err(),
            GenAiError::Auth { status: 401, .. }
        ));
        assert!(matches!(
            p.complete(simple()).await.unwrap_err(),
            GenAiError::ContextLengthExceeded { .. }
        ));
        match p.complete(simple()).await.unwrap_err() {
            GenAiError::Api { status, message } => {
                assert_eq!(status, 400);
                assert_eq!(
                    message,
                    "invalid_request_error: messages: roles must alternate"
                );
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }

    /// A custom auth provider (here an OAuth-style bearer token) replaces the
    /// `x-api-key` header, and the key is never printed.
    #[tokio::test]
    async fn custom_auth_and_debug_redaction() {
        let server = Server::one(Reply::json(200, OK)).await;
        let p = ClaudeProvider::with_config(
            ClaudeProviderConfig::with_auth(Arc::new(BearerAuth::new("oauth-token"))),
            Some(ProviderOptions::with_base_url(&server.url)),
        );
        p.complete(simple()).await.unwrap();
        let sent = server.request(0);
        assert_eq!(sent.header("authorization"), Some("Bearer oauth-token"));
        assert_eq!(sent.header("x-api-key"), None);

        let debug = format!(
            "{:?} {:?}",
            ClaudeProvider::new(KEY, None),
            ClaudeProviderConfig::new(KEY)
        );
        assert!(!debug.contains(KEY), "{debug}");
    }

    /// The compiled-in catalog names current models and dates its prices.
    #[test]
    fn catalog_is_current_and_dated() {
        let catalog = ClaudeProvider::new(KEY, None).model_catalog();
        let opus = catalog
            .iter()
            .find(|m| m.name == "claude-opus-5-5")
            .unwrap();
        assert_eq!(opus.max_input_tokens, 1_000_000);
        assert_eq!(opus.max_output_tokens, 128_000);
        assert!(catalog
            .iter()
            .all(|m| m.as_of.is_some() && m.quality_rank.is_some()));
        assert!(catalog.iter().all(|m| m.is_generative()));
        assert!(ClaudeProvider::new(KEY, None)
            .model_info_for("claude-haiku-5-5")
            .is_some());
        assert!(ClaudeProvider::new(KEY, None)
            .model_info_for("claude-3-5-haiku")
            .is_none());
    }
}

// ---- Ollama ----------------------------------------------------------------

#[cfg(feature = "ollama")]
mod ollama {
    use super::*;
    use ferroly::clients::BearerAuth;
    use ferroly::genai::{
        CompletionRequest, EmbedRequest, Embedder, GenAiProvider, OllamaProvider, Options,
        ProviderOptions, ResponseFormat,
    };

    fn provider(server: &Server) -> OllamaProvider {
        OllamaProvider::new(Some(ProviderOptions::with_base_url(&server.url)))
    }

    const OK: &str = r#"{"model":"llama3","message":{"role":"assistant","content":"ok"},"done":true,"done_reason":"stop","prompt_eval_count":8,"eval_count":3}"#;

    fn simple() -> CompletionRequest {
        CompletionRequest::builder("llama3")
            .message(Message::user("hi"))
            .build()
    }

    /// P0-11: assistant tool calls and tool results are encoded in Ollama's
    /// chat shape (arguments as an object, results matched by `tool_name`),
    /// and parallel tool calls are parsed from the answer.
    #[tokio::test]
    async fn tool_calls_and_results_round_trip() {
        let server = Server::one(Reply::json(
            200,
            r#"{"model":"llama3.1","message":{"role":"assistant","content":"","tool_calls":[
                {"function":{"name":"get_weather","arguments":{"city":"Oslo"}}},
                {"function":{"name":"get_weather","arguments":{"city":"Bergen"}}}
            ]},"done":true,"done_reason":"stop","prompt_eval_count":40,"eval_count":12}"#,
        ))
        .await;
        let request = CompletionRequest::builder("llama3.1")
            .messages(tool_conversation())
            .tool(weather_tool())
            .build();
        let response = provider(&server).complete(request).await.unwrap();

        let sent = server.request(0);
        assert_eq!(sent.path, "/api/chat");
        assert_eq!(
            sent.body,
            concat!(
                r#"{"model":"llama3.1","messages":["#,
                r#"{"role":"user","content":"Weather in Paris and Rome?"},"#,
                r#"{"role":"assistant","content":"Let me check.","tool_calls":["#,
                r#"{"function":{"name":"get_weather","arguments":{"city":"Paris"}}},"#,
                r#"{"function":{"name":"get_weather","arguments":{"city":"Rome"}}}]},"#,
                r#"{"role":"tool","content":"18C","tool_name":"get_weather"},"#,
                r#"{"role":"tool","content":"{\"temp\":24}","tool_name":"get_weather"}],"#,
                r#""stream":false,"#,
                r#""tools":[{"type":"function","function":{"name":"get_weather","description":"Get weather","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}]}"#
            )
        );

        // Ollama says "stop" for a turn that ends in tool calls.
        assert_eq!(response.finish_reason, Some(FinishReason::ToolCalls));
        assert_eq!(response.raw_finish_reason.as_deref(), Some("stop"));
        let calls = response.tool_calls();
        assert_eq!(calls.len(), 2);
        // Ids are synthesized so results can be matched to calls.
        assert_eq!((calls[0].0, calls[0].1), ("call_0", "get_weather"));
        assert_eq!(calls[0].2.get("city").unwrap().as_str(), Some("Oslo"));
        assert_eq!(calls[1].0, "call_1");
        assert_eq!(calls[1].2.get("city").unwrap().as_str(), Some("Bergen"));
        assert_eq!(response.usage.unwrap().total_tokens, Some(52));
    }

    /// G2: the NDJSON stream carries text, tool calls, usage and
    /// `done_reason`; an `{"error": …}` line is an `Err`.
    #[tokio::test]
    async fn stream_carries_text_tools_usage_and_errors() {
        let server = Server::start(vec![
            Reply::ndjson(&[
                r#"{"model":"llama3.1","message":{"role":"assistant","content":"Che"},"done":false}"#,
                r#"{"model":"llama3.1","message":{"role":"assistant","content":"cking"},"done":false}"#,
                r#"{"model":"llama3.1","message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"get_weather","arguments":{"city":"Paris"}}},{"function":{"name":"get_time","arguments":{}}}]},"done":false}"#,
                r#"{"model":"llama3.1","message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","prompt_eval_count":30,"eval_count":11}"#,
            ]),
            Reply::ndjson(&[
                r#"{"model":"llama3","message":{"role":"assistant","content":"par"},"done":false}"#,
                r#"{"error":"llama runner process has terminated"}"#,
            ]),
            Reply::ndjson(&[
                r#"{"model":"llama3","message":{"role":"assistant","content":"cut off"},"done":true,"done_reason":"length","eval_count":64}"#,
            ]),
            // The connection closes before `done: true`.
            Reply::ndjson(&[
                r#"{"model":"llama3","message":{"role":"assistant","content":"par"},"done":false}"#,
            ]),
        ])
        .await;
        let p = provider(&server);

        let response = collect_stream(p.complete_stream(simple()).await.unwrap())
            .await
            .unwrap();
        assert_eq!(response.model, "llama3.1");
        assert_eq!(response.text(), "Checking");
        assert_eq!(response.finish_reason, Some(FinishReason::ToolCalls));
        let calls = response.tool_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!((calls[0].0, calls[0].1), ("call_0", "get_weather"));
        assert_eq!(calls[0].2.get("city").unwrap().as_str(), Some("Paris"));
        assert_eq!((calls[1].0, calls[1].1), ("call_1", "get_time"));
        let usage = response.usage.unwrap();
        assert_eq!(
            (usage.prompt_tokens, usage.completion_tokens),
            (Some(30), Some(11))
        );

        let items = drain(p.complete_stream(simple()).await.unwrap()).await;
        let err = items.last().unwrap().as_ref().unwrap_err();
        assert!(
            matches!(err, GenAiError::Api { status: 500, message } if message.contains("terminated")),
            "{err:?}"
        );

        let truncated = collect_stream(p.complete_stream(simple()).await.unwrap())
            .await
            .unwrap();
        assert_eq!(truncated.finish_reason, Some(FinishReason::Length));
        assert_eq!(truncated.raw_finish_reason.as_deref(), Some("length"));

        let items = drain(p.complete_stream(simple()).await.unwrap()).await;
        assert!(matches!(
            items.last().unwrap(),
            Err(GenAiError::Transport(_))
        ));
    }

    /// G3 for NDJSON: a multi-byte character and a line split across chunks.
    #[tokio::test]
    async fn ndjson_framing_survives_chunk_boundaries() {
        let line = "{\"model\":\"llama3\",\"message\":{\"content\":\"na\u{ef}ve \u{1f600}\"},\"done\":false}\r\n";
        let bytes = line.as_bytes();
        let cut_a = line.find('\u{ef}').unwrap() + 1;
        let cut_b = line.find('\u{1f600}').unwrap() + 2;
        let server = Server::one(Reply::chunked(
            "application/x-ndjson",
            &[
                &bytes[..cut_a],
                &bytes[cut_a..cut_b],
                &bytes[cut_b..],
                b"{\"message\":{\"content\":\"\"},\"done\":true,\"done_reason\":\"stop\"}",
            ],
        ))
        .await;
        let response = collect_stream(provider(&server).complete_stream(simple()).await.unwrap())
            .await
            .unwrap();
        assert_eq!(response.text(), "na\u{ef}ve \u{1f600}");
        assert_eq!(response.finish_reason, Some(FinishReason::Stop));
    }

    /// Options go into Ollama's `options` object; custom keys are merged at
    /// the top level and into `options`; a schema becomes `format`.
    #[tokio::test]
    async fn typed_and_custom_options_are_sent() {
        let server = Server::start(vec![Reply::json(200, OK), Reply::json(200, OK)]).await;
        let schema = j(r#"{"type":"object","properties":{"n":{"type":"integer"}}}"#);
        let options = Options::builder()
            .max_tokens(64)
            .temperature(0.1)
            .top_p(0.5)
            .top_k(10)
            .seed(42)
            .stop("END")
            .presence_penalty(1.5)
            .frequency_penalty(1.0)
            .system_instructions("Be brief.")
            .custom("keep_alive", "10m")
            .custom("think", false)
            .custom("options", j(r#"{"num_ctx":8192,"seed":1}"#))
            .build();
        let request = CompletionRequest::builder("llama3")
            .message(Message::user("hi"))
            .response_format(ResponseFormat::JsonSchema(schema))
            .options(options)
            .build();
        let p = provider(&server);
        p.complete(request).await.unwrap();
        assert_eq!(
            server.request(0).body,
            concat!(
                r#"{"model":"llama3","messages":[{"role":"system","content":"Be brief."},{"role":"user","content":"hi"}],"#,
                r#""stream":false,"format":{"type":"object","properties":{"n":{"type":"integer"}}},"#,
                r#""options":{"temperature":0.1,"top_p":0.5,"top_k":10,"num_predict":64,"seed":42,"stop":["END"],"#,
                r#""presence_penalty":1.5,"frequency_penalty":1.0,"num_ctx":8192},"#,
                r#""keep_alive":"10m","think":false}"#
            )
        );

        let request = CompletionRequest::builder("llama3")
            .message(Message::user("hi"))
            .response_format(ResponseFormat::Json)
            .build();
        p.complete(request).await.unwrap();
        assert_eq!(
            server.request(1).json().get("format").unwrap().as_str(),
            Some("json")
        );
    }

    /// Errors, images, auth passthrough and embeddings.
    #[tokio::test]
    async fn errors_images_auth_and_embeddings() {
        let server = Server::start(vec![
            Reply::json(404, r#"{"error":"model 'nope' not found"}"#),
            Reply::json(200, OK),
            Reply::json(
                200,
                r#"{"model":"nomic-embed-text","embeddings":[[0.5,1],[2,3.5]],"prompt_eval_count":6}"#,
            ),
        ])
        .await;
        let opts = ProviderOptions::with_base_url(&server.url)
            .auth(Arc::new(BearerAuth::new("proxy-token")));
        let p = OllamaProvider::new(Some(opts));

        match p.complete(simple()).await.unwrap_err() {
            GenAiError::Api { status, message } => {
                assert_eq!(status, 404);
                assert_eq!(message, "model 'nope' not found");
            }
            other => panic!("expected Api, got {other:?}"),
        }

        let mut message = Message::user("what is this?");
        message.add_binary_part(vec![1, 2, 3], "image/png");
        let request = CompletionRequest::builder("llava").message(message).build();
        p.complete(request).await.unwrap();
        let sent = server.request(1);
        assert_eq!(sent.header("authorization"), Some("Bearer proxy-token"));
        assert_eq!(
            sent.body,
            r#"{"model":"llava","messages":[{"role":"user","content":"what is this?","images":["AQID"]}],"stream":false}"#
        );

        let response = p
            .embed(
                EmbedRequest::new("nomic-embed-text", vec!["a".into(), "b".into()]).dimensions(2),
            )
            .await
            .unwrap();
        assert_eq!(response.embeddings, vec![vec![0.5, 1.0], vec![2.0, 3.5]]);
        assert_eq!(response.usage.unwrap().prompt_tokens, Some(6));
        let sent = server.request(2);
        assert_eq!(sent.path, "/api/embed");
        assert_eq!(
            sent.body,
            r#"{"model":"nomic-embed-text","input":["a","b"],"dimensions":2}"#
        );
    }
}

// ---- tracing (obs) ---------------------------------------------------------

#[cfg(all(feature = "openai", feature = "obs"))]
mod tracing {
    use super::*;
    use ferroly::genai::{
        CompletionRequest, GenAiProvider, ObsObserver, OpenAiProvider, ProviderOptions,
    };
    use ferroly::obs::{self, Exporter, FinishedSpan};

    struct Collect(Arc<Mutex<Vec<FinishedSpan>>>);

    impl Exporter for Collect {
        fn export(&self, span: &FinishedSpan) {
            self.0.lock().unwrap().push(span.clone());
        }
    }

    /// The `obs` adapter records each call as a `chat {model}` client span
    /// with the `gen_ai.*` fields and sends the span's trace context to the
    /// provider.
    #[tokio::test]
    async fn obs_observer_records_a_span_and_propagates_trace_context() {
        let spans: Arc<Mutex<Vec<FinishedSpan>>> = Arc::default();
        obs::set_level(obs::Level::Info);
        obs::set_exporter(Box::new(Collect(spans.clone())));

        let server = Server::start(vec![
            Reply::json(
                200,
                r#"{"model":"gpt-4o-2024","choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":7}}"#,
            ),
            Reply::json(500, r#"{"error":{"message":"boom"}}"#),
        ])
        .await;
        let opts =
            ProviderOptions::with_base_url(&server.url).observer(Arc::new(ObsObserver::new()));
        let p = OpenAiProvider::new("sk-secret-key-0123456789", Some(opts));
        let request = || {
            CompletionRequest::builder("gpt-4o")
                .message(Message::user("a very private prompt"))
                .build()
        };
        p.complete(request()).await.unwrap();
        p.complete(request()).await.unwrap_err();
        obs::flush();

        let spans = spans.lock().unwrap().clone();
        let chat: Vec<&FinishedSpan> = spans.iter().filter(|s| s.name == "chat gpt-4o").collect();
        assert_eq!(chat.len(), 2, "one span per call: {spans:?}");
        let field = |span: &FinishedSpan, key: &str| -> Option<Value> {
            span.fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };
        let ok = chat[0];
        assert_eq!(
            field(ok, "gen_ai.provider.name"),
            Some(Value::from("openai"))
        );
        assert_eq!(
            field(ok, "gen_ai.request.model"),
            Some(Value::from("gpt-4o"))
        );
        assert_eq!(
            field(ok, "gen_ai.response.model"),
            Some(Value::from("gpt-4o-2024"))
        );
        assert_eq!(
            field(ok, "gen_ai.usage.input_tokens").and_then(|v| v.as_u64()),
            Some(11)
        );
        assert_eq!(
            field(ok, "gen_ai.usage.output_tokens").and_then(|v| v.as_u64()),
            Some(7)
        );
        assert_eq!(field(chat[1], "error.type"), Some(Value::from("api")));

        // The outgoing request carried that span's W3C trace context.
        let traceparent = server.request(0).header("traceparent").unwrap().to_string();
        assert_eq!(
            traceparent,
            format!("00-{}-{}-01", ok.trace_id, ok.span_id),
            "traceparent names the chat span"
        );

        // Neither the prompt nor the key is recorded anywhere on the spans.
        let dump = format!("{spans:?}");
        assert!(!dump.contains("private prompt") && !dump.contains("sk-secret-key"));
    }
}

// ---- grounding, proxy passthrough ------------------------------------------

#[cfg(feature = "openai")]
mod openai_extras {
    use super::*;
    use ferroly::genai::{CompletionRequest, GenAiProvider, OpenAiProvider, ProviderOptions};
    use ferroly::http::Proxy;

    fn simple() -> CompletionRequest {
        CompletionRequest::builder("gpt-4o")
            .message(Message::user("hi"))
            .build()
    }

    /// Pending B4: URL citations the API returns are exposed as grounding.
    #[tokio::test]
    async fn url_citations_become_grounding() {
        let server = Server::one(Reply::json(
            200,
            r#"{"model":"gpt-4o","choices":[{"message":{"role":"assistant","content":"Rust 1.0 shipped in 2015.","annotations":[
                {"type":"url_citation","url_citation":{"start_index":0,"end_index":25,"url":"https://blog.rust-lang.org/2015/05/15/Rust-1.0.html","title":"Announcing Rust 1.0"}}
            ]},"finish_reason":"stop"}]}"#,
        ))
        .await;
        let p = OpenAiProvider::new("sk-test", Some(ProviderOptions::with_base_url(&server.url)));
        let response = p.complete(simple()).await.unwrap();
        let grounding = response.grounding.expect("citations");
        assert_eq!(grounding.citations.len(), 1);
        let c = &grounding.citations[0];
        assert_eq!(c.title.as_deref(), Some("Announcing Rust 1.0"));
        assert_eq!(
            c.url.as_deref(),
            Some("https://blog.rust-lang.org/2015/05/15/Rust-1.0.html")
        );
        assert_eq!((c.start, c.end), (Some(0), Some(25)));
    }

    /// `ProviderOptions::proxy` reaches the HTTP client: the request for an
    /// (unresolvable) API host arrives at the proxy in absolute form.
    #[tokio::test]
    async fn proxy_setting_is_passed_to_the_http_client() {
        let proxy = Server::one(Reply::json(
            200,
            r#"{"model":"gpt-4o","choices":[{"message":{"content":"via proxy"},"finish_reason":"stop"}]}"#,
        ))
        .await;
        let opts = ProviderOptions::with_base_url("http://llm.invalid")
            .proxy(Proxy::all(&proxy.url).unwrap())
            .connect_timeout(Duration::from_secs(5));
        let p = OpenAiProvider::new("sk-test", Some(opts));
        assert_eq!(p.complete(simple()).await.unwrap().text(), "via proxy");
        let sent = proxy.request(0);
        assert_eq!(sent.path, "http://llm.invalid/v1/chat/completions");
        assert_eq!(sent.header("host"), Some("llm.invalid"));
    }
}

#[cfg(feature = "claude")]
mod claude_extras {
    use super::*;
    use ferroly::genai::{ClaudeProvider, CompletionRequest, GenAiProvider, ProviderOptions};

    /// Pending B4: citation blocks are exposed as grounding, and blocks the
    /// normalized model has no part for survive as `ProviderData`.
    #[tokio::test]
    async fn citations_become_grounding_and_unknown_blocks_are_preserved() {
        let server = Server::one(Reply::json(
            200,
            r#"{"id":"msg_3","model":"claude-opus-5-5","content":[
                {"type":"thinking","thinking":"Look at the document.","signature":"sig-1"},
                {"type":"text","text":"The grass is green.","citations":[
                    {"type":"char_location","cited_text":"The grass is green.","document_index":0,"document_title":"Nature notes","start_char_index":0,"end_char_index":19}
                ]},
                {"type":"redacted_thinking","data":"opaque"}
            ],"stop_reason":"end_turn","usage":{"input_tokens":20,"output_tokens":9}}"#,
        ))
        .await;
        let p = ClaudeProvider::new(
            "sk-ant-test",
            Some(ProviderOptions::with_base_url(&server.url)),
        );
        let request = CompletionRequest::builder("claude-opus-5-5")
            .message(Message::user("What colour is the grass?"))
            .build();
        let response = p.complete(request).await.unwrap();

        assert_eq!(response.text(), "The grass is green.");
        assert_eq!(response.reasoning.as_deref(), Some("Look at the document."));
        let c = &response.grounding.as_ref().expect("citations").citations[0];
        assert_eq!(c.cited_text.as_deref(), Some("The grass is green."));
        assert_eq!(c.title.as_deref(), Some("Nature notes"));
        assert_eq!(c.document_index, Some(0));
        assert_eq!((c.start, c.end), (Some(0), Some(19)));

        // thinking, text, redacted_thinking — in the order they were returned.
        let kinds: Vec<String> = response
            .message
            .parts
            .iter()
            .map(|part| match part {
                MessagePart::Text(_) => "text".to_string(),
                MessagePart::ProviderData { provider, data } => format!(
                    "{provider}:{}",
                    data.get("type").and_then(Value::as_str).unwrap_or("?")
                ),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            ["claude:thinking", "text", "claude:redacted_thinking"]
        );
    }
}

// ---- body size caps ---------------------------------------------------------

#[cfg(feature = "openai")]
mod openai_limits {
    use super::*;
    use ferroly::genai::{CompletionRequest, GenAiProvider, OpenAiProvider, ProviderOptions};

    fn simple() -> CompletionRequest {
        CompletionRequest::builder("gpt-4o")
            .message(Message::user("hi"))
            .build()
    }

    /// G5: the cap on buffered responses does not cut a stream short — a
    /// stream has its own, explicit cap.
    #[tokio::test]
    async fn stream_cap_is_separate_from_the_buffered_response_cap() {
        let events = [
            "data: {\"choices\":[{\"delta\":{\"content\":\"a fairly long first delta, \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"and a second one\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ];
        let body = r#"{"model":"gpt-4o","choices":[{"message":{"content":"a buffered answer that is longer than the cap"},"finish_reason":"stop"}]}"#;
        let server = Server::start(vec![
            Reply::sse(&events),
            Reply::json(200, body),
            Reply::sse(&events),
        ])
        .await;

        // A tiny cap on buffered responses: the stream is unaffected …
        let mut opts = ProviderOptions::with_base_url(&server.url);
        opts.max_response_bytes = Some(64);
        let p = OpenAiProvider::new("sk-test", Some(opts.clone()));
        let response = collect_stream(p.complete_stream(simple()).await.unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.text(),
            "a fairly long first delta, and a second one"
        );

        // … but it does bound a non-streaming response.
        let err = p.complete(simple()).await.unwrap_err();
        assert!(
            matches!(&err, GenAiError::Transport(m) if m.contains("limit")),
            "{err:?}"
        );

        // An explicit stream cap ends an over-long stream with an error.
        opts.max_stream_bytes = Some(64);
        let p = OpenAiProvider::new("sk-test", Some(opts));
        let err = collect_stream(p.complete_stream(simple()).await.unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, GenAiError::Transport(m) if m.contains("limit")),
            "{err:?}"
        );
    }
}
