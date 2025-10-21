use anyhow::Context;
use anyhow::Result;
use codex_core::ModelProviderInfo;
use codex_core::config::Config;
use codex_core::default_client::create_client;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use tokio::runtime::Builder;
use tokio::runtime::Handle;
use tokio::task::block_in_place;

use crate::settings::CustomProvider;
use crate::settings::Settings;
use codex_core::WireApi;
use codex_protocol::config_types::ReasoningSummary;

/// Default endpoint for a local Ollama instance.
pub const DEFAULT_OLLAMA_ENDPOINT: &str = "http://localhost:11434";
pub const OSS_PROVIDER_ID: &str = "oss";
pub const DEFAULT_OPENAI_PROVIDER_ID: &str = "openai";

fn oss_endpoint(settings: &Settings) -> String {
    settings
        .providers
        .as_ref()
        .and_then(|providers| providers.oss.as_ref())
        .and_then(|oss| oss.endpoint.clone())
        .unwrap_or_else(|| DEFAULT_OLLAMA_ENDPOINT.to_string())
}

pub async fn list_models_for_provider(
    settings: &Settings,
    provider_id: &str,
) -> Result<Vec<String>> {
    match provider_id {
        OSS_PROVIDER_ID => {
            let endpoint = oss_endpoint(settings);
            let client = create_client();
            let tags_url = format!("{}/api/tags", endpoint.trim_end_matches('/'));
            let response = client
                .get(&tags_url)
                .send()
                .await
                .with_context(|| format!("failed to connect to Ollama at {endpoint}"))?;
            if !response.status().is_success() {
                return Ok(Vec::new());
            }
            let value = response
                .json::<JsonValue>()
                .await
                .with_context(|| format!("failed to decode response from {endpoint}"))?;
            let models = value
                .get("models")
                .and_then(|models| models.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.get("name").and_then(|name| name.as_str()))
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Ok(models)
        }
        other => {
            if let Some(provider) = settings.custom_provider(other) {
                if let Some(models) = provider.cached_models.clone() {
                    return Ok(models);
                }
                if let Some(default_model) = provider.default_model.clone() {
                    return Ok(vec![default_model]);
                }
                return Ok(Vec::new());
            }
            Err(anyhow::anyhow!("unsupported provider: {other}"))
        }
    }
}

pub fn list_models_for_provider_blocking(
    settings: &Settings,
    provider_id: &str,
) -> Result<Vec<String>> {
    if let Ok(handle) = Handle::try_current() {
        let settings_clone = settings.clone();
        let provider = provider_id.to_string();
        return block_in_place(move || {
            handle.block_on(async { list_models_for_provider(&settings_clone, &provider).await })
        });
    }

    Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build async runtime")?
        .block_on(list_models_for_provider(settings, provider_id))
}

pub fn provider_endpoint(settings: &Settings, provider_id: &str) -> Option<String> {
    match provider_id {
        OSS_PROVIDER_ID => Some(oss_endpoint(settings)),
        _ => settings
            .custom_provider(provider_id)
            .and_then(|provider| provider.base_url.clone()),
    }
}

pub fn resolve_provider(settings: &Settings, override_id: Option<&str>) -> Option<String> {
    if let Some(id) = override_id {
        return Some(id.to_string());
    }
    settings
        .model
        .as_ref()
        .and_then(|model| model.provider.clone())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelProviderResolution {
    pub model: Option<String>,
    pub provider_override: Option<String>,
    pub oss_active: bool,
    pub include_plan_tool: bool,
}

#[derive(Debug)]
pub struct ResolveModelProviderArgs<'a> {
    pub settings: &'a Settings,
    pub requested_model: Option<String>,
    pub force_oss: bool,
}

impl<'a> ResolveModelProviderArgs<'a> {
    pub fn new(settings: &'a Settings) -> Self {
        Self {
            settings,
            requested_model: None,
            force_oss: false,
        }
    }

    pub fn with_model(mut self, model: Option<String>) -> Self {
        self.requested_model = model;
        self
    }

    pub fn with_force_oss(mut self, force: bool) -> Self {
        self.force_oss = force;
        self
    }
}

