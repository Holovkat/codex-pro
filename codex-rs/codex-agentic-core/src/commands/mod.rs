use std::collections::HashMap;
use std::fmt;

use anyhow::Result;
use anyhow::anyhow;
use serde_json::json;

use crate::index::commands as index_commands;
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
}
