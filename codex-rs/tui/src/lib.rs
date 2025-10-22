// Forbid accidental stdout/stderr writes in the *library* portion of the TUI.
// The standalone `codex-tui` binary prints a short help message before the
// alternate‑screen mode starts; that file opts‑out locally via `allow`.
#![deny(clippy::print_stdout, clippy::print_stderr)]
#![deny(clippy::disallowed_methods)]
use crate::chatwidget::refresh_model_metadata;
use app::App;
pub use app::AppExitInfo;
use codex_agentic_core::apply_overlay_to_config;
use codex_agentic_core::default_base_prompt;
use codex_agentic_core::global_prompt;
use codex_agentic_core::init_global_prompt;
use codex_agentic_core::init_global_settings;
use codex_agentic_core::load_settings;
use codex_agentic_core::provider::DEFAULT_OPENAI_PROVIDER_ID;
use codex_agentic_core::provider::custom_provider_model_info;
use codex_app_server_protocol::AuthMode;
use codex_core::AuthManager;
use codex_core::CodexAuth;
use codex_core::INTERACTIVE_SESSION_SOURCES;
use codex_core::RolloutRecorder;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_core::config::ConfigToml;
use codex_core::config::OPENAI_DEFAULT_MODEL;
use codex_core::config::find_codex_home;
use codex_core::config::load_config_as_toml_with_cli_overrides;
use codex_core::find_conversation_path_by_id_str;
use codex_core::protocol::AskForApproval;
use codex_core::protocol::SandboxPolicy;
use codex_protocol::config_types::SandboxMode;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use std::fs::OpenOptions;
use std::path::PathBuf;
use tracing::error;
use tracing_appender::non_blocking;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::prelude::*;

mod app;
mod app_backtrack;
mod app_event;
mod app_event_sender;
mod ascii_animation;
mod bottom_pane;
mod chatwidget;
mod citation_regex;
mod cli;
mod clipboard_paste;
mod color;
pub mod custom_terminal;
mod diff_render;
mod exec_cell;
mod exec_command;
mod file_search;
mod frames;
mod get_git_diff;
mod history_cell;
mod index_delta;
mod index_status;
mod index_worker;
pub mod insert_history;
mod key_hint;
pub mod live_wrap;
mod markdown;
mod markdown_render;
mod markdown_stream;
pub mod onboarding;
mod pager_overlay;
pub mod public_widgets;
mod render;
mod resume_picker;
mod session_log;
mod shimmer;
mod slash_command;
mod status;
mod status_indicator_widget;
mod streaming;
mod style;
mod terminal_palette;
mod text_formatting;
mod tui;
mod ui_consts;
mod version;
mod wrapping;

#[cfg(test)]
pub mod test_backend;

#[cfg(not(debug_assertions))]
mod updates;

use crate::onboarding::TrustDirectorySelection;
use crate::onboarding::WSL_INSTRUCTIONS;
use crate::onboarding::onboarding_screen::OnboardingScreenArgs;
use crate::onboarding::onboarding_screen::run_onboarding_app;
use crate::tui::Tui;
pub use cli::Cli;
pub use markdown_render::render_markdown_text;
pub use public_widgets::composer_input::ComposerAction;
pub use public_widgets::composer_input::ComposerInput;
use std::io::Write as _;

// (tests access modules directly within the crate)

