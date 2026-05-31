use crate::{LLMClient, LLMError, LLMFuture, Message, MessageRole, ToolCall, ToolDef};
use serde_json::{json, Value};
use std::env;
use std::future::Future;
use std::pin::Pin;
use std::process::{Command, Output};
use std::sync::Arc;

type CommandFuture<'a> = Pin<Box<dyn Future<Output = Result<CommandOutput, LLMError>> + Send + 'a>>;

trait ClaudeCommandRunner: Send + Sync {
    fn run<'a>(&'a self, request: CommandRequest) -> CommandFuture<'a>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandRequest {
    program: String,
    args: Vec<String>,
    env_remove: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandOutput {
    status_success: bool,
    stdout: String,
    stderr: String,
}

#[derive(Debug, Clone)]
struct SystemCommandRunner;

impl ClaudeCommandRunner for SystemCommandRunner {
    fn run<'a>(&'a self, request: CommandRequest) -> CommandFuture<'a> {
        Box::pin(async move {
            let mut command = Command::new(&request.program);
            command.args(&request.args);
            for key in &request.env_remove {
                command.env_remove(key);
            }
            command
                .output()
                .map(command_output)
                .map_err(|error| LLMError::Provider(format!("spawn `claude`: {error}")))
        })
    }
}

/// Configuration for [`ClaudeCliClient`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCliConfig {
    /// Executable used for Claude Code.
    pub command: String,
}

impl ClaudeCliConfig {
    /// Load configuration from environment variables.
    ///
    /// `QUIZDOM_CLAUDE_COMMAND` overrides the executable name. The default is
    /// `claude`.
    pub fn from_env() -> Self {
        Self {
            command: env::var("QUIZDOM_CLAUDE_COMMAND").unwrap_or_else(|_| "claude".to_string()),
        }
    }
}

/// Claude Code CLI-backed client using Max-plan OAuth/keychain auth.
#[derive(Clone)]
pub struct ClaudeCliClient {
    config: ClaudeCliConfig,
    runner: Arc<dyn ClaudeCommandRunner>,
}

impl ClaudeCliClient {
    /// Build a client from environment variables.
    pub fn from_env() -> Self {
        Self::new(ClaudeCliConfig::from_env())
    }

    /// Build a client from explicit configuration.
    pub fn new(config: ClaudeCliConfig) -> Self {
        Self {
            config,
            runner: Arc::new(SystemCommandRunner),
        }
    }

    #[cfg(test)]
    fn with_runner<R>(config: ClaudeCliConfig, runner: R) -> Self
    where
        R: ClaudeCommandRunner + 'static,
    {
        Self {
            config,
            runner: Arc::new(runner),
        }
    }

    async fn call_inner(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<(String, Vec<ToolCall>), LLMError> {
        let request = CommandRequest {
            program: self.config.command.clone(),
            args: vec![
                "-p".to_string(),
                "--output-format".to_string(),
                "json".to_string(),
                prompt(system, messages, tools),
            ],
            env_remove: vec!["ANTHROPIC_API_KEY".to_string()],
        };
        let output = self.runner.run(request).await?;
        parse_output(output)
    }
}

impl LLMClient for ClaudeCliClient {
    fn call<'a>(
        &'a self,
        system: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolDef],
    ) -> LLMFuture<'a> {
        // trace:STORY-40 | ai:codex
        Box::pin(async move { self.call_inner(system, messages, tools).await })
    }
}

fn prompt(system: &str, messages: &[Message], tools: &[ToolDef]) -> String {
    let mut prompt = format!("{system}\n\nConversation:\n");
    for message in messages {
        let role = match message.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };
        prompt.push_str(&format!("{role}: {}\n", message.content));
    }
    if !tools.is_empty() {
        prompt.push_str("\nAvailable tools as JSON schema definitions:\n");
        for tool in tools {
            prompt.push_str(
                &json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.input_schema,
                })
                .to_string(),
            );
            prompt.push('\n');
        }
    }
    prompt
}

fn parse_output(output: CommandOutput) -> Result<(String, Vec<ToolCall>), LLMError> {
    if !output.status_success {
        return Err(LLMError::Provider(format!(
            "claude CLI failed: {}",
            output.stderr.trim()
        )));
    }
    parse_json_output(&output.stdout)
}

fn parse_json_output(stdout: &str) -> Result<(String, Vec<ToolCall>), LLMError> {
    let value: Value = serde_json::from_str(stdout.trim())
        .map_err(|error| LLMError::InvalidResponse(error.to_string()))?;
    let mut text = value
        .get("result")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut tool_calls = Vec::new();

    collect_content_blocks(value.get("content"), &mut text, &mut tool_calls);
    collect_content_blocks(
        value
            .get("message")
            .and_then(|message| message.get("content")),
        &mut text,
        &mut tool_calls,
    );

    Ok((text, tool_calls))
}

