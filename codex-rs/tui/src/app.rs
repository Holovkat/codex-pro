use crate::UpdateAction;
use crate::app_backtrack::BacktrackState;
use crate::app_event::AppEvent;
use crate::app_event::ByokDraftField;
use crate::app_event::CustomProviderForm;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::ApprovalRequest;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::custom_prompt_view::CustomPromptView;
use crate::bottom_pane::custom_prompt_view::PromptSubmitted;
use crate::chatwidget::ChatWidget;
use crate::chatwidget::refresh_model_metadata;
use crate::diff_render::DiffSummary;
use crate::exec_command::strip_bash_lc_and_escape;
use crate::file_search::FileSearchManager;
use crate::history_cell::HistoryCell;
use crate::history_cell::MemorySuggestionEntry;
#[cfg(not(debug_assertions))]
use crate::history_cell::UpdateAvailableHistoryCell;
use crate::index_delta::SnapshotDiff;
use crate::index_delta::spawn_delta_monitor;
use crate::index_status::IndexStatusSnapshot;
use crate::index_status::format_age;
use crate::index_worker::IndexWorker;
use crate::memory_manager::run_memory_manager;
use crate::pager_overlay::Overlay;
use crate::render::highlight::highlight_bash_to_lines;
use crate::resume_picker::ResumeSelection;
use crate::tui;
use crate::tui::TuiEvent;
use anyhow::Error as AnyError;
use chrono::DateTime;
use chrono::Utc;
use codex_agentic_core::CustomProvider;
use codex_agentic_core::DEFAULT_SEARCH_CONFIDENCE_MIN;
use codex_agentic_core::fetch_custom_provider_models;
use codex_agentic_core::index::builder::BuildOptions;
use codex_agentic_core::index::events::IndexEvent as CoreIndexEvent;
use codex_agentic_core::index::query::QueryHit;
use codex_agentic_core::index::query::query_index;
use codex_agentic_core::persist_default_model_selection;
use codex_agentic_core::provider::DEFAULT_OLLAMA_ENDPOINT;
use codex_agentic_core::provider::DEFAULT_OPENAI_PROVIDER_ID;
use codex_agentic_core::provider::OSS_PROVIDER_ID;
use codex_agentic_core::provider::sanitize_reasoning_overrides;
use codex_agentic_core::provider::sanitize_tool_overrides;
use codex_agentic_core::settings;
use codex_agentic_core::settings::Settings;
use codex_ansi_escape::ansi_escape_line;
use codex_core::AuthManager;
use codex_core::ConversationManager;
use codex_core::WireApi;
use codex_core::config::Config;
use codex_core::config::OPENAI_DEFAULT_MODEL;
use codex_core::config::persist_model_selection;
use codex_core::config::set_hide_full_access_warning;
use codex_core::config_types::ProviderKind;
use codex_core::memory::MemoryPreviewModeExt;
use codex_core::memory::MemoryRetriever;
use codex_core::memory::MemoryRuntime;
use codex_core::protocol::EventMsg;
use codex_core::protocol::SessionSource;
use codex_core::protocol::TokenUsage;
use codex_core::protocol_config_types::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::ConversationId;
use color_eyre::eyre::Result;
use color_eyre::eyre::WrapErr;
use color_eyre::eyre::eyre;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use ratatui::style::Stylize;
use ratatui::text::Line;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use tokio::select;
use tokio::sync::mpsc::unbounded_channel;
use tokio::time;
use tracing::warn;

const INDEX_STATUS_REFRESH_SECS: u64 = 60;
const INDEX_TOAST_DURATION_SECS: u64 = 5;
const INDEX_TOAST_TICK_SECS: u64 = 1;
const INDEX_DELTA_POLL_SECS: u64 = 300;
const PROGRESS_BAR_WIDTH: usize = 20;
const SEARCH_CODE_TOP_K: usize = 12;

#[derive(Debug, Clone)]
pub struct AppExitInfo {
    pub token_usage: TokenUsage,
    pub conversation_id: Option<ConversationId>,
    pub update_action: Option<UpdateAction>,
}

#[derive(Debug, Clone)]
struct ByokDraft {
    original_id: Option<String>,
    name: String,
    provider_id: String,
    base_url: Option<String>,
    default_model: Option<String>,
    extra_headers: Option<String>,
    provider_kind: ProviderKind,
    think_enabled: bool,
    postprocess_reasoning: bool,
    anthropic_budget_tokens: Option<u32>,
    anthropic_budget_weight: Option<f32>,
    api_key: ApiKeyDraft,
    has_stored_api_key: bool,
    slug_locked: bool,
}

#[derive(Debug, Clone)]
enum ApiKeyDraft {
    Unchanged,
    Set(String),
    Clear,
}

impl ByokDraft {
    fn new() -> Self {
        Self {
            original_id: None,
            name: String::new(),
            provider_id: String::new(),
            base_url: None,
            default_model: None,
            extra_headers: None,
            provider_kind: ProviderKind::OpenAiResponses,
            think_enabled: false,
            postprocess_reasoning: true,
            anthropic_budget_tokens: None,
            anthropic_budget_weight: None,
            api_key: ApiKeyDraft::Unchanged,
            has_stored_api_key: false,
            slug_locked: false,
        }
    }

    fn from_existing(id: &str, provider: &CustomProvider, has_key: bool) -> Self {
        Self {
            original_id: Some(id.to_string()),
            name: provider.name.clone(),
            provider_id: id.to_string(),
            base_url: provider.base_url.clone(),
            default_model: provider.default_model.clone(),
            extra_headers: provider
                .extra_headers
                .as_ref()
                .map(Self::format_extra_headers),
            provider_kind: provider.provider_kind,
            think_enabled: provider.reasoning_controls.think_enabled,
            postprocess_reasoning: provider.reasoning_controls.postprocess_reasoning,
            anthropic_budget_tokens: provider.reasoning_controls.anthropic_budget_tokens,
            anthropic_budget_weight: provider.reasoning_controls.anthropic_budget_weight,
            api_key: ApiKeyDraft::Unchanged,
            has_stored_api_key: has_key,
            slug_locked: true,
        }
    }

    fn apply_field(&mut self, field: ByokDraftField, value: String) -> Result<(), String> {
        let trimmed = value.trim().to_string();
        match field {
            ByokDraftField::Name => {
                self.name = trimmed.clone();
                if !self.slug_locked && self.original_id.is_none() {
                    let slug = slugify_provider_id(&trimmed);
                    if !slug.is_empty() {
                        self.provider_id = slug;
                    }
                }
            }
            ByokDraftField::ProviderId => {
                if !trimmed.is_empty() {
                    self.provider_id = trimmed;
                    self.slug_locked = true;
                }
            }
            ByokDraftField::BaseUrl => {
                if trimmed.eq_ignore_ascii_case("!clear") || trimmed.is_empty() {
                    self.base_url = None;
                } else {
                    self.base_url = Some(trimmed);
                }
            }
            ByokDraftField::DefaultModel => {
                if trimmed.eq_ignore_ascii_case("!clear") || trimmed.is_empty() {
                    self.default_model = None;
                } else {
                    self.default_model = Some(trimmed);
                }
            }
            ByokDraftField::ExtraHeaders => {
                if trimmed.eq_ignore_ascii_case("!clear") || trimmed.is_empty() {
                    self.extra_headers = None;
                } else {
                    self.extra_headers = Some(trimmed);
                }
            }
            ByokDraftField::ApiKey => {
                if trimmed.eq_ignore_ascii_case("!clear") || trimmed.is_empty() {
                    self.api_key = ApiKeyDraft::Clear;
                } else {
                    self.api_key = ApiKeyDraft::Set(trimmed);
                }
            }
            ByokDraftField::AnthropicBudgetTokens => {
                if trimmed.eq_ignore_ascii_case("!clear") || trimmed.is_empty() {
                    self.anthropic_budget_tokens = None;
                } else {
                    let parsed = trimmed
                        .parse::<u32>()
                        .map_err(|_| "Enter a whole number of tokens".to_string())?;
                    self.anthropic_budget_tokens = Some(parsed);
                }
            }
            ByokDraftField::AnthropicBudgetWeight => {
                if trimmed.eq_ignore_ascii_case("!clear") || trimmed.is_empty() {
                    self.anthropic_budget_weight = None;
                } else {
                    let parsed = trimmed
                        .parse::<f32>()
                        .map_err(|_| "Enter a numeric weight (e.g. 0.5)".to_string())?;
                    if parsed.is_sign_negative() {
                        return Err("Weight must be non-negative".to_string());
                    }
                    self.anthropic_budget_weight = Some(parsed);
                }
            }
        }
        Ok(())
    }

    fn format_extra_headers(headers: &BTreeMap<String, String>) -> String {
        headers
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn api_key_status_label(&self) -> &'static str {
        match (&self.api_key, self.has_stored_api_key) {
            (ApiKeyDraft::Set(_), _) => "Will update",
            (ApiKeyDraft::Clear, _) => "Will remove",
            (ApiKeyDraft::Unchanged, true) => "Stored",
            (ApiKeyDraft::Unchanged, false) => "Not stored",
        }
    }

    fn set_provider_kind(&mut self, kind: ProviderKind) {
        self.provider_kind = kind;
        match kind {
            ProviderKind::OpenAiResponses => {
                self.think_enabled = false;
                self.postprocess_reasoning = true;
                self.anthropic_budget_tokens = None;
                self.anthropic_budget_weight = None;
            }
            ProviderKind::Ollama => {
                self.anthropic_budget_tokens = None;
                self.anthropic_budget_weight = None;
                if !self.postprocess_reasoning {
                    self.postprocess_reasoning = true;
                }
            }
            ProviderKind::AnthropicClaude => {
                self.think_enabled = false;
                self.postprocess_reasoning = true;
            }
        }
        if kind != ProviderKind::AnthropicClaude {
            self.anthropic_budget_tokens = None;
            self.anthropic_budget_weight = None;
        }
    }

    fn cycle_provider_kind(&mut self) {
        let next = match self.provider_kind {
            ProviderKind::OpenAiResponses => ProviderKind::Ollama,
            ProviderKind::Ollama => ProviderKind::AnthropicClaude,
            ProviderKind::AnthropicClaude => ProviderKind::OpenAiResponses,
        };
        self.set_provider_kind(next);
    }

    fn toggle_think(&mut self) {
        if self.provider_kind == ProviderKind::Ollama {
            self.think_enabled = !self.think_enabled;
        }
    }

    fn toggle_postprocess(&mut self) {
        if self.provider_kind == ProviderKind::Ollama {
            self.postprocess_reasoning = !self.postprocess_reasoning;
        }
    }
}

fn provider_kind_label(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenAiResponses => "OpenAI Responses",
        ProviderKind::Ollama => "Ollama",
        ProviderKind::AnthropicClaude => "Anthropic Claude",
    }
}

fn slugify_provider_id(name: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in name.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            slug.push(lower);
            last_dash = false;
        } else if matches!(lower, '-' | '_' | ' ') && !slug.is_empty() && !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

fn is_valid_provider_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_')
}

fn is_reserved_provider_id(id: &str) -> bool {
    id == DEFAULT_OPENAI_PROVIDER_ID || id == OSS_PROVIDER_ID || matches!(id, "codex" | "ollama")
}

