//! Multi-part messages and roles.

use ferroly::codec::Value;

/// The author role of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// System / developer instructions.
    System,
    /// End-user input.
    User,
    /// Model output.
    Assistant,
    /// A tool/function result fed back to the model.
    Tool,
}

impl Role {
    /// The lowercase wire name of the role (`"user"`, `"system"`, …).
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// A single piece of a [`Message`]. Messages may combine several parts (e.g.
/// text plus an image, or a tool call plus its result).
///
/// `#[non_exhaustive]`: keep a wildcard arm when matching.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum MessagePart {
    /// Plain text.
    Text(String),
    /// Inline binary data (e.g. an image), with its MIME type.
    Image {
        /// Raw bytes.
        data: Vec<u8>,
        /// MIME type, e.g. `image/png`.
        mime_type: String,
    },
    /// A reference to an external file (not inlined), e.g. `gs://bucket/doc.pdf`.
    FileRef {
        /// The file URI.
        uri: String,
        /// MIME type of the referenced file.
        mime_type: String,
    },
    /// A tool/function invocation requested by the model.
    ToolCall {
        /// Provider-assigned call id.
        id: String,
        /// Tool name.
        name: String,
        /// Arguments as a structured value.
        arguments: Value,
    },
    /// The result of a previously requested tool call.
    ToolResult {
        /// The id of the call this result answers.
        call_id: String,
        /// The result payload as a structured value. A string is sent to the
        /// model as is; any other value is sent as its JSON encoding.
        result: Value,
    },
    /// A provider-specific content block with no normalized form — for
    /// example a Claude thinking block and its signature. Providers return it
    /// so that a multi-turn conversation can send it back **unchanged**; it is
    /// re-sent only to the provider named here and skipped by every other.
    ProviderData {
        /// The provider that produced (and understands) the block.
        provider: String,
        /// The block, verbatim.
        data: Value,
    },
}

/// A role-tagged, multi-part message.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// The author role.
    pub role: Role,
    /// An optional caller-supplied identifier for the message.
    pub id: Option<String>,
    /// The ordered parts making up this message.
    pub parts: Vec<MessagePart>,
}

impl Message {
    /// Creates a text message.
    pub fn text(role: Role, id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role,
            id: Some(id.into()),
            parts: vec![MessagePart::Text(text.into())],
        }
    }

    /// Creates a binary (e.g. image) message.
    pub fn binary(
        role: Role,
        id: impl Into<String>,
        bytes: Vec<u8>,
        mime_type: impl Into<String>,
    ) -> Self {
        Self {
            role,
            id: Some(id.into()),
            parts: vec![MessagePart::Image {
                data: bytes,
                mime_type: mime_type.into(),
            }],
        }
    }

    /// Creates a file-reference message.
    pub fn file_ref(
        role: Role,
        id: impl Into<String>,
        uri: impl Into<String>,
        mime_type: impl Into<String>,
    ) -> Self {
        Self {
            role,
            id: Some(id.into()),
            parts: vec![MessagePart::FileRef {
                uri: uri.into(),
                mime_type: mime_type.into(),
            }],
        }
    }

    /// Creates a message whose text part is the JSON encoding of `val`.
    /// Infallible — the in-house JSON encoder cannot fail for a `Encode` value.
    pub fn json<T: ferroly::codec::Encode>(role: Role, id: impl Into<String>, val: &T) -> Self {
        Self::text(role, id, ferroly::codec::json::encode(val))
    }

    /// Convenience: a user text message with no id.
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            id: None,
            parts: vec![MessagePart::Text(text.into())],
        }
    }

    /// Convenience: a system text message with no id.
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            id: None,
            parts: vec![MessagePart::Text(text.into())],
        }
    }

    /// Convenience: an assistant text message with no id.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            id: None,
            parts: vec![MessagePart::Text(text.into())],
        }
    }

    /// An assistant message requesting one tool call. Chain
    /// [`add_tool_call`](Message::add_tool_call) for parallel calls.
    pub fn tool_call(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        Self {
            role: Role::Assistant,
            id: None,
            parts: vec![MessagePart::ToolCall {
                id: id.into(),
                name: name.into(),
                arguments,
            }],
        }
    }

    /// A [`Role::Tool`] message answering the tool call `call_id`. Chain
    /// [`add_tool_result`](Message::add_tool_result) to answer several
    /// parallel calls in one message.
    pub fn tool_result(call_id: impl Into<String>, result: impl Into<Value>) -> Self {
        Self {
            role: Role::Tool,
            id: None,
            parts: vec![MessagePart::ToolResult {
                call_id: call_id.into(),
                result: result.into(),
            }],
        }
    }

    /// Appends a tool-call part.
    pub fn add_tool_call(
        &mut self,
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: Value,
    ) -> &mut Self {
        self.parts.push(MessagePart::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        });
        self
    }

    /// Appends a tool-result part.
    pub fn add_tool_result(
        &mut self,
        call_id: impl Into<String>,
        result: impl Into<Value>,
    ) -> &mut Self {
        self.parts.push(MessagePart::ToolResult {
            call_id: call_id.into(),
            result: result.into(),
        });
        self
    }

    /// The tool calls in this message, as `(id, name, arguments)`.
    pub fn tool_calls(&self) -> Vec<(&str, &str, &Value)> {
        self.parts
            .iter()
            .filter_map(|p| match p {
                MessagePart::ToolCall {
                    id,
                    name,
                    arguments,
                } => Some((id.as_str(), name.as_str(), arguments)),
                _ => None,
            })
            .collect()
    }

    /// Appends a text part.
    pub fn add_text_part(&mut self, text: impl Into<String>) -> &mut Self {
        self.parts.push(MessagePart::Text(text.into()));
        self
    }

    /// Appends a binary part.
    pub fn add_binary_part(&mut self, bytes: Vec<u8>, mime_type: impl Into<String>) -> &mut Self {
        self.parts.push(MessagePart::Image {
            data: bytes,
            mime_type: mime_type.into(),
        });
        self
    }

    /// Concatenates all text parts, ignoring non-text parts. Useful for simple
    /// providers and for reading assistant replies.
    pub fn text_content(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                MessagePart::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_multi_part_message() {
        let mut msg = Message::text(Role::User, "m1", "describe this");
        msg.add_binary_part(vec![0x89, 0x50], "image/png");
        assert_eq!(msg.parts.len(), 2);
        assert_eq!(msg.text_content(), "describe this");
    }

    #[test]
    fn role_as_str_lowercase() {
        assert_eq!(Role::Assistant.as_str(), "assistant");
        assert_eq!(Role::System.as_str(), "system");
    }

    #[test]
    fn json_message_encodes_value() {
        #[derive(ferroly::codec::Encode)]
        struct P {
            a: i32,
        }
        let msg = Message::json(Role::User, "j1", &P { a: 5 });
        assert_eq!(msg.text_content(), "{\"a\":5}");
    }
}
