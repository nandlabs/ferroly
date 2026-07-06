//! Completion request types.

use ferroly::codec::Value;

use ferroly::genai::{Message, Options};

/// A tool/function the model may call.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDefinition {
    /// The tool's name.
    pub name: String,
    /// A human/model-readable description of what the tool does.
    pub description: String,
    /// A JSON Schema describing the tool's parameters.
    pub parameters: Value,
}

impl ToolDefinition {
    /// Creates a tool definition.
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

/// The requested shape of the model's response.
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseFormat {
    /// Free-form text (the default).
    Text,
    /// A JSON object (provider "JSON mode").
    Json,
    /// A JSON object conforming to the given JSON Schema.
    JsonSchema(Value),
}

/// How the model should decide whether to call a tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    /// The model decides (the default when tools are present).
    Auto,
    /// The model must not call a tool.
    None,
    /// The model must call some tool.
    Required,
    /// The model must call the named tool.
    Named(String),
}

/// A provider-agnostic completion request.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    /// The model identifier (e.g. `gpt-4o`, `claude-sonnet-5`).
    pub model: String,
    /// The conversation so far.
    pub messages: Vec<Message>,
    /// Tools the model may call.
    pub tools: Vec<ToolDefinition>,
    /// How the model should choose among `tools`, if constrained.
    pub tool_choice: Option<ToolChoice>,
    /// The desired response format, if constrained.
    pub response_format: Option<ResponseFormat>,
    /// Generation options (temperature, max tokens, system instructions, …).
    pub options: Options,
}

impl CompletionRequest {
    /// Creates a request for `model` with the given messages and default options.
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
            tool_choice: None,
            response_format: None,
            options: Options::new(),
        }
    }

    /// Starts a builder for `model`.
    pub fn builder(model: impl Into<String>) -> CompletionRequestBuilder {
        CompletionRequestBuilder {
            request: CompletionRequest::new(model, Vec::new()),
        }
    }
}

/// Fluent builder for [`CompletionRequest`].
#[derive(Debug)]
#[must_use]
pub struct CompletionRequestBuilder {
    request: CompletionRequest,
}

impl CompletionRequestBuilder {
    /// Appends a message.
    pub fn message(mut self, message: Message) -> Self {
        self.request.messages.push(message);
        self
    }

    /// Replaces the message list.
    pub fn messages(mut self, messages: Vec<Message>) -> Self {
        self.request.messages = messages;
        self
    }

    /// Appends a tool definition.
    pub fn tool(mut self, tool: ToolDefinition) -> Self {
        self.request.tools.push(tool);
        self
    }

    /// Sets the tool-choice policy.
    pub fn tool_choice(mut self, choice: ToolChoice) -> Self {
        self.request.tool_choice = Some(choice);
        self
    }

    /// Sets the response format.
    pub fn response_format(mut self, format: ResponseFormat) -> Self {
        self.request.response_format = Some(format);
        self
    }

    /// Sets the generation options.
    pub fn options(mut self, options: Options) -> Self {
        self.request.options = options;
        self
    }

    /// Finalizes the builder.
    pub fn build(self) -> CompletionRequest {
        self.request
    }
}