fn parse_extra_headers(value: &str) -> color_eyre::Result<BTreeMap<String, String>> {
    let mut headers = BTreeMap::new();
    for entry in value.split(',') {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (key, val) = trimmed.split_once('=').ok_or_else(|| {
            eyre!(
                "Invalid header `{trimmed}`; expected key=value (comma separated for multiple entries)"
            )
        })?;
        let key = key.trim();
        let val = val.trim();
        if key.is_empty() || val.is_empty() {
            return Err(eyre!(
                "Invalid header `{trimmed}`; key and value must be non-empty"
            ));
        }
        headers.insert(key.to_string(), val.to_string());
    }
    Ok(headers)
}

fn normalize_custom_provider_base_url(
    input: Option<String>,
) -> color_eyre::Result<(Option<String>, WireApi)> {
    let mut wire_api = WireApi::Responses;
    let normalized = input.and_then(|url| {
        let trimmed = url.trim();
        if trimmed.eq_ignore_ascii_case("!clear") || trimmed.is_empty() {
            return None;
        }

        let mut base = trimmed.trim_end_matches('/').to_string();
        let lower = base.to_ascii_lowercase();
        if lower.ends_with("/chat/completions") {
            wire_api = WireApi::Chat;
            base.truncate(base.len() - "/chat/completions".len());
            base = base.trim_end_matches('/').to_string();
        } else if lower.ends_with("/responses") {
            wire_api = WireApi::Responses;
            base.truncate(base.len() - "/responses".len());
            base = base.trim_end_matches('/').to_string();
        }

        Some(base)
    });

    if let Some(ref url) = normalized
        && !(url.starts_with("http://") || url.starts_with("https://"))
    {
        return Err(eyre!("Base URL must start with http:// or https://"));
    }

    Ok((normalized, wire_api))
}

pub(crate) struct App {
    pub(crate) server: Arc<ConversationManager>,
    pub(crate) app_event_tx: AppEventSender,
    pub(crate) chat_widget: ChatWidget,
    pub(crate) auth_manager: Arc<AuthManager>,

    /// Config is stored here so we can recreate ChatWidgets as needed.
    pub(crate) config: Config,
    pub(crate) active_profile: Option<String>,
    settings: Settings,
    byok_draft: Option<ByokDraft>,

    pub(crate) file_search: FileSearchManager,
    index_worker: IndexWorker,
    index_status: Option<IndexStatusSnapshot>,
    index_progress: Option<IndexProgressState>,
    last_index_attempt: Option<DateTime<Utc>>,
    index_completion_toast_until: Option<Instant>,
    index_completion_message: Option<String>,

    pub(crate) transcript_cells: Vec<Arc<dyn HistoryCell>>,

    // Pager overlay state (Transcript or Static like Diff)
    pub(crate) overlay: Option<Overlay>,
    pub(crate) deferred_history_lines: Vec<Line<'static>>,
    has_emitted_history_lines: bool,

    pub(crate) enhanced_keys_supported: bool,

    /// Controls the animation thread that sends CommitTick events.
    pub(crate) commit_anim_running: Arc<AtomicBool>,

    // Esc-backtracking state grouped
    pub(crate) backtrack: crate::app_backtrack::BacktrackState,
    pub(crate) feedback: codex_feedback::CodexFeedback,
    pub(crate) pending_update_action: Option<UpdateAction>,
    memory_runtime: Option<MemoryRuntime>,
}