pub fn resolve_model_provider(args: ResolveModelProviderArgs<'_>) -> ModelProviderResolution {
    let ResolveModelProviderArgs {
        settings,
        mut requested_model,
        force_oss,
    } = args;

    let settings_model = settings
        .model
        .as_ref()
        .and_then(|model| model.default.clone());
    let settings_provider = settings
        .model
        .as_ref()
        .and_then(|model| model.provider.clone());

    let mut provider_override = if force_oss {
        Some(OSS_PROVIDER_ID.to_string())
    } else {
        settings_provider
    };

    let mut model = requested_model.take().or_else(|| {
        if force_oss {
            Some(codex_ollama::DEFAULT_OSS_MODEL.to_string())
        } else {
            settings_model
        }
    });

    if provider_override.is_none() {
        provider_override = Some(DEFAULT_OPENAI_PROVIDER_ID.to_string());
    }

    if matches!(provider_override.as_deref(), Some(id) if id == OSS_PROVIDER_ID) && model.is_none()
    {
        model = Some(codex_ollama::DEFAULT_OSS_MODEL.to_string());
    }

    if let Some(id) = provider_override.as_deref()
        && let Some(custom) = settings.custom_provider(id)
    {
        if model.is_none() {
            model = custom.default_model.clone();
        }

        if let Some(ref requested) = model
            && !custom_supports_model(custom, requested)
        {
            provider_override = Some(DEFAULT_OPENAI_PROVIDER_ID.to_string());
        }
    }

    if provider_override
        .as_ref()
        .is_none_or(|id| id == DEFAULT_OPENAI_PROVIDER_ID)
        && let Some(model_name) = model.as_ref()
        && let Some((provider_id, custom)) = custom_provider_for_model(settings, model_name)
    {
        provider_override = Some(provider_id);
        if model.is_none() {
            model = custom.default_model;
        }
    }

    if let Some(model_name) = model.as_ref() {
        if model_name.contains(':') {
            let is_custom_provider = provider_override
                .as_deref()
                .and_then(|id| settings.custom_provider(id))
                .is_some();
            if !is_custom_provider {
                provider_override = Some(OSS_PROVIDER_ID.to_string());
            }
        } else if matches!(provider_override.as_deref(), Some(id) if id == OSS_PROVIDER_ID) {
            if force_oss {
                model = Some(codex_ollama::DEFAULT_OSS_MODEL.to_string());
            } else {
                // A colon-free slug implies an OpenAI (Responses) model. Even if
                // the saved provider prefers OSS, we switch back to the default
                // OpenAI provider to avoid mismatched pairings.
                provider_override = Some(DEFAULT_OPENAI_PROVIDER_ID.to_string());
            }
        }
    } else if matches!(provider_override.as_deref(), Some(id) if id == OSS_PROVIDER_ID) {
        model = Some(codex_ollama::DEFAULT_OSS_MODEL.to_string());
    } else if let Some(id) = provider_override.as_deref()
        && let Some(custom) = settings.custom_provider(id)
        && model.is_none()
    {
        model = custom.default_model.clone();
    }

    let provider_id = provider_override
        .as_deref()
        .unwrap_or(DEFAULT_OPENAI_PROVIDER_ID);
    let oss_active = provider_id == OSS_PROVIDER_ID;
    let include_plan_tool = plan_tool_supported(provider_id, model.as_deref());

    ModelProviderResolution {
        model,
        provider_override,
        oss_active,
        include_plan_tool,
    }
}

pub fn plan_tool_supported(provider_id: &str, model: Option<&str>) -> bool {
    if provider_id == OSS_PROVIDER_ID {
        let slug = model.unwrap_or(codex_ollama::DEFAULT_OSS_MODEL);
        oss_model_supports_tools(slug)
    } else if let Some(custom) = crate::settings::global().custom_provider(provider_id) {
        custom.plan_tool_enabled()
    } else {
        true
    }
}

fn oss_model_supports_tools(model: &str) -> bool {
    let slug_without_namespace = model
        .rsplit_once('/')
        .map(|(_, slug)| slug)
        .unwrap_or(model);
    let slug_without_variant = slug_without_namespace
        .split_once(':')
        .map(|(slug, _)| slug)
        .unwrap_or(slug_without_namespace);

    slug_without_variant.starts_with("gpt-oss") && !slug_without_namespace.contains("qwen2.5vl")
}

