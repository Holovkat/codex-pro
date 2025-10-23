use std::path::PathBuf;

use codex_agentic_core::index::events::IndexEvent;
use codex_agentic_core::index::query::QueryHit;
use codex_common::model_presets::ModelPreset;
use codex_core::config_types::ProviderKind;
use codex_core::protocol::ConversationPathResponseEvent;
use codex_core::protocol::Event;
use codex_file_search::FileMatch;

use crate::bottom_pane::ApprovalRequest;
use crate::history_cell::HistoryCell;
use crate::index_delta::SnapshotDiff;
use crate::index_status::IndexStatusSnapshot;

use codex_core::protocol::AskForApproval;
use codex_core::protocol::SandboxPolicy;
use codex_core::protocol_config_types::ReasoningEffort;

#[derive(Debug, Clone, Default)]
pub(crate) struct CustomProviderForm {
    pub name: String,
    pub provider_id: String,
    pub base_url: Option<String>,
    pub default_model: Option<String>,
    pub extra_headers: Option<String>,
    pub provider_kind: ProviderKind,
    pub think_enabled: bool,
    pub postprocess_reasoning: bool,
    pub anthropic_budget_tokens: Option<u32>,
    pub anthropic_budget_weight: Option<f32>,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub(crate) enum AppEvent {
    CodexEvent(Event),

    /// Start a new session.
    NewSession,

    /// Request to exit the application gracefully.
    ExitRequest,

    /// Forward an `Op` to the Agent. Using an `AppEvent` for this avoids
    /// bubbling channels through layers of widgets.
    CodexOp(codex_core::protocol::Op),

    /// Kick off an asynchronous file search for the given query (text after
    /// the `@`). Previous searches may be cancelled by the app layer so there
    /// is at most one in-flight search.
    StartFileSearch(String),

    /// Result of a completed asynchronous file search. The `query` echoes the
    /// original search term so the UI can decide whether the results are
    /// still relevant.
    FileSearchResult {
        query: String,
        matches: Vec<FileMatch>,
    },

    /// Result of computing a `/diff` command.
    DiffResult(String),

    /// Index worker emitted an event during build.
    IndexProgress(IndexEvent),

    /// Refresh cached index status snapshot.
    IndexStatusUpdated(Option<IndexStatusSnapshot>),

    /// Periodic refresh tick for index status labels.
    IndexStatusTick,

    /// Frequent tick to retire transient toasts.
    IndexToastTick,

    /// Filesystem delta detected that should trigger an index refresh.
    IndexDeltaDetected(SnapshotDiff),

    /// Request to start a semantic index build.
    StartIndexBuild,

    /// Launch the search manager modal after `/search-code` without arguments.
    OpenSearchManager,

    /// Prompt for a search query.
    SearchCodePrompt,

    /// Prompt for a new minimum confidence value.
    SearchConfidencePrompt,

    /// Request to run a semantic code search for the given query.
    SearchCodeRequested {
        query: String,
    },

    /// Persist a new minimum confidence value from the prompt.
    SearchConfidenceSubmitted {
        raw: String,
    },

    /// Completed search results for a `/search-code` query.
    SearchCodeResult {
        query: String,
        confidence: f32,
        hits: Vec<QueryHit>,
    },

    /// Error while running a `/search-code` query.
    SearchCodeError {
        query: String,
        error: String,
    },

    InsertHistoryCell(Box<dyn HistoryCell>),

    StartCommitAnimation,
    StopCommitAnimation,
    CommitTick,

    /// Update the current reasoning effort in the running app and widget.
    UpdateReasoningEffort(Option<ReasoningEffort>),

    /// Update the active model provider in the running app and widget.
    UpdateModelProvider(String),

    /// Update the current model slug in the running app and widget.
    UpdateModel(String),

    /// Persist the selected model and reasoning effort to the appropriate config.
    PersistModelSelection {
        model: String,
        effort: Option<ReasoningEffort>,
    },

    /// Open the reasoning selection popup after picking a model.
    OpenReasoningPopup {
        model: String,
        provider_id: String,
        presets: Vec<ModelPreset>,
    },

    /// Update the current approval policy in the running app and widget.
    UpdateAskForApprovalPolicy(AskForApproval),

    /// Update the current sandbox policy in the running app and widget.
    UpdateSandboxPolicy(SandboxPolicy),

    /// Forwarded conversation history snapshot from the current conversation.
    ConversationHistory(ConversationPathResponseEvent),

    /// Open the branch picker option from the review popup.
    OpenReviewBranchPicker(PathBuf),

    /// Open the commit picker option from the review popup.
    OpenReviewCommitPicker(PathBuf),

    /// Open the custom prompt option from the review popup.
    OpenReviewCustomPrompt,

    /// Open the approval popup.
    FullScreenApprovalRequest(ApprovalRequest),

    /// Launch the BYOK manager modal.
    OpenByokManager,

    /// Show actions for a specific custom provider.
    ShowByokProviderActions {
        provider_id: String,
    },

    /// Begin editing or creating a BYOK provider.
    StartByokEdit {
        existing_id: Option<String>,
    },

    /// Submit BYOK provider form data.
    SubmitByokForm {
        original_id: Option<String>,
        form: CustomProviderForm,
    },

    /// Confirm and perform provider deletion.
    DeleteCustomProvider {
        provider_id: String,
    },

    /// Result of asynchronously fetching models for a custom provider.
    CustomProviderModelsFetched {
        provider_id: String,
        result: std::result::Result<Vec<String>, String>,
    },

    /// Begin editing a draft field for a custom provider.
    BeginByokFieldEdit {
        field: ByokDraftField,
    },

    /// Apply a new value to a draft field.
    UpdateByokDraftField {
        field: ByokDraftField,
        value: String,
    },

    /// Cycle the provider kind for the BYOK draft.
    CycleByokProviderKind,

    /// Toggle Ollama thinking flag for the BYOK draft.
    ToggleByokThink,

    /// Toggle postprocess reasoning flag for the BYOK draft.
    ToggleByokPostprocess,

    /// Refresh models by testing connectivity for a provider.
    RefreshByokProviderModels {
        provider_id: String,
    },

    /// Show cached models for a provider.
    ShowByokProviderModels {
        provider_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ByokDraftField {
    Name,
    ProviderId,
    BaseUrl,
    DefaultModel,
    ApiKey,
    ExtraHeaders,
    AnthropicBudgetTokens,
    AnthropicBudgetWeight,
}
