use anyhow::Result;
use codex_agentic_core::CommandContext;
use codex_agentic_core::CommandRegistry;
use codex_agentic_core::CommandResult;
use codex_agentic_core::OSS_PROVIDER_ID;
use codex_agentic_core::list_models_for_provider_blocking;
use codex_agentic_core::provider_endpoint;
use codex_agentic_core::resolve_provider;
use serde_json::json;

pub fn register_overrides(registry: &mut CommandRegistry) {
    registry.register_with_descriptor(
        "models.list",
        Some("List configured model providers".to_string()),
        models_list,
    );
}

fn models_list(ctx: &CommandContext, args: &[String]) -> Result<CommandResult> {
    let settings = &ctx.settings;
    let default_provider = settings
        .model
        .as_ref()
        .and_then(|model| model.provider.clone());
    let default_model = settings
        .model
        .as_ref()
        .and_then(|model| model.default.clone());
    let provider_override = args
        .iter()
        .find(|arg| arg.as_str() == "--oss")
        .map(|_| OSS_PROVIDER_ID);
    let resolved_provider = resolve_provider(settings, provider_override);

    let Some(provider_id) = resolved_provider else {
        return Ok(CommandResult::Json(json!({
            "default_provider": default_provider,
            "default_model": default_model,
            "status": "no-provider",
            "message": "No model provider configured. Use --oss or update settings.model.provider.",
            "raw_args": args,
        })));
    };
    let endpoint = provider_endpoint(settings, &provider_id);
    let models_result = list_models_for_provider_blocking(settings, &provider_id);

    let (status, models, error) = match models_result {
        Ok(models) => ("ok", Some(models), None),
        Err(err) => ("error", None, Some(err.to_string())),
    };

    Ok(CommandResult::Json(json!({
        "default_provider": default_provider,
        "default_model": default_model,
        "selected_provider": provider_id,
        "endpoint": endpoint,
        "status": status,
        "models": models,
        "error": error,
        "raw_args": args,
    })))
}
