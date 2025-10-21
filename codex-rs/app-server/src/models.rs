use codex_app_server_protocol::Model;
use codex_app_server_protocol::ReasoningEffortOption;
use codex_common::model_presets::ModelPreset;
use codex_common::model_presets::ReasoningEffortPreset;
use codex_common::model_presets::builtin_model_presets;
use codex_core::protocol_config_types::ReasoningEffort;

pub fn supported_models() -> Vec<Model> {
    builtin_model_presets(None)
        .into_iter()
        .map(model_from_preset)
        .collect()
}

fn model_from_preset(preset: ModelPreset) -> Model {
    let supported = normalize_supported_efforts(
        preset.default_reasoning_effort,
        preset.supported_reasoning_efforts,
    );
    Model {
        id: preset.id,
        model: preset.model,
        display_name: preset.display_name,
        description: preset.description,
        supported_reasoning_efforts: supported
            .iter()
            .map(|preset| ReasoningEffortOption {
                reasoning_effort: preset.effort,
                description: preset.description.clone(),
            })
            .collect(),
        default_reasoning_effort: preset.default_reasoning_effort,
        is_default: preset.is_default,
    }
}

fn normalize_supported_efforts(
    default: ReasoningEffort,
    mut efforts: Vec<ReasoningEffortPreset>,
) -> Vec<ReasoningEffortPreset> {
    if efforts.is_empty() {
        efforts.push(ReasoningEffortPreset {
            effort: default,
            description: String::new(),
        });
    }
    efforts
}