pub async fn run_main(
    cli: Cli,
    codex_linux_sandbox_exe: Option<PathBuf>,
) -> std::io::Result<AppExitInfo> {
    let mut settings = init_global_settings(load_settings());
    let overlay_prompt = init_global_prompt(&settings).unwrap_or_else(|_| global_prompt());
    let base_prompt = default_base_prompt();
    let (sandbox_mode, approval_policy) = if cli.full_auto {
        (
            Some(SandboxMode::WorkspaceWrite),
            Some(AskForApproval::OnRequest),
        )
    } else if cli.dangerously_bypass_approvals_and_sandbox {
        (
            Some(SandboxMode::DangerFullAccess),
            Some(AskForApproval::Never),
        )
    } else {
        (
            cli.sandbox_mode.map(Into::<SandboxMode>::into),
            cli.approval_policy.map(Into::into),
        )
    };

    let resolution = codex_agentic_core::provider::resolve_model_provider(
        codex_agentic_core::provider::ResolveModelProviderArgs::new(&settings)
            .with_model(cli.model.clone())
            .with_force_oss(cli.oss),
    );
    let oss_active = resolution.oss_active;

    if (cli.model.is_some() || cli.oss)
        && let Some(model) = resolution.model.clone()
    {
        if let Ok(updated_settings) = codex_agentic_core::persist_default_model_selection(
            &model,
            resolution.provider_override.as_deref(),
        ) {
            settings = init_global_settings(updated_settings);
        } else {
            tracing::warn!(
                "failed to persist default model selection for CLI override; continuing with in-memory settings"
            );
        }
    }

    // canonicalize the cwd
    let cwd = cli.cwd.clone().map(|p| p.canonicalize().unwrap_or(p));

    let custom_provider_selected = resolution
        .provider_override
        .clone()
        .filter(|id| settings.custom_provider(id).is_some());
    let desired_model = resolution.model.clone();

    let mut overrides = ConfigOverrides {
        model: resolution.model.clone(),
        review_model: None,
        approval_policy,
        sandbox_mode,
        cwd,
        model_provider: resolution.provider_override.clone(),
        config_profile: cli.config_profile.clone(),
        codex_linux_sandbox_exe,
        base_instructions: Some(base_prompt.clone()),
        include_plan_tool: Some(resolution.include_plan_tool),
        include_apply_patch_tool: None,
        include_view_image_tool: None,
        show_raw_agent_reasoning: oss_active.then_some(true),
        tools_web_search_request: cli.web_search.then_some(true),
    };

    if custom_provider_selected.is_none() && resolution.provider_override.is_none() {
        overrides.model_provider = Some(DEFAULT_OPENAI_PROVIDER_ID.to_string());
        overrides.model = Some(OPENAI_DEFAULT_MODEL.to_string());
    }
    let raw_overrides = cli.config_overrides.raw_overrides.clone();
    let overrides_cli = codex_common::CliConfigOverrides { raw_overrides };
    let cli_kv_overrides = match overrides_cli.parse_overrides() {
        Ok(v) => v,
        #[allow(clippy::print_stderr)]
        Err(e) => {
            eprintln!("Error parsing -c overrides: {e}");
            std::process::exit(1);
        }
    };

    let mut config = {
        // Load configuration and support CLI overrides.

        #[allow(clippy::print_stderr)]
        match Config::load_with_cli_overrides(cli_kv_overrides.clone(), overrides).await {
            Ok(config) => config,
            Err(err) => {
                eprintln!("Error loading configuration: {err}");
                std::process::exit(1);
            }
        }
    };

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

    // we load config.toml here to determine project state.
    #[allow(clippy::print_stderr)]
    let config_toml = {
        let codex_home = match find_codex_home() {
            Ok(codex_home) => codex_home,
            Err(err) => {
                eprintln!("Error finding codex home: {err}");
                std::process::exit(1);
            }
        };

        match load_config_as_toml_with_cli_overrides(&codex_home, cli_kv_overrides).await {
            Ok(config_toml) => config_toml,
            Err(err) => {
                eprintln!("Error loading config.toml: {err}");
                std::process::exit(1);
            }
        }
    };

    let cli_profile_override = cli.config_profile.clone();
    let active_profile = cli_profile_override
        .clone()
        .or_else(|| config_toml.profile.clone());

    let should_show_trust_screen = determine_repo_trust_state(
        &mut config,
        &config_toml,
        approval_policy,
        sandbox_mode,
        cli_profile_override,
    )?;

    let log_dir = codex_core::config::log_dir(&config)?;
    std::fs::create_dir_all(&log_dir)?;
    // Open (or create) your log file, appending to it.
    let mut log_file_opts = OpenOptions::new();
    log_file_opts.create(true).append(true);

    // Ensure the file is only readable and writable by the current user.
    // Doing the equivalent to `chmod 600` on Windows is quite a bit more code
    // and requires the Windows API crates, so we can reconsider that when
    // Codex CLI is officially supported on Windows.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        log_file_opts.mode(0o600);
    }

    let log_file = log_file_opts.open(log_dir.join("codex-tui.log"))?;

    // Wrap file in non‑blocking writer.
    let (non_blocking, _guard) = non_blocking(log_file);

    // use RUST_LOG env var, default to info for codex crates.
    let env_filter = || {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("codex_core=info,codex_tui=info,codex_rmcp_client=info")
        })
    };

    // Build layered subscriber:
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_target(false)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .with_filter(env_filter());
    let feedback = codex_feedback::CodexFeedback::new();
    let targets = Targets::new().with_default(tracing::Level::TRACE);
    let feedback_layer = tracing_subscriber::fmt::layer()
        .with_writer(feedback.make_writer())
        .with_ansi(false)
        .with_target(false)
        .with_filter(targets);

    if oss_active {
        codex_ollama::ensure_oss_ready(&config)
            .await
            .map_err(|e| std::io::Error::other(format!("OSS setup failed: {e}")))?;
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

    let subscriber = tracing_subscriber::registry()
        .with(file_layer)
        .with(feedback_layer);

    match (otel.as_ref(), subscriber) {
        (Some(provider), subscriber) => {
            let otel_layer = OpenTelemetryTracingBridge::new(&provider.logger).with_filter(
                tracing_subscriber::filter::filter_fn(codex_core::otel_init::codex_export_filter),
            );
            let _ = subscriber.with(otel_layer).try_init();
        }
        (None, subscriber) => {
            let _ = subscriber.try_init();
        }
    };

    run_ratatui_app(
        cli,
        config,
        active_profile,
        should_show_trust_screen,
        feedback,
    )
    .await
    .map_err(|err| std::io::Error::other(err.to_string()))
}

