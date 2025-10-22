use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use chrono::DateTime;
use chrono::Local;
use codex_core::CodexConversation;
use codex_core::ConversationManager;
use codex_core::NewConversation;
use codex_core::config::Config;
use codex_core::config_types::McpServerTransportConfig;
use codex_core::protocol::AgentMessageDeltaEvent;
use codex_core::protocol::AgentMessageEvent;
use codex_core::protocol::AgentReasoningDeltaEvent;
use codex_core::protocol::AgentReasoningEvent;
use codex_core::protocol::AgentReasoningRawContentDeltaEvent;
use codex_core::protocol::AgentReasoningRawContentEvent;
use codex_core::protocol::ErrorEvent;
use codex_core::protocol::EventMsg;
use codex_core::protocol::InputItem;
use codex_core::protocol::McpAuthStatus;
use codex_core::protocol::McpListToolsResponseEvent;
use codex_core::protocol::Op;
use codex_core::protocol::RateLimitSnapshot;
use codex_core::protocol::TaskCompleteEvent;
use codex_core::protocol::TokenCountEvent;
use codex_core::protocol::TokenUsageInfo;
use codex_core::protocol::TurnAbortReason;
use codex_protocol::ConversationId;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::BufWriter;
use tokio::io::{self};
use tokio::process::Command;
use tracing::debug;
use tracing::error;
use uuid::Uuid;

use super::Invocation;
use super::Message;
use super::RunExecution;
use super::RunStatus;
use super::RuntimeOptions;
use super::execute_invocation;
use super::status::render_status_card;
use crate::CommandContext;
use crate::CommandRegistry;
use crate::provider::list_models_for_provider_blocking;
use crate::provider::provider_endpoint;
use crate::provider::resolve_provider;
use crate::settings;

const JSONRPC_VERSION: &str = "2.0";
const INIT_PROMPT: &str = include_str!("../../../tui/prompt_for_init_command.md");

const ACP_SLASH_COMMANDS: &[(&str, &str)] = &[
    ("index", "Rebuild the semantic index"),
    (
        "search-code",
        "Search indexed code using the semantic index",
    ),
    ("model", "Choose what model and reasoning effort to use"),
    ("models", "List configured models for the active provider"),
    ("byok", "Manage custom model providers"),
    ("approvals", "Choose what Codex can do without approval"),
    ("review", "Review my current changes and find issues"),
    ("new", "Start a new chat during a conversation"),
    (
        "init",
        "Create an AGENTS.md file with instructions for Codex",
    ),
    (
        "compact",
        "Summarize conversation to prevent hitting the context limit",
    ),
    ("undo", "Restore the workspace to the last Codex snapshot"),
    ("diff", "Show git diff (including untracked files)"),
    ("mention", "Mention a file"),
    (
        "status",
        "Show current session configuration and token usage",
    ),
    ("mcp", "List configured MCP tools"),
    ("logout", "Log out of Codex"),
    ("quit", "Exit Codex"),
];

pub async fn run(
    opts: RuntimeOptions,
    registry: Arc<CommandRegistry>,
    base_ctx: CommandContext,
) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut writer = BufWriter::new(stdout);
    let mut runtime = RuntimeState::new(opts, registry, base_ctx);
    let mut buffer = String::new();

    loop {
        buffer.clear();
        let read = reader.read_line(&mut buffer).await?;
        if read == 0 {
            debug!("acp stdio: EOF reached");
            break;
        }

        let trimmed = buffer.trim_end();
        if trimmed.is_empty() {
            continue;
        }

        match serde_json::from_str::<Value>(trimmed) {
            Ok(value) => {
                if let Err(err) = runtime.handle_message(value, &mut writer).await {
                    error!("acp stdio: error handling message: {err:?}");
                }
            }
            Err(err) => {
                error!("acp stdio: failed to parse request: {err}");
                send_error(
                    &mut writer,
                    Value::Null,
                    -32700,
                    format!("Parse error: {err}"),
                )
                .await?;
            }
        }
    }

    writer.flush().await?;
    Ok(())
}

struct RuntimeState {
    options: RuntimeOptions,
    registry: Arc<CommandRegistry>,
    base_ctx: CommandContext,
    initialized: bool,
    conversation_manager: Arc<ConversationManager>,
    sessions: HashMap<String, SessionState>,
}

#[derive(Clone)]
struct SessionState {
    config: Config,
    conversation: Option<Arc<CodexConversation>>,
    conversation_id: Option<ConversationId>,
    last_usage: Option<TokenUsageInfo>,
    rate_limits: Option<RateLimitSnapshot>,
    rate_limits_captured_at: Option<DateTime<Local>>,
}

#[derive(Deserialize)]
struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
}

#[derive(Deserialize, Default)]
struct NewSessionParams {
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct SessionPromptParams {
    #[serde(rename = "sessionId")]
    session_id: String,
    prompt: Vec<Value>,
}

#[derive(Deserialize)]
struct CancelParams {
    #[serde(rename = "sessionId")]
    session_id: String,
}

impl RuntimeState {
    fn new(
        options: RuntimeOptions,
        registry: Arc<CommandRegistry>,
        base_ctx: CommandContext,
    ) -> Self {
        let conversation_manager = Arc::new(ConversationManager::new(
            Arc::clone(&options.auth_manager),
            options.session_source,
        ));
        Self {
            options,
            registry,
            base_ctx,
            initialized: false,
            conversation_manager,
            sessions: HashMap::new(),
        }
    }

