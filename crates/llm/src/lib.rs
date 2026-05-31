//! Provider-agnostic LLM interface for quizdom.
//!
//! This crate deliberately contains no AIDA session logic, AIDA tools, server
//! sent events, or concrete provider client. It is the narrow contract that
//! later provider crates or application code can implement and, if useful,
//! extract into another project.

use serde_json::Value;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

mod anthropic;
mod claude_cli;

pub use anthropic::{AnthropicClient, AnthropicConfig};
pub use claude_cli::{ClaudeCliClient, ClaudeCliConfig};

/// Text plus tool calls returned by [`LLMClient::call`].
pub type LLMOutput = (String, Vec<ToolCall>);

/// Boxed future returned by [`LLMClient::call`].
pub type LLMFuture<'a> = Pin<Box<dyn Future<Output = Result<LLMOutput, LLMError>> + Send + 'a>>;

/// Provider-independent async LLM client.
///
/// Implementations receive a system prompt, a conversation history, and a list
/// of tool definitions the model may call. They return assistant text plus any
/// structured tool calls requested by the model.
///
/// trace:STORY-35 | ai:codex
pub trait LLMClient {
    /// Call the provider and return assistant text plus requested tool calls.
    fn call<'a>(
        &'a self,
        system: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolDef],
    ) -> LLMFuture<'a>;
}

/// One conversation message passed to an LLM provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Message role, such as user, assistant, or tool.
    pub role: MessageRole,
    /// Text content for the message.
    pub content: String,
}

impl Message {
    /// Construct a user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
        }
    }

    /// Construct an assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
        }
    }

    /// Construct a tool-result message.
    pub fn tool(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: content.into(),
        }
    }
}

/// Provider-neutral message role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    /// Human/user-authored message.
    User,
    /// Assistant/model-authored message.
    Assistant,
    /// Tool result supplied back to the model.
    Tool,
}

/// Tool definition made available to the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    /// Stable tool name passed to the provider.
    pub name: String,
    /// Human-readable behavior summary for the model.
    pub description: String,
    /// JSON schema for the tool input.
    pub input_schema: Value,
}

impl ToolDef {
    /// Construct a tool definition from a JSON input schema.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }
}

/// Tool invocation requested by the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    /// Provider-supplied call id when available.
    pub id: Option<String>,
    /// Tool name to execute.
    pub name: String,
    /// JSON arguments supplied by the model.
    pub arguments: Value,
}

impl ToolCall {
    /// Construct a tool call.
    pub fn new(id: Option<String>, name: impl Into<String>, arguments: Value) -> Self {
        Self {
            id,
            name: name.into(),
            arguments,
        }
    }
}

/// Provider-independent LLM failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LLMError {
    /// Provider rejected the request, authentication failed, or returned a
    /// transport-level error.
    Provider(String),
    /// Provider response could not be converted to this crate's surface.
    InvalidResponse(String),
    /// Request timed out before completion.
    Timeout(String),
    /// Client-side configuration is missing or invalid.
    Config(String),
}

impl fmt::Display for LLMError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Provider(message)
            | Self::InvalidResponse(message)
            | Self::Timeout(message)
            | Self::Config(message) => write!(f, "{message}"),
        }
    }
}

impl Error for LLMError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn output_carries_text_and_tool_calls() {
        let output: LLMOutput = (
            "Need more context".to_string(),
            vec![ToolCall::new(
                Some("call-1".to_string()),
                "lookup",
                json!({"query": "free will"}),
            )],
        );

        assert_eq!(output.0, "Need more context");
        assert_eq!(output.1[0].name, "lookup");
    }

    #[test]
    fn tool_definition_keeps_json_schema() {
        let tool = ToolDef::new(
            "lookup",
            "Look up a term",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                },
                "required": ["query"]
            }),
        );

        assert_eq!(tool.name, "lookup");
        assert_eq!(tool.input_schema["required"][0], "query");
    }
}
