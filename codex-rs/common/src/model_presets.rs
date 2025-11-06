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
        self.provider_label.as_deref().unwrap_or(OPENAI_PROVIDER_LABEL)
    }
}

#[derive(Clone, Copy)]
struct StaticReasoningEffortPreset {
    effort: ReasoningEffort,
    description: &'static str,
}

#[derive(Clone, Copy)]
struct StaticModelPreset {
    id: &'static str,
    model: &'static str,
    display_name: &'static str,
    description: &'static str,
    default_reasoning_effort: ReasoningEffort,
    supported_reasoning_efforts: &'static [StaticReasoningEffortPreset],
    is_default: bool,
}

const PRESETS: &[StaticModelPreset] = &[
    StaticModelPreset {
        id: "gpt-5-codex",
        model: "gpt-5-codex",
        display_name: "gpt-5-codex",
        description: "Optimized for codex.",
        default_reasoning_effort: ReasoningEffort::Medium,
        supported_reasoning_efforts: &[
            StaticReasoningEffortPreset {
                effort: ReasoningEffort::Low,
                description: "Fastest responses with limited reasoning",
            },
            StaticReasoningEffortPreset {
                effort: ReasoningEffort::Medium,
                description: "Dynamically adjusts reasoning based on the task",
            },
            StaticReasoningEffortPreset {
                effort: ReasoningEffort::High,
                description: "Maximizes reasoning depth for complex or ambiguous problems",
            },
        ],
        is_default: true,
    },
    StaticModelPreset {
        id: "gpt-5-codex-mini",
        model: "gpt-5-codex-mini",
        display_name: "gpt-5-codex-mini",
        description: "Optimized for codex. Cheaper, faster, and less capable.",
        default_reasoning_effort: ReasoningEffort::Medium,
        supported_reasoning_efforts: &[
            StaticReasoningEffortPreset {
                effort: ReasoningEffort::Medium,
                description: "Dynamically adjusts reasoning based on the task",
            },
            StaticReasoningEffortPreset {
                effort: ReasoningEffort::High,
                description: "Maximizes reasoning depth for complex or ambiguous problems",
            },
        ],
        is_default: false,
    },
    StaticModelPreset {
        id: "gpt-5",
        model: "gpt-5",
        display_name: "gpt-5",
        description: "Broad world knowledge with strong general reasoning.",
        default_reasoning_effort: ReasoningEffort::Medium,
        supported_reasoning_efforts: &[
            StaticReasoningEffortPreset {
                effort: ReasoningEffort::Minimal,
                description: "Fastest responses with little reasoning",
            },
            StaticReasoningEffortPreset {
                effort: ReasoningEffort::Low,
                description: "Balances speed with some reasoning; useful for straightforward queries and short explanations",
            },
            StaticReasoningEffortPreset {
                effort: ReasoningEffort::Medium,
                description: "Provides a solid balance of reasoning depth and latency for general-purpose tasks",
            },
            StaticReasoningEffortPreset {
                effort: ReasoningEffort::High,
                description: "Maximizes reasoning depth for complex or ambiguous problems",
            },
        ],
        is_default: false,
    },
];

impl From<&StaticReasoningEffortPreset> for ReasoningEffortPreset {
    fn from(value: &StaticReasoningEffortPreset) -> Self {
        Self {
            effort: value.effort,
            description: value.description.to_string(),
        }
    }
}

impl From<&StaticModelPreset> for ModelPreset {
    fn from(value: &StaticModelPreset) -> Self {
        Self {
            id: value.id.to_string(),
            model: value.model.to_string(),
            display_name: value.display_name.to_string(),
            description: value.description.to_string(),
            default_reasoning_effort: value.default_reasoning_effort,
            supported_reasoning_efforts: value
                .supported_reasoning_efforts
                .iter()
                .map(ReasoningEffortPreset::from)
                .collect(),
            is_default: value.is_default,
            provider_id: Some(OPENAI_PROVIDER_ID.to_string()),
            provider_label: Some(OPENAI_PROVIDER_LABEL.to_string()),
        }
    }
}

pub fn builtin_model_presets(auth_mode: Option<AuthMode>) -> Vec<ModelPreset> {
    let allow_codex_mini = matches!(auth_mode, Some(AuthMode::ChatGPT));
    PRESETS
        .iter()
        .filter(|preset| allow_codex_mini || preset.id != "gpt-5-codex-mini")
        .map(ModelPreset::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_default_model_is_configured() {
        let defaults = PRESETS.iter().filter(|preset| preset.is_default).count();
        assert_eq!(defaults, 1);
    }
}