#[derive(Debug, Default, Clone)]
struct IndexProgressState {
    processed_files: usize,
    total_files: usize,
    processed_chunks: usize,
    total_chunks: usize,
    current_path: Option<String>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        tui: &mut tui::Tui,
        auth_manager: Arc<AuthManager>,
        config: Config,
        active_profile: Option<String>,
        initial_prompt: Option<String>,
        initial_images: Vec<PathBuf>,
        resume_selection: ResumeSelection,
        feedback: codex_feedback::CodexFeedback,
    ) -> Result<AppExitInfo> {
        use tokio_stream::StreamExt;
        let (app_event_tx, mut app_event_rx) = unbounded_channel();
        let app_event_tx = AppEventSender::new(app_event_tx);

        let conversation_manager = Arc::new(ConversationManager::new(
            auth_manager.clone(),
            SessionSource::Cli,
        ));

        let enhanced_keys_supported = tui.enhanced_keys_supported();

        let chat_widget = match resume_selection {
            ResumeSelection::StartFresh | ResumeSelection::Exit => {
                let init = crate::chatwidget::ChatWidgetInit {
                    config: config.clone(),
                    frame_requester: tui.frame_requester(),
                    app_event_tx: app_event_tx.clone(),
                    initial_prompt: initial_prompt.clone(),
                    initial_images: initial_images.clone(),
                    enhanced_keys_supported,
                    auth_manager: auth_manager.clone(),
                    feedback: feedback.clone(),
                };
                ChatWidget::new(init, conversation_manager.clone())
            }
            ResumeSelection::Resume(path) => {
                let resumed = conversation_manager
                    .resume_conversation_from_rollout(
                        config.clone(),
                        path.clone(),
                        auth_manager.clone(),
                    )
                    .await
                    .wrap_err_with(|| {
                        format!("Failed to resume session from {}", path.display())
                    })?;
                let init = crate::chatwidget::ChatWidgetInit {
                    config: config.clone(),
                    frame_requester: tui.frame_requester(),
                    app_event_tx: app_event_tx.clone(),
                    initial_prompt: initial_prompt.clone(),
                    initial_images: initial_images.clone(),
                    enhanced_keys_supported,
                    auth_manager: auth_manager.clone(),
                    feedback: feedback.clone(),
                };
                ChatWidget::new_from_existing(
                    init,
                    resumed.conversation,
                    resumed.session_configured,
                )
            }
        };

        let cwd = config.cwd.clone();
        let index_worker = IndexWorker::new(cwd.clone(), app_event_tx.clone());
        let index_status = IndexStatusSnapshot::load(&cwd).ok().flatten();
        let settings = settings::global();
        #[cfg(not(debug_assertions))]
        let update_config = codex_agentic_core::updates::from_settings(&settings);
        let file_search = FileSearchManager::new(config.cwd.clone(), app_event_tx.clone());
        let last_index_attempt = index_status
            .as_ref()
            .and_then(|snapshot| snapshot.analytics.last_attempt_ts);
        #[cfg(not(debug_assertions))]
        let upgrade_version = crate::updates::get_upgrade_version(&config, &update_config);

        let memory_runtime = match MemoryRuntime::load(config.codex_home.join("memory")).await {
            Ok(runtime) => Some(runtime),
            Err(err) => {
                warn!(
                    error = %err,
                    "memory runtime unavailable; disabling memory suggestions"
                );
                None
            }
        };

        let mut app = Self {
            server: conversation_manager,
            app_event_tx,
            chat_widget,
            auth_manager: auth_manager.clone(),
            config,
            active_profile,
            settings,
            byok_draft: None,
            file_search,
            index_worker,
            index_status,
            index_progress: None,
            last_index_attempt,
            index_completion_toast_until: None,
            index_completion_message: None,
            enhanced_keys_supported,
            transcript_cells: Vec::new(),
            overlay: None,
            deferred_history_lines: Vec::new(),
            has_emitted_history_lines: false,
            commit_anim_running: Arc::new(AtomicBool::new(false)),
            backtrack: BacktrackState::default(),
            feedback,
            pending_update_action: None,
            memory_runtime,
        };

        if app.settings.auto_build_index() && !app.index_manifest_exists() {
            let mut options = BuildOptions::default();
            options.project_root = app.config.cwd.clone();
            app.show_index_toast("Semantic index missing — building now…".to_string());
            app.index_worker.spawn_build(options);
        }

        app.refresh_index_status_line();

        spawn_status_refresh(app.app_event_tx.clone());
        spawn_toast_tick(app.app_event_tx.clone());
        spawn_delta_monitor(
            app.config.cwd.clone(),
            app.app_event_tx.clone(),
            Duration::from_secs(INDEX_DELTA_POLL_SECS),
        );
        #[cfg(not(debug_assertions))]
        if let Some(upgrade_info) = upgrade_version {
            let update_action = crate::updates::get_update_action();
            app.handle_event(
                tui,
                AppEvent::InsertHistoryCell(Box::new(UpdateAvailableHistoryCell::new(
                    upgrade_info.latest_version.clone(),
                    update_action,
                ))),
            )
            .await?;
            app.pending_update_action = update_action;
        }

        let tui_events = tui.event_stream();
        tokio::pin!(tui_events);

        tui.frame_requester().schedule_frame();

        while select! {
            Some(event) = app_event_rx.recv() => {
                app.handle_event(tui, event).await?
            }
            Some(event) = tui_events.next() => {
                app.handle_tui_event(tui, event).await?
            }
        } {}
        tui.terminal.clear()?;
        Ok(AppExitInfo {
            token_usage: app.token_usage(),
            conversation_id: app.chat_widget.conversation_id(),
            update_action: app.pending_update_action,
        })
    }

    fn refresh_index_status_line(&mut self) {
        if self.index_progress.is_none() && !self.index_manifest_exists() {
            let message = if self.settings.auto_build_index() {
                "Semantic index not found — building automatically…"
            } else {
                "Semantic index not found — run /index to build it."
            };
            self.chat_widget
                .set_index_status_line(Some(message.to_string()));
            return;
        }
        if let Some(state) = &self.index_progress {
            let line = render_progress_status(state);
            self.chat_widget.set_index_status_line(Some(line));
            return;
        }
        if let Some(until) = self.index_completion_toast_until {
            if Instant::now() < until {
                if let Some(message) = &self.index_completion_message {
                    self.chat_widget
                        .set_index_status_line(Some(message.clone()));
                    return;
                }
            } else {
                self.index_completion_toast_until = None;
                self.index_completion_message = None;
            }
        }
        let line = self.index_status.as_ref().map(format_index_status);
        self.chat_widget.set_index_status_line(line);
    }

    fn index_manifest_exists(&self) -> bool {
        self.config.cwd.join(".codex/index/manifest.json").exists()
    }

    fn apply_model_provider(&mut self, provider_id: &str) {
        if self.config.model_provider_id == provider_id {
            return;
        }
        if let Some(info) = self.config.model_providers.get(provider_id).cloned() {
            self.config.model_provider_id = provider_id.to_string();
            self.config.model_provider = info;
            refresh_model_metadata(&mut self.config);
            sanitize_reasoning_overrides(&mut self.config);
            sanitize_tool_overrides(&mut self.config);
            self.chat_widget
                .set_model_provider(&self.config.model_provider_id, &self.config.model_provider);
        } else {
            tracing::warn!(
                provider_id,
                "selected model provider not found in config when applying model preset"
            );
        }
    }

    fn start_index_build(&mut self) {
        if self.index_progress.is_some() {
            self.show_index_toast("Index build already running".to_string());
            return;
        }
        self.index_completion_toast_until = None;
        self.index_completion_message = None;
        self.last_index_attempt = Some(Utc::now());
        self.index_progress = Some(IndexProgressState::default());
        self.refresh_index_status_line();
        let options = BuildOptions {
            requested_model: Some(self.config.model.clone()),
            ..BuildOptions::default()
        };
        self.index_worker.spawn_build(options);
    }

    fn on_index_status_event(&mut self, event: CoreIndexEvent) {
        match event {
            CoreIndexEvent::Started { total_files } => {
                self.index_progress = Some(IndexProgressState {
                    total_files,
                    ..IndexProgressState::default()
                });
                self.refresh_index_status_line();
            }
            CoreIndexEvent::Progress {
                processed_files,
                total_files,
                processed_chunks,
                total_chunks,
                current_path,
                ..
            } => {
                if let Some(state) = self.index_progress.as_mut() {
                    state.processed_files = processed_files;
                    state.total_files = total_files;
                    state.processed_chunks = processed_chunks;
                    state.total_chunks = total_chunks;
                    state.current_path = Some(current_path);
                }
                self.refresh_index_status_line();
            }
            CoreIndexEvent::Completed(summary) => {
                self.index_progress = None;
                self.show_index_toast(format!(
                    "Index complete • {} files • {} chunks",
                    summary.total_files, summary.total_chunks
                ));
                self.reload_index_status_snapshot();
            }
            CoreIndexEvent::Error { message } => {
                self.index_progress = None;
                self.show_index_toast(format!("Index build failed: {message}"));
            }
        }
    }

    fn on_index_status_updated(&mut self, snapshot: Option<IndexStatusSnapshot>) {
        self.index_status = snapshot;
        if let Some(snapshot) = &self.index_status {
            self.last_index_attempt = snapshot.analytics.last_attempt_ts;
        }
        self.refresh_index_status_line();
    }

    fn maybe_refresh_index_post_turn(&mut self) {
        if !self.settings.post_turn_refresh_enabled() {
            return;
        }
        if self.index_progress.is_some() {
            return;
        }
        let min_secs = self.settings.refresh_min_secs().max(0);
        let now = Utc::now();
        let last_attempt = self.last_index_attempt.or_else(|| {
            self.index_status
                .as_ref()
                .and_then(|snap| snap.analytics.last_attempt_ts)
        });
        if let Some(last) = last_attempt
            && (now - last).num_seconds() < min_secs
        {
            return;
        }
        self.start_index_build();
    }
    fn reload_index_status_snapshot(&mut self) {
        let cwd = self.config.cwd.clone();
        match IndexStatusSnapshot::load(&cwd) {
            Ok(snapshot) => {
                self.index_status = snapshot;
                if let Some(snapshot) = &self.index_status {
                    self.last_index_attempt = snapshot.analytics.last_attempt_ts;
                }
            }
            Err(err) => {
                tracing::debug!(error = %err, "failed to reload index status");
            }
        }
        self.refresh_index_status_line();
    }

    fn maybe_expire_index_toast(&mut self) -> bool {
        if let Some(until) = self.index_completion_toast_until
            && Instant::now() >= until
        {
            self.index_completion_toast_until = None;
            self.index_completion_message = None;
            return true;
        }
        false
    }

    fn show_index_toast(&mut self, message: String) {
        self.index_completion_message = Some(message);
        self.index_completion_toast_until =
            Some(Instant::now() + Duration::from_secs(INDEX_TOAST_DURATION_SECS));
        self.refresh_index_status_line();
    }

    fn on_index_delta_detected(&mut self, diff: SnapshotDiff) {
        if !diff.has_changes() {
            return;
        }
        let summary = format!(
            "Detected index changes • +{} / ~{} / -{}",
            diff.added.len(),
            diff.modified.len(),
            diff.removed.len()
        );
        self.show_index_toast(summary);
        if self.settings.post_turn_refresh_enabled() && self.index_progress.is_none() {
            self.start_index_build();
        }
    }

    pub(crate) async fn handle_tui_event(
        &mut self,
        tui: &mut tui::Tui,
        event: TuiEvent,
    ) -> Result<bool> {
        if self.overlay.is_some() {
            let _ = self.handle_backtrack_overlay_event(tui, event).await?;
        } else {
            match event {
                TuiEvent::Key(key_event) => {
                    self.handle_key_event(tui, key_event).await;
                }
                TuiEvent::Paste(pasted) => {
                    // Many terminals convert newlines to \r when pasting (e.g., iTerm2),
                    // but tui-textarea expects \n. Normalize CR to LF.
                    // [tui-textarea]: https://github.com/rhysd/tui-textarea/blob/4d18622eeac13b309e0ff6a55a46ac6706da68cf/src/textarea.rs#L782-L783
                    // [iTerm2]: https://github.com/gnachman/iTerm2/blob/5d0c0d9f68523cbd0494dad5422998964a2ecd8d/sources/iTermPasteHelper.m#L206-L216
                    let pasted = pasted.replace("\r", "\n");
                    self.chat_widget.handle_paste(pasted);
                }
                TuiEvent::Draw => {
                    self.chat_widget.maybe_post_pending_notification(tui);
                    if self
                        .chat_widget
                        .handle_paste_burst_tick(tui.frame_requester())
                    {
                        return Ok(true);
                    }
                    tui.draw(
                        self.chat_widget.desired_height(tui.terminal.size()?.width),
                        |frame| {
                            frame.render_widget_ref(&self.chat_widget, frame.area());
                            if let Some((x, y)) = self.chat_widget.cursor_pos(frame.area()) {
                                frame.set_cursor_position((x, y));
                            }
                        },
                    )?;
                }
            }
        }
        Ok(true)
    }

    async fn handle_event(&mut self, tui: &mut tui::Tui, event: AppEvent) -> Result<bool> {
        match event {
            AppEvent::NewSession => {
                let init = crate::chatwidget::ChatWidgetInit {
                    config: self.config.clone(),
                    frame_requester: tui.frame_requester(),
                    app_event_tx: self.app_event_tx.clone(),
                    initial_prompt: None,
                    initial_images: Vec::new(),
                    enhanced_keys_supported: self.enhanced_keys_supported,
                    auth_manager: self.auth_manager.clone(),
                    feedback: self.feedback.clone(),
                };
                self.chat_widget = ChatWidget::new(init, self.server.clone());
                tui.frame_requester().schedule_frame();
            }
            AppEvent::InsertHistoryCell(cell) => {
                let cell: Arc<dyn HistoryCell> = cell.into();
                if let Some(Overlay::Transcript(t)) = &mut self.overlay {
                    t.insert_cell(cell.clone());
                    tui.frame_requester().schedule_frame();
                }
                self.transcript_cells.push(cell.clone());
                let mut display = cell.display_lines(tui.terminal.last_known_screen_size.width);
                if !display.is_empty() {
                    // Only insert a separating blank line for new cells that are not
                    // part of an ongoing stream. Streaming continuations should not
                    // accrue extra blank lines between chunks.
                    if !cell.is_stream_continuation() {
                        if self.has_emitted_history_lines {
                            display.insert(0, Line::from(""));
                        } else {
                            self.has_emitted_history_lines = true;
                        }
                    }
                    if self.overlay.is_some() {
                        self.deferred_history_lines.extend(display);
                    } else {
                        tui.insert_history_lines(display);
                    }
                }
            }
            AppEvent::StartCommitAnimation => {
                if self
                    .commit_anim_running
                    .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    let tx = self.app_event_tx.clone();
                    let running = self.commit_anim_running.clone();
                    thread::spawn(move || {
                        while running.load(Ordering::Relaxed) {
                            thread::sleep(Duration::from_millis(50));
                            tx.send(AppEvent::CommitTick);
                        }
                    });
                }
            }
            AppEvent::StopCommitAnimation => {
                self.commit_anim_running.store(false, Ordering::Release);
            }
            AppEvent::CommitTick => {
                self.chat_widget.on_commit_tick();
            }
            AppEvent::CodexEvent(event) => {
                let turn_complete = matches!(&event.msg, EventMsg::TaskComplete(_));
                self.chat_widget.handle_codex_event(event);
                if turn_complete {
                    self.maybe_refresh_index_post_turn();
                }
            }
            AppEvent::ConversationHistory(ev) => {
                self.on_conversation_history_for_backtrack(tui, ev).await?;
            }
            AppEvent::ExitRequest => {
                return Ok(false);
            }
            AppEvent::CodexOp(op) => self.chat_widget.submit_op(op),
            AppEvent::DiffResult(text) => {
                // Clear the in-progress state in the bottom pane
                self.chat_widget.on_diff_complete();
                // Enter alternate screen using TUI helper and build pager lines
                let _ = tui.enter_alt_screen();
                let pager_lines: Vec<ratatui::text::Line<'static>> = if text.trim().is_empty() {
                    vec!["No changes detected.".italic().into()]
                } else {
                    text.lines().map(ansi_escape_line).collect()
                };
                self.overlay = Some(Overlay::new_static_with_lines(
                    pager_lines,
                    "D I F F".to_string(),
                ));
                tui.frame_requester().schedule_frame();
            }
            AppEvent::StartFileSearch(query) => {
                if !query.is_empty() {
                    self.file_search.on_user_query(query);
                }
            }
            AppEvent::IndexStatus(progress) => {
                self.on_index_status_event(progress);
            }
            AppEvent::IndexStatusUpdated(snapshot) => {
                self.on_index_status_updated(snapshot);
            }
            AppEvent::IndexStatusTick => {
                self.refresh_index_status_line();
            }
            AppEvent::IndexToastTick => {
                if self.maybe_expire_index_toast() {
                    self.refresh_index_status_line();
                }
            }
            AppEvent::IndexDeltaDetected(diff) => {
                self.on_index_delta_detected(diff);
            }
            AppEvent::StartIndexBuild => {
                self.start_index_build();
            }
            AppEvent::OpenMemoryManager => {
                run_memory_manager(tui, &self.config.codex_home).await?;
            }
            AppEvent::OpenMemoryPreview { preview } => {
                let _ = tui.enter_alt_screen();
                self.overlay = Some(Overlay::new_memory_preview(
                    preview,
                    self.app_event_tx.clone(),
                ));
                tui.frame_requester().schedule_frame();
            }
            AppEvent::OpenFullAccessConfirmation { preset } => {
                self.chat_widget.open_full_access_confirmation(preset);
            }
            AppEvent::UpdateFullAccessWarningAcknowledged(acknowledged) => {
                self.chat_widget
                    .set_full_access_warning_acknowledged(acknowledged);
                self.config.notices.hide_full_access_warning = Some(acknowledged);
            }
            AppEvent::PersistFullAccessWarningAcknowledged => {
                if let Some(value) = self.config.notices.hide_full_access_warning
                    && let Err(err) = set_hide_full_access_warning(&self.config.codex_home, value)
                {
                    tracing::error!("failed to persist full access warning acknowledgement: {err}");
                }
            }
            AppEvent::OpenApprovalsPopup => {
                self.chat_widget.open_approvals_popup();
            }
            AppEvent::OpenSearchManager => {
                self.open_search_manager();
            }
            AppEvent::SearchCodePrompt => {
                self.show_search_code_prompt();
            }
            AppEvent::MemorySuggestPrompt => {
                self.show_memory_suggest_prompt();
            }
            AppEvent::SearchConfidencePrompt => {
                self.show_search_confidence_prompt();
            }
            AppEvent::SearchCodeRequested { query } => {
                self.run_search_code(query);
            }
            AppEvent::MemorySuggestRequested { query } => {
                self.run_memory_suggest(query);
            }
            AppEvent::SearchConfidenceSubmitted { raw } => {
                self.handle_search_confidence_submission(raw);
            }
            AppEvent::SearchCodeResult {
                query,
                confidence,
                hits,
            } => {
                self.handle_search_code_result(query, confidence, hits);
            }
            AppEvent::SearchCodeError { query, error } => {
                self.handle_search_code_error(query, error);
            }
            AppEvent::MemorySuggestResult {
                query,
                min_confidence,
                entries,
            } => {
                self.handle_memory_suggest_result(query, min_confidence, entries);
            }
            AppEvent::MemorySuggestError { query, error } => {
                self.handle_memory_suggest_error(query, error);
            }
            AppEvent::FileSearchResult { query, matches } => {
                self.chat_widget.apply_file_search_result(query, matches);
            }
            AppEvent::UpdateReasoningEffort(effort) => {
                self.on_update_reasoning_effort(effort);
            }
            AppEvent::OpenReasoningPopup { model } => {
                if let Some(provider_id) = model.provider_id.as_deref() {
                    self.apply_model_provider(provider_id);
                } else {
                    self.apply_model_provider(DEFAULT_OPENAI_PROVIDER_ID);
                }
                self.chat_widget.open_reasoning_popup(model);
            }
            AppEvent::UpdateModel(model) => {
                self.chat_widget.set_model(&model);
                self.config.model = model;
                refresh_model_metadata(&mut self.config);
                sanitize_reasoning_overrides(&mut self.config);
                sanitize_tool_overrides(&mut self.config);
            }
            AppEvent::OpenFeedbackNote {
                category,
                include_logs,
            } => {
                self.chat_widget.open_feedback_note(category, include_logs);
            }
            AppEvent::OpenFeedbackConsent { category } => {
                self.chat_widget.open_feedback_consent(category);
            }
            AppEvent::ShowWindowsAutoModeInstructions => {
                self.chat_widget.open_windows_auto_mode_instructions();
            }
            AppEvent::PersistModelSelection { model, effort } => {
                let profile = self.active_profile.as_deref();
                match persist_model_selection(&self.config.codex_home, profile, &model, effort)
                    .await
                {
                    Ok(()) => {
                        let effort_label = effort
                            .map(|eff| format!(" with {eff} reasoning"))
                            .unwrap_or_else(|| " with default reasoning".to_string());
                        if let Some(profile) = profile {
                            self.chat_widget.add_info_message(
                                format!(
                                    "Model changed to {model}{effort_label} for {profile} profile"
                                ),
                                None,
                            );
                        } else {
                            self.chat_widget.add_info_message(
                                format!("Model changed to {model}{effort_label}"),
                                None,
                            );
                        }
                        match persist_default_model_selection(
                            &model,
                            Some(self.config.model_provider_id.as_str()),
                        ) {
                            Ok(updated) => {
                                self.settings = updated;
                            }
                            Err(err) => {
                                tracing::warn!(
                                    error = %err,
                                    "failed to persist model selection to settings.json"
                                );
                                self.chat_widget.add_error_message(format!(
                                    "Failed to update settings.json with new default model: {err}"
                                ));
                            }
                        }
                    }
                    Err(err) => {
                        tracing::error!(
                            error = %err,
                            "failed to persist model selection"
                        );
                        if let Some(profile) = profile {
                            self.chat_widget.add_error_message(format!(
                                "Failed to save model for profile `{profile}`: {err}"
                            ));
                        } else {
                            self.chat_widget
                                .add_error_message(format!("Failed to save default model: {err}"));
                        }
                    }
                }
            }
            AppEvent::OpenByokManager => {
                self.open_byok_manager();
            }
            AppEvent::ShowByokProviderActions { provider_id } => {
                self.show_byok_provider_actions(&provider_id);
            }
            AppEvent::StartByokEdit { existing_id } => {
                self.start_byok_edit(existing_id);
            }
            AppEvent::BeginByokFieldEdit { field } => {
                self.begin_byok_field_edit(field);
            }
            AppEvent::UpdateByokDraftField { field, value } => {
                self.update_byok_draft_field(field, value);
            }
            AppEvent::CycleByokProviderKind => {
                self.cycle_byok_provider_kind();
            }
            AppEvent::ToggleByokThink => {
                self.toggle_byok_think();
            }
            AppEvent::ToggleByokPostprocess => {
                self.toggle_byok_postprocess();
            }
            AppEvent::RefreshByokProviderModels { provider_id } => {
                self.refresh_byok_provider_models(&provider_id);
            }
            AppEvent::ShowByokProviderModels { provider_id } => {
                self.show_byok_provider_models(&provider_id);
            }
            AppEvent::SubmitByokForm { original_id, form } => {
                if let Err(err) = self.submit_byok_form(original_id, form) {
                    self.chat_widget
                        .add_error_message(format!("Failed to save provider: {err}"));
                    self.show_byok_edit_view();
                }
            }
            AppEvent::DeleteCustomProvider { provider_id } => {
                if let Err(err) = self.delete_custom_provider(&provider_id) {
                    self.chat_widget
                        .add_error_message(format!("Failed to delete provider: {err}"));
                    self.open_byok_manager();
                }
            }
            AppEvent::CustomProviderModelsFetched {
                provider_id,
                result,
            } => {
                self.on_custom_provider_models_fetched(provider_id, result);
            }
            AppEvent::UpdateAskForApprovalPolicy(policy) => {
                self.chat_widget.set_approval_policy(policy);
            }
            AppEvent::UpdateSandboxPolicy(policy) => {
                self.chat_widget.set_sandbox_policy(policy);
            }
            AppEvent::OpenReviewBranchPicker(cwd) => {
                self.chat_widget.show_review_branch_picker(&cwd).await;
            }
            AppEvent::OpenReviewCommitPicker(cwd) => {
                self.chat_widget.show_review_commit_picker(&cwd).await;
            }
            AppEvent::OpenReviewCustomPrompt => {
                self.chat_widget.show_review_custom_prompt();
            }
            AppEvent::FullScreenApprovalRequest(request) => match request {
                ApprovalRequest::ApplyPatch { cwd, changes, .. } => {
                    let _ = tui.enter_alt_screen();
                    let diff_summary = DiffSummary::new(changes, cwd);
                    self.overlay = Some(Overlay::new_static_with_renderables(
                        vec![diff_summary.into()],
                        "P A T C H".to_string(),
                    ));
                }
                ApprovalRequest::Exec { command, .. } => {
                    let _ = tui.enter_alt_screen();
                    let full_cmd = strip_bash_lc_and_escape(&command);
                    let full_cmd_lines = highlight_bash_to_lines(&full_cmd);
                    self.overlay = Some(Overlay::new_static_with_lines(
                        full_cmd_lines,
                        "E X E C".to_string(),
                    ));
                }
            },
        }
        Ok(true)
    }

    pub(crate) fn token_usage(&self) -> codex_core::protocol::TokenUsage {
        self.chat_widget.token_usage()
    }

    fn on_update_reasoning_effort(&mut self, effort: Option<ReasoningEffortConfig>) {
        self.chat_widget.set_reasoning_effort(effort);
        self.config.model_reasoning_effort = effort;
    }

    fn open_search_manager(&mut self) {
        let confidence_percent = self.settings.search_confidence_min_percent();
        let default_percent = (DEFAULT_SEARCH_CONFIDENCE_MIN * 100.0).round() as u8;

        let mut items: Vec<SelectionItem> = Vec::new();
        items.push(SelectionItem {
            name: "Run search".to_string(),
            description: Some("Enter a semantic query to search indexed code.".to_string()),
            actions: vec![Box::new(|tx: &AppEventSender| {
                tx.send(AppEvent::SearchCodePrompt);
            })],
            dismiss_on_select: true,
            ..Default::default()
        });
        items.push(SelectionItem {
            name: "Set minimum confidence".to_string(),
            description: Some(format!("Currently {confidence_percent}%")),
            actions: vec![Box::new(|tx: &AppEventSender| {
                tx.send(AppEvent::SearchConfidencePrompt);
            })],
            dismiss_on_select: true,
            ..Default::default()
        });
        items.push(SelectionItem {
            name: "Rebuild index".to_string(),
            description: Some("Run a full index build for freshest results.".to_string()),
            actions: vec![Box::new(|tx: &AppEventSender| {
                tx.send(AppEvent::StartIndexBuild);
            })],
            dismiss_on_select: true,
            ..Default::default()
        });

        let params = SelectionViewParams {
            title: Some("Semantic Search".to_string()),
            subtitle: Some(format!(
                "Minimum confidence: {confidence_percent}% (default {default_percent}%)"
            )),
            footer_hint: Some("Enter to choose · Esc to dismiss".into()),
            items,
            ..Default::default()
        };
        self.chat_widget.show_selection_view(params);
    }

    fn show_search_code_prompt(&mut self) {
        let confidence_percent = self.settings.search_confidence_min_percent();
        let context = Some(format!(
            "Results filtered at ≥ {confidence_percent}% confidence"
        ));
        let tx = self.app_event_tx.clone();
        let on_submit: PromptSubmitted = Box::new(move |value: String| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return;
            }
            tx.send(AppEvent::SearchCodeRequested {
                query: trimmed.to_string(),
            });
        });
        let view = CustomPromptView::new(
            "Search indexed code".to_string(),
            "Enter keywords or code to search".to_string(),
            context,
            on_submit,
        );
        self.chat_widget
            .push_bottom_view(move |pane| pane.show_view(Box::new(view)));
    }

    fn show_memory_suggest_prompt(&mut self) {
        let tx = self.app_event_tx.clone();
        let on_submit: PromptSubmitted = Box::new(move |value: String| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return;
            }
            tx.send(AppEvent::MemorySuggestRequested {
                query: trimmed.to_string(),
            });
        });
        let view = CustomPromptView::new(
            "Suggest stored memories".to_string(),
            "Describe what you want to recall".to_string(),
            Some("Call memory_fetch(\"<id>\") after reviewing suggestions.".to_string()),
            on_submit,
        );
        self.chat_widget
            .push_bottom_view(move |pane| pane.show_view(Box::new(view)));
    }

    fn show_search_confidence_prompt(&mut self) {
        let confidence_percent = self.settings.search_confidence_min_percent();
        let default_percent = (DEFAULT_SEARCH_CONFIDENCE_MIN * 100.0).round() as u8;
        let context = Some(format!(
            "Current: {confidence_percent}% · Default: {default_percent}%"
        ));
        let tx = self.app_event_tx.clone();
        let on_submit: PromptSubmitted = Box::new(move |value: String| {
            tx.send(AppEvent::SearchConfidenceSubmitted { raw: value });
        });
        let view = CustomPromptView::new(
            "Minimum confidence (%)".to_string(),
            "Enter 0-100 or type !reset".to_string(),
            context,
            on_submit,
        )
        .with_allow_empty_submit(true);
        self.chat_widget
            .push_bottom_view(move |pane| pane.show_view(Box::new(view)));
    }

    fn handle_search_confidence_submission(&mut self, raw: String) {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return;
        }

        if trimmed.eq_ignore_ascii_case("!reset") {
            match codex_agentic_core::persist_search_confidence_min(None) {
                Ok(updated) => {
                    self.settings = updated;
                    let percent = self.settings.search_confidence_min_percent();
                    self.chat_widget
                        .add_info_message(format!("Search confidence reset to {percent}%."), None);
                    self.open_search_manager();
                }
                Err(err) => {
                    self.chat_widget
                        .add_error_message(format!("Failed to reset search confidence: {err}"));
                    self.show_search_confidence_prompt();
                }
            }
            return;
        }

        let cleaned = trimmed.trim_end_matches('%').trim();
        let parsed = cleaned.parse::<f32>();
        let value = match parsed {
            Ok(v) => v,
            Err(_) => {
                self.chat_widget
                    .add_error_message(format!("'{trimmed}' is not a valid percentage."));
                self.show_search_confidence_prompt();
                return;
            }
        };

        if !(0.0..=100.0).contains(&value) {
            self.chat_widget
                .add_error_message("Confidence must be between 0 and 100.".to_string());
            self.show_search_confidence_prompt();
            return;
        }

        let normalized = (value / 100.0).clamp(0.0, 1.0);
        match codex_agentic_core::persist_search_confidence_min(Some(normalized)) {
            Ok(updated) => {
                self.settings = updated;
                self.chat_widget
                    .add_info_message(format!("Search confidence set to {value:.0}%."), None);
                self.open_search_manager();
            }
            Err(err) => {
                self.chat_widget
                    .add_error_message(format!("Failed to persist search confidence: {err}"));
                self.show_search_confidence_prompt();
            }
        }
    }

    fn run_search_code(&mut self, query: String) {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            self.chat_widget
                .add_error_message("Search query cannot be empty.".to_string());
            return;
        }
        let query = trimmed.to_string();
        let project_root = self.config.cwd.clone();
        let confidence = self.settings.search_confidence_min();
        let tx = self.app_event_tx.clone();

        tokio::spawn(async move {
            let query_for_results = query.clone();
            let result = tokio::task::spawn_blocking(move || {
                let response = query_index(&project_root, &query, SEARCH_CODE_TOP_K, None)?;
                let filtered = response.with_confidence_min(confidence);
                Ok::<_, AnyError>(filtered.hits)
            })
            .await;

            match result {
                Ok(Ok(hits)) => {
                    tx.send(AppEvent::SearchCodeResult {
                        query: query_for_results,
                        confidence,
                        hits,
                    });
                }
                Ok(Err(err)) => {
                    tx.send(AppEvent::SearchCodeError {
                        query: query_for_results,
                        error: err.to_string(),
                    });
                }
                Err(join_err) => {
                    tracing::error!(error = %join_err, "search-code worker panicked");
                    tx.send(AppEvent::SearchCodeError {
                        query: query_for_results,
                        error: "Search worker failed".to_string(),
                    });
                }
            }
        });
    }

    fn run_memory_suggest(&mut self, query: String) {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            self.chat_widget
                .add_error_message("Memory query cannot be empty.".to_string());
            return;
        }
        let Some(runtime) = self.memory_runtime.clone() else {
            self.chat_widget
                .add_error_message("Memory runtime is unavailable in this session.".to_string());
            return;
        };
        let tx = self.app_event_tx.clone();
        let query = trimmed.to_string();
        tokio::spawn(async move {
            let retriever = MemoryRetriever::new(runtime);
            let result = retriever.retrieve_for_text(&query, Some(10)).await;
            match result {
                Ok(retrieval) => {
                    if retrieval.settings.preview_mode.requires_user_confirmation() {
                        tx.send(AppEvent::MemorySuggestError {
                            query,
                            error: "Memory preview mode requires manual confirmation. Open the memory manager to approve suggestions."
                                .to_string(),
                        });
                        return;
                    }
                    let min_confidence = retrieval.settings.min_confidence;
                    let entries = retrieval
                        .candidates
                        .into_iter()
                        .take(10)
                        .map(|hit| {
                            let confidence_percent =
                                ((hit.record.confidence * 100.0).round() as i32).clamp(0, 100)
                                    as u8;
                            MemorySuggestionEntry {
                                record_id: hit.record.record_id.to_string(),
                                summary: hit.record.summary.trim().to_string(),
                                confidence_percent,
                                score: hit.score,
                            }
                        })
                        .collect();
                    tx.send(AppEvent::MemorySuggestResult {
                        query,
                        min_confidence,
                        entries,
                    });
                }
                Err(err) => {
                    tx.send(AppEvent::MemorySuggestError {
                        query,
                        error: err.to_string(),
                    });
                }
            }
        });
    }

    fn handle_search_code_result(&mut self, query: String, confidence: f32, hits: Vec<QueryHit>) {
        self.chat_widget.add_search_results(query, confidence, hits);
    }

    fn handle_search_code_error(&mut self, query: String, error: String) {
        let lower = error.to_lowercase();
        let message = if lower.contains("index manifest missing") {
            "Semantic index not found. Run /index to build it.".to_string()
        } else if lower.contains("no indexed chunks available") {
            "Semantic index is empty. Run /index to rebuild it.".to_string()
        } else {
            format!("Search for \"{query}\" failed: {error}")
        };
        self.chat_widget.add_error_message(message);
    }

    fn handle_memory_suggest_result(
        &mut self,
        query: String,
        min_confidence: f32,
        entries: Vec<MemorySuggestionEntry>,
    ) {
        self.chat_widget
            .add_memory_suggestions(query, min_confidence, entries);
    }

    fn handle_memory_suggest_error(&mut self, query: String, error: String) {
        let message = format!("Memory suggestion for \"{query}\" failed: {error}");
        self.chat_widget.add_error_message(message);
    }

    fn open_byok_manager(&mut self) {
        self.byok_draft = None;
        let mut items: Vec<SelectionItem> = Vec::new();

        for (id, provider) in self.settings.custom_providers() {
            let name = if provider.name.trim().is_empty() {
                id.clone()
            } else {
                provider.name.clone()
            };

            let has_key = match self.auth_manager.custom_provider_api_key(id) {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(err) => {
                    warn!(provider_id = id, %err, "failed to read custom provider API key");
                    false
                }
            };

            let mut details: Vec<String> = Vec::new();
            details.push(format!(
                "Kind: {}",
                provider_kind_label(provider.provider_kind)
            ));
            if let Some(url) = provider.base_url.as_deref() {
                details.push(url.to_string());
            }
            if let Some(model) = provider.default_model.as_deref() {
                details.push(format!("Default model: {model}"));
            }
            if let Some(models) = provider.cached_models.as_ref()
                && !models.is_empty()
            {
                details.push(format!("Cached models: {}", models.len()));
            }
            if has_key {
                details.push("API key stored".to_string());
            }
            let description = if details.is_empty() {
                None
            } else {
                Some(details.join(" • "))
            };

            let provider_id_for_action = id.clone();
            items.push(SelectionItem {
                name,
                description,
                is_current: self.config.model_provider_id == *id,
                actions: vec![Box::new(move |tx: &AppEventSender| {
                    tx.send(AppEvent::ShowByokProviderActions {
                        provider_id: provider_id_for_action.clone(),
                    });
                })],
                dismiss_on_select: true,
                search_value: Some(format!("{} {id}", provider.name)),
                ..Default::default()
            });
        }

        if items.is_empty() {
            items.push(SelectionItem {
                name: "No custom providers configured".to_string(),
                description: Some("Add a provider to connect third-party endpoints.".to_string()),
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        items.push(SelectionItem {
            name: "Add provider".to_string(),
            description: Some("Create a new custom provider entry".to_string()),
            actions: vec![Box::new(|tx: &AppEventSender| {
                tx.send(AppEvent::StartByokEdit { existing_id: None });
            })],
            dismiss_on_select: true,
            ..Default::default()
        });

        items.push(SelectionItem {
            name: "Close".to_string(),
            dismiss_on_select: true,
            ..Default::default()
        });

        let params = SelectionViewParams {
            title: Some("/BYOK — Custom Providers".to_string()),
            subtitle: Some("Manage custom OpenAI-compatible providers.".to_string()),
            footer_hint: Some("Enter to manage, esc to close.".into()),
            items,
            ..Default::default()
        };

        self.chat_widget.show_selection_view(params);
    }

    fn show_byok_provider_actions(&mut self, provider_id: &str) {
        let Some(provider) = self.settings.custom_provider(provider_id) else {
            self.chat_widget
                .add_error_message(format!("Provider `{provider_id}` not found"));
            self.open_byok_manager();
            return;
        };

        let display_name = if provider.name.trim().is_empty() {
            provider_id.to_string()
        } else {
            provider.name.clone()
        };

        let mut description_parts: Vec<String> = Vec::new();
        description_parts.push(format!(
            "Kind: {}",
            provider_kind_label(provider.provider_kind)
        ));
        if let Some(url) = provider.base_url.as_deref() {
            description_parts.push(url.to_string());
        }
        if let Some(model) = provider.default_model.as_deref() {
            description_parts.push(format!("Default model: {model}"));
        }
        if let Some(models) = provider.cached_models.as_ref()
            && !models.is_empty()
        {
            description_parts.push(format!("Cached models: {}", models.len()));
        }

        let provider_id_for_edit = provider_id.to_string();
        let provider_id_for_delete = provider_id.to_string();
        let provider_id_for_refresh = provider_id.to_string();
        let provider_id_for_models = provider_id.to_string();

        let mut items = vec![SelectionItem {
            name: "Edit provider".to_string(),
            description: None,
            actions: vec![Box::new(move |tx: &AppEventSender| {
                tx.send(AppEvent::StartByokEdit {
                    existing_id: Some(provider_id_for_edit.clone()),
                });
            })],
            dismiss_on_select: true,
            ..Default::default()
        }];

        items.push(SelectionItem {
            name: "Refresh models".to_string(),
            description: Some("Test connectivity and update the cached model list.".to_string()),
            actions: vec![Box::new(move |tx: &AppEventSender| {
                tx.send(AppEvent::RefreshByokProviderModels {
                    provider_id: provider_id_for_refresh.clone(),
                });
            })],
            dismiss_on_select: true,
            ..Default::default()
        });

        items.push(SelectionItem {
            name: "View cached models".to_string(),
            description: Some("Show cached models and default selection.".to_string()),
            actions: vec![Box::new(move |tx: &AppEventSender| {
                tx.send(AppEvent::ShowByokProviderModels {
                    provider_id: provider_id_for_models.clone(),
                });
            })],
            dismiss_on_select: true,
            ..Default::default()
        });

        items.push(SelectionItem {
            name: "Delete provider".to_string(),
            description: Some("Remove this provider and its stored credentials.".to_string()),
            actions: vec![Box::new(move |tx: &AppEventSender| {
                tx.send(AppEvent::DeleteCustomProvider {
                    provider_id: provider_id_for_delete.clone(),
                });
            })],
            dismiss_on_select: true,
            ..Default::default()
        });

        items.push(SelectionItem {
            name: "Back".to_string(),
            dismiss_on_select: true,
            actions: vec![Box::new(|tx: &AppEventSender| {
                tx.send(AppEvent::OpenByokManager);
            })],
            ..Default::default()
        });

        let description = if description_parts.is_empty() {
            None
        } else {
            Some(description_parts.join(" • "))
        };

        let params = SelectionViewParams {
            title: Some(format!("{display_name} ({provider_id})")),
            subtitle: description,
            footer_hint: Some("Enter to choose an action, esc to go back.".into()),
            items,
            ..Default::default()
        };

        self.chat_widget.show_selection_view(params);
    }

    fn start_byok_edit(&mut self, existing_id: Option<String>) {
        let draft = if let Some(id) = existing_id {
            let Some(provider) = self.settings.custom_provider(&id) else {
                self.chat_widget
                    .add_error_message(format!("Provider `{id}` not found"));
                self.open_byok_manager();
                return;
            };
            let has_key = match self.auth_manager.custom_provider_api_key(&id) {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(err) => {
                    warn!(provider_id = id, %err, "failed to check provider key");
                    false
                }
            };
            ByokDraft::from_existing(&id, provider, has_key)
        } else {
            ByokDraft::new()
        };

        self.byok_draft = Some(draft);
        self.show_byok_edit_view();
    }

    fn show_byok_edit_view(&mut self) {
        let Some(draft) = self.byok_draft.as_ref() else {
            self.open_byok_manager();
            return;
        };

        let mut items: Vec<SelectionItem> = Vec::new();

        let name_display = if draft.name.trim().is_empty() {
            "<required>".to_string()
        } else {
            draft.name.clone()
        };
        items.push(SelectionItem {
            name: format!("Display name: {name_display}"),
            description: Some("Human-readable provider label".to_string()),
            actions: vec![Box::new(|tx: &AppEventSender| {
                tx.send(AppEvent::BeginByokFieldEdit {
                    field: ByokDraftField::Name,
                });
            })],
            dismiss_on_select: true,
            ..Default::default()
        });

        let id_display = if draft.provider_id.trim().is_empty() {
            "<required>".to_string()
        } else {
            draft.provider_id.clone()
        };
        items.push(SelectionItem {
            name: format!("Provider ID: {id_display}"),
            description: Some("Slug used in settings and CLI".to_string()),
            actions: vec![Box::new(|tx: &AppEventSender| {
                tx.send(AppEvent::BeginByokFieldEdit {
                    field: ByokDraftField::ProviderId,
                });
            })],
            dismiss_on_select: true,
            ..Default::default()
        });

        let provider_kind_display = provider_kind_label(draft.provider_kind);
        items.push(SelectionItem {
            name: format!("Provider kind: {provider_kind_display}"),
            description: Some("Cycle between supported providers".to_string()),
            actions: vec![Box::new(|tx: &AppEventSender| {
                tx.send(AppEvent::CycleByokProviderKind);
            })],
            dismiss_on_select: true,
            ..Default::default()
        });

        if draft.provider_kind == ProviderKind::Ollama {
            let think_label = if draft.think_enabled {
                "Enabled"
            } else {
                "Disabled"
            };
            items.push(SelectionItem {
                name: format!("Ollama thinking: {think_label}"),
                description: Some("Toggle the Ollama `think` request flag".to_string()),
                actions: vec![Box::new(|tx: &AppEventSender| {
                    tx.send(AppEvent::ToggleByokThink);
                })],
                dismiss_on_select: true,
                ..Default::default()
            });

            let postprocess_label = if draft.postprocess_reasoning {
                "Enabled"
            } else {
                "Disabled"
            };
            items.push(SelectionItem {
                name: format!("Post-process reasoning: {postprocess_label}"),
                description: Some(
                    "Strip <think> blocks from output and emit reasoning events".to_string(),
                ),
                actions: vec![Box::new(|tx: &AppEventSender| {
                    tx.send(AppEvent::ToggleByokPostprocess);
                })],
                dismiss_on_select: true,
                ..Default::default()
            });

            items.push(SelectionItem {
                name: "Tip: run `ollama pull` before refreshing".to_string(),
                display_shortcut: None,
                description: Some(
                    "Model refresh uses the Ollama `/api/tags` endpoint to list models."
                        .to_string(),
                ),
                is_current: false,
                actions: Vec::new(),
                dismiss_on_select: false,
                search_value: None,
            });
        }

        if draft.provider_kind == ProviderKind::AnthropicClaude {
            let tokens_display = draft
                .anthropic_budget_tokens
                .map(|value| value.to_string())
                .unwrap_or_else(|| "<unset>".to_string());
            items.push(SelectionItem {
                name: format!("Thinking tokens: {tokens_display}"),
                description: Some("Optional max tokens for Anthropic thinking budget".to_string()),
                actions: vec![Box::new(|tx: &AppEventSender| {
                    tx.send(AppEvent::BeginByokFieldEdit {
                        field: ByokDraftField::AnthropicBudgetTokens,
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            });

            let weight_display = draft
                .anthropic_budget_weight
                .map(|value| format!("{value:.2}"))
                .unwrap_or_else(|| "<unset>".to_string());
            items.push(SelectionItem {
                name: format!("Thinking weight: {weight_display}"),
                description: Some("Optional budget weight (0.0 - 1.0)".to_string()),
                actions: vec![Box::new(|tx: &AppEventSender| {
                    tx.send(AppEvent::BeginByokFieldEdit {
                        field: ByokDraftField::AnthropicBudgetWeight,
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            });

            items.push(SelectionItem {
                name: "Requires an Anthropic Claude API key".to_string(),
                display_shortcut: None,
                description: Some(
                    "Set thinking budgets to cap Claude's hidden reasoning output.".to_string(),
                ),
                is_current: false,
                actions: Vec::new(),
                dismiss_on_select: false,
                search_value: None,
            });
        }

        let base_url_display = draft
            .base_url
            .as_deref()
            .map(str::to_string)
            .unwrap_or_else(|| match draft.provider_kind {
                ProviderKind::Ollama => DEFAULT_OLLAMA_ENDPOINT.to_string(),
                _ => "Using https://api.openai.com/v1".to_string(),
            });
        items.push(SelectionItem {
            name: format!("Base URL: {base_url_display}"),
            description: Some("Enter !clear to reset to default".to_string()),
            actions: vec![Box::new(|tx: &AppEventSender| {
                tx.send(AppEvent::BeginByokFieldEdit {
                    field: ByokDraftField::BaseUrl,
                });
            })],
            dismiss_on_select: true,
            ..Default::default()
        });

        let default_model_display = draft
            .default_model
            .as_deref()
            .map(str::to_string)
            .unwrap_or_else(|| "<unset>".to_string());
        items.push(SelectionItem {
            name: format!("Default model: {default_model_display}"),
            description: Some("Optional fallback used when model list unavailable".to_string()),
            actions: vec![Box::new(|tx: &AppEventSender| {
                tx.send(AppEvent::BeginByokFieldEdit {
                    field: ByokDraftField::DefaultModel,
                });
            })],
            dismiss_on_select: true,
            ..Default::default()
        });

        let extra_headers_display = draft
            .extra_headers
            .as_deref()
            .map(str::to_string)
            .unwrap_or_else(|| "<none>".to_string());
        items.push(SelectionItem {
            name: format!("Extra headers: {extra_headers_display}"),
            description: Some(
                "Optional request headers, comma-separated key=value (use !clear to remove)"
                    .to_string(),
            ),
            actions: vec![Box::new(|tx: &AppEventSender| {
                tx.send(AppEvent::BeginByokFieldEdit {
                    field: ByokDraftField::ExtraHeaders,
                });
            })],
            dismiss_on_select: true,
            ..Default::default()
        });

        items.push(SelectionItem {
            name: format!("API key: {}", draft.api_key_status_label()),
            description: Some("Enter new key, !clear to remove, or Esc to keep".to_string()),
            actions: vec![Box::new(|tx: &AppEventSender| {
                tx.send(AppEvent::BeginByokFieldEdit {
                    field: ByokDraftField::ApiKey,
                });
            })],
            dismiss_on_select: true,
            ..Default::default()
        });

        let form = CustomProviderForm {
            name: draft.name.clone(),
            provider_id: draft.provider_id.clone(),
            base_url: draft.base_url.clone(),
            default_model: draft.default_model.clone(),
            extra_headers: draft.extra_headers.clone(),
            provider_kind: draft.provider_kind,
            think_enabled: draft.think_enabled,
            postprocess_reasoning: draft.postprocess_reasoning,
            anthropic_budget_tokens: draft.anthropic_budget_tokens,
            anthropic_budget_weight: draft.anthropic_budget_weight,
        };

        let original_id = draft.original_id.clone();
        items.push(SelectionItem {
            name: "Save provider".to_string(),
            description: Some("Persist changes to settings.json".to_string()),
            actions: vec![Box::new(move |tx: &AppEventSender| {
                tx.send(AppEvent::SubmitByokForm {
                    original_id: original_id.clone(),
                    form: form.clone(),
                });
            })],
            dismiss_on_select: true,
            ..Default::default()
        });

        items.push(SelectionItem {
            name: "Cancel".to_string(),
            actions: vec![Box::new(|tx: &AppEventSender| {
                tx.send(AppEvent::OpenByokManager);
            })],
            dismiss_on_select: true,
            ..Default::default()
        });

        let title = if let Some(original) = &draft.original_id {
            format!("Edit provider ({original})")
        } else {
            "Add custom provider".to_string()
        };

        let params = SelectionViewParams {
            title: Some(title),
            subtitle: Some("Select a field to edit, then save.".to_string()),
            footer_hint: Some("Enter to edit field, esc to cancel.".into()),
            items,
            ..Default::default()
        };

        self.chat_widget.show_selection_view(params);
    }

    fn begin_byok_field_edit(&mut self, field: ByokDraftField) {
        let Some(draft) = self.byok_draft.as_ref() else {
            self.open_byok_manager();
            return;
        };

        let (title, placeholder, context): (String, String, Option<String>) = match field {
            ByokDraftField::Name => (
                "Provider name".to_string(),
                "Enter a friendly display name".to_string(),
                (!draft.name.trim().is_empty()).then(|| format!("Current: {}", draft.name)),
            ),
            ByokDraftField::ProviderId => (
                "Provider ID".to_string(),
                "Lowercase slug (letters, numbers, hyphen)".to_string(),
                Some(format!("Current: {}", draft.provider_id)),
            ),
            ByokDraftField::BaseUrl => (
                "Base URL".to_string(),
                "Override API base URL or type !clear".to_string(),
                draft
                    .base_url
                    .as_deref()
                    .map(|url| format!("Current: {url}"))
                    .or_else(|| Some("Default: https://api.openai.com/v1".to_string())),
            ),
            ByokDraftField::DefaultModel => (
                "Default model".to_string(),
                "Optional fallback model or !clear".to_string(),
                draft
                    .default_model
                    .as_deref()
                    .map(|model| format!("Current: {model}")),
            ),
            ByokDraftField::ExtraHeaders => (
                "Extra headers".to_string(),
                "Comma-separated key=value pairs or !clear".to_string(),
                draft
                    .extra_headers
                    .as_deref()
                    .map(|headers| format!("Current: {headers}")),
            ),
            ByokDraftField::ApiKey => (
                "API key".to_string(),
                "Enter new key, !clear to remove, or Esc to keep".to_string(),
                None,
            ),
            ByokDraftField::AnthropicBudgetTokens => (
                "Thinking budget tokens".to_string(),
                "Enter max thinking tokens or !clear".to_string(),
                draft
                    .anthropic_budget_tokens
                    .map(|tokens| format!("Current: {tokens}")),
            ),
            ByokDraftField::AnthropicBudgetWeight => (
                "Thinking budget weight".to_string(),
                "Enter weight (0.0 - 1.0) or !clear".to_string(),
                draft
                    .anthropic_budget_weight
                    .map(|weight| format!("Current: {weight}")),
            ),
        };

        let field_for_event = field;
        let tx = self.app_event_tx.clone();
        let on_submit: PromptSubmitted = Box::new(move |value: String| {
            let trimmed = value.trim().to_string();
            if trimmed.is_empty()
                && !matches!(
                    field_for_event,
                    ByokDraftField::BaseUrl
                        | ByokDraftField::DefaultModel
                        | ByokDraftField::ExtraHeaders
                        | ByokDraftField::ApiKey
                        | ByokDraftField::AnthropicBudgetTokens
                        | ByokDraftField::AnthropicBudgetWeight
                )
            {
                return;
            }
            tx.send(AppEvent::UpdateByokDraftField {
                field: field_for_event,
                value: trimmed,
            });
        });

        let view = CustomPromptView::new(title, placeholder, context, on_submit)
            .with_allow_empty_submit(matches!(
                field,
                ByokDraftField::BaseUrl
                    | ByokDraftField::DefaultModel
                    | ByokDraftField::ExtraHeaders
                    | ByokDraftField::ApiKey
                    | ByokDraftField::AnthropicBudgetTokens
                    | ByokDraftField::AnthropicBudgetWeight
            ));
        self.chat_widget
            .push_bottom_view(move |pane| pane.show_view(Box::new(view)));
    }

    fn update_byok_draft_field(&mut self, field: ByokDraftField, value: String) {
        if let Some(draft) = self.byok_draft.as_mut() {
            match draft.apply_field(field, value) {
                Ok(()) => self.show_byok_edit_view(),
                Err(message) => {
                    self.chat_widget.add_error_message(message);
                    self.show_byok_edit_view();
                }
            }
        } else {
            self.open_byok_manager();
        }
    }

    fn cycle_byok_provider_kind(&mut self) {
        if let Some(draft) = self.byok_draft.as_mut() {
            draft.cycle_provider_kind();
            self.show_byok_edit_view();
        } else {
            self.open_byok_manager();
        }
    }

    fn toggle_byok_think(&mut self) {
        if let Some(draft) = self.byok_draft.as_mut() {
            draft.toggle_think();
            self.show_byok_edit_view();
        } else {
            self.open_byok_manager();
        }
    }

    fn toggle_byok_postprocess(&mut self) {
        if let Some(draft) = self.byok_draft.as_mut() {
            draft.toggle_postprocess();
            self.show_byok_edit_view();
        } else {
            self.open_byok_manager();
        }
    }

    fn refresh_byok_provider_models(&mut self, provider_id: &str) {
        let Some(provider_snapshot) = self.settings.custom_provider(provider_id).cloned() else {
            self.chat_widget
                .add_error_message(format!("Provider `{provider_id}` not found"));
            self.open_byok_manager();
            return;
        };

        let api_key_result = self.auth_manager.custom_provider_api_key(provider_id);
        let api_key_opt = match api_key_result {
            Ok(value) => value,
            Err(err) => {
                warn!(provider_id = provider_id, %err, "failed to load provider API key");
                self.chat_widget.add_error_message(format!(
                    "Failed to read stored API key for `{provider_id}`: {err}"
                ));
                return;
            }
        };

        if provider_snapshot.provider_kind != ProviderKind::Ollama && api_key_opt.is_none() {
            self.chat_widget.add_error_message(format!(
                "Add an API key before refreshing models for `{provider_id}`."
            ));
            return;
        }

        self.chat_widget
            .add_info_message(format!("Refreshing models for `{provider_id}`…"), None);

        let tx = self.app_event_tx.clone();
        let provider_id_clone = provider_id.to_string();
        let api_key_clone = api_key_opt;
        tokio::spawn(async move {
            let result = fetch_custom_provider_models(
                &provider_id_clone,
                &provider_snapshot,
                api_key_clone.as_deref(),
            )
            .await
            .map_err(|err| err.to_string());
            tx.send(AppEvent::CustomProviderModelsFetched {
                provider_id: provider_id_clone,
                result,
            });
        });
    }

    fn show_byok_provider_models(&mut self, provider_id: &str) {
        let Some(provider) = self.settings.custom_provider(provider_id) else {
            self.chat_widget
                .add_error_message(format!("Provider `{provider_id}` not found"));
            self.open_byok_manager();
            return;
        };

        let mut items: Vec<SelectionItem> = Vec::new();

        if let Some(default_model) = provider.default_model.as_deref() {
            items.push(SelectionItem {
                name: format!("Default model: {default_model}"),
                description: Some("Used when no cached models are available.".to_string()),
                actions: Vec::new(),
                dismiss_on_select: false,
                ..Default::default()
            });
        }

        if let Some(models) = provider.cached_models.as_ref() {
            if models.is_empty() {
                items.push(SelectionItem {
                    name: "No cached models yet".to_string(),
                    description: Some("Refresh to discover models for this provider.".to_string()),
                    actions: Vec::new(),
                    dismiss_on_select: false,
                    ..Default::default()
                });
            } else {
                for model in models {
                    let is_current =
                        self.config.model == *model && self.config.model_provider_id == provider_id;
                    items.push(SelectionItem {
                        name: model.clone(),
                        description: None,
                        is_current,
                        actions: Vec::new(),
                        dismiss_on_select: false,
                        ..Default::default()
                    });
                }
            }
        } else {
            items.push(SelectionItem {
                name: "No cached models yet".to_string(),
                description: Some("Refresh to discover models for this provider.".to_string()),
                actions: Vec::new(),
                dismiss_on_select: false,
                ..Default::default()
            });
        }

        let refresh_id = provider_id.to_string();
        items.push(SelectionItem {
            name: "Refresh models".to_string(),
            description: Some("Test connectivity and update the cached list.".to_string()),
            actions: vec![Box::new(move |tx: &AppEventSender| {
                tx.send(AppEvent::RefreshByokProviderModels {
                    provider_id: refresh_id.clone(),
                });
            })],
            dismiss_on_select: true,
            ..Default::default()
        });

        let title = format!(
            "Models — {}",
            if provider.name.trim().is_empty() {
                provider_id.to_string()
            } else {
                provider.name.clone()
            }
        );

        let mut subtitle_parts = Vec::new();
        if let Some(url) = provider.base_url.as_deref() {
            subtitle_parts.push(url.to_string());
        }
        if let Some(refreshed) = provider.last_model_refresh.as_deref() {
            subtitle_parts.push(format!("Last refresh: {refreshed}"));
        } else {
            subtitle_parts.push("Never refreshed".to_string());
        }

        let params = SelectionViewParams {
            title: Some(title),
            subtitle: Some(subtitle_parts.join(" • ")),
            footer_hint: Some("Esc to close.".into()),
            items,
            ..Default::default()
        };

        self.chat_widget.show_selection_view(params);
    }

    fn submit_byok_form(
        &mut self,
        original_id: Option<String>,
        form: CustomProviderForm,
    ) -> color_eyre::Result<()> {
        let api_key_action = self
            .byok_draft
            .as_ref()
            .map(|draft| draft.api_key.clone())
            .unwrap_or(ApiKeyDraft::Unchanged);

        let name = form.name.trim();
        if name.is_empty() {
            return Err(eyre!("Display name is required"));
        }

        let provider_id_raw = form.provider_id.trim();
        if provider_id_raw.is_empty() {
            return Err(eyre!("Provider ID is required"));
        }
        if !is_valid_provider_id(provider_id_raw) {
            return Err(eyre!(
                "Provider ID must contain lowercase letters, digits, hyphen, or underscore"
            ));
        }
        if is_reserved_provider_id(provider_id_raw)
            && !matches!(original_id.as_deref(), Some(existing) if existing == provider_id_raw)
        {
            return Err(eyre!("Provider ID `{provider_id_raw}` is reserved"));
        }
        let provider_id = provider_id_raw.to_string();

        if let Some(existing) = self.settings.custom_provider(&provider_id) {
            if Some(&provider_id) != original_id.as_ref() {
                return Err(eyre!(
                    "A custom provider with ID `{provider_id}` already exists"
                ));
            } else if existing.name.is_empty() && name.is_empty() {
                // pass
            }
        }

        if let Some(existing_provider) = self
            .config
            .model_providers
            .get(&provider_id)
            .map(|info| info.name.clone())
            && Some(&provider_id) != original_id.as_ref()
        {
            return Err(eyre!(
                "Provider ID `{provider_id}` conflicts with built-in provider `{existing_provider}`"
            ));
        }

        let (mut base_url, mut wire_api) = normalize_custom_provider_base_url(form.base_url)?;

        let default_model = form.default_model.and_then(|value| {
            let trimmed = value.trim().to_string();
            if trimmed.eq_ignore_ascii_case("!clear") || trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        });

        let mut provider = if let Some(original) = original_id.as_ref()
            && let Some(existing) = self.settings.custom_provider(original).cloned()
        {
            existing
        } else {
            CustomProvider::default()
        };

        if form.provider_kind == ProviderKind::Ollama {
            if base_url.is_none() {
                base_url = Some(DEFAULT_OLLAMA_ENDPOINT.to_string());
            }
            wire_api = WireApi::Chat;
        }

        provider.name = name.to_string();
        provider.base_url = base_url;
        provider.wire_api = wire_api;
        provider.default_model = default_model;
        provider.extra_headers = match form.extra_headers.as_ref().map(|s| s.trim()) {
            None => provider.extra_headers.clone(),
            Some(value) if value.eq_ignore_ascii_case("!clear") || value.is_empty() => None,
            Some(value) => {
                let parsed = parse_extra_headers(value)?;
                if parsed.is_empty() {
                    None
                } else {
                    Some(parsed)
                }
            }
        };
        provider.provider_kind = form.provider_kind;
        provider.reasoning_controls.think_enabled =
            form.think_enabled && form.provider_kind == ProviderKind::Ollama;
        provider.reasoning_controls.postprocess_reasoning =
            if form.provider_kind == ProviderKind::Ollama {
                form.postprocess_reasoning
            } else {
                true
            };
        provider.reasoning_controls.anthropic_budget_tokens =
            if form.provider_kind == ProviderKind::AnthropicClaude {
                form.anthropic_budget_tokens
            } else {
                None
            };
        provider.reasoning_controls.anthropic_budget_weight =
            if form.provider_kind == ProviderKind::AnthropicClaude {
                form.anthropic_budget_weight
            } else {
                None
            };
        if form.provider_kind == ProviderKind::Ollama {
            provider.wire_api = WireApi::Chat;
        }
        if provider.added_at.is_none() {
            provider.added_at = Some(Utc::now().to_rfc3339());
        }
        if original_id.as_ref() != Some(&provider_id) {
            provider.cached_models = None;
            provider.last_model_refresh = None;
        }

        let mut preserved_api_key: Option<String> = None;
        let mut updated_settings = self.settings.clone();
        if let Some(original) = original_id.as_ref()
            && original != &provider_id
        {
            if let Ok(Some(existing_key)) = self.auth_manager.custom_provider_api_key(original) {
                preserved_api_key = Some(existing_key);
            }
            updated_settings.custom_providers_mut().remove(original);
        }
        updated_settings
            .custom_providers_mut()
            .insert(provider_id.clone(), provider);

        let persisted = codex_agentic_core::persist_settings(updated_settings.clone())
            .map_err(|err| eyre!("failed to persist custom provider to settings.json: {err}"))?;
        self.settings = persisted;

        codex_agentic_core::merge_custom_providers_into_config(&mut self.config, &self.settings);
        self.chat_widget
            .sync_model_providers(&self.config.model_providers);
        sanitize_reasoning_overrides(&mut self.config);
        sanitize_tool_overrides(&mut self.config);

        if let Some(original) = original_id.as_ref()
            && original != &provider_id
            && self.config.model_provider_id == *original
            && let Some(info) = self.config.model_providers.get(&provider_id).cloned()
        {
            self.config.model_provider_id = provider_id.clone();
            self.config.model_provider = info.clone();
            self.chat_widget.set_model_provider(&provider_id, &info);
        }

        if let Some(original) = original_id.as_ref()
            && original != &provider_id
        {
            let _ = self.auth_manager.delete_custom_provider_api_key(original);
        }

        match api_key_action {
            ApiKeyDraft::Unchanged => {
                if let Some(existing_key) = preserved_api_key.as_ref() {
                    self.auth_manager
                        .store_custom_provider_api_key(&provider_id, existing_key)?;
                }
            }
            ApiKeyDraft::Set(value) => {
                self.auth_manager
                    .store_custom_provider_api_key(&provider_id, &value)?;
            }
            ApiKeyDraft::Clear => {
                self.auth_manager
                    .delete_custom_provider_api_key(&provider_id)?;
            }
        }

        self.chat_widget
            .add_info_message(format!("Saved provider `{provider_id}`"), None);
        self.byok_draft = None;
        self.open_byok_manager();
        Ok(())
    }

    fn delete_custom_provider(&mut self, provider_id: &str) -> color_eyre::Result<()> {
        if self.settings.custom_provider(provider_id).is_none() {
            return Err(eyre!(format!("Provider `{provider_id}` not found")));
        }

        let mut updated_settings = self.settings.clone();
        updated_settings.custom_providers_mut().remove(provider_id);

        let persisted = codex_agentic_core::persist_settings(updated_settings)
            .map_err(|err| eyre!("failed to persist provider removal: {err}"))?;
        self.settings = persisted;

        codex_agentic_core::merge_custom_providers_into_config(&mut self.config, &self.settings);
        self.chat_widget
            .sync_model_providers(&self.config.model_providers);

        let _ = self
            .auth_manager
            .delete_custom_provider_api_key(provider_id);

        if self.config.model_provider_id == provider_id {
            self.config.model_provider_id = DEFAULT_OPENAI_PROVIDER_ID.to_string();
            if let Some(info) = self
                .config
                .model_providers
                .get(DEFAULT_OPENAI_PROVIDER_ID)
                .cloned()
            {
                self.config.model_provider = info.clone();
                self.config.model = self
                    .settings
                    .model
                    .as_ref()
                    .and_then(|m| m.default.clone())
                    .unwrap_or_else(|| OPENAI_DEFAULT_MODEL.to_string());
                self.chat_widget
                    .set_model_provider(DEFAULT_OPENAI_PROVIDER_ID, &info);
                refresh_model_metadata(&mut self.config);
                self.chat_widget.set_model(&self.config.model);
            }
        }

        self.chat_widget
            .add_info_message(format!("Deleted provider `{provider_id}`"), None);
        self.byok_draft = None;
        self.open_byok_manager();
        Ok(())
    }

    fn on_custom_provider_models_fetched(
        &mut self,
        provider_id: String,
        result: std::result::Result<Vec<String>, String>,
    ) {
        match result {
            Ok(models) => {
                let mut updated = self.settings.clone();
                if let Some(provider) = updated.custom_providers_mut().get_mut(&provider_id) {
                    provider.cached_models = Some(models);
                    provider.last_model_refresh = Some(Utc::now().to_rfc3339());
                    if let Ok(persisted) = codex_agentic_core::persist_settings(updated.clone()) {
                        self.settings = persisted;
                        codex_agentic_core::merge_custom_providers_into_config(
                            &mut self.config,
                            &self.settings,
                        );
                        self.chat_widget
                            .sync_model_providers(&self.config.model_providers);
                        self.chat_widget.add_info_message(
                            format!("Refreshed models for `{provider_id}`"),
                            None,
                        );
                    } else {
                        self.chat_widget.add_error_message(format!(
                            "Saved provider but failed to persist refreshed models for `{provider_id}`"
                        ));
                    }
                } else {
                    self.chat_widget.add_error_message(format!(
                        "Received model list for unknown provider `{provider_id}`"
                    ));
                }
            }
            Err(err) => {
                self.chat_widget
                    .add_error_message(format!("Failed to list models for `{provider_id}`: {err}"));
            }
        }
    }

    async fn handle_key_event(&mut self, tui: &mut tui::Tui, key_event: KeyEvent) {
        match key_event {
            KeyEvent {
                code: KeyCode::Char('t'),
                modifiers: crossterm::event::KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                ..
            } => {
                // Enter alternate screen and set viewport to full size.
                let _ = tui.enter_alt_screen();
                self.overlay = Some(Overlay::new_transcript(self.transcript_cells.clone()));
                tui.frame_requester().schedule_frame();
            }
            // Esc primes/advances backtracking only in normal (not working) mode
            // with an empty composer. In any other state, forward Esc so the
            // active UI (e.g. status indicator, modals, popups) handles it.
            KeyEvent {
                code: KeyCode::Esc,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            } => {
                if self.chat_widget.is_normal_backtrack_mode()
                    && self.chat_widget.composer_is_empty()
                {
                    self.handle_backtrack_esc_key(tui);
                } else {
                    self.chat_widget.handle_key_event(key_event);
                }
            }
            // Enter confirms backtrack when primed + count > 0. Otherwise pass to widget.
            KeyEvent {
                code: KeyCode::Enter,
                kind: KeyEventKind::Press,
                ..
            } if self.backtrack.primed
                && self.backtrack.nth_user_message != usize::MAX
                && self.chat_widget.composer_is_empty() =>
            {
                // Delegate to helper for clarity; preserves behavior.
                self.confirm_backtrack_from_main();
            }
            KeyEvent {
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            } => {
                // Any non-Esc key press should cancel a primed backtrack.
                // This avoids stale "Esc-primed" state after the user starts typing
                // (even if they later backspace to empty).
                if key_event.code != KeyCode::Esc && self.backtrack.primed {
                    self.reset_backtrack_state();
                }
                self.chat_widget.handle_key_event(key_event);
            }
            _ => {
                // Ignore Release key events.
            }
        };
    }
}

fn format_index_status(snapshot: &IndexStatusSnapshot) -> String {
    let indexed_label = format_age("Indexed", snapshot.manifest.updated_at);
    format!(
        "{} • {} files · {} chunks",
        indexed_label, snapshot.manifest.total_files, snapshot.manifest.total_chunks
    )
}

fn render_progress_bar(percent: f64) -> String {
    let clamped = percent.clamp(0.0, 1.0);
    let filled = (clamped * PROGRESS_BAR_WIDTH as f64).round() as usize;
    let filled = filled.min(PROGRESS_BAR_WIDTH);
    let empty = PROGRESS_BAR_WIDTH.saturating_sub(filled);
    let filled_str = "#".repeat(filled);
    let empty_str = ".".repeat(empty);
    let pct = (clamped * 100.0).round() as u32;
    format!("[{filled_str}{empty_str}] {pct:>3}%")
}

fn render_progress_status(state: &IndexProgressState) -> String {
    if state.total_chunks > 0 {
        let percent = state.processed_chunks as f64 / state.total_chunks.max(1) as f64;
        let bar = render_progress_bar(percent);
        let mut line = format!(
            "{} • {}/{} files • {}/{} chunks",
            bar,
            state.processed_files,
            state.total_files,
            state.processed_chunks,
            state.total_chunks
        );
        if let Some(path) = &state.current_path {
            line.push_str(" • ");
            line.push_str(path);
        }
        line
    } else {
        "Indexing…".to_string()
    }
}

fn spawn_status_refresh(sender: AppEventSender) {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(INDEX_STATUS_REFRESH_SECS));
        loop {
            interval.tick().await;
            sender.send(AppEvent::IndexStatusTick);
        }
    });
}

fn spawn_toast_tick(sender: AppEventSender) {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(INDEX_TOAST_TICK_SECS));
        loop {
            interval.tick().await;
            sender.send(AppEvent::IndexToastTick);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_backtrack::BacktrackState;
    use crate::app_backtrack::user_count;
    use crate::chatwidget::tests::make_chatwidget_manual_with_sender;
    use crate::file_search::FileSearchManager;
    use crate::history_cell::AgentMessageCell;
    use crate::history_cell::HistoryCell;
    use crate::history_cell::UserHistoryCell;
    use crate::history_cell::new_session_info;
    use codex_agentic_core::settings::Settings;
    use codex_core::AuthManager;
    use codex_core::CodexAuth;
    use codex_core::ConversationManager;
    use codex_core::protocol::SessionConfiguredEvent;
    use codex_protocol::ConversationId;
    use ratatui::prelude::Line;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    fn make_test_app() -> App {
        let (chat_widget, app_event_tx, _rx, _op_rx) = make_chatwidget_manual_with_sender();
        let config = chat_widget.config_ref().clone();

        let server = Arc::new(ConversationManager::with_auth(CodexAuth::from_api_key(
            "Test API Key",
        )));
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
        let file_search = FileSearchManager::new(config.cwd.clone(), app_event_tx.clone());
        let index_worker = IndexWorker::new(config.cwd.clone(), app_event_tx.clone());

        App {
            server,
            app_event_tx,
            chat_widget,
            auth_manager,
            config,
            active_profile: None,
            settings: Settings::default(),
            byok_draft: None,
            file_search,
            index_worker,
            index_status: None,
            index_progress: None,
            last_index_attempt: None,
            index_completion_toast_until: None,
            index_completion_message: None,
            transcript_cells: Vec::new(),
            overlay: None,
            deferred_history_lines: Vec::new(),
            has_emitted_history_lines: false,
            enhanced_keys_supported: false,
            commit_anim_running: Arc::new(AtomicBool::new(false)),
            backtrack: BacktrackState::default(),
            feedback: codex_feedback::CodexFeedback::new(),
            pending_update_action: None,
            memory_runtime: None,
        }
    }

    #[test]
    fn update_reasoning_effort_updates_config() {
        let mut app = make_test_app();
        app.config.model_reasoning_effort = Some(ReasoningEffortConfig::Medium);
        app.chat_widget
            .set_reasoning_effort(Some(ReasoningEffortConfig::Medium));

        app.on_update_reasoning_effort(Some(ReasoningEffortConfig::High));

        assert_eq!(
            app.config.model_reasoning_effort,
            Some(ReasoningEffortConfig::High)
        );
        assert_eq!(
            app.chat_widget.config_ref().model_reasoning_effort,
            Some(ReasoningEffortConfig::High)
        );
    }

    #[test]
    fn slugify_provider_id_from_name() {
        assert_eq!(slugify_provider_id("Anthropic Claude"), "anthropic-claude");
        assert_eq!(slugify_provider_id("  Mixed__Value  "), "mixed-value");
    }

    #[test]
    fn provider_id_validation() {
        assert!(is_valid_provider_id("custom-provider"));
        assert!(!is_valid_provider_id("Custom"));
        assert!(is_reserved_provider_id("openai"));
    }

    #[test]
    fn backtrack_selection_with_duplicate_history_targets_unique_turn() {
        let mut app = make_test_app();

        let user_cell = |text: &str| -> Arc<dyn HistoryCell> {
            Arc::new(UserHistoryCell {
                message: text.to_string(),
            }) as Arc<dyn HistoryCell>
        };
        let agent_cell = |text: &str| -> Arc<dyn HistoryCell> {
            Arc::new(AgentMessageCell::new(
                vec![Line::from(text.to_string())],
                true,
            )) as Arc<dyn HistoryCell>
        };

        let make_header = |is_first| {
            let event = SessionConfiguredEvent {
                session_id: ConversationId::new(),
                model: "gpt-test".to_string(),
                reasoning_effort: None,
                history_log_id: 0,
                history_entry_count: 0,
                initial_messages: None,
                rollout_path: PathBuf::new(),
            };
            Arc::new(new_session_info(
                app.chat_widget.config_ref(),
                event,
                is_first,
            )) as Arc<dyn HistoryCell>
        };

        // Simulate the transcript after trimming for a fork, replaying history, and
        // appending the edited turn. The session header separates the retained history
        // from the forked conversation's replayed turns.
        app.transcript_cells = vec![
            make_header(true),
            user_cell("first question"),
            agent_cell("answer first"),
            user_cell("follow-up"),
            agent_cell("answer follow-up"),
            make_header(false),
            user_cell("first question"),
            agent_cell("answer first"),
            user_cell("follow-up (edited)"),
            agent_cell("answer edited"),
        ];

        assert_eq!(user_count(&app.transcript_cells), 2);

        app.backtrack.base_id = Some(ConversationId::new());
        app.backtrack.primed = true;
        app.backtrack.nth_user_message = user_count(&app.transcript_cells).saturating_sub(1);

        app.confirm_backtrack_from_main();

        let (_, nth, prefill) = app.backtrack.pending.clone().expect("pending backtrack");
        assert_eq!(nth, 1);
        assert_eq!(prefill, "follow-up (edited)");
    }
}
