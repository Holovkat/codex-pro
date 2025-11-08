use std::collections::HashMap;
use std::fmt;

use anyhow::Result;
use anyhow::anyhow;
use serde_json::json;

use crate::index::commands as index_commands;
use crate::provider;
use crate::provider::DEFAULT_OPENAI_PROVIDER_ID;
use crate::provider::OSS_PROVIDER_ID;
use crate::settings::CustomProvider;
use crate::settings::Settings;

/// Return value for a command invocation.
#[derive(Debug, Clone)]
pub enum CommandResult {
    Unit,
    Text(String),
    Json(serde_json::Value),
}

impl CommandResult {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            CommandResult::Text(text) => Some(text),
            _ => None,
        }
    }
}

type Handler = dyn Fn(&CommandContext, &[String]) -> Result<CommandResult> + Send + Sync + 'static;

/// Describes runtime context supplied to command handlers.
#[derive(Clone, Debug)]
pub struct CommandContext {
    pub settings: Settings,
    pub working_dir: Option<String>,
    pub binary_name: String,
}

impl CommandContext {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            working_dir: None,
            binary_name: "codex-agentic".to_string(),
        }
    }

    pub fn with_working_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    pub fn with_binary_name(mut self, bin: impl Into<String>) -> Self {
        self.binary_name = bin.into();
        self
    }
}

/// Metadata describing a command entry.
#[derive(Clone, Debug)]
pub struct CommandDescriptor {
    pub name: String,
    pub summary: Option<String>,
}

impl fmt::Display for CommandDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(summary) = &self.summary {
            write!(f, "{} — {}", self.name, summary)
        } else {
            write!(f, "{}", self.name)
        }
    }
}

/// Registry of string-addressable handlers shared by CLI and other entrypoints.
pub struct CommandRegistry {
    handlers: HashMap<String, Box<Handler>>,
    descriptors: HashMap<String, CommandDescriptor>,
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
            descriptors: HashMap::new(),
        }
    }

    pub fn register<F>(&mut self, name: &str, handler: F) -> &mut Self
    where
        F: Fn(&CommandContext, &[String]) -> Result<CommandResult> + Send + Sync + 'static,
    {
        self.handlers.insert(name.to_string(), Box::new(handler));
        self
    }

    pub fn register_with_descriptor<F>(
        &mut self,
        name: &str,
        summary: impl Into<Option<String>>,
        handler: F,
    ) -> &mut Self
    where
        F: Fn(&CommandContext, &[String]) -> Result<CommandResult> + Send + Sync + 'static,
    {
        self.register(name, handler);
        self.descriptors.insert(
            name.to_string(),
            CommandDescriptor {
                name: name.to_string(),
                summary: summary.into(),
            },
        );
        self
    }

    pub fn run(&self, ctx: &CommandContext, name: &str, args: &[String]) -> Result<CommandResult> {
        let handler = self
            .handlers
            .get(name)
            .ok_or_else(|| anyhow!("unknown command: {name}"))?;
        handler(ctx, args)
    }

    pub fn describe(&self, name: &str) -> Option<&CommandDescriptor> {
        self.descriptors.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.handlers.keys()
    }
}

/// Register built-in commands shared across entrypoints.
pub fn register_defaults(registry: &mut CommandRegistry) {
    registry.register_with_descriptor(
        "help-recipes",
        Some("Print common Codex Agentic invocations".to_string()),
        |ctx, _| {
            let bin = &ctx.binary_name;
            let text = format!(
                concat!(
                    "Common recipes:\n",
                    "  1. {bin} --model gpt-4o-mini --reasoning-effort medium\n",
                    "  2. {bin} --oss --model qwq:latest\n",
                    "  3. {bin} resume --last --full-auto --search\n",
                    "  4. {bin} search-code \"hotfix patch builder\"\n",
                    "  5. {bin} memory suggest --query \"summarize recent regressions\"\n"
                ),
                bin = bin
            );
            Ok(CommandResult::Text(text))
        },
    );

    registry.register_with_descriptor(
        "index.build",
        Some("Build the semantic index".to_string()),
        index_commands::build_command,
    );
    registry.register_with_descriptor(
        "index.query",
        Some("Query the semantic index".to_string()),
        index_commands::query_command,
    );
    registry.register_with_descriptor(
        "index.status",
        Some("Show semantic index manifest".to_string()),
        index_commands::status_command,
    );
    registry.register_with_descriptor(
        "index.verify",
        Some("Verify semantic index assets".to_string()),
        index_commands::verify_command,
    );
    registry.register_with_descriptor(
        "index.clean",
        Some("Remove semantic index caches".to_string()),
        index_commands::clean_command,
    );
    registry.register_with_descriptor(
        "index.ignore",
        Some("Manage .index-ignore entries".to_string()),
        index_commands::ignore_command,
    );
    registry.register_with_descriptor(
        "search-code",
        Some("Search code using semantic index (filtered by confidence)".to_string()),
        index_commands::search_command,
    );
    registry.register_with_descriptor(
        "search.confidence",
        Some("View or update search confidence threshold".to_string()),
        index_commands::search_confidence_command,
    );
    registry.register_with_descriptor(
        "apply",
        Some("Apply workspace diff via agentic tooling".to_string()),
        index_commands::apply_command,
    );
    registry.register_with_descriptor(
        "diff",
        Some("Show working tree diff".to_string()),
        |_ctx, args| {
            Ok(CommandResult::Json(json!({
                "status": "unimplemented",
                "command": "diff",
                "args": args,
            })))
        },
    );

    registry.register_with_descriptor(
        "models.list",
        Some("List cached models for providers".to_string()),
        models_list_command,
    );
}