fn collect_content_blocks(
    value: Option<&Value>,
    text: &mut String,
    tool_calls: &mut Vec<ToolCall>,
) {
    let Some(blocks) = value.and_then(Value::as_array) else {
        return;
    };
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if text.is_empty() {
                    if let Some(block_text) = block.get("text").and_then(Value::as_str) {
                        text.push_str(block_text);
                    }
                }
            }
            Some("tool_use") => {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let id = block.get("id").and_then(Value::as_str).map(str::to_string);
                let arguments = block.get("input").cloned().unwrap_or(Value::Null);
                tool_calls.push(ToolCall::new(id, name, arguments));
            }
            _ => {}
        }
    }
}

fn command_output(output: Output) -> CommandOutput {
    CommandOutput {
        status_success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolDef;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct MockRunner {
        requests: Arc<Mutex<Vec<CommandRequest>>>,
        outputs: Arc<Mutex<Vec<CommandOutput>>>,
    }

    impl MockRunner {
        fn with_outputs(outputs: Vec<CommandOutput>) -> Self {
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                outputs: Arc::new(Mutex::new(outputs)),
            }
        }

        fn requests(&self) -> Vec<CommandRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl ClaudeCommandRunner for MockRunner {
        fn run<'a>(&'a self, request: CommandRequest) -> CommandFuture<'a> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(request);
                let mut outputs = self.outputs.lock().unwrap();
                if outputs.is_empty() {
                    return Err(LLMError::Provider("no mock output".to_string()));
                }
                Ok(outputs.remove(0))
            })
        }
    }

    #[tokio::test]
    async fn calls_claude_cli_json_and_strips_api_key() {
        let runner = MockRunner::with_outputs(vec![ok_output(json!({
            "result": "hello",
        }))]);
        let inspector = runner.clone();
        let client = ClaudeCliClient::with_runner(
            ClaudeCliConfig {
                command: "claude-test".to_string(),
            },
            runner,
        );

        let (text, calls) = client
            .call("system prompt", &[Message::user("hi")], &[])
            .await
            .unwrap();

        assert_eq!(text, "hello");
        assert!(calls.is_empty());
        let requests = inspector.requests();
        assert_eq!(requests[0].program, "claude-test");
        assert_eq!(requests[0].args[0], "-p");
        assert_eq!(requests[0].args[1], "--output-format");
        assert_eq!(requests[0].args[2], "json");
        assert!(requests[0].args[3].contains("system prompt"));
        assert!(requests[0].args[3].contains("user: hi"));
        assert_eq!(requests[0].env_remove, vec!["ANTHROPIC_API_KEY"]);
    }

    #[tokio::test]
    async fn includes_tool_definitions_in_prompt_and_parses_tool_use() {
        let runner = MockRunner::with_outputs(vec![ok_output(json!({
            "message": {
                "content": [
                    {"type": "text", "text": "checking"},
                    {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"query": "free will"}}
                ]
            }
        }))]);
        let inspector = runner.clone();
        let client = ClaudeCliClient::with_runner(
            ClaudeCliConfig {
                command: "claude".to_string(),
            },
            runner,
        );
        let tools = [ToolDef::new(
            "lookup",
            "Look up a topic",
            json!({"type": "object"}),
        )];

        let (text, calls) = client
            .call("system", &[Message::user("hi")], &tools)
            .await
            .unwrap();

        assert_eq!(text, "checking");
        assert_eq!(calls[0].id, Some("toolu_1".to_string()));
        assert_eq!(calls[0].name, "lookup");
        assert_eq!(calls[0].arguments["query"], "free will");
        assert!(inspector.requests()[0].args[3].contains("\"name\":\"lookup\""));
    }

    #[tokio::test]
    async fn maps_nonzero_exit_to_provider_error() {
        let client = ClaudeCliClient::with_runner(
            ClaudeCliConfig {
                command: "claude".to_string(),
            },
            MockRunner::with_outputs(vec![CommandOutput {
                status_success: false,
                stdout: String::new(),
                stderr: "not logged in".to_string(),
            }]),
        );

        let error = client.call("system", &[], &[]).await.unwrap_err();

        assert_eq!(
            error,
            LLMError::Provider("claude CLI failed: not logged in".to_string())
        );
    }

    #[tokio::test]
    #[ignore = "requires Claude Code CLI login and makes a live claude -p call"]
    async fn live_claude_cli_smoke() {
        let client = ClaudeCliClient::from_env();
        let (text, _calls) = client
            .call("Answer briefly.", &[Message::user("Say hello.")], &[])
            .await
            .unwrap();
        assert!(!text.trim().is_empty());
    }

    fn ok_output(value: Value) -> CommandOutput {
        CommandOutput {
            status_success: true,
            stdout: value.to_string(),
            stderr: String::new(),
        }
    }
}