async fn run_ratatui_app(
    cli: Cli,
    config: Config,
    active_profile: Option<String>,
    should_show_trust_screen: bool,
    feedback: codex_feedback::CodexFeedback,
) -> color_eyre::Result<AppExitInfo> {
    let mut config = config;
    color_eyre::install()?;

    // Forward panic reports through tracing so they appear in the UI status
    // line, but do not swallow the default/color-eyre panic handler.
    // Chain to the previous hook so users still get a rich panic report
    // (including backtraces) after we restore the terminal.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        prev_hook(info);
    }));
    let mut terminal = tui::init()?;
    terminal.clear()?;

    let mut tui = Tui::new(terminal);

    // Show update banner in terminal history (instead of stderr) so it is visible
    // within the TUI scrollback. Building spans keeps styling consistent.
    #[cfg(not(debug_assertions))]
    let update_config =
        codex_agentic_core::updates::from_settings(&codex_agentic_core::global_settings());

    #[cfg(not(debug_assertions))]
    if let Some(upgrade) = updates::get_upgrade_version(&config, &update_config) {
        use crate::history_cell::padded_emoji;
        use crate::history_cell::with_border_with_inner_width;
        use ratatui::style::Stylize as _;
        use ratatui::text::Line;
        use ratatui::text::Span;

        let current_version = env!("CARGO_PKG_VERSION");
        let exe = std::env::current_exe()?;
        let managed_by_npm = std::env::var_os("CODEX_MANAGED_BY_NPM").is_some();
        let release_url = upgrade
            .release_url
            .clone()
            .unwrap_or_else(|| "https://github.com/openai/codex/releases/latest".to_string());

        let mut content_lines: Vec<Line<'static>> = vec![
            Line::from(vec![
                padded_emoji("✨").bold().cyan(),
                "Update available!".bold().cyan(),
                " ".into(),
                format!("{current_version} -> {}.", upgrade.latest_version).bold(),
            ]),
            Line::from(""),
            Line::from("See full release notes:"),
            Line::from(""),
            Line::from(Span::raw(release_url.clone()).cyan().underlined()),
            Line::from(""),
        ];

        if let Some(custom_cmd) = upgrade.upgrade_cmd.as_deref() {
            content_lines.push(Line::from(vec![
                "Run ".into(),
                Span::raw(custom_cmd.to_owned()).cyan(),
                " to update.".into(),
            ]));
        } else if managed_by_npm {
            let npm_cmd = "npm install -g @openai/codex@latest";
            content_lines.push(Line::from(vec![
                "Run ".into(),
                npm_cmd.cyan(),
                " to update.".into(),
            ]));
        } else if cfg!(target_os = "macos")
            && (exe.starts_with("/opt/homebrew") || exe.starts_with("/usr/local"))
        {
            let brew_cmd = "brew upgrade codex";
            content_lines.push(Line::from(vec![
                "Run ".into(),
                brew_cmd.cyan(),
                " to update.".into(),
            ]));
        } else {
            content_lines.push(Line::from(vec![
                "See ".into(),
                Span::raw(release_url.clone()).cyan().underlined(),
                " for installation options.".into(),
            ]));
        }

        let viewport_width = tui.terminal.viewport_area.width as usize;
        let inner_width = viewport_width.saturating_sub(4).max(1);
        let mut lines = with_border_with_inner_width(content_lines, inner_width);
        lines.push("".into());
        tui.insert_history_lines(lines);
    }

    // Initialize high-fidelity session event logging if enabled.
    session_log::maybe_init(&config);

    let auth_manager = AuthManager::shared(config.codex_home.clone(), false);
    let login_status = get_login_status(&config);
    let should_show_windows_wsl_screen =
        cfg!(target_os = "windows") && !config.windows_wsl_setup_acknowledged;
    let should_show_onboarding = should_show_onboarding(
        login_status,
        &config,
        should_show_trust_screen,
        should_show_windows_wsl_screen,
    );
    if should_show_onboarding {
        let onboarding_result = run_onboarding_app(
            OnboardingScreenArgs {
                show_windows_wsl_screen: should_show_windows_wsl_screen,
                show_login_screen: should_show_login_screen(login_status, &config),
                show_trust_screen: should_show_trust_screen,
                login_status,
                auth_manager: auth_manager.clone(),
                config: config.clone(),
            },
            &mut tui,
        )
        .await?;
        if onboarding_result.windows_install_selected {
            restore();
            session_log::log_session_end();
            let _ = tui.terminal.clear();
            if let Err(err) = writeln!(std::io::stdout(), "{WSL_INSTRUCTIONS}") {
                tracing::error!("Failed to write WSL instructions: {err}");
            }
            return Ok(AppExitInfo {
                token_usage: codex_core::protocol::TokenUsage::default(),
                conversation_id: None,
                update_action: None,
            });
        }
        if should_show_windows_wsl_screen {
            config.windows_wsl_setup_acknowledged = true;
        }
        if let Some(TrustDirectorySelection::Trust) = onboarding_result.directory_trust_decision {
            config.approval_policy = AskForApproval::OnRequest;
            config.sandbox_policy = SandboxPolicy::new_workspace_write_policy();
        }
    }

    // Determine resume behavior: explicit id, then resume last, then picker.
    let resume_selection = if let Some(id_str) = cli.resume_session_id.as_deref() {
        match find_conversation_path_by_id_str(&config.codex_home, id_str).await? {
            Some(path) => resume_picker::ResumeSelection::Resume(path),
            None => {
                error!("Error finding conversation path: {id_str}");
                resume_picker::ResumeSelection::StartFresh
            }
        }
    } else if cli.resume_last {
        match RolloutRecorder::list_conversations(
            &config.codex_home,
            1,
            None,
            INTERACTIVE_SESSION_SOURCES,
        )
        .await
        {
            Ok(page) => page
                .items
                .first()
                .map(|it| resume_picker::ResumeSelection::Resume(it.path.clone()))
                .unwrap_or(resume_picker::ResumeSelection::StartFresh),
            Err(_) => resume_picker::ResumeSelection::StartFresh,
        }
    } else if cli.resume_picker {
        match resume_picker::run_resume_picker(&mut tui, &config.codex_home).await? {
            resume_picker::ResumeSelection::Exit => {
                restore();
                session_log::log_session_end();
                return Ok(AppExitInfo {
                    token_usage: codex_core::protocol::TokenUsage::default(),
                    conversation_id: None,
                    update_action: None,
                });
            }
            other => other,
        }
    } else {
        resume_picker::ResumeSelection::StartFresh
    };

    let Cli { prompt, images, .. } = cli;

    let app_result = App::run(
        &mut tui,
        auth_manager,
        config,
        active_profile,
        prompt,
        images,
        resume_selection,
        feedback,
    )
    .await;

    restore();
    // Mark the end of the recorded session.
    session_log::log_session_end();
    // ignore error when collecting usage – report underlying error instead
    app_result
}

