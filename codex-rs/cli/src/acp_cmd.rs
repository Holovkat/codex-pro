use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use clap::ArgAction;
use clap::Parser;
use codex_agentic_core::CommandContext;
use codex_agentic_core::CommandRegistry;
use codex_agentic_core::acp::RuntimeOptions;
use codex_agentic_core::acp::render_status_card;
use codex_agentic_core::acp::{self};
use codex_agentic_core::init_global_prompt;
use codex_agentic_core::init_global_settings;
use codex_agentic_core::merge_custom_providers_into_config;
use codex_agentic_core::provider::ResolveModelProviderArgs;
use codex_agentic_core::provider::plan_tool_supported;
use codex_agentic_core::provider::resolve_model_provider;
use codex_agentic_core::settings::Acp as AcpSettings;
use codex_agentic_core::settings::Model as ModelSettings;
use codex_agentic_core::settings::Settings;
use codex_common::CliConfigOverrides;
use codex_core::AuthManager;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_core::get_model_info;
use codex_core::model_family::derive_default_model_family;
use codex_core::model_family::find_family_for_model;
use codex_core::protocol::SessionSource;

#[derive(Debug, Parser)]
pub struct AcpCli {
    #[clap(flatten)]
    pub config_overrides: CliConfigOverrides,

    /// Override the working directory that ACP commands operate within.
    #[clap(long = "cd", short = 'C', value_name = "DIR")]
    pub cwd: Option<PathBuf>,

    /// Preferred default model for ACP sessions.
    #[arg(long, short = 'm')]
    pub model: Option<String>,

    /// Preferred model provider for ACP sessions.
    #[arg(long)]
    pub provider: Option<String>,

    /// Reasoning view override applied to ACP sessions.
    #[arg(long = "reasoning-view")]
    pub reasoning_view: Option<String>,

    /// Enable web search tooling without requiring approvals (dangerous).
    #[arg(long = "yolo-with-search", action = ArgAction::SetTrue)]
    pub yolo_with_search: bool,

    /// Agent name surfaced to ACP clients.
    #[arg(long = "name", default_value = "codex-agentic")]
    pub agent_name: String,

    /// Expose the ACP HTTP surface alongside stdio transport.
    #[arg(long = "http")]
    pub enable_http: bool,

    /// Address to bind when HTTP is enabled.
    #[arg(long = "listen", requires = "enable_http", value_name = "ADDR")]
    pub listen: Option<String>,

    /// Public URL advertised in discovery metadata.
    #[arg(long = "public-url", requires = "enable_http", value_name = "URL")]
    pub public_url: Option<String>,
}