pub fn custom_providers(settings: &Settings) -> BTreeMap<String, CustomProvider> {
    settings
        .providers
        .as_ref()
        .map(|providers| providers.custom.clone())
        .unwrap_or_default()
}

pub fn custom_provider_model_info(provider_id: &str, custom: &CustomProvider) -> ModelProviderInfo {
    let mut info = ModelProviderInfo {
        name: custom.name.clone(),
        base_url: custom.base_url.clone(),
        env_key: None,
        env_key_instructions: None,
        wire_api: custom.wire_api(),
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: None,
        stream_max_retries: None,
        stream_idle_timeout_ms: None,
        requires_openai_auth: false,
    };

    if matches!(info.wire_api, WireApi::Responses)
        && needs_coding_plan_chat_mode(provider_id, &info)
    {
        info.wire_api = WireApi::Chat;
    }

    info
}

pub fn merge_custom_providers_into_config(config: &mut Config, settings: &Settings) {
    for (provider_id, custom) in custom_providers(settings) {
        config.model_providers.insert(
            provider_id.clone(),
            custom_provider_model_info(&provider_id, &custom),
        );
    }
}

fn custom_supports_model(custom: &CustomProvider, model: &str) -> bool {
    if let Some(models) = custom.cached_models.as_ref()
        && models.iter().any(|m| m == model)
    {
        return true;
    }
    if let Some(default_model) = custom.default_model.as_ref()
        && default_model == model
    {
        return true;
    }
    false
}

fn custom_provider_for_model(settings: &Settings, model: &str) -> Option<(String, CustomProvider)> {
    settings.providers.as_ref().and_then(|providers| {
        providers
            .custom
            .iter()
            .find(|(_, provider)| custom_supports_model(provider, model))
            .map(|(id, provider)| (id.clone(), provider.clone()))
    })
}

fn needs_coding_plan_chat_mode(provider_id: &str, info: &ModelProviderInfo) -> bool {
    provider_id.eq_ignore_ascii_case("zai")
        || info
            .base_url
            .as_deref()
            .map(|url| url.contains("open.bigmodel.cn/api/coding/paas/"))
            .unwrap_or(false)
}

/// Clear reasoning overrides when the selected model does not support reasoning summaries.
pub fn sanitize_reasoning_overrides(config: &mut Config) {
    if !config.model_family.supports_reasoning_summaries {
        config.model_reasoning_effort = None;
        config.model_reasoning_summary = ReasoningSummary::default();
    }
}

fn provider_supports_tool_calls(provider_id: &str, provider: &ModelProviderInfo) -> bool {
    if provider_id.eq_ignore_ascii_case("zai") {
        return false;
    }

    provider
        .base_url
        .as_deref()
        .map(|url| !url.contains("open.bigmodel.cn/api/coding/paas/"))
        .unwrap_or(true)
}

/// Disable tool surfaces that the active provider cannot support.
pub fn sanitize_tool_overrides(config: &mut Config) {
    if provider_supports_tool_calls(&config.model_provider_id, &config.model_provider) {
        return;
    }

    config.include_plan_tool = false;
    config.include_apply_patch_tool = false;
    config.include_view_image_tool = false;
    config.tools_web_search_request = false;
}

#[derive(Deserialize)]
struct ListModelsResponse {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

pub async fn fetch_custom_provider_models(
    provider_id: &str,
    provider: &CustomProvider,
    api_key: &str,
) -> Result<Vec<String>> {
    let base_url = provider
        .base_url
        .clone()
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let endpoint = format!("{}/models", base_url.trim_end_matches('/'));

    let client = create_client();
    let response = client
        .get(&endpoint)
        .bearer_auth(api_key)
        .header("Accept", "application/json")
        .send()
        .await
        .with_context(|| format!("failed to contact {endpoint} for provider {provider_id}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "provider {provider_id} returned {status} when listing models: {body}"
        ));
    }

    let mut models: Vec<String> = response
        .json::<ListModelsResponse>()
        .await
        .with_context(|| format!("failed to parse /models response for provider {provider_id}"))?
        .data
        .into_iter()
        .map(|entry| entry.id)
        .collect();

    models.sort();
    models.dedup();
    Ok(models)
}
