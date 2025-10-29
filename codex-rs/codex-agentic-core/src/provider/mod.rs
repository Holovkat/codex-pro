use anyhow::Context;
use anyhow::Result;
use codex_core::ModelProviderInfo;
use codex_core::config::Config;
use codex_core::config_types::ProviderKind;
use codex_core::default_client::create_client;
use codex_core::features::Feature;
use codex_core::features::Features;
use codex_core::model_family::find_family_for_model;
use codex_core::oss_model_supports_tools;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::collections::HashMap;
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

fn normalized_provider_id(id: &str) -> String {
    let mut normalized = String::with_capacity(id.len());
    for ch in id.chars() {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch.to_ascii_lowercase());
        }
    }
    normalized
}

fn is_zai_provider_id(id: &str) -> bool {
    normalized_provider_id(id) == "zai"
}

fn is_zai_base_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains("open.bigmodel.cn/api/coding/paas/")
        || lower.contains("api.z.ai/api/coding/paas/")
        || lower.contains("api.z.ai/api/paas/")
}

fn oss_endpoint(settings: &Settings) -> String {
    if let Some(provider) = settings
        .custom_provider("ollama")
        .and_then(|provider| provider.base_url.clone())
    {
        return provider;
    }

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
            } else if !find_family_for_model(model_name)
                .map(|family| family.family.starts_with("gpt-oss"))
                .unwrap_or(false)
            {
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
    ModelProviderResolution {
        model,
        provider_override,
        oss_active,
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
        base_url: None,
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
        provider_kind: custom.provider_kind,
        reasoning_controls: custom.reasoning_controls.clone(),
    };

    info.base_url = match custom.provider_kind {
        ProviderKind::Ollama => {
            let root = custom
                .base_url
                .clone()
                .unwrap_or_else(|| DEFAULT_OLLAMA_ENDPOINT.to_string());
            let trimmed = root.trim_end_matches('/');
            let chat_base = if let Some(stripped) = trimmed.strip_suffix("/v1") {
                stripped.to_string()
            } else {
                trimmed.to_string()
            };
            Some(format!("{chat_base}/v1"))
        }
        _ => custom.base_url.clone(),
    };

    if matches!(info.wire_api, WireApi::Responses)
        && needs_coding_plan_chat_mode(provider_id, &info)
    {
        info.wire_api = WireApi::Chat;
    }

    if let Some(headers) = custom.extra_headers.as_ref() {
        let map: HashMap<String, String> = headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !map.is_empty() {
            info.http_headers = Some(map);
        }
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
    is_zai_provider_id(provider_id)
        || info
            .base_url
            .as_deref()
            .map(is_zai_base_url)
            .unwrap_or(false)
}

/// Clear reasoning overrides when the selected model does not support reasoning summaries.
pub fn sanitize_reasoning_overrides(config: &mut Config) {
    if !config.model_family.supports_reasoning_summaries {
        config.model_reasoning_effort = None;
        config.model_reasoning_summary = ReasoningSummary::default();
    }
}

/// Disable tool surfaces that the active provider cannot support.
pub fn sanitize_tool_overrides(config: &mut Config) {
    let provider_allows_tools = config.provider_allows_tool_calls();

    let apply_patch_enabled = provider_allows_tools && config.include_apply_patch_tool;
    config.include_apply_patch_tool = apply_patch_enabled;
    toggle_feature(
        &mut config.features,
        Feature::ApplyPatchFreeform,
        apply_patch_enabled,
    );

    let view_image_enabled = provider_allows_tools && config.include_view_image_tool;
    config.include_view_image_tool = view_image_enabled;
    toggle_feature(
        &mut config.features,
        Feature::ViewImageTool,
        view_image_enabled,
    );

    let web_search_enabled = provider_allows_tools && config.tools_web_search_request;
    config.tools_web_search_request = web_search_enabled;
    toggle_feature(
        &mut config.features,
        Feature::WebSearchRequest,
        web_search_enabled,
    );

    if !provider_allows_tools {
        config.use_experimental_streamable_shell_tool = false;
        config.use_experimental_unified_exec_tool = false;
        config.features.disable(Feature::StreamableShell);
        config.features.disable(Feature::UnifiedExec);
        config.features.disable(Feature::RmcpClient);
    }
}

fn toggle_feature(features: &mut Features, feature: Feature, enabled: bool) {
    if enabled {
        features.enable(feature);
    } else {
        features.disable(feature);
    }
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
    api_key: Option<&str>,
) -> Result<Vec<String>> {
    let client = create_client();

    if matches!(provider.provider_kind, ProviderKind::Ollama) {
        let base_url = provider
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_OLLAMA_ENDPOINT.to_string());
        let trimmed = base_url.trim_end_matches('/');
        let host = trimmed
            .strip_suffix("/v1")
            .unwrap_or(trimmed)
            .trim_end_matches('/');
        let endpoint = format!("{host}/api/tags");

        let mut request = client.get(&endpoint);
        if let Some(headers) = provider.extra_headers.as_ref() {
            for (key, value) in headers {
                request = request.header(key, value);
            }
        }

        let response = request
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

        let value = response.json::<JsonValue>().await.with_context(|| {
            format!("failed to parse /api/tags response for provider {provider_id}")
        })?;

        let mut models: Vec<String> = value
            .get("models")
            .and_then(|models| models.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        item.get("name")
                            .or_else(|| item.get("model"))
                            .and_then(|v| v.as_str())
                    })
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        models.sort();
        models.dedup();
        return Ok(models);
    }

    let api_key = api_key.ok_or_else(|| {
        anyhow::anyhow!("provider {provider_id} requires an API key to refresh cached models")
    })?;

    let base_url = provider
        .base_url
        .clone()
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let endpoint = format!("{}/models", base_url.trim_end_matches('/'));

    let mut request = client
        .get(&endpoint)
        .bearer_auth(api_key)
        .header("Accept", "application/json");

    if let Some(headers) = provider.extra_headers.as_ref() {
        for (key, value) in headers {
            request = request.header(key, value);
        }
    }

    let response = request
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
