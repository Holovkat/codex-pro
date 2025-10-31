use anyhow::Result;
use serde_json::json;

use codex_agentic_core::CommandRegistry;
use codex_agentic_core::CommandResult;
use codex_agentic_core::OSS_PROVIDER_ID;
use codex_agentic_core::commands::CommandContext;
use codex_agentic_core::index::commands::search_command;
use codex_agentic_core::index::commands::search_confidence_command;
use codex_agentic_core::list_models_for_provider_blocking;
use codex_agentic_core::provider_endpoint;
use codex_agentic_core::register_default_commands;
use codex_agentic_core::resolve_provider;

pub fn build_cli_registry() -> CommandRegistry {
    let mut registry = CommandRegistry::new();
    register_default_commands(&mut registry);
    register_cli_overrides(&mut registry);
    registry
}

fn register_cli_overrides(registry: &mut CommandRegistry) {
    registry.register_with_descriptor(
        "help-recipes",
        Some("Print curated Codex Agentic usage examples".to_string()),
        |_ctx, _| Ok(CommandResult::Text(render_help_recipes())),
    );
    registry.register_with_descriptor(
        "models.list",
        Some("List configured model providers".to_string()),
        render_model_list,
    );
    registry.register_with_descriptor(
        "search-code",
        Some(
            "Run semantic code search and filter results below the confidence threshold"
                .to_string(),
        ),
        search_command,
    );
    registry.register_with_descriptor(
        "search.confidence",
        Some("Inspect or update search confidence threshold".to_string()),
        search_confidence_command,
    );
}

fn render_help_recipes() -> String {
    [
        "# Agentic Recipes",
        "",
        "1) codex-agentic --model gpt-4o-mini --reasoning-effort medium",
        "2) codex-agentic --oss --model qwq:latest",
        "3) codex-agentic resume --last --full-auto --search",
        "4) codex-agentic apply",
    ]
    .join("\n")
}

fn render_model_list(ctx: &CommandContext, args: &[String]) -> Result<CommandResult> {
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

pub fn command_output_to_string(result: &CommandResult) -> Result<Option<String>> {
    let rendered = match result {
        CommandResult::Unit => None,
        CommandResult::Text(text) => Some(text.clone()),
        CommandResult::Json(value) => Some(serde_json::to_string_pretty(value)?),
    };
    Ok(rendered)
}
