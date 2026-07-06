#![cfg(feature = "genai")]
//! Unit coverage for the provider-agnostic GenAI types: messages, requests,
//! options, responses, the provider trait, and error Display.

use ferroly::codec::Value;
use ferroly::genai::{
    BoxFuture, Capability, ChunkStream, CompletionChunk, CompletionRequest, CompletionResponse,
    GenAiError, GenAiProvider, Message, MessagePart, Options, ResponseFormat, Role, ToolChoice,
    ToolDefinition, Usage,
};
use std::sync::Arc;

#[test]
fn message_constructors_and_parts() {
    let bin = Message::binary(Role::User, "m1", vec![1, 2, 3], "image/png");
    assert!(matches!(bin.parts[0], MessagePart::Image { .. }));

    let file = Message::file_ref(Role::User, "m2", "gs://b/x.pdf", "application/pdf");
    assert!(matches!(file.parts[0], MessagePart::FileRef { .. }));

    let js = Message::json(Role::Tool, "m3", &vec![1i32, 2]);
    assert_eq!(js.role, Role::Tool);
    assert!(js.text_content().contains('['));

    let mut u = Message::user("hello");
    assert!(u.id.is_none());
    u.add_text_part(" world")
        .add_binary_part(vec![9], "image/jpeg");
    assert_eq!(u.text_content(), "hello world");
    assert_eq!(u.parts.len(), 3);

    let s = Message::system("be brief");
    assert_eq!(s.role, Role::System);
    assert_eq!(Role::Tool.as_str(), "tool");
    assert_eq!(Role::User.as_str(), "user");
}

#[test]
fn request_builder_and_tools() {
    let tool = ToolDefinition::new("lookup", "look things up", Value::Null);
    let req = CompletionRequest::builder("gpt-4o")
        .message(Message::user("hi"))
        .messages(vec![Message::user("again")])
        .tool(tool.clone())
        .response_format(ResponseFormat::JsonSchema(Value::Null))
        .options(Options::builder().max_tokens(16).build())
        .build();
    assert_eq!(req.model, "gpt-4o");
    assert_eq!(req.messages.len(), 1);
    assert_eq!(req.tools[0], tool);
    assert_eq!(
        req.response_format,
        Some(ResponseFormat::JsonSchema(Value::Null))
    );
    assert_eq!(req.options.max_tokens, Some(16));

    // plain constructor + the other response formats
    let _ = CompletionRequest::new("m", vec![Message::user("x")]);
    assert_ne!(ResponseFormat::Text, ResponseFormat::Json);
}

#[test]
fn tool_choice_and_structured_output() {
    use ferroly::codec::{Decode, Encode};

    // tool_choice records on the request
    let req = CompletionRequest::builder("m")
        .tool_choice(ToolChoice::Named("lookup".into()))
        .build();
    assert_eq!(req.tool_choice, Some(ToolChoice::Named("lookup".into())));

    // structured output: decode the response text into a typed struct
    #[derive(Encode, Decode, PartialEq, Debug)]
    struct Answer {
        city: String,
        population: u32,
    }
    let resp = CompletionResponse {
        model: "m".into(),
        message: Message::text(
            Role::Assistant,
            "r",
            r#"{"city":"Paris","population":2100000}"#,
        ),
        finish_reason: Some("stop".into()),
        usage: None,
    };
    let answer: Answer = resp.decode().unwrap();
    assert_eq!(
        answer,
        Answer {
            city: "Paris".into(),
            population: 2_100_000
        }
    );
}

#[test]
fn options_typed_and_custom() {
    let opts = Options::builder()
        .temperature(0.5)
        .top_p(0.9)
        .system_instructions("sys")
        .custom("seed", 42i64)
        .build();
    assert_eq!(opts.temperature, Some(0.5));
    assert_eq!(opts.top_p, Some(0.9));
    assert_eq!(opts.system_instructions.as_deref(), Some("sys"));
    assert_eq!(opts.custom.get("seed").unwrap().as_i64(), Some(42));

    // struct-literal construction works too, since the fields are public.
    let m = Options {
        max_tokens: Some(8),
        ..Default::default()
    };
    assert_eq!(m.max_tokens, Some(8));
    assert!(m.temperature.is_none());
}

#[test]
fn response_helpers() {
    let usage = Usage {
        prompt_tokens: Some(3),
        completion_tokens: Some(5),
        total_tokens: Some(8),
    };
    let resp = CompletionResponse {
        model: "m".into(),
        message: Message::text(Role::Assistant, "r", "hi there"),
        finish_reason: Some("stop".into()),
        usage: Some(usage.clone()),
    };
    assert_eq!(resp.text(), "hi there");
    assert_eq!(resp.usage.unwrap(), usage);

    let chunk = CompletionChunk::default();
    assert!(chunk.delta.is_empty());
    assert_eq!(Usage::default(), Usage::default());
}

#[test]
fn error_display_covers_every_variant() {
    let errors = [
        GenAiError::Template("t".into()),
        GenAiError::TemplateNotFound("n".into()),
        GenAiError::Unsupported {
            provider: "p".into(),
            capability: Capability::Vision,
        },
        GenAiError::Transport("x".into()),
        GenAiError::Api {
            status: 500,
            message: "boom".into(),
        },
        GenAiError::ResponseParse("bad".into()),
        GenAiError::Config("cfg".into()),
    ];
    for e in &errors {
        assert!(!e.to_string().is_empty());
    }
}

/// A minimal provider used to exercise the trait directly.
struct Mock;

impl GenAiProvider for Mock {
    fn name(&self) -> &str {
        "mock"
    }
    fn complete(
        &self,
        _request: CompletionRequest,
    ) -> BoxFuture<'_, Result<CompletionResponse, GenAiError>> {
        Box::pin(async {
            Ok(CompletionResponse {
                model: "mock".into(),
                message: Message::text(Role::Assistant, "r", "pong"),
                finish_reason: None,
                usage: None,
            })
        })
    }
    fn complete_stream(
        &self,
        _request: CompletionRequest,
    ) -> BoxFuture<'_, Result<ChunkStream, GenAiError>> {
        Box::pin(async {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            tx.send(Ok(CompletionChunk {
                delta: "pong".into(),
                ..Default::default()
            }))
            .await
            .unwrap();
            Ok(rx)
        })
    }
    fn supports(&self, capability: Capability) -> bool {
        capability == Capability::Streaming
    }
}

#[tokio::test]
async fn provider_trait_via_dyn() {
    // Held behind Arc<dyn _> — the runtime indirection the registry used to give.
    let p: Arc<dyn GenAiProvider> = Arc::new(Mock);
    assert_eq!(p.name(), "mock");
    assert_eq!(p.description(), ""); // defaulted
    assert!(p.supports(Capability::Streaming));
    assert!(!p.supports(Capability::ToolUse));

    let resp = p
        .complete(CompletionRequest::new("m", vec![]))
        .await
        .unwrap();
    assert_eq!(resp.text(), "pong");

    let mut stream = p
        .complete_stream(CompletionRequest::new("m", vec![]))
        .await
        .unwrap();
    let chunk = stream.recv().await.unwrap().unwrap();
    assert_eq!(chunk.delta, "pong");
}
