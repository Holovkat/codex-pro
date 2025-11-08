// - In the default output mode, it is paramount that the only thing written to
//   stdout is the final message (if any).
// - In --json mode, stdout must be valid JSONL, one event per line.
// For both modes, any other output must be written to stderr.
#![deny(clippy::print_stdout)]

mod cli;
mod event_processor;
mod event_processor_with_human_output;
pub mod event_processor_with_jsonl_output;
pub mod exec_events;

use anyhow::Context;
pub use cli::Cli;
use codex_agentic_core::AgentProfile;
use codex_agentic_core::AgentRunContext;
use codex_agentic_core::AgentRunRecord;
use codex_agentic_core::AgentStore;
use codex_agentic_core::apply_overlay_to_config;
use codex_agentic_core::default_base_prompt;
use codex_agentic_core::global_prompt;
use codex_agentic_core::init_global_prompt;
use codex_agentic_core::init_global_settings;
use codex_agentic_core::load_settings;
use codex_agentic_core::provider::DEFAULT_OPENAI_PROVIDER_ID;
use codex_agentic_core::provider::ResolveModelProviderArgs;
use codex_agentic_core::provider::custom_provider_model_info;
use codex_agentic_core::serialize_agent_log_record;
use codex_core::AuthManager;
use codex_core::ConversationManager;
use codex_core::NewConversation;
use codex_core::auth::enforce_login_restrictions;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_core::config::OPENAI_DEFAULT_MODEL;
use codex_core::get_model_info;
use codex_core::git_info::get_git_repo_root;
use codex_core::model_family::derive_default_model_family;
use codex_core::model_family::find_family_for_model;
use codex_core::protocol::AskForApproval;
use codex_core::protocol::Event;
use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use codex_core::protocol::SessionSource;
use codex_protocol::config_types::SandboxMode;
use codex_protocol::user_input::UserInput;
use event_processor_with_human_output::EventProcessorWithHumanOutput;
use event_processor_with_jsonl_output::EventProcessorWithJsonOutput;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use serde_json::Value;
use std::fs;
use std::fs::OpenOptions;
use std::io::IsTerminal;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use supports_color::Stream;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;
use uuid::Uuid;

const AGENT_PROMPT_PREVIEW_CHARS: usize = 160;

use crate::cli::Command as ExecCommand;
use crate::event_processor::CodexStatus;
use crate::event_processor::EventProcessor;
use codex_core::default_client::set_default_originator;
use codex_core::find_conversation_path_by_id_str;

struct AgentExecutionContext {
    store: AgentStore,
    profile: AgentProfile,
    run: Option<AgentRunRecord>,
    log_path: Option<PathBuf>,
}