    async fn handle_message(
        &mut self,
        value: Value,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<()> {
        let Some(obj) = value.as_object() else {
            send_error(
                writer,
                Value::Null,
                -32600,
                "Invalid request: expected JSON object",
            )
            .await?;
            return Ok(());
        };

        let version = obj.get("jsonrpc").and_then(Value::as_str).unwrap_or("");
        if version != JSONRPC_VERSION {
            let id = obj.get("id").cloned().unwrap_or(Value::Null);
            send_error(
                writer,
                id,
                -32600,
                "Invalid request: jsonrpc must equal \"2.0\"",
            )
            .await?;
            return Ok(());
        }

        let method = obj.get("method").and_then(Value::as_str);
        let id = obj.get("id").cloned();
        let params = obj.get("params").cloned().unwrap_or(Value::Null);

        match (method, id) {
            (Some("initialize"), Some(id)) => self.handle_initialize(id, params, writer).await?,
            (Some("authenticate"), Some(id)) => self.handle_authenticate(id, writer).await?,
            (Some("session/new"), Some(id)) => self.handle_session_new(id, params, writer).await?,
            (Some("session/prompt"), Some(id)) => {
                self.handle_session_prompt(id, params, writer).await?
            }
            (Some("session/cancel"), maybe_id) => {
                self.handle_session_cancel(params, writer).await?;
                if let Some(id) = maybe_id {
                    send_response(writer, id, json!({ "acknowledged": true })).await?;
                }
            }
            (Some(method), Some(id)) => {
                send_error(writer, id, -32601, format!("Method {method} not found")).await?;
            }
            (Some(method), None) => {
                debug!("acp stdio: ignoring notification for unsupported method: {method}");
            }
            (None, Some(id)) => {
                send_error(writer, id, -32600, "Invalid request: missing method").await?;
            }
            (None, None) => {
                // Nothing to do.
            }
        }

        Ok(())
    }

    async fn handle_index_command(
        &mut self,
        session_id: &str,
        session_state: &SessionState,
        args: &[String],
        id: Value,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<bool> {
        let sub = args.first().map(std::string::String::as_str);
        let (command, remainder) = match sub {
            Some("status") => ("index.status", &args[1..]),
            Some("query") => ("index.query", &args[1..]),
            Some("verify") => ("index.verify", &args[1..]),
            Some("clean") => ("index.clean", &args[1..]),
            Some("ignore") => ("index.ignore", &args[1..]),
            Some("build") | None => ("index.build", &args[..0]),
            Some(other) => {
                let message = format!(
                    "Unknown /index subcommand `{other}`. Expected one of: build, status, query, verify, clean, ignore."
                );
                self.send_agent_message(session_id, &message, writer)
                    .await?;
                send_response(writer, id, json!({ "stopReason": "end_turn" })).await?;
                return Ok(true);
            }
        };

        let mut rebuilt = String::from("/");
        rebuilt.push_str(command);
        if !remainder.is_empty() {
            rebuilt.push(' ');
            rebuilt.push_str(&remainder.join(" "));
        }
        let res = self
            .run_slash_command(session_id, session_state, &rebuilt, id, writer)
            .await;
        match res {
            Ok(_) => Ok(true),
            Err(err) => Err(err),
        }
    }

    async fn emit_status_summary(
        &self,
        session_id: &str,
        session_state: &SessionState,
        id: Value,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<bool> {
        let summary = markdown_block(render_status_card(
            &session_state.config,
            &self.base_ctx,
            session_state.last_usage.as_ref(),
            session_state.rate_limits.as_ref(),
            session_state.rate_limits_captured_at.as_ref(),
            session_state.conversation_id.as_ref(),
        ));

        self.send_agent_message(session_id, &summary, writer)
            .await?;
        send_response(writer, id, json!({ "stopReason": "end_turn" })).await?;
        Ok(true)
    }

    async fn handle_compact_command(
        &self,
        session_id: &str,
        session_state: &mut SessionState,
        id: Value,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<bool> {
        let outcome = match self
            .submit_conversation_op(session_id, session_state, Op::Compact, writer)
            .await
        {
            Ok(outcome) => outcome,
            Err(err) => {
                let message = format!("Failed to compact conversation: {err}");
                self.send_agent_message(session_id, &message, writer)
                    .await?;
                send_error(writer, id, -32002, message).await?;
                return Ok(true);
            }
        };

        if let Some(err) = outcome.error {
            send_error(writer, id, -32001, err).await?;
        } else {
            let mut response_payload = json!({ "stopReason": outcome.stop_reason });
            if let Some(info) = &session_state.last_usage
                && let Some(obj) = response_payload.as_object_mut()
            {
                obj.insert("usage".to_string(), token_usage_to_json(info));
            }
            send_response(writer, id, response_payload).await?;
        }
        Ok(true)
    }

    async fn handle_diff_command(
        &self,
        session_id: &str,
        session_state: &SessionState,
        id: Value,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<bool> {
        match compute_git_diff(session_state.config.cwd.as_path()).await {
            Ok((false, _)) => {
                self.send_agent_message(
                    session_id,
                    "`/diff` — not inside a git repository.",
                    writer,
                )
                .await?;
            }
            Ok((true, diff)) => {
                if diff.trim().is_empty() {
                    self.send_agent_message(session_id, "Working tree clean.", writer)
                        .await?;
                } else {
                    self.send_agent_message(session_id, &diff, writer).await?;
                }
            }
            Err(err) => {
                let message = format!("Failed to compute diff: {err}");
                self.send_agent_message(session_id, &message, writer)
                    .await?;
            }
        }

        send_response(writer, id, json!({ "stopReason": "end_turn" })).await?;
        Ok(true)
    }

    async fn emit_model_summary(
        &self,
        session_id: &str,
        session_state: &SessionState,
        id: Value,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<bool> {
        let config = &session_state.config;
        let mut lines = Vec::new();
        lines.push(format!("Active model: {}", config.model));
        lines.push(format!("Provider: {}", config.model_provider_id));
        if let Some(effort) = config.model_reasoning_effort {
            lines.push(format!("Reasoning effort: {effort:?}"));
        }
        lines.push(format!(
            "Reasoning summary: {:?}",
            config.model_reasoning_summary
        ));
        if let Some(mode) = self
            .base_ctx
            .settings
            .model
            .as_ref()
            .and_then(|m| m.reasoning_view.clone())
        {
            lines.push(format!("Reasoning view: {mode}"));
        }
        let summary = markdown_block(lines.join("\n"));
        self.send_agent_message(session_id, &summary, writer)
            .await?;
        send_response(writer, id, json!({ "stopReason": "end_turn" })).await?;
        Ok(true)
    }

    async fn emit_models_summary(
        &self,
        session_id: &str,
        session_state: &SessionState,
        id: Value,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<bool> {
        let settings_snapshot = settings::global();
        let provider_id = if session_state.config.model_provider_id.is_empty() {
            resolve_provider(&settings_snapshot, None).unwrap_or_else(|| "openai".to_string())
        } else {
            session_state.config.model_provider_id.clone()
        };

        let mut message = String::new();
        let _ = writeln!(&mut message, "Selected provider: {provider_id}");
        if let Some(endpoint) = provider_endpoint(&settings_snapshot, &provider_id) {
            let _ = writeln!(&mut message, "Endpoint: {endpoint}");
        }

        if !session_state.config.model.is_empty() {
            let _ = writeln!(&mut message, "Active model: {}", session_state.config.model);
        }

        match list_models_for_provider_blocking(&settings_snapshot, &provider_id) {
            Ok(models) if !models.is_empty() => {
                let _ = writeln!(&mut message, "\nAvailable models:");
                for model in models {
                    let _ = writeln!(&mut message, "  • {model}");
                }
            }
            Ok(_) => {
                let _ = writeln!(
                    &mut message,
                    "\nNo models reported for provider `{provider_id}`."
                );
            }
            Err(err) => {
                let _ = writeln!(
                    &mut message,
                    "\nFailed to list models for provider `{provider_id}`: {err}"
                );
            }
        }

        if let Some(custom) = settings_snapshot.providers.as_ref()
            && !custom.custom.is_empty()
        {
            let _ = writeln!(&mut message, "\nCustom providers:");
            for (id, provider) in &custom.custom {
                let name = provider.name.trim();
                let display = if name.is_empty() { id.as_str() } else { name };
                let _ = writeln!(&mut message, "  • {display} ({id})");
            }
        }

        let message = markdown_block(message);
        self.send_agent_message(session_id, &message, writer)
            .await?;
        send_response(writer, id, json!({ "stopReason": "end_turn" })).await?;
        Ok(true)
    }

    async fn emit_byok_summary(
        &self,
        session_id: &str,
        id: Value,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<bool> {
        let settings_snapshot = settings::global();
        let custom = settings_snapshot
            .providers
            .as_ref()
            .map(|providers| providers.custom.clone())
            .unwrap_or_default();

        if custom.is_empty() {
            self.send_agent_message(
                session_id,
                "No custom providers configured. Use /BYOK in the TUI or edit settings.json.",
                writer,
            )
            .await?;
        } else {
            let mut lines = Vec::new();
            lines.push("Custom providers:".to_string());
            for (id, provider) in custom {
                let mut line = format!("- {id}");
                if !provider.name.trim().is_empty() {
                    line.push_str(&format!(" ({})", provider.name));
                }
                if let Some(model) = provider.default_model {
                    line.push_str(&format!(" → default model {model}"));
                }
                lines.push(line);
            }
            let payload = markdown_block(lines.join("\n"));
            self.send_agent_message(session_id, &payload, writer)
                .await?;
        }

        send_response(writer, id, json!({ "stopReason": "end_turn" })).await?;
        Ok(true)
    }

    async fn emit_approvals_summary(
        &self,
        session_id: &str,
        session_state: &SessionState,
        id: Value,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<bool> {
        let config = &session_state.config;
        let message = markdown_block(format!(
            "Approval policy: {policy:?}\nSandbox policy: {sandbox:?}",
            policy = config.approval_policy,
            sandbox = config.sandbox_policy
        ));
        self.send_agent_message(session_id, &message, writer)
            .await?;
        send_response(writer, id, json!({ "stopReason": "end_turn" })).await?;
        Ok(true)
    }

    async fn handle_new_conversation(
        &self,
        session_id: &str,
        session_state: &mut SessionState,
        id: Value,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<bool> {
        session_state.conversation = None;
        session_state.conversation_id = None;
        session_state.last_usage = None;
        session_state.rate_limits = None;
        session_state.rate_limits_captured_at = None;
        self.send_agent_message(session_id, "Starting a new conversation...", writer)
            .await?;
        self.ensure_session_conversation(session_id, session_state, writer)
            .await?;
        send_response(writer, id, json!({ "stopReason": "end_turn" })).await?;
        Ok(true)
    }

    async fn handle_init_command(
        &self,
        session_id: &str,
        session_state: &mut SessionState,
        id: Value,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<bool> {
        let outcome = match self
            .submit_text_turn(session_id, session_state, INIT_PROMPT, writer)
            .await
        {
            Ok(outcome) => outcome,
            Err(err) => {
                let message = format!("Failed to submit init prompt: {err}");
                self.send_agent_message(session_id, &message, writer)
                    .await?;
                send_error(writer, id, -32002, message).await?;
                return Ok(true);
            }
        };

        if let Some(err) = outcome.error {
            send_error(writer, id, -32001, err).await?;
        } else {
            let mut response_payload = json!({ "stopReason": outcome.stop_reason });
            if let Some(info) = &session_state.last_usage
                && let Some(obj) = response_payload.as_object_mut()
            {
                obj.insert("usage".to_string(), token_usage_to_json(info));
            }
            send_response(writer, id, response_payload).await?;
        }

        Ok(true)
    }

    async fn emit_mcp_summary(
        &self,
        session_id: &str,
        session_state: &mut SessionState,
        id: Value,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<bool> {
        if session_state.config.mcp_servers.is_empty() {
            self.send_agent_message(
                session_id,
                "No MCP servers configured. Use settings.json or the TUI to add one.",
                writer,
            )
            .await?;
            send_response(writer, id, json!({ "stopReason": "end_turn" })).await?;
            return Ok(true);
        }
        let outcome = match self
            .submit_conversation_op(session_id, session_state, Op::ListMcpTools, writer)
            .await
        {
            Ok(outcome) => outcome,
            Err(err) => {
                let message = format!("Failed to list MCP tools: {err}");
                self.send_agent_message(session_id, &message, writer)
                    .await?;
                send_error(writer, id, -32002, message).await?;
                return Ok(true);
            }
        };

        if let Some(err) = outcome.error {
            send_error(writer, id, -32001, err).await?;
        } else {
            send_response(writer, id, json!({ "stopReason": outcome.stop_reason })).await?;
        }
        Ok(true)
    }

    async fn handle_logout_command(
        &self,
        session_id: &str,
        id: Value,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<bool> {
        match self.options.auth_manager.logout() {
            Ok(true) => {
                self.send_agent_message(session_id, "Logged out successfully.", writer)
                    .await?
            }
            Ok(false) => {
                self.send_agent_message(session_id, "No active login found.", writer)
                    .await?
            }
            Err(err) => {
                let message = format!("Failed to logout: {err}");
                self.send_agent_message(session_id, &message, writer)
                    .await?;
                send_error(writer, id, -32003, message).await?;
                return Ok(true);
            }
        }

        send_response(writer, id, json!({ "stopReason": "end_turn" })).await?;
        Ok(true)
    }

    async fn handle_quit_command(
        &self,
        session_id: &str,
        id: Value,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<bool> {
        self.send_agent_message(
            session_id,
            "To exit, terminate the ACP process or close the client connection.",
            writer,
        )
        .await?;
        send_response(writer, id, json!({ "stopReason": "end_turn" })).await?;
        Ok(true)
    }

    async fn handle_initialize(
        &mut self,
        id: Value,
        params: Value,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<()> {
        if self.initialized {
            send_error(writer, id, -32600, "initialize already called").await?;
            return Ok(());
        }

        let params: InitializeParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(_) => {
                send_error(writer, id, -32602, "Invalid initialize parameters").await?;
                return Ok(());
            }
        };

        if params.protocol_version != 1 {
            send_error(
                writer,
                id,
                -32000,
                format!(
                    "Unsupported protocol version {} (only version 1 is supported)",
                    params.protocol_version
                ),
            )
            .await?;
            return Ok(());
        }

        self.initialized = true;

        let response = json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": false,
                "promptCapabilities": {
                    "audio": false,
                    "embeddedContext": false,
                    "image": false
                },
                "mcpCapabilities": {
                    "http": false,
                    "sse": false
                }
            },
            "authMethods": []
        });

        send_response(writer, id, response).await
    }

    async fn handle_authenticate(
        &mut self,
        id: Value,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<()> {
        if !self.initialized {
            send_error(
                writer,
                id,
                -32600,
                "initialize must be called before authenticate",
            )
            .await?;
            return Ok(());
        }

        send_response(writer, id, json!({})).await
    }

    async fn handle_session_new(
        &mut self,
        id: Value,
        params: Value,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<()> {
        if !self.initialized {
            send_error(
                writer,
                id,
                -32600,
                "initialize must be called before session/new",
            )
            .await?;
            return Ok(());
        }

        let params: NewSessionParams = match params {
            Value::Null => NewSessionParams::default(),
            other => match serde_json::from_value(other) {
                Ok(p) => p,
                Err(_) => {
                    send_error(writer, id, -32602, "Invalid session/new parameters").await?;
                    return Ok(());
                }
            },
        };

        let session_id = format!("sess_{}", Uuid::new_v4().simple());
        let mut config = self.options.base_config.clone();
        if let Some(ref cwd) = params.cwd {
            config.cwd = PathBuf::from(cwd);
        }
        let state = SessionState {
            config,
            conversation: None,
            conversation_id: None,
            last_usage: None,
            rate_limits: None,
            rate_limits_captured_at: None,
        };
        self.sessions.insert(session_id.clone(), state);

        let response = json!({ "sessionId": session_id });
        send_response(writer, id, response).await?;

        if let Some(summary) = self.session_summary(&session_id) {
            self.send_agent_message(&session_id, &summary, writer)
                .await?;
        }
        self.send_available_commands(&session_id, writer).await?;

        Ok(())
    }

    async fn handle_session_prompt(
        &mut self,
        id: Value,
        params: Value,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<()> {
        if !self.initialized {
            send_error(
                writer,
                id,
                -32600,
                "initialize must be called before session/prompt",
            )
            .await?;
            return Ok(());
        }

        let params: SessionPromptParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(_) => {
                send_error(writer, id, -32602, "Invalid session/prompt parameters").await?;
                return Ok(());
            }
        };

        let Some(text) = extract_text_prompt(&params.prompt) else {
            send_error(
                writer,
                id,
                -32602,
                "session/prompt requires a text content block",
            )
            .await?;
            return Ok(());
        };

        let trimmed = text.trim();
        if trimmed.is_empty() {
            send_error(writer, id, -32602, "session/prompt text content is empty").await?;
            return Ok(());
        }

        let session_id = params.session_id.clone();
        let Some(mut session_state) = self.sessions.remove(&session_id) else {
            send_error(
                writer,
                id,
                -32000,
                format!("Unknown sessionId {}", params.session_id),
            )
            .await?;
            return Ok(());
        };

        if trimmed.starts_with('/') {
            let command_line = trimmed.trim_start_matches('/');
            let mut parts = command_line.split_whitespace();
            let Some(verb) = parts.next() else {
                send_error(writer, id, -32602, "session/prompt text content is empty").await?;
                return Ok(());
            };
            let args: Vec<String> = parts.map(std::string::ToString::to_string).collect();

            let handled = match verb {
                "index" => {
                    self.handle_index_command(
                        &session_id,
                        &session_state,
                        &args,
                        id.clone(),
                        writer,
                    )
                    .await?
                }
                "status" => {
                    self.emit_status_summary(&session_id, &session_state, id.clone(), writer)
                        .await?
                }
                "compact" => {
                    self.handle_compact_command(&session_id, &mut session_state, id.clone(), writer)
                        .await?
                }
                "diff" => {
                    self.handle_diff_command(&session_id, &session_state, id.clone(), writer)
                        .await?
                }
                "model" => {
                    self.emit_model_summary(&session_id, &session_state, id.clone(), writer)
                        .await?
                }
                "models" | "models.list" => {
                    self.emit_models_summary(&session_id, &session_state, id.clone(), writer)
                        .await?
                }
                "byok" => {
                    self.emit_byok_summary(&session_id, id.clone(), writer)
                        .await?
                }
                "approvals" => {
                    self.emit_approvals_summary(&session_id, &session_state, id.clone(), writer)
                        .await?
                }
                "new" => {
                    self.handle_new_conversation(
                        &session_id,
                        &mut session_state,
                        id.clone(),
                        writer,
                    )
                    .await?
                }
                "init" => {
                    self.handle_init_command(&session_id, &mut session_state, id.clone(), writer)
                        .await?
                }
                "mcp" => {
                    self.emit_mcp_summary(&session_id, &mut session_state, id.clone(), writer)
                        .await?
                }
                "logout" => {
                    self.handle_logout_command(&session_id, id.clone(), writer)
                        .await?
                }
                "quit" => {
                    self.handle_quit_command(&session_id, id.clone(), writer)
                        .await?
                }
                "mention" => {
                    self.send_agent_message(
                        &session_id,
                        "Mention a file by typing @path/to/file in your prompt.",
                        writer,
                    )
                    .await?;
                    send_response(writer, id.clone(), json!({ "stopReason": "end_turn" })).await?;
                    true
                }
                "undo" => {
                    self.send_agent_message(
                        &session_id,
                        "Undo is not available in ACP sessions yet.",
                        writer,
                    )
                    .await?;
                    send_response(writer, id.clone(), json!({ "stopReason": "end_turn" })).await?;
                    true
                }
                "review" => {
                    self.send_agent_message(
                        &session_id,
                        "Review mode is only available in the interactive TUI today.",
                        writer,
                    )
                    .await?;
                    send_response(writer, id.clone(), json!({ "stopReason": "end_turn" })).await?;
                    true
                }
                _ => false,
            };

            if handled {
                self.sessions.insert(session_id, session_state);
                return Ok(());
            }

            let res = self
                .run_slash_command(&session_id, &session_state, trimmed, id.clone(), writer)
                .await;
            self.sessions.insert(session_id, session_state);
            return res;
        }

        let res = self
            .run_free_form_prompt(&session_id, &mut session_state, trimmed, id, writer)
            .await;
        self.sessions.insert(session_id, session_state);
        res
    }

    async fn run_slash_command(
        &mut self,
        session_id: &str,
        session_state: &SessionState,
        raw_input: &str,
        id: Value,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<()> {
        let sanitized = raw_input.trim_end_matches(['.', '!', '?', ';', ':']);
        if sanitized.is_empty() {
            send_error(writer, id, -32602, "session/prompt text content is empty").await?;
            return Ok(());
        }
        if !sanitized.starts_with('/') {
            send_error(
                writer,
                id,
                -32602,
                "session/prompt text content must start with '/' for command dispatch",
            )
            .await?;
            return Ok(());
        }
        debug!(command = sanitized, "dispatching slash command");

        let mut parts = sanitized.splitn(2, char::is_whitespace);
        let command = parts.next().unwrap_or("");
        let args = parts.next().map(str::trim).unwrap_or("");
        if command == "/search-code" && args.is_empty() {
            let confidence = settings::global().search_confidence_min_percent();
            let usage = format!(
                "Usage: /search-code <query>\nSemantic search matches meaning, not regex (patterns like .* are ignored).\nUse function names, doc phrases, or natural text (e.g. /search-code \"load_config error handling\").\nQuote multi-word queries for clarity.\nCurrent minimum confidence: {confidence}%\nAdjust the threshold with `codex search-code --min-confidence <percent>` or edit `.codex/settings.json` (or `~/.codex/settings.json`)."
            );
            self.send_agent_message(session_id, &usage, writer).await?;
            send_response(writer, id, json!({ "stopReason": "end_turn" })).await?;
            return Ok(());
        }

        let invocation = Invocation {
            agent_name: Some(self.options.agent_name.clone()),
            input: vec![Message {
                role: "user".to_string(),
                parts: vec![super::MessagePart {
                    content_type: "text/plain".to_string(),
                    content: Value::String(sanitized.to_string()),
                }],
            }],
        };

        let mut ctx = self.base_ctx.clone();
        ctx = ctx.with_working_dir(session_state.config.cwd.display().to_string());

        let execution = execute_invocation(
            Arc::clone(&self.registry),
            &ctx,
            &self.options.agent_name,
            invocation,
        );

        match execution.status {
            RunStatus::Completed => {
                if let Some(message) = flatten_execution_output(&execution) {
                    self.send_agent_message(session_id, &message, writer)
                        .await?;
                }
                send_response(writer, id, json!({ "stopReason": "end_turn" })).await?;
            }
            RunStatus::Failed => {
                let message = execution
                    .error
                    .map(|err| err.message)
                    .unwrap_or_else(|| "Command failed".to_string());
                self.send_agent_message(session_id, &message, writer)
                    .await?;
                send_error(writer, id, -32001, message).await?;
            }
        }

        Ok(())
    }

    async fn run_free_form_prompt(
        &mut self,
        session_id: &str,
        session_state: &mut SessionState,
        text: &str,
        id: Value,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<()> {
        if text.is_empty() {
            send_error(
                writer,
                id,
                -32602,
                "session/prompt requires a text content block",
            )
            .await?;
            return Ok(());
        }

        let outcome = match self
            .submit_text_turn(session_id, session_state, text, writer)
            .await
        {
            Ok(outcome) => outcome,
            Err(err) => {
                let message = format!("Failed to submit prompt: {err}");
                self.send_agent_message(session_id, &message, writer)
                    .await?;
                send_error(writer, id, -32002, message).await?;
                return Ok(());
            }
        };

        if let Some(err) = outcome.error {
            send_error(writer, id, -32001, err).await?;
            return Ok(());
        }

        let mut response_payload = json!({ "stopReason": outcome.stop_reason });
        if let Some(info) = &session_state.last_usage
            && let Some(obj) = response_payload.as_object_mut()
        {
            obj.insert("usage".to_string(), token_usage_to_json(info));
        }

        send_response(writer, id, response_payload).await
    }

    async fn ensure_session_conversation(
        &self,
        session_id: &str,
        session_state: &mut SessionState,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<Arc<CodexConversation>> {
        if let Some(conversation) = &session_state.conversation {
            return Ok(Arc::clone(conversation));
        }

        let NewConversation {
            conversation_id,
            conversation,
            session_configured,
        } = self
            .conversation_manager
            .new_conversation(session_state.config.clone())
            .await?;

        let readiness_message = if let Some(effort) = session_configured.reasoning_effort {
            format!(
                "Session ready. Model: {} (reasoning effort: {effort:?}).",
                session_configured.model
            )
        } else {
            format!("Session ready. Model: {}.", session_configured.model)
        };
        self.send_agent_thought(session_id, &readiness_message, writer)
            .await?;

        session_state.conversation = Some(conversation.clone());
        session_state.conversation_id = Some(conversation_id);
        Ok(conversation)
    }

    async fn submit_text_turn(
        &self,
        session_id: &str,
        session_state: &mut SessionState,
        text: &str,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<TurnOutcome> {
        if text.trim().is_empty() {
            return Err(anyhow!("empty prompt"));
        }

        let conversation = self
            .ensure_session_conversation(session_id, session_state, writer)
            .await?;

        let items = vec![InputItem::Text {
            text: text.to_string(),
        }];

        let submit_id = conversation
            .submit(Op::UserTurn {
                items,
                cwd: session_state.config.cwd.clone(),
                approval_policy: session_state.config.approval_policy,
                sandbox_policy: session_state.config.sandbox_policy.clone(),
                model: session_state.config.model.clone(),
                effort: session_state.config.model_reasoning_effort,
                summary: session_state.config.model_reasoning_summary,
                final_output_json_schema: None,
            })
            .await
            .map_err(|err| anyhow!(err))?;

        self.process_turn_events(session_id, conversation, submit_id, session_state, writer)
            .await
    }

    async fn submit_conversation_op(
        &self,
        session_id: &str,
        session_state: &mut SessionState,
        op: Op,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<TurnOutcome> {
        let conversation = self
            .ensure_session_conversation(session_id, session_state, writer)
            .await?;
        let submit_id = conversation.submit(op).await.map_err(|err| anyhow!(err))?;
        self.process_turn_events(session_id, conversation, submit_id, session_state, writer)
            .await
    }

    async fn process_turn_events(
        &self,
        session_id: &str,
        conversation: Arc<CodexConversation>,
        submit_id: String,
        session_state: &mut SessionState,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<TurnOutcome> {
        let mut stop_reason = "end_turn".to_string();
        let mut error: Option<String> = None;
        let mut had_message_delta = false;
        let mut agent_message_emitted = false;

        loop {
            let event = match conversation.next_event().await {
                Ok(event) => event,
                Err(err) => {
                    error = Some(format!("Failed to receive agent event: {err}"));
                    stop_reason = "error".to_string();
                    break;
                }
            };

            if event.id != submit_id {
                continue;
            }

            match event.msg {
                EventMsg::AgentMessageDelta(AgentMessageDeltaEvent { delta }) => {
                    if !delta.is_empty() {
                        had_message_delta = true;
                        agent_message_emitted = true;
                        self.send_agent_message(session_id, &delta, writer).await?;
                    }
                }
                EventMsg::AgentMessage(AgentMessageEvent { message }) => {
                    if !message.is_empty() && !had_message_delta {
                        self.send_agent_message(session_id, &message, writer)
                            .await?;
                        agent_message_emitted = true;
                    }
                }
                EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent { delta })
                | EventMsg::AgentReasoningRawContentDelta(AgentReasoningRawContentDeltaEvent {
                    delta,
                }) => {
                    if !delta.is_empty() {
                        self.send_agent_thought(session_id, &delta, writer).await?;
                    }
                }
                EventMsg::AgentReasoning(AgentReasoningEvent { text })
                | EventMsg::AgentReasoningRawContent(AgentReasoningRawContentEvent { text }) => {
                    if !text.is_empty() {
                        self.send_agent_thought(session_id, &text, writer).await?;
                    }
                }
                EventMsg::McpListToolsResponse(ev) => {
                    let message = format_mcp_tools_output(&session_state.config, &ev);
                    self.send_agent_message(session_id, &message, writer)
                        .await?;
                    stop_reason = "end_turn".to_string();
                    break;
                }
                EventMsg::TokenCount(TokenCountEvent { info, rate_limits }) => {
                    session_state.last_usage = info.clone();
                    if let Some(snapshot) = rate_limits {
                        session_state.rate_limits = Some(snapshot);
                        session_state.rate_limits_captured_at = Some(Local::now());
                    }
                }
                EventMsg::TaskComplete(TaskCompleteEvent { last_agent_message }) => {
                    if let Some(message) = last_agent_message
                        && !message.is_empty()
                        && !had_message_delta
                        && !agent_message_emitted
                    {
                        self.send_agent_message(session_id, &message, writer)
                            .await?;
                    }
                    break;
                }
                EventMsg::Error(ErrorEvent { message }) => {
                    if !message.is_empty() {
                        self.send_agent_message(session_id, &message, writer)
                            .await?;
                    }
                    error = Some(message);
                    stop_reason = "error".to_string();
                    break;
                }
                EventMsg::TurnAborted(ev) => {
                    stop_reason = match ev.reason {
                        TurnAbortReason::Interrupted => "cancelled".to_string(),
                        TurnAbortReason::Replaced => "replaced".to_string(),
                        TurnAbortReason::ReviewEnded => "review-ended".to_string(),
                    };
                    self.send_agent_message(
                        session_id,
                        "Turn aborted. Awaiting next instruction.",
                        writer,
                    )
                    .await?;
                    break;
                }
                EventMsg::StreamError(stream_err) => {
                    self.send_agent_message(session_id, &stream_err.message, writer)
                        .await?;
                }
                EventMsg::PlanUpdate(update) => {
                    let summary = format_plan_update(&update);
                    if !summary.is_empty() {
                        self.send_agent_thought(session_id, &summary, writer)
                            .await?;
                    }
                }
                _ => {
                    // Ignore other events for now (tool execution, approvals, etc.).
                }
            }
        }

        Ok(TurnOutcome { stop_reason, error })
    }

    async fn send_agent_thought(
        &self,
        session_id: &str,
        text: &str,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }

        let notification = json!({
            "jsonrpc": JSONRPC_VERSION,
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "agent_thought_chunk",
                    "content": {
                        "type": "text",
                        "text": text,
                    }
                }
            }
        });

        send_notification(writer, notification).await
    }

    async fn handle_session_cancel(
        &mut self,
        params: Value,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<()> {
        let params: CancelParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(_) => {
                debug!("acp stdio: ignoring malformed session/cancel notification");
                return Ok(());
            }
        };

        let conversation = self
            .sessions
            .get(&params.session_id)
            .and_then(|state| state.conversation.clone());

        if let Some(conv) = conversation
            && let Err(err) = conv.submit(Op::Interrupt).await
        {
            debug!(
                ?err,
                "acp stdio: failed to propagate cancellation interrupt"
            );
        }

        if self.sessions.contains_key(&params.session_id) {
            self.send_agent_message(
                &params.session_id,
                "Cancellation noted. No long-running operations were in progress.",
                writer,
            )
            .await?;
        }

        Ok(())
    }

    fn session_summary(&self, session_id: &str) -> Option<String> {
        let state = self.sessions.get(session_id)?;
        Some(markdown_block(render_status_card(
            &state.config,
            &self.base_ctx,
            state.last_usage.as_ref(),
            state.rate_limits.as_ref(),
            state.rate_limits_captured_at.as_ref(),
            state.conversation_id.as_ref(),
        )))
    }

    async fn send_agent_message(
        &self,
        session_id: &str,
        text: &str,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<()> {
        let notification = json!({
            "jsonrpc": JSONRPC_VERSION,
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {
                        "type": "text",
                        "text": text
                    }
                }
            }
        });
        send_notification(writer, notification).await
    }

    async fn send_available_commands(
        &self,
        session_id: &str,
        writer: &mut BufWriter<io::Stdout>,
    ) -> Result<()> {
        let mut commands = Vec::new();
        for (name, description) in ACP_SLASH_COMMANDS {
            commands.push(json!({
                "name": name,
                "displayName": format!("/{name}"),
                "description": description,
            }));
        }

        let notification = json!({
            "jsonrpc": JSONRPC_VERSION,
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "available_commands_update",
                    "availableCommands": commands,
                }
            }
        });

        send_notification(writer, notification).await
    }
}

struct TurnOutcome {
    stop_reason: String,
    error: Option<String>,
}

fn token_usage_to_json(info: &TokenUsageInfo) -> Value {
    let last = &info.last_token_usage;
    json!({
        "inputTokens": last.input_tokens,
        "cachedInputTokens": last.cached_input_tokens,
        "outputTokens": last.output_tokens,
        "reasoningOutputTokens": last.reasoning_output_tokens,
        "totalTokens": last.total_tokens,
        "cumulativeTotalTokens": info.total_token_usage.total_tokens,
    })
}

fn format_plan_update(update: &UpdatePlanArgs) -> String {
    let mut lines = Vec::new();
    if let Some(explanation) = &update.explanation {
        let trimmed = explanation.trim();
        if !trimmed.is_empty() {
            lines.push(trimmed.to_string());
        }
    }

    for item in &update.plan {
        let status = step_status_label(&item.status);
        let step = item.step.trim();
        if step.is_empty() {
            continue;
        }
        lines.push(format!("[{status}] {step}"));
    }

    lines.join("\n")
}

fn step_status_label(status: &StepStatus) -> &'static str {
    match status {
        StepStatus::Pending => "pending",
        StepStatus::InProgress => "in-progress",
        StepStatus::Completed => "completed",
    }
}

fn markdown_block(text: String) -> String {
    format!("```text\n{text}\n```")
}

fn format_mcp_tools_output(config: &Config, event: &McpListToolsResponseEvent) -> String {
    let mut lines: Vec<String> = vec![
        "/mcp".to_string(),
        String::new(),
        "🔌  MCP Tools".to_string(),
        String::new(),
    ];

    let mut tools_by_server: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for full_name in event.tools.keys() {
        if let Some((server, tool)) = full_name.split_once("__") {
            tools_by_server
                .entry(server.to_string())
                .or_default()
                .push(tool.to_string());
        }
    }
    for names in tools_by_server.values_mut() {
        names.sort();
    }

    let mut servers: Vec<_> = config.mcp_servers.iter().collect();
    servers.sort_by(|a, b| a.0.cmp(b.0));

    for (server, cfg) in servers {
        let status = if cfg.enabled { "enabled" } else { "disabled" };
        let auth = event
            .auth_statuses
            .get(server.as_str())
            .copied()
            .unwrap_or(McpAuthStatus::Unsupported);
        lines.push(format!("  • Server: {server}"));
        lines.push(format!("    • Status: {status}"));
        lines.push(format!("    • Auth: {auth}"));

        match &cfg.transport {
            McpServerTransportConfig::Stdio {
                command, args, env, ..
            } => {
                let mut cmd = command.clone();
                if !args.is_empty() {
                    cmd.push(' ');
                    cmd.push_str(&args.join(" "));
                }
                lines.push("    • Transport: stdio".to_string());
                lines.push(format!("    • Command: {cmd}"));

                if let Some(env) = env {
                    if env.is_empty() {
                        lines.push("    • Env: <none>".to_string());
                    } else {
                        let mut pairs: Vec<String> =
                            env.iter().map(|(k, v)| format!("{k}={v}")).collect();
                        pairs.sort();
                        lines.push(format!("    • Env: {}", pairs.join(" ")));
                    }
                }
            }
            McpServerTransportConfig::StreamableHttp {
                url,
                bearer_token_env_var,
                ..
            } => {
                lines.push("    • Transport: streamable-http".to_string());
                lines.push(format!("    • URL: {url}"));
                if let Some(var) = bearer_token_env_var {
                    lines.push(format!("    • Token Env: {var}"));
                }
            }
        }

        let tool_names = tools_by_server
            .get(server.as_str())
            .cloned()
            .unwrap_or_default();
        if !cfg.enabled {
            lines.push("    • Tools: (disabled)".to_string());
        } else if tool_names.is_empty() {
            lines.push("    • Tools: (none)".to_string());
        } else {
            lines.push(format!("    • Tools: {}", tool_names.join(", ")));
        }

        lines.push(String::new());
    }

    markdown_block(lines.join("\n"))
}

async fn compute_git_diff(cwd: &Path) -> std::io::Result<(bool, String)> {
    if !is_inside_git_repo(cwd).await? {
        return Ok((false, String::new()));
    }

    let (tracked_diff_res, status_res) = tokio::join!(
        run_git_capture_diff(cwd, &["diff", "--color"]),
        run_git_capture_stdout(cwd, &["status", "--short"]),
    );

    let tracked_diff = tracked_diff_res?;
    let status_listing = status_res?;

    let mut output = tracked_diff;
    if !status_listing.trim().is_empty() {
        output.push_str("\n# Working tree summary (git status --short)\n");
        output.push_str(&status_listing);
    }

    Ok((true, output))
}

async fn run_git_capture_stdout(cwd: &Path, args: &[&str]) -> std::io::Result<String> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(std::io::Error::other(format!(
            "git {:?} failed with status {}",
            args, output.status
        )))
    }
}

async fn run_git_capture_diff(cwd: &Path, args: &[&str]) -> std::io::Result<String> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await?;

    if output.status.success() || output.status.code() == Some(1) {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(std::io::Error::other(format!(
            "git {:?} failed with status {}",
            args, output.status
        )))
    }
}

async fn is_inside_git_repo(cwd: &Path) -> std::io::Result<bool> {
    let status = Command::new("git")
        .current_dir(cwd)
        .args(["rev-parse", "--is-inside-work-tree"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    match status {
        Ok(s) if s.success() => Ok(true),
        Ok(_) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn flatten_execution_output(execution: &RunExecution) -> Option<String> {
    let mut parts = Vec::new();
    for message in &execution.output {
        for part in &message.parts {
            match part.content_type.as_str() {
                "text/plain" => {
                    if let Some(text) = part.content.as_str() {
                        parts.push(text.to_string());
                    }
                }
                _ => {
                    parts.push(part.content.to_string());
                }
            }
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn extract_text_prompt(prompt: &[Value]) -> Option<String> {
    for block in prompt {
        let obj = block.as_object()?;
        let block_type = obj.get("type")?.as_str()?;
        if block_type == "text"
            && let Some(text) = obj.get("text").and_then(Value::as_str)
        {
            return Some(text.to_string());
        }
    }
    None
}

async fn send_response(writer: &mut BufWriter<io::Stdout>, id: Value, result: Value) -> Result<()> {
    let message = json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "result": result,
    });
    write_message(writer, message).await
}

async fn send_error(
    writer: &mut BufWriter<io::Stdout>,
    id: Value,
    code: i64,
    message: impl Into<String>,
) -> Result<()> {
    let payload = json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": {
            "code": code,
            "message": message.into(),
        }
    });
    write_message(writer, payload).await
}

async fn send_notification(writer: &mut BufWriter<io::Stdout>, notification: Value) -> Result<()> {
    write_message(writer, notification).await
}

async fn write_message(writer: &mut BufWriter<io::Stdout>, value: Value) -> Result<()> {
    let mut buf = serde_json::to_vec(&value)?;
    buf.push(b'\n');
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}