#[expect(
    clippy::print_stderr,
    reason = "TUI should no longer be displayed, so we can write to stderr."
)]
fn restore() {
    if let Err(err) = tui::restore() {
        eprintln!(
            "failed to restore terminal. Run `reset` or restart your terminal to recover: {err}"
        );
    }
}

/// Get the update action from the environment.
/// Returns `None` if not managed by npm, bun, or brew.
#[cfg(not(debug_assertions))]
pub(crate) fn get_update_action() -> Option<UpdateAction> {
    let exe = std::env::current_exe().unwrap_or_default();
    let managed_by_npm = std::env::var_os("CODEX_MANAGED_BY_NPM").is_some();
    let managed_by_bun = std::env::var_os("CODEX_MANAGED_BY_BUN").is_some();
    if managed_by_npm {
        Some(UpdateAction::NpmGlobalLatest)
    } else if managed_by_bun {
        Some(UpdateAction::BunGlobalLatest)
    } else if cfg!(target_os = "macos")
        && (exe.starts_with("/opt/homebrew") || exe.starts_with("/usr/local"))
    {
        Some(UpdateAction::BrewUpgrade)
    } else {
        None
    }
}

#[cfg(debug_assertions)]
pub(crate) fn get_update_action() -> Option<UpdateAction> {
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    NpmGlobalLatest,
    BunGlobalLatest,
    BrewUpgrade,
}

