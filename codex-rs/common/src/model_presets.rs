use codex_app_server_protocol::AuthMode;
use codex_core::protocol_config_types::ReasoningEffort;

/// A simple preset pairing a model slug with a reasoning effort.
#[derive(Debug, Clone)]
pub struct ModelPreset {
    /// Stable identifier for the preset.
    pub id: String,
    /// Display label shown in UIs.
    pub label: String,
    /// Short human description shown next to the label in UIs.
    pub description: String,
    /// Model slug (e.g., "gpt-5").
    pub model: String,
    /// Reasoning effort to apply for this preset.
    pub effort: Option<ReasoningEffort>,
}

pub fn builtin_model_presets(_auth_mode: Option<AuthMode>) -> Vec<ModelPreset> {
    vec![
        ModelPreset {
            id: "gpt-5-codex-low".to_string(),
            label: "gpt-5-codex low".to_string(),
            description: "Fastest responses with limited reasoning".to_string(),
            model: "gpt-5-codex".to_string(),
            effort: Some(ReasoningEffort::Low),
        },
        ModelPreset {
            id: "gpt-5-codex-medium".to_string(),
            label: "gpt-5-codex medium".to_string(),
            description: "Dynamically adjusts reasoning based on the task".to_string(),
            model: "gpt-5-codex".to_string(),
            effort: Some(ReasoningEffort::Medium),
        },
        ModelPreset {
            id: "gpt-5-codex-high".to_string(),
            label: "gpt-5-codex high".to_string(),
            description: "Maximizes reasoning depth for complex or ambiguous problems".to_string(),
            model: "gpt-5-codex".to_string(),
            effort: Some(ReasoningEffort::High),
        },
        ModelPreset {
            id: "gpt-5-minimal".to_string(),
            label: "gpt-5 minimal".to_string(),
            description: "Fastest responses with little reasoning".to_string(),
            model: "gpt-5".to_string(),
            effort: Some(ReasoningEffort::Minimal),
        },
        ModelPreset {
            id: "gpt-5-low".to_string(),
            label: "gpt-5 low".to_string(),
            description: "Balances speed with some reasoning; useful for straightforward queries and short explanations".to_string(),
            model: "gpt-5".to_string(),
            effort: Some(ReasoningEffort::Low),
        },
        ModelPreset {
            id: "gpt-5-medium".to_string(),
            label: "gpt-5 medium".to_string(),
            description: "Provides a solid balance of reasoning depth and latency for general-purpose tasks".to_string(),
            model: "gpt-5".to_string(),
            effort: Some(ReasoningEffort::Medium),
        },
        ModelPreset {
            id: "gpt-5-high".to_string(),
            label: "gpt-5 high".to_string(),
            description: "Maximizes reasoning depth for complex or ambiguous problems".to_string(),
            model: "gpt-5".to_string(),
            effort: Some(ReasoningEffort::High),
        },
    ]
}