pub async fn run_main(cli: Cli, codex_linux_sandbox_exe: Option<PathBuf>) -> anyhow::Result<()> {
    if let Err(err) = set_default_originator("codex_exec".to_string()) {
        tracing::warn!(?err, "Failed to set codex exec originator override {err:?}");
    }

    let Cli {
        command,
        images,
        model: model_cli_arg,
        oss,
        config_profile,
        full_auto,
        dangerously_bypass_approvals_and_sandbox,
        cwd,
        skip_git_repo_check,
        color,
        last_message_file,
        json: json_mode,
        sandbox_mode: sandbox_mode_cli_arg,
        agent,
        prompt_file,
        prompt_json,
        mut enable_tools,
        max_duration_secs,
        max_tool_calls,
        parallel,
        on_complete,
        no_wait,
        prompt,
        output_schema: output_schema_path,
        config_overrides,
    } = cli;

    if no_wait {
        eprintln!("--no-wait is not supported yet in this build.");
        std::process::exit(1);
    }
    if parallel != 1 {
        eprintln!("--parallel currently supports only a value of 1.");
        std::process::exit(1);
    }
    if max_duration_secs.is_some() {
        eprintln!("--max-duration is not supported yet in this build.");
        std::process::exit(1);
    }
    if max_tool_calls.is_some() {
        eprintln!("--max-tool-calls is not supported yet in this build.");
        std::process::exit(1);
    }
    if on_complete.is_some() {
        eprintln!("--on-complete is not supported yet in this build.");
        std::process::exit(1);
    }

    // Determine the prompt source (parent or subcommand) and read from stdin/file as needed.
    let prompt_arg = match &command {
        Some(ExecCommand::Resume(args)) => args.prompt.clone().or(prompt),
        None => prompt,
    };

    let mut prompt = if let Some(path) = prompt_file {
        match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) => {
                eprintln!("Failed to read prompt file {}: {error}", path.display());
                std::process::exit(1);
            }
        }
    } else if let Some(path) = prompt_json {
        match fs::read_to_string(&path) {
            Ok(raw) => {
                if let Err(error) = serde_json::from_str::<serde_json::Value>(&raw) {
                    eprintln!("Invalid JSON prompt in {}: {error}", path.display());
                    std::process::exit(1);
                }
                raw
            }
            Err(error) => {
                eprintln!(
                    "Failed to read JSON prompt file {}: {error}",
                    path.display()
                );
                std::process::exit(1);
            }
        }
    } else {
        match prompt_arg {
            Some(p) if p != "-" => p,
            maybe_dash => {
                let force_stdin = matches!(maybe_dash.as_deref(), Some("-"));

                if std::io::stdin().is_terminal() && !force_stdin {
                    eprintln!(
                        "No prompt provided. Either specify one as an argument or pipe the prompt into stdin."
                    );
                    std::process::exit(1);
                }

                if !force_stdin {
                    eprintln!("Reading prompt from stdin...");
                }
                let mut buffer = String::new();
                if let Err(e) = std::io::stdin().read_to_string(&mut buffer) {
                    eprintln!("Failed to read prompt from stdin: {e}");
                    std::process::exit(1);
                } else if buffer.trim().is_empty() {
                    eprintln!("No prompt provided via stdin.");
                    std::process::exit(1);
                }
                buffer
            }
        }
    };

    let mut agent_context: Option<AgentExecutionContext> = None;
    if let Some(agent_selector) = agent {
        let store = AgentStore::new().context("failed to open agents directory")?;
        let profile = store
            .load_profile_by_selector(&agent_selector)
            .with_context(|| format!("failed to load agent profile '{agent_selector}'"))?;
        if !profile.priming_prompt().is_empty() {
            let priming = profile.priming_prompt();
            if !priming.trim().is_empty() {
                prompt = format!("{priming}\n\n{prompt}");
            }
        }

        for tool in &profile.enabled_tools {
            if !enable_tools.iter().any(|existing| existing == tool) {
                enable_tools.push(tool.clone());
            }
        }

        agent_context = Some(AgentExecutionContext {
            store,
            profile,
            run: None,
            log_path: None,
        });
    }

    let output_schema = load_output_schema(output_schema_path);

    let (stdout_with_ansi, stderr_with_ansi) = match color {
        cli::Color::Always => (true, true),
        cli::Color::Never => (false, false),
        cli::Color::Auto => (
            supports_color::on_cached(Stream::Stdout).is_some(),
            supports_color::on_cached(Stream::Stderr).is_some(),
        ),
    };

    // Build fmt layer (existing logging) to compose with OTEL layer.
    let default_level = "error";

    // Build env_filter separately and attach via with_filter.
    let env_filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(default_level))
        .unwrap_or_else(|_| EnvFilter::new(default_level));

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_ansi(stderr_with_ansi)
        .with_writer(std::io::stderr)
        .with_filter(env_filter);

    let sandbox_mode = if full_auto {
        Some(SandboxMode::WorkspaceWrite)
    } else if dangerously_bypass_approvals_and_sandbox {
        Some(SandboxMode::DangerFullAccess)
    } else {
        sandbox_mode_cli_arg.map(Into::<SandboxMode>::into)
    };

    // Load settings so the CLI respects user defaults when no explicit model is provided.
    let mut settings = init_global_settings(load_settings());
    let overlay_prompt = init_global_prompt(&settings).unwrap_or_else(|_| global_prompt());
    let base_prompt = default_base_prompt();
    let model_cli_provided = model_cli_arg.is_some();

    let resolution = codex_agentic_core::provider::resolve_model_provider(
        ResolveModelProviderArgs::new(&settings)
            .with_model(model_cli_arg.clone())
            .with_force_oss(oss),
    );
    let oss_active = resolution.oss_active;
    let custom_provider_selected = resolution
        .provider_override
        .clone()
        .filter(|id| settings.custom_provider(id).is_some());
    let desired_model = resolution.model.clone();

    let mut overrides = ConfigOverrides {
        model: resolution.model.clone(),
        review_model: None,
        config_profile,
        // This CLI is intended to be headless and has no affordances for asking
        // the user for approval.
        approval_policy: Some(AskForApproval::Never),
        sandbox_mode,
        cwd: cwd.map(|p| p.canonicalize().unwrap_or(p)),
        model_provider: resolution.provider_override.clone(),
        codex_linux_sandbox_exe,
        base_instructions: Some(base_prompt.clone()),
        developer_instructions: None,
        compact_prompt: None,
        include_apply_patch_tool: None,
        show_raw_agent_reasoning: oss_active.then_some(true),
        tools_web_search_request: None,
        experimental_sandbox_command_assessment: None,
        additional_writable_roots: Vec::new(),
    };

    if enable_tools.iter().any(|tool| tool == "web_search_request") {
        overrides.tools_web_search_request = Some(true);
    }

    if custom_provider_selected.is_none() && resolution.provider_override.is_none() {
        overrides.model_provider = Some(DEFAULT_OPENAI_PROVIDER_ID.to_string());
        overrides.model = Some(OPENAI_DEFAULT_MODEL.to_string());
    }

    if model_cli_provided && let Some(model) = resolution.model.clone() {
        if let Ok(updated) = codex_agentic_core::persist_default_model_selection(
            &model,
            resolution.provider_override.as_deref(),
        ) {
            settings = init_global_settings(updated);
        } else {
            tracing::warn!(
                "failed to persist default model selection for CLI override; continuing with in-memory settings"
            );
        }
    }
    // Parse `-c` overrides.
    let cli_kv_overrides = match config_overrides.parse_overrides() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error parsing -c overrides: {e}");
            std::process::exit(1);
        }
    };

    let mut config = Config::load_with_cli_overrides(cli_kv_overrides, overrides).await?;
    codex_agentic_core::merge_custom_providers_into_config(&mut config, &settings);
    if let Some(custom_id) = custom_provider_selected
        && let Some(custom_info) = config.model_providers.get(&custom_id).cloned().or_else(|| {
            settings
                .custom_provider(&custom_id)
                .map(|provider| custom_provider_model_info(&custom_id, provider))
        })
    {
        config
            .model_providers
            .insert(custom_id.clone(), custom_info.clone());
        config.model_provider_id = custom_id.clone();
        config.model_provider = custom_info;
        if let Some(model) = desired_model {
            config.model = model;
        }
    }
    refresh_model_metadata(&mut config);
    codex_agentic_core::provider::sanitize_reasoning_overrides(&mut config);
    codex_agentic_core::provider::sanitize_tool_overrides(&mut config);
    apply_overlay_to_config(&mut config, &overlay_prompt);

    if let Err(err) = enforce_login_restrictions(&config).await {
        eprintln!("{err}");
        std::process::exit(1);
    }

    let otel = codex_core::otel_init::build_provider(&config, env!("CARGO_PKG_VERSION"));

    #[allow(clippy::print_stderr)]
    let otel = match otel {
        Ok(otel) => otel,
        Err(e) => {
            eprintln!("Could not create otel exporter: {e}");
            std::process::exit(1);
        }
    };

    if let Some(provider) = otel.as_ref() {
        let otel_layer = OpenTelemetryTracingBridge::new(&provider.logger).with_filter(
            tracing_subscriber::filter::filter_fn(codex_core::otel_init::codex_export_filter),
        );

        let _ = tracing_subscriber::registry()
            .with(fmt_layer)
            .with(otel_layer)
            .try_init();
    } else {
        let _ = tracing_subscriber::registry().with(fmt_layer).try_init();
    }

    let mut event_processor: Box<dyn EventProcessor> = match json_mode {
        true => Box::new(EventProcessorWithJsonOutput::new(last_message_file.clone())),
        _ => Box::new(EventProcessorWithHumanOutput::create_with_ansi(
            stdout_with_ansi,
            &config,
            last_message_file.clone(),
        )),
    };

    if oss_active {
        codex_ollama::ensure_oss_ready(&config)
            .await
            .map_err(|e| anyhow::anyhow!("OSS setup failed: {e}"))?;
    }

    let default_cwd = config.cwd.to_path_buf();
    let default_approval_policy = config.approval_policy;
    let default_sandbox_policy = config.sandbox_policy.clone();
    let default_model = config.model.clone();
    let default_effort = config.model_reasoning_effort;
    let default_summary = config.model_reasoning_summary;

    if !skip_git_repo_check && get_git_repo_root(&default_cwd).is_none() {
        eprintln!("Not inside a trusted directory and --skip-git-repo-check was not specified.");
        std::process::exit(1);
    }

    let auth_manager = AuthManager::shared(
        config.codex_home.clone(),
        true,
        config.cli_auth_credentials_store_mode,
    );
    let conversation_manager = ConversationManager::new(auth_manager.clone(), SessionSource::Exec);

    // Handle resume subcommand by resolving a rollout path and using explicit resume API.
    let NewConversation {
        conversation_id: _,
        conversation,
        session_configured,
    } = if let Some(ExecCommand::Resume(args)) = command {
        let resume_path = resolve_resume_path(&config, &args).await?;

        if let Some(path) = resume_path {
            conversation_manager
                .resume_conversation_from_rollout(config.clone(), path, auth_manager.clone())
                .await?
        } else {
            conversation_manager
                .new_conversation(config.clone())
                .await?
        }
    } else {
        conversation_manager
            .new_conversation(config.clone())
            .await?
    };
    // Print the effective configuration and prompt so users can see what Codex
    // is using.
    event_processor.print_config_summary(&config, &overlay_prompt, &session_configured);

    info!("Codex initialized with event: {session_configured:?}");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    {
        let conversation = conversation.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        tracing::debug!("Keyboard interrupt");
                        // Immediately notify Codex to abort any in‑flight task.
                        conversation.submit(Op::Interrupt).await.ok();

                        // Exit the inner loop and return to the main input prompt. The codex
                        // will emit a `TurnInterrupted` (Error) event which is drained later.
                        break;
                    }
                    res = conversation.next_event() => match res {
                        Ok(event) => {
                            debug!("Received event: {event:?}");

                            let is_shutdown_complete = matches!(event.msg, EventMsg::ShutdownComplete);
                            if let Err(e) = tx.send(event) {
                                error!("Error sending event: {e:?}");
                                break;
                            }
                            if is_shutdown_complete {
                                info!("Received shutdown event, exiting event loop.");
                                break;
                            }
                        },
                        Err(e) => {
                            error!("Error receiving event: {e:?}");
                            break;
                        }
                    }
                }
            }
        });
    }

    // Package images and prompt into a single user input turn.
    let prompt_for_metadata = prompt.clone();
    let mut items: Vec<UserInput> = images
        .into_iter()
        .map(|path| UserInput::LocalImage { path })
        .collect();
    items.push(UserInput::Text { text: prompt });

    if let Some(ctx) = agent_context.as_mut() {
        let run_id = Uuid::new_v4().to_string();
        let mut run_context: AgentRunContext = (&ctx.profile).into();
        run_context.enabled_tools = enable_tools.clone();
        if dangerously_bypass_approvals_and_sandbox {
            run_context
                .dangerous_flags
                .push("dangerously_bypass_approvals_and_sandbox".to_string());
        }
        let mut record = ctx
            .store
            .begin_run(
                &ctx.profile.slug,
                &run_id,
                Some(prompt_for_metadata.clone()),
                Some(run_context),
            )
            .with_context(|| format!("failed to register run for agent '{}'", ctx.profile.name))?;
        if !enable_tools.is_empty() {
            record.summary = Some(format!("tools: {}", enable_tools.join(",")));
        }
        let log_path = ctx
            .store
            .instance_dir(&ctx.profile.slug, &run_id)
            .join("events.jsonl");
        record.log_path = Some(log_path.clone());
        if let Err(err) = ctx
            .store
            .write_run_record(&ctx.profile.slug, &record.run_id, &record)
        {
            warn!(?err, "failed to update agent run record with log path");
        }
        eprintln!(
            "Launching agent '{}' (run-id: {}).",
            ctx.profile.name, record.run_id
        );
        let prompt_preview = agent_prompt_preview(&prompt_for_metadata);
        log_agent_line(
            &log_path,
            "info",
            &format!("agent: {} ({})", ctx.profile.name, ctx.profile.slug),
        );
        if !ctx.profile.default_command.is_empty() {
            log_agent_line(
                &log_path,
                "info",
                &format!(
                    "defaults.command: {}",
                    ctx.profile.default_command.join(" ")
                ),
            );
        }
        if !ctx.profile.enabled_tools.is_empty() {
            log_agent_line(
                &log_path,
                "info",
                &format!("defaults.tools: {}", ctx.profile.enabled_tools.join(", ")),
            );
        }
        if !enable_tools.is_empty() {
            log_agent_line(
                &log_path,
                "info",
                &format!("cli.tools: {}", enable_tools.join(", ")),
            );
        }
        if dangerously_bypass_approvals_and_sandbox {
            log_agent_line(
                &log_path,
                "info",
                "dangerous_flags: dangerously_bypass_approvals_and_sandbox",
            );
        }
        if !prompt_preview.is_empty() {
            log_agent_line(&log_path, "info", &format!("prompt: {prompt_preview}"));
        }
        ctx.log_path = Some(log_path);
        ctx.run = Some(record);
    }

    let initial_prompt_task_id = conversation
        .submit(Op::UserTurn {
            items,
            cwd: default_cwd,
            approval_policy: default_approval_policy,
            sandbox_policy: default_sandbox_policy,
            model: default_model,
            effort: default_effort,
            summary: default_summary,
            final_output_json_schema: output_schema,
        })
        .await?;
    info!("Sent prompt with event ID: {initial_prompt_task_id}");

    // Run the loop until the task is complete.
    // Track whether a fatal error was reported by the server so we can
    // exit with a non-zero status for automation-friendly signaling.
    let mut error_seen = false;
    while let Some(event) = rx.recv().await {
        if matches!(event.msg, EventMsg::Error(_)) {
            error_seen = true;
        }
        let shutdown: CodexStatus = event_processor.process_event(event);
        match shutdown {
            CodexStatus::Running => continue,
            CodexStatus::InitiateShutdown => {
                conversation.submit(Op::Shutdown).await?;
            }
            CodexStatus::Shutdown => {
                break;
            }
        }
    }
    event_processor.print_final_output();

    if let Some(ctx) = agent_context.as_mut()
        && let Some(record) = ctx.run.take()
    {
        let exit_code = if error_seen { Some(1) } else { Some(0) };
        if let Some(log_path) = ctx.log_path.as_ref() {
            let status_line = if error_seen {
                exit_code
                    .map(|code| format!("status: failed (exit {code})"))
                    .unwrap_or_else(|| "status: failed".to_string())
            } else {
                "status: completed".to_string()
            };
            log_agent_line(log_path, "info", &status_line);
        }
        if let Err(err) =
            ctx.store
                .complete_run(&ctx.profile.slug, &record.run_id, exit_code, error_seen)
        {
            tracing::warn!(?err, "failed to finalize agent run {}", record.run_id);
        }
    }

    if error_seen {
        std::process::exit(1);
    }

    Ok(())
}

