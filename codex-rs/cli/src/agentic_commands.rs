use anyhow::Result;
use serde_json::json;

use codex_agentic_core::CommandRegistry;
use codex_agentic_core::CommandResult;
use codex_agentic_core::OSS_PROVIDER_ID;
use codex_agentic_core::commands::CommandContext;
use codex_agentic_core::index::commands::search_command;
use codex_agentic_core::index::commands::search_confidence_command;
use codex_agentic_core::list_models_for_provider_blocking;
use codex_agentic_core::provider::DEFAULT_OPENAI_PROVIDER_ID;
use codex_agentic_core::provider_endpoint;
use codex_agentic_core::register_default_commands;
use codex_agentic_core::resolve_provider;
use codex_agentic_core::settings::Settings;

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
            "Run semantic code search (uses the search_code tool under the hood) and filter results below the confidence threshold"
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
        "4) codex-agentic search-code \"hotfix patch builder\"",
        "5) codex memory suggest --query \"summarize recent regressions\"",
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
    let mut provider_override: Option<String> = None;
    let mut include_all = false;
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--oss" => provider_override = Some(OSS_PROVIDER_ID.to_string()),
            "--all" => include_all = true,
            "--provider" => {
                if let Some(value) = iter.next() {
                    let trimmed = value.trim();
                    if !trimmed.is_empty() {
                        provider_override = Some(trimmed.to_string());
                    }
                }
            }
            value if value.starts_with("--provider=") => {
                let trimmed = value.trim_start_matches("--provider=").trim();
                if !trimmed.is_empty() {
                    provider_override = Some(trimmed.to_string());
                }
            }
            value if value.eq_ignore_ascii_case("all") => {
                include_all = true;
            }
            value if !value.starts_with('-') && provider_override.is_none() => {
                provider_override = Some(value.to_string());
            }
            _ => {}
        }
    }
    let provider_override_ref = provider_override.as_deref();
    let resolved_provider = resolve_provider(settings, provider_override_ref);

    let Some(provider_id) = resolved_provider else {
        return Ok(CommandResult::Json(json!({
            "default_provider": default_provider,
            "default_model": default_model,
            "status": "no-provider",
            "message": "No model provider configured. Use --oss or update settings.model.provider.",
            "requested_all": include_all,
            "requested_provider": provider_override,
            "raw_args": args,
        })));
    };
    let endpoint = provider_endpoint(settings, &provider_id);
    let models_result = list_models_for_provider_blocking(settings, &provider_id);

    let mut selected_models: Option<Vec<String>> = None;
    let mut selected_error: Option<String> = None;
    let (status, models, error) = match models_result {
        Ok(models) => {
            selected_models = Some(models.clone());
            ("ok", Some(models), None)
        }
        Err(err) => {
            let message = err.to_string();
            selected_error = Some(message.clone());
            ("error", None, Some(message))
        }
    };

    let list_all = include_all || provider_override.is_none();
    let providers = summarize_providers(
        settings,
        &provider_id,
        selected_models.as_deref(),
        selected_error.as_deref(),
        list_all,
        provider_override.as_deref(),
    );

    Ok(CommandResult::Json(json!({
        "default_provider": default_provider,
        "default_model": default_model,
        "selected_provider": provider_id,
        "endpoint": endpoint,
        "status": status,
        "models": models,
        "error": error,
        "providers": providers,
        "requested_provider": provider_override,
        "requested_all": list_all,
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

fn summarize_providers(
    settings: &Settings,
    selected_provider: &str,
    selected_models: Option<&[String]>,
    selected_error: Option<&str>,
    list_all: bool,
    provider_override: Option<&str>,
) -> Vec<serde_json::Value> {
    let provider_ids = if list_all {
        let mut ids = vec![
            DEFAULT_OPENAI_PROVIDER_ID.to_string(),
            OSS_PROVIDER_ID.to_string(),
        ];
        for (id, _) in settings.custom_providers() {
            ids.push(id.clone());
        }
        ids.sort();
        ids.dedup();
        ids
    } else {
        vec![provider_override.unwrap_or(selected_provider).to_string()]
    };

    provider_ids
        .into_iter()
        .map(|provider_id| {
            let provider_id_str = provider_id.as_str();
            let endpoint = provider_endpoint(settings, provider_id_str);
            let (status, models, error) = if provider_id_str == selected_provider {
                if let Some(models) = selected_models {
                    ("ok", Some(models.to_vec()), None)
                } else if let Some(message) = selected_error {
                    ("error", None, Some(message.to_string()))
                } else {
                    ("ok", None, None)
                }
            } else {
                match list_models_for_provider_blocking(settings, provider_id_str) {
                    Ok(list) => ("ok", Some(list), None),
                    Err(err) => ("error", None, Some(err.to_string())),
                }
            };

            let source = if provider_id_str == DEFAULT_OPENAI_PROVIDER_ID
                || provider_id_str == OSS_PROVIDER_ID
            {
                "built-in"
            } else if settings.custom_provider(provider_id_str).is_some() {
                "custom"
            } else {
                "unknown"
            };

            let name = settings
                .custom_provider(provider_id_str)
                .map(|provider| provider.name.clone())
                .or_else(|| {
                    if provider_id_str == DEFAULT_OPENAI_PROVIDER_ID {
                        Some("OpenAI".to_string())
                    } else if provider_id_str == OSS_PROVIDER_ID {
                        Some("Ollama (local)".to_string())
                    } else {
                        None
                    }
                });

            json!({
                "id": provider_id_str,
                "name": name,
                "source": source,
                "endpoint": endpoint,
                "status": status,
                "models": models,
                "error": error,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_agentic_core::settings::CustomProvider;
    use codex_agentic_core::settings::Model;
    use codex_agentic_core::settings::Providers;
    use pretty_assertions::assert_eq;
    use std::collections::BTreeMap;

    fn sample_context() -> CommandContext {
        let mut custom = BTreeMap::new();
        custom.insert(
            "zai".to_string(),
            CustomProvider {
                name: "z.ai".to_string(),
                base_url: Some("https://api.z.ai/api/coding/paas/v4".to_string()),
                cached_models: Some(vec!["glm-4.5".to_string(), "glm-4.6".to_string()]),
                ..CustomProvider::default()
            },
        );

        CommandContext::new(Settings {
            model: Some(Model {
                provider: Some("openai".to_string()),
                default: Some("gpt-5-codex".to_string()),
                reasoning_effort: None,
                reasoning_view: None,
            }),
            providers: Some(Providers { oss: None, custom }),
            ..Settings::default()
        })
    }

    #[test]
    fn models_list_respects_provider_override() {
        let ctx = sample_context();
        let args = vec!["--provider=zai".to_string()];
        let result = render_model_list(&ctx, &args).expect("command result");

        let CommandResult::Json(value) = result else {
            panic!("expected JSON command result");
        };

        assert_eq!(value["selected_provider"], "zai");
        assert_eq!(value["requested_provider"], "zai");
        assert_eq!(value["requested_all"], false);

        let providers = value["providers"].as_array().expect("providers array");
        assert_eq!(providers.len(), 1);

        let provider = providers[0].as_object().expect("provider object");
        assert_eq!(provider["id"], "zai");
        let models: Vec<String> = provider["models"]
            .as_array()
            .expect("models array")
            .iter()
            .map(|value| value.as_str().unwrap().to_string())
            .collect();
        assert_eq!(models, vec!["glm-4.5".to_string(), "glm-4.6".to_string()]);
    }

    #[test]
    fn models_list_defaults_to_all_providers() {
        let ctx = sample_context();
        let result = render_model_list(&ctx, &[]).expect("command result");

        let CommandResult::Json(value) = result else {
            panic!("expected JSON command result");
        };

        assert_eq!(value["requested_all"], true);
        let providers = value["providers"].as_array().expect("providers array");
        assert!(
            providers.iter().any(|provider| provider["id"] == "zai"),
            "expected BYOK provider to be listed"
        );
    }
}
