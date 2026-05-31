use crate::{LLMClient, LLMError, LLMFuture, Message, MessageRole, ToolCall, ToolDef};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
const DEFAULT_MAX_TOKENS: u32 = 1024;
const DEFAULT_MAX_RETRIES: usize = 2;

type TransportFuture<'a> =
    Pin<Box<dyn Future<Output = Result<TransportResponse, LLMError>> + Send + 'a>>;

trait AnthropicTransport: Send + Sync {
    fn post<'a>(&'a self, request: TransportRequest) -> TransportFuture<'a>;
}

#[derive(Debug, Clone)]
struct TransportRequest {
    url: String,
    api_key: String,
    anthropic_version: String,
    body: AnthropicRequest,
}

#[derive(Debug, Clone)]
struct TransportResponse {
    status: u16,
    body: String,
}

#[derive(Debug, Clone)]
struct ReqwestTransport {
    client: reqwest::Client,
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl AnthropicTransport for ReqwestTransport {
    fn post<'a>(&'a self, request: TransportRequest) -> TransportFuture<'a> {
        Box::pin(async move {
            let response = self
                .client
                .post(&request.url)
                .header("x-api-key", request.api_key)
                .header("anthropic-version", request.anthropic_version)
                .json(&request.body)
                .send()
                .await
                .map_err(|error| LLMError::Provider(error.to_string()))?;

            let status = response.status().as_u16();
            let body = response
                .text()
                .await
                .map_err(|error| LLMError::Provider(error.to_string()))?;

            Ok(TransportResponse { status, body })
        })
    }
}

/// Configuration for [`AnthropicClient`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicConfig {
    /// Anthropic API key.
    pub api_key: String,
    /// Model name sent in the `model` field.
    pub model: String,
    /// Maximum output tokens requested from Anthropic.
    pub max_tokens: u32,
    /// Maximum transient retry attempts after the first request.
    pub max_retries: usize,
}

impl AnthropicConfig {
    /// Load configuration from `.env` and environment variables.
    ///
    /// Reads `ANTHROPIC_API_KEY`; `QUIZDOM_MODEL` overrides the default model.
    pub fn from_env() -> Result<Self, LLMError> {
        dotenvy::dotenv().ok();
        let api_key = env::var("ANTHROPIC_API_KEY")
            .map_err(|_| LLMError::Config("ANTHROPIC_API_KEY is not set".to_string()))?;
        let model = env::var("QUIZDOM_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        Ok(Self {
            api_key,
            model,
            max_tokens: DEFAULT_MAX_TOKENS,
            max_retries: DEFAULT_MAX_RETRIES,
        })
    }
}

/// Reqwest-backed Anthropic Messages API client.
#[derive(Clone)]
pub struct AnthropicClient {
    config: AnthropicConfig,
    transport: Arc<dyn AnthropicTransport>,
}

impl AnthropicClient {
    /// Build a client from `.env` and environment variables.
    pub fn from_env() -> Result<Self, LLMError> {
        Ok(Self::new(AnthropicConfig::from_env()?))
    }

    /// Build a reqwest client from explicit config.
    pub fn new(config: AnthropicConfig) -> Self {
        Self {
            config,
            transport: Arc::new(ReqwestTransport::default()),
        }
    }

    #[cfg(test)]
    fn with_transport<T>(config: AnthropicConfig, transport: T) -> Self
    where
        T: AnthropicTransport + 'static,
    {
        Self {
            config,
            transport: Arc::new(transport),
        }
    }

    async fn call_inner(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<(String, Vec<ToolCall>), LLMError> {
        let body = AnthropicRequest {
            model: self.config.model.clone(),
            max_tokens: self.config.max_tokens,
            system: vec![SystemBlock::cached(system)],
            messages: messages
                .iter()
                .map(AnthropicMessage::from_message)
                .collect(),
            tools: tools.iter().map(AnthropicTool::from_tool_def).collect(),
        };
        let request = TransportRequest {
            url: ANTHROPIC_MESSAGES_URL.to_string(),
            api_key: self.config.api_key.clone(),
            anthropic_version: ANTHROPIC_VERSION.to_string(),
            body,
        };

        let mut attempts = 0;
        loop {
            let response = self.transport.post(request.clone()).await?;
            if is_transient(response.status) && attempts < self.config.max_retries {
                attempts += 1;
                backoff(attempts).await;
                continue;
            }
            return parse_response(response);
        }
    }
}

impl LLMClient for AnthropicClient {
    fn call<'a>(
        &'a self,
        system: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolDef],
    ) -> LLMFuture<'a> {
        // trace:STORY-36 | ai:codex
        Box::pin(async move { self.call_inner(system, messages, tools).await })
    }
}

fn is_transient(status: u16) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS.as_u16() || (500..600).contains(&status)
}

async fn backoff(attempt: usize) {
    let millis = 50 * attempt as u64;
    tokio::time::sleep(Duration::from_millis(millis)).await;
}

