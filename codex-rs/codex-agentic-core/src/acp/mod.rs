mod http;
pub mod status;
mod stdio;

use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use shlex::split as shlex_split;
use uuid::Uuid;

use codex_core::AuthManager;
use codex_core::config::Config;
use codex_core::protocol::SessionSource;

use crate::CommandContext;
use crate::CommandRegistry;
use crate::CommandResult;

pub use http::run as run_http;
pub use status::render_status_card;
pub use stdio::run as run_stdio;

#[derive(Clone, Debug)]
pub struct RuntimeOptions {
    pub agent_name: String,
    pub enable_http: bool,
    pub listen: String,
    pub public_url: Option<String>,
    pub initial_status: Option<String>,
    pub base_config: Config,
    pub auth_manager: Arc<AuthManager>,
    pub session_source: SessionSource,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Invocation {
    #[serde(default)]
    pub agent_name: Option<String>,
    #[serde(default)]
    pub input: Vec<Message>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub parts: Vec<MessagePart>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagePart {
    #[serde(rename = "content_type")]
    pub content_type: String,
    #[serde(default)]
    pub content: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunError {
    pub code: String,
    pub message: String,
}

impl RunError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunExecution {
    pub run_id: Uuid,
    pub status: RunStatus,
    pub output: Vec<Message>,
    pub error: Option<RunError>,
}

#[derive(Debug)]
struct ParsedCommand {
    name: String,
    args: Vec<String>,
}

pub(crate) fn execute_invocation(
    registry: Arc<CommandRegistry>,
    base_ctx: &CommandContext,
    expected_agent: &str,
    invocation: Invocation,
) -> RunExecution {
    let run_id = Uuid::new_v4();
    if let Some(agent_name) = invocation.agent_name.as_deref()
        && !agent_name.is_empty()
        && !agent_name.eq_ignore_ascii_case(expected_agent)
    {
        return RunExecution {
            run_id,
            status: RunStatus::Failed,
            output: Vec::new(),
            error: Some(RunError::new(
                "agent_mismatch",
                format!("unsupported agent \"{agent_name}\" (expected {expected_agent})"),
            )),
        };
    }

    let parsed = match parse_command(&invocation.input) {
        Ok(command) => command,
        Err(err) => {
            return RunExecution {
                run_id,
                status: RunStatus::Failed,
                output: Vec::new(),
                error: Some(err),
            };
        }
    };

    let ctx = base_ctx.clone();
    match registry.run(&ctx, &parsed.name, &parsed.args) {
        Ok(result) => {
            let output = command_result_to_messages(result);
            RunExecution {
                run_id,
                status: RunStatus::Completed,
                output,
                error: None,
            }
        }
        Err(err) => RunExecution {
            run_id,
            status: RunStatus::Failed,
            output: Vec::new(),
            error: Some(RunError::new("command_failed", format!("{err:?}"))),
        },
    }
}

fn parse_command(messages: &[Message]) -> std::result::Result<ParsedCommand, RunError> {
    let user_message = messages
        .iter()
        .find(|message| message.role.eq_ignore_ascii_case("user"))
        .ok_or_else(|| RunError::new("invalid_request", "no user message provided"))?;

    let text_part = user_message
        .parts
        .iter()
        .find(|part| part.content_type == "text/plain")
        .ok_or_else(|| RunError::new("invalid_request", "user message missing text/plain part"))?;

    let Some(content) = text_part.content.as_str() else {
        return Err(RunError::new(
            "invalid_request",
            "text/plain content must be a string",
        ));
    };

    let content = content.trim();
    if content.is_empty() {
        return Err(RunError::new("invalid_request", "user message is empty"));
    }

    if !content.starts_with('/') {
        return Err(RunError::new(
            "invalid_request",
            "commands must start with '/' (e.g. /index.build --json)",
        ));
    }

    let command_line = content.trim_start_matches('/').trim();
    if command_line.is_empty() {
        return Err(RunError::new(
            "invalid_request",
            "command is missing after '/'",
        ));
    }

    let parts = shlex_split(command_line).ok_or_else(|| {
        RunError::new(
            "invalid_request",
            "failed to parse command arguments (invalid quoting)",
        )
    })?;

    let Some((name, args)) = parts.split_first() else {
        return Err(RunError::new(
            "invalid_request",
            "command must include a verb (e.g. index.status)",
        ));
    };

    Ok(ParsedCommand {
        name: name.to_string(),
        args: args.iter().map(std::string::ToString::to_string).collect(),
    })
}

fn command_result_to_messages(result: CommandResult) -> Vec<Message> {
    match result {
        CommandResult::Unit => Vec::new(),
        CommandResult::Text(text) => vec![Message {
            role: "assistant".to_string(),
            parts: vec![MessagePart {
                content_type: "text/plain".to_string(),
                content: serde_json::Value::String(text),
            }],
        }],
        CommandResult::Json(value) => {
            let mut parts = Vec::new();
            if let Ok(pretty) = serde_json::to_string_pretty(&value) {
                parts.push(MessagePart {
                    content_type: "text/plain".to_string(),
                    content: serde_json::Value::String(pretty),
                });
            }
            parts.push(MessagePart {
                content_type: "application/json".to_string(),
                content: value,
            });
            vec![Message {
                role: "assistant".to_string(),
                parts,
            }]
        }
    }
}

pub(crate) fn execution_to_response(run: RunExecution, agent: &str) -> TransportResponse {
    TransportResponse {
        agent: agent.to_string(),
        run_id: run.run_id,
        status: run.status,
        output: run.output,
        error: run.error,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TransportResponse {
    pub agent: String,
    #[serde(serialize_with = "uuid_to_string")]
    pub run_id: Uuid,
    pub status: RunStatus,
    pub output: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RunError>,
}

fn uuid_to_string<S>(value: &Uuid, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&value.to_string())
}