fn models_list_command(ctx: &CommandContext, args: &[String]) -> Result<CommandResult> {
    let parsed = ModelsListArgs::parse(args)?;
    let default_provider = ctx
        .settings
        .model
        .as_ref()
        .and_then(|model| model.provider.clone())
        .unwrap_or_else(|| DEFAULT_OPENAI_PROVIDER_ID.to_string());

    let mut providers = Vec::new();
    if parsed.all_providers {
        providers.push(DEFAULT_OPENAI_PROVIDER_ID.to_string());
        providers.push(OSS_PROVIDER_ID.to_string());
        if let Some(custom) = ctx
            .settings
            .providers
            .as_ref()
            .map(|p| p.custom.keys().cloned().collect::<Vec<_>>())
        {
            providers.extend(custom);
        }
    } else if let Some(id) = parsed.provider_override {
        providers.push(id);
    } else {
        providers.push(default_provider.clone());
    }

    let mut lines = Vec::new();
    for provider_id in providers {
        match render_provider_models(ctx, &provider_id, &default_provider) {
            Ok(Some(output)) => lines.push(output),
            Ok(None) => continue,
            Err(err) => lines.push(format!(
                "Provider `{provider_id}`: failed to load models ({err})"
            )),
        }
    }

    if lines.is_empty() {
        lines.push("No models discovered for the requested provider(s).".to_string());
    }

    Ok(CommandResult::Text(lines.join("\n\n")))
}

fn render_provider_models(
    ctx: &CommandContext,
    provider_id: &str,
    default_provider: &str,
) -> Result<Option<String>> {
    let settings = &ctx.settings;
    let mut header = String::new();
    let mut cached_models: Vec<String> = Vec::new();
    let mut default_model: Option<String> = None;
    let mut last_refresh: Option<String> = None;

    match provider_id {
        id if id == DEFAULT_OPENAI_PROVIDER_ID => {
            header.push_str("OpenAI (built-in)");
            if let Some(model) = settings
                .model
                .as_ref()
                .and_then(|model| model.default.clone())
            {
                cached_models.push(model.clone());
                default_model = Some(model);
            }
        }
        id if id == OSS_PROVIDER_ID => {
            header.push_str("Ollama (OSS)");
            cached_models = provider::list_models_for_provider_blocking(settings, OSS_PROVIDER_ID)?;
        }
        other => {
            let Some(custom) = settings.custom_provider(other) else {
                return Ok(None);
            };
            header.push_str(&custom_provider_display_name(other, custom));
            if let Some(models) = custom.cached_models.clone() {
                cached_models = models;
            }
            default_model = custom.default_model.clone();
            last_refresh = custom.last_model_refresh.clone();
        }
    }

    let mut lines = Vec::new();
    let is_default = provider_id == default_provider;
    let mut header_line = if is_default {
        format!("Provider: {header} [{provider_id}] (active)")
    } else {
        format!("Provider: {header} [{provider_id}]")
    };
    if let Some(url) = settings
        .custom_provider(provider_id)
        .and_then(|provider| provider.base_url.clone())
    {
        header_line.push_str(&format!("\n  Endpoint: {url}"));
    } else if provider_id == OSS_PROVIDER_ID
        && let Some(url) = provider::provider_endpoint(settings, provider_id)
    {
        header_line.push_str(&format!("\n  Endpoint: {url}"));
    }
    if let Some(refreshed) = last_refresh {
        header_line.push_str(&format!("\n  Last refresh: {refreshed}"));
    }
    if let Some(default) = default_model.clone() {
        header_line.push_str(&format!("\n  Default model: {default}"));
    }
    lines.push(header_line);

    if cached_models.is_empty() {
        lines.push("  (no cached models found)".to_string());
    } else {
        for model in cached_models {
            if default_model
                .as_ref()
                .map(|default| default == &model)
                .unwrap_or(false)
            {
                lines.push(format!("  • {model}  (default)"));
            } else {
                lines.push(format!("  • {model}"));
            }
        }
    }

    Ok(Some(lines.join("\n")))
}

fn custom_provider_display_name(id: &str, provider: &CustomProvider) -> String {
    if provider.name.trim().is_empty() {
        id.to_string()
    } else {
        provider.name.clone()
    }
}

#[derive(Default)]
struct ModelsListArgs {
    provider_override: Option<String>,
    all_providers: bool,
}

impl ModelsListArgs {
    fn parse(args: &[String]) -> Result<Self> {
        let mut parsed = ModelsListArgs::default();
        let mut iter = args.iter().peekable();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--oss" => parsed.provider_override = Some(OSS_PROVIDER_ID.to_string()),
                "--all" => parsed.all_providers = true,
                v if v.starts_with("--provider=") => {
                    let id = v.trim_start_matches("--provider=").trim().to_string();
                    if id.is_empty() {
                        return Err(anyhow::anyhow!("--provider requires an identifier"));
                    }
                    parsed.provider_override = Some(id);
                }
                "--provider" => {
                    let Some(id) = iter.next() else {
                        return Err(anyhow::anyhow!("--provider requires an identifier"));
                    };
                    parsed.provider_override = Some(id.trim().to_string());
                }
                value if value.eq_ignore_ascii_case("all") => {
                    parsed.all_providers = true;
                }
                value if value.starts_with('-') => {
                    // Unknown flag; ignore so we stay forward compatible.
                }
                value => {
                    parsed.provider_override = Some(value.to_string());
                }
            }
        }

        Ok(parsed)
    }
}