impl UpdateAction {
    pub fn command_args(&self) -> (&'static str, &'static [&'static str]) {
        match self {
            UpdateAction::NpmGlobalLatest => ("npm", &["install", "-g", "@openai/codex@latest"]),
            UpdateAction::BunGlobalLatest => ("bun", &["install", "-g", "@openai/codex@latest"]),
            UpdateAction::BrewUpgrade => ("brew", &["upgrade", "codex"]),
        }
    }

    pub fn command_str(&self) -> String {
        let (cmd, args) = self.command_args();
        format!("{} {}", cmd, args.join(" "))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginStatus {
    AuthMode(AuthMode),
    NotAuthenticated,
}

fn get_login_status(config: &Config) -> LoginStatus {
    if config.model_provider.requires_openai_auth {
        // Reading the OpenAI API key is an async operation because it may need
        // to refresh the token. Block on it.
        let codex_home = config.codex_home.clone();
        match CodexAuth::from_codex_home(&codex_home) {
            Ok(Some(auth)) => LoginStatus::AuthMode(auth.mode),
            Ok(None) => LoginStatus::NotAuthenticated,
            Err(err) => {
                error!("Failed to read auth.json: {err}");
                LoginStatus::NotAuthenticated
            }
        }
    } else {
        LoginStatus::NotAuthenticated
    }
}

/// Determine if user has configured a sandbox / approval policy,
/// or if the current cwd project is trusted, and updates the config
/// accordingly.
fn determine_repo_trust_state(
    config: &mut Config,
    config_toml: &ConfigToml,
    approval_policy_overide: Option<AskForApproval>,
    sandbox_mode_override: Option<SandboxMode>,
    config_profile_override: Option<String>,
) -> std::io::Result<bool> {
    let config_profile = config_toml.get_config_profile(config_profile_override)?;

    if approval_policy_overide.is_some() || sandbox_mode_override.is_some() {
        // if the user has overridden either approval policy or sandbox mode,
        // skip the trust flow
        Ok(false)
    } else if config_profile.approval_policy.is_some() {
        // if the user has specified settings in a config profile, skip the trust flow
        // todo: profile sandbox mode?
        Ok(false)
    } else if config_toml.approval_policy.is_some() || config_toml.sandbox_mode.is_some() {
        // if the user has specified either approval policy or sandbox mode in config.toml
        // skip the trust flow
        Ok(false)
    } else if config.active_project.is_trusted() {
        // if the current project is trusted and no config has been set
        // skip the trust flow and set the approval policy and sandbox mode
        config.approval_policy = AskForApproval::OnRequest;
        config.sandbox_policy = SandboxPolicy::new_workspace_write_policy();
        Ok(false)
    } else {
        // if none of the above conditions are met, show the trust screen
        Ok(true)
    }
}

fn should_show_onboarding(
    login_status: LoginStatus,
    config: &Config,
    show_trust_screen: bool,
    show_windows_wsl_screen: bool,
) -> bool {
    if show_windows_wsl_screen {
        return true;
    }

    if show_trust_screen {
        return true;
    }

    should_show_login_screen(login_status, config)
}

fn should_show_login_screen(login_status: LoginStatus, config: &Config) -> bool {
    // Only show the login screen for providers that actually require OpenAI auth
    // (OpenAI or equivalents). For OSS/other providers, skip login entirely.
    if !config.model_provider.requires_openai_auth {
        return false;
    }

    login_status == LoginStatus::NotAuthenticated
}
