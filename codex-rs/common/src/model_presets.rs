use codex_app_server_protocol::AuthMode;
use codex_core::protocol_config_types::ReasoningEffort;

const OPENAI_PROVIDER_ID: &str = "openai";
const OPENAI_PROVIDER_LABEL: &str = "OpenAI";

/// A reasoning effort option that can be surfaced for a model.
#[derive(Debug, Clone)]
pub struct ReasoningEffortPreset {
    /// Effort level that the model supports.
    pub effort: ReasoningEffort,
    /// Short human description shown next to the effort in UIs.
    pub description: String,
}

/// Metadata describing a Codex-supported model.
#[derive(Debug, Clone)]
pub struct ModelPreset {
    /// Stable identifier for the preset.
    pub id: String,
    /// Model slug (e.g., "gpt-5").
    pub model: String,
    /// Display name shown in UIs.
    pub display_name: String,
    /// Short human description shown in UIs.
    pub description: String,
    /// Reasoning effort applied when none is explicitly chosen.
    pub default_reasoning_effort: ReasoningEffort,
    /// Supported reasoning effort options.
    pub supported_reasoning_efforts: Vec<ReasoningEffortPreset>,
    /// Whether this is the default model for new users.
    pub is_default: bool,
    /// Identifier of the provider that surfaces this model, if known.
    pub provider_id: Option<String>,
    /// Human-friendly provider label for UI display.
    pub provider_label: Option<String>,
}

impl ModelPreset {
    pub fn provider_label(&self) -> &str {
        self.provider_label.as_deref().unwrap_or(OPENAI_PROVIDER_ID)
    }
}

fn builtin_presets() -> Vec<ModelPreset> {
    vec![
        ModelPreset {
            id: "gpt-5-codex".to_string(),
            model: "gpt-5-codex".to_string(),
            display_name: "gpt-5-codex".to_string(),
            description: "Optimized for coding tasks with many tools.".to_string(),
            default_reasoning_effort: ReasoningEffort::Medium,
            supported_reasoning_efforts: vec![
                ReasoningEffortPreset {
                    effort: ReasoningEffort::Low,
                    description: "Fastest responses with limited reasoning".to_string(),
                },
                ReasoningEffortPreset {
                    effort: ReasoningEffort::Medium,
                    description: "Dynamically adjusts reasoning based on the task".to_string(),
                },
                ReasoningEffortPreset {
                    effort: ReasoningEffort::High,
                    description: "Maximizes reasoning depth for complex or ambiguous problems"
                        .to_string(),
                },
            ],
            is_default: true,
            provider_id: Some(OPENAI_PROVIDER_ID.to_string()),
            provider_label: Some(OPENAI_PROVIDER_LABEL.to_string()),
        },
        ModelPreset {
            id: "gpt-5".to_string(),
            model: "gpt-5".to_string(),
            display_name: "gpt-5".to_string(),
            description: "Broad world knowledge with strong general reasoning.".to_string(),
            default_reasoning_effort: ReasoningEffort::Medium,
            supported_reasoning_efforts: vec![
                ReasoningEffortPreset {
                    effort: ReasoningEffort::Minimal,
                    description: "Fastest responses with little reasoning".to_string(),
                },
                ReasoningEffortPreset {
                    effort: ReasoningEffort::Low,
                    description: "Balances speed with some reasoning; useful for straightforward queries and short explanations".to_string(),
                },
                ReasoningEffortPreset {
                    effort: ReasoningEffort::Medium,
                    description: "Provides a solid balance of reasoning depth and latency for general-purpose tasks".to_string(),
                },
                ReasoningEffortPreset {
                    effort: ReasoningEffort::High,
                    description: "Maximizes reasoning depth for complex or ambiguous problems"
                        .to_string(),
                },
            ],
            is_default: false,
            provider_id: Some(OPENAI_PROVIDER_ID.to_string()),
            provider_label: Some(OPENAI_PROVIDER_LABEL.to_string()),
        },
    ]
}

pub fn builtin_model_presets(_auth_mode: Option<AuthMode>) -> Vec<ModelPreset> {
    builtin_presets()
}