fn parse_response(response: TransportResponse) -> Result<(String, Vec<ToolCall>), LLMError> {
    if !(200..300).contains(&response.status) {
        return Err(LLMError::Provider(format!(
            "Anthropic API returned HTTP {}: {}",
            response.status, response.body
        )));
    }

    let message: AnthropicResponse = serde_json::from_str(&response.body)
        .map_err(|error| LLMError::InvalidResponse(error.to_string()))?;
    let mut text = String::new();
    let mut tool_calls = Vec::new();

    for block in message.content {
        match block {
            AnthropicContentBlock::Text {
                text: block_text, ..
            } => text.push_str(&block_text),
            AnthropicContentBlock::ToolUse {
                id, name, input, ..
            } => tool_calls.push(ToolCall::new(Some(id), name, input)),
            AnthropicContentBlock::Other => {}
        }
    }

    Ok((text, tool_calls))
}

#[derive(Debug, Clone, Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    system: Vec<SystemBlock>,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool>,
}

#[derive(Debug, Clone, Serialize)]
struct SystemBlock {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
    cache_control: CacheControl,
}

impl SystemBlock {
    fn cached(text: &str) -> Self {
        Self {
            kind: "text",
            text: text.to_string(),
            cache_control: CacheControl::ephemeral(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct AnthropicMessage {
    role: &'static str,
    content: Vec<AnthropicRequestContentBlock>,
}

impl AnthropicMessage {
    fn from_message(message: &Message) -> Self {
        let role = match message.role {
            MessageRole::User | MessageRole::Tool => "user",
            MessageRole::Assistant => "assistant",
        };
        Self {
            role,
            content: vec![AnthropicRequestContentBlock::text(&message.content)],
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct AnthropicRequestContentBlock {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
}

impl AnthropicRequestContentBlock {
    fn text(text: &str) -> Self {
        Self {
            kind: "text",
            text: text.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: Value,
    cache_control: CacheControl,
}

impl AnthropicTool {
    fn from_tool_def(tool: &ToolDef) -> Self {
        Self {
            name: tool.name.clone(),
            description: tool.description.clone(),
            input_schema: tool.input_schema.clone(),
            cache_control: CacheControl::ephemeral(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

impl CacheControl {
    fn ephemeral() -> Self {
        Self { kind: "ephemeral" }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContentBlock>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct MockTransport {
        responses: Arc<Mutex<Vec<TransportResponse>>>,
        requests: Arc<Mutex<Vec<TransportRequest>>>,
    }

    impl MockTransport {
        fn with_responses(responses: Vec<TransportResponse>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses)),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn requests(&self) -> Vec<TransportRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl AnthropicTransport for MockTransport {
        fn post<'a>(&'a self, request: TransportRequest) -> TransportFuture<'a> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(request);
                let mut responses = self.responses.lock().unwrap();
                if responses.is_empty() {
                    return Err(LLMError::Provider("no mock response".to_string()));
                }
                Ok(responses.remove(0))
            })
        }
    }

    fn config() -> AnthropicConfig {
        AnthropicConfig {
            api_key: "test-key".to_string(),
            model: "test-model".to_string(),
            max_tokens: 100,
            max_retries: 1,
        }
    }

    #[tokio::test]
    async fn maps_request_and_response_without_live_api() {
        let transport = MockTransport::with_responses(vec![TransportResponse {
            status: 200,
            body: json!({
                "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"query": "free will"}}
                ]
            })
            .to_string(),
        }]);
        let inspector = transport.clone();
        let client = AnthropicClient::with_transport(config(), transport);
        let tools = [ToolDef::new(
            "lookup",
            "Look up a topic",
            json!({"type": "object"}),
        )];

        let (text, calls) = client
            .call("system prompt", &[Message::user("hi")], &tools)
            .await
            .unwrap();

        assert_eq!(text, "hello");
        assert_eq!(calls[0].name, "lookup");
        assert_eq!(calls[0].arguments["query"], "free will");

        let requests = inspector.requests();
        let request = &requests[0];
        assert_eq!(request.url, ANTHROPIC_MESSAGES_URL);
        assert_eq!(request.api_key, "test-key");
        assert_eq!(request.anthropic_version, ANTHROPIC_VERSION);
        assert_eq!(request.body.model, "test-model");
        assert_eq!(request.body.system[0].cache_control.kind, "ephemeral");
        assert_eq!(request.body.tools[0].cache_control.kind, "ephemeral");
    }

    #[tokio::test]
    async fn retries_transient_errors() {
        let transport = MockTransport::with_responses(vec![
            TransportResponse {
                status: 529,
                body: "overloaded".to_string(),
            },
            TransportResponse {
                status: 200,
                body: json!({"content": [{"type": "text", "text": "ok"}]}).to_string(),
            },
        ]);
        let inspector = transport.clone();
        let client = AnthropicClient::with_transport(config(), transport);

        let (text, calls) = client.call("system", &[], &[]).await.unwrap();

        assert_eq!(text, "ok");
        assert!(calls.is_empty());
        assert_eq!(inspector.requests().len(), 2);
    }

    #[tokio::test]
    #[ignore = "requires ANTHROPIC_API_KEY and makes a live API call"]
    async fn live_anthropic_smoke() {
        let client = AnthropicClient::from_env().unwrap();
        let (text, _calls) = client
            .call("Answer briefly.", &[Message::user("Say hello.")], &[])
            .await
            .unwrap();
        assert!(!text.trim().is_empty());
    }
}