pub async fn run(
    cli: AcpCli,
    _codex_linux_sandbox_exe: Option<PathBuf>,
    registry: Arc<CommandRegistry>,
    mut command_ctx: CommandContext,
) -> Result<()> {
    let parsed_overrides = cli
        .config_overrides
        .parse_overrides()
        .map_err(|err| anyhow!("invalid -c/--config override: {err}"))?;

    let mut settings = command_ctx.settings.clone();
    apply_model_overrides(&mut settings, &cli);
    apply_acp_overrides(&mut settings, &cli);

    let resolution = resolve_model_provider(
        ResolveModelProviderArgs::new(&settings).with_model(cli.model.clone()),
    );

    let mut config_overrides = ConfigOverrides {
        model: resolution.model.clone(),
        model_provider: resolution.provider_override.clone(),
        include_plan_tool: Some(resolution.include_plan_tool),
        ..ConfigOverrides::default()
    };

    if (cli.model.is_some() || cli.provider.is_some())
        && let Some(model) = resolution.model.clone()
    {
        if let Ok(updated) = codex_agentic_core::persist_default_model_selection(
            &model,
            resolution.provider_override.as_deref(),
        ) {
            settings = updated;
        } else {
            eprintln!(
                "warning: failed to persist default model selection for CLI override; continuing with in-memory settings"
            );
        }
    }

    // Persist overrides to the global singleton so downstream helpers see them.
    init_global_settings(settings.clone());
    command_ctx.settings = settings.clone();

    if let Some(cwd) = cli.cwd.clone() {
        config_overrides.cwd = Some(cwd);
    }

    let mut config = Config::load_with_cli_overrides(parsed_overrides, config_overrides)
        .await
        .map_err(|err| anyhow!("failed to load config: {err}"))?;
    merge_custom_providers_into_config(&mut config, &command_ctx.settings);

    if let Some(model) = resolution.model.clone() {
        config.model = model;
    }

    if let Some(provider_id) = resolution.provider_override.clone() {
        if let Some(provider_info) = config.model_providers.get(&provider_id).cloned() {
            config.model_provider_id = provider_id;
            config.model_provider = provider_info;
        } else {
            eprintln!(
                "warning: resolved provider {provider_id} missing after BYOK merge; keeping {}",
                config.model_provider_id
            );
        }
    }
    config.include_plan_tool = resolution.include_plan_tool;

    refresh_model_metadata(&mut config);
    codex_agentic_core::provider::sanitize_reasoning_overrides(&mut config);
    codex_agentic_core::provider::sanitize_tool_overrides(&mut config);

    if let Err(err) = init_global_prompt(&command_ctx.settings) {
        eprintln!("warning: failed to load system prompt overlay while starting ACP: {err}");
    }

    let agent_name = cli.agent_name.clone();
    command_ctx = command_ctx.with_binary_name(agent_name.clone());

    if let Some(cwd) = cli.cwd {
        command_ctx = command_ctx.with_working_dir(cwd.to_string_lossy().into_owned());
    }

    let initial_status = build_status_summary(&config, &command_ctx);
    eprintln!("ACP stdio ready. Initial status:\n{initial_status}");
    let default_cwd = command_ctx
        .working_dir
        .clone()
        .unwrap_or_else(|| config.cwd.display().to_string());
    eprintln!("Handshake steps:");
    eprintln!(
        "  1) initialize  : {{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1}}}}"
    );
    eprintln!(
        "  2) session/new : {{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/new\",\"params\":{{\"cwd\":\"{default_cwd}\"}}}}"
    );
    eprintln!(
        "  3) session/prompt (after you get sessionId): {{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"<sessionId>\",\"prompt\":[{{\"type\":\"text\",\"text\":\"/help-recipes\"}}]}}}}"
    );

    let runtime_options = RuntimeOptions {
        agent_name,
        enable_http: cli.enable_http,
        listen: cli
            .listen
            .clone()
            .unwrap_or_else(|| "127.0.0.1:8000".to_string()),
        public_url: cli.public_url.clone(),
        initial_status: Some(initial_status),
        base_config: config.clone(),
        auth_manager: AuthManager::shared(config.codex_home.clone(), true),
        session_source: SessionSource::Cli,
    };

    if cli.enable_http {
        acp::run_http(runtime_options, registry, command_ctx).await
    } else {
        acp::run_stdio(runtime_options, registry, command_ctx).await
    }
}

fn apply_model_overrides(settings: &mut Settings, cli: &AcpCli) {
    if cli.model.is_none() && cli.provider.is_none() && cli.reasoning_view.is_none() {
        return;
    }

    let model = settings.model.get_or_insert_with(ModelSettings::default);

    if let Some(value) = &cli.model {
        model.default = Some(value.clone());
    }
    if let Some(value) = &cli.provider {
        model.provider = Some(value.clone());
    }
    if let Some(value) = &cli.reasoning_view {
        model.reasoning_view = Some(value.clone());
    }
}

fn apply_acp_overrides(settings: &mut Settings, cli: &AcpCli) {
    if !cli.yolo_with_search {
        return;
    }

    let acp = settings.acp.get_or_insert_with(AcpSettings::default);
    acp.yolo_with_search = Some(true);
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

    let model_slug = if config.model.is_empty() {
        None
    } else {
        Some(config.model.as_str())
    };
    config.include_plan_tool = plan_tool_supported(config.model_provider_id.as_str(), model_slug);
}

fn build_status_summary(config: &Config, ctx: &CommandContext) -> String {
    render_status_card(config, ctx, None, None, None, None)
}