fn refresh_model_metadata(config: &mut Config) {
    let family = find_family_for_model(&config.model)
        .unwrap_or_else(|| derive_default_model_family(&config.model));
    config.model_family = family;

    if let Some(info) = get_model_info(&config.model_family) {
        config.model_context_window = Some(info.context_window());
        config.model_max_output_tokens = Some(info.max_output_tokens());
        config.model_auto_compact_token_limit = info.auto_compact_token_limit;
    } else {
        config.model_context_window = None;
        config.model_max_output_tokens = None;
        config.model_auto_compact_token_limit = None;
    }
}

fn agent_prompt_preview(prompt: &str) -> String {
    let mut normalized = prompt.replace(['\n', '\r'], " ");
    if normalized.len() > AGENT_PROMPT_PREVIEW_CHARS {
        normalized.truncate(AGENT_PROMPT_PREVIEW_CHARS);
        normalized.push('…');
    }
    normalized
}

fn log_agent_line(path: &Path, stream: &str, line: &str) {
    if let Err(err) = write_agent_log_line(path, stream, line) {
        warn!(?err, "failed to append agent log line");
    }
}

fn write_agent_log_line(path: &Path, stream: &str, line: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("failed to create agent log directory {}", parent.display())
        })?;
    }
    let serialized = serialize_agent_log_record(stream, line)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open agent log {}", path.display()))?;
    file.write_all(serialized.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

async fn resolve_resume_path(
    config: &Config,
    args: &crate::cli::ResumeArgs,
) -> anyhow::Result<Option<PathBuf>> {
    if args.last {
        let default_provider_filter = vec![config.model_provider_id.clone()];
        match codex_core::RolloutRecorder::list_conversations(
            &config.codex_home,
            1,
            None,
            &[],
            Some(default_provider_filter.as_slice()),
            &config.model_provider_id,
        )
        .await
        {
            Ok(page) => Ok(page.items.first().map(|it| it.path.clone())),
            Err(e) => {
                error!("Error listing conversations: {e}");
                Ok(None)
            }
        }
    } else if let Some(id_str) = args.session_id.as_deref() {
        let path = find_conversation_path_by_id_str(&config.codex_home, id_str).await?;
        Ok(path)
    } else {
        Ok(None)
    }
}

fn load_output_schema(path: Option<PathBuf>) -> Option<Value> {
    let path = path?;

    let schema_str = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) => {
            eprintln!(
                "Failed to read output schema file {}: {err}",
                path.display()
            );
            std::process::exit(1);
        }
    };

    match serde_json::from_str::<Value>(&schema_str) {
        Ok(value) => Some(value),
        Err(err) => {
            eprintln!(
                "Output schema file {} is not valid JSON: {err}",
                path.display()
            );
            std::process::exit(1);
        }
    }
}
