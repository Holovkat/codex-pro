use strum::IntoEnumIterator;
use strum_macros::AsRefStr;
use strum_macros::EnumIter;
use strum_macros::EnumString;
use strum_macros::IntoStaticStr;

/// Commands that can be invoked by starting a message with a leading slash.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, EnumIter, AsRefStr, IntoStaticStr,
)]
#[strum(serialize_all = "kebab-case")]
pub enum SlashCommand {
    #[strum(serialize = "index")]
    IndexBuild,
    SearchCode,
    MemorySuggest,
    // DO NOT ALPHA-SORT! Enum order is presentation order in the popup, so
    // more frequently used commands should be listed first.
    Model,
    #[strum(serialize = "byok", serialize = "BYOK")]
    Byok,
    Approvals,
    Review,
    New,
    Init,
    Compact,
    Undo,
    Diff,
    Mention,
    Status,
    Mcp,
    Logout,
    Feedback,
    Quit,
    Memory,
    #[cfg(debug_assertions)]
    TestApproval,
}

impl SlashCommand {
    /// User-visible description shown in the popup.
    pub fn description(self) -> &'static str {
        match self {
            SlashCommand::New => "start a new chat during a conversation",
            SlashCommand::Init => "create an AGENTS.md file with instructions for Codex",
            SlashCommand::Compact => "summarize conversation to prevent hitting the context limit",
            SlashCommand::Review => "review my current changes and find issues",
            SlashCommand::Undo => "restore the workspace to the last Codex snapshot",
            SlashCommand::Quit => "exit Codex",
            SlashCommand::IndexBuild => "rebuild the semantic index",
            SlashCommand::SearchCode => {
                "run semantic code search and adjust the confidence threshold"
            }
            SlashCommand::MemorySuggest => "list stored memories related to the current question",
            SlashCommand::Diff => "show git diff (including untracked files)",
            SlashCommand::Mention => "mention a file",
            SlashCommand::Status => "show current session configuration and token usage",
            SlashCommand::Model => "choose what model and reasoning effort to use",
            SlashCommand::Byok => "manage custom model providers",
            SlashCommand::Approvals => "choose what Codex can do without approval",
            SlashCommand::Mcp => "list configured MCP tools",
            SlashCommand::Feedback => "send logs to maintainers",
            SlashCommand::Logout => "log out of Codex",
            SlashCommand::Memory => "inspect and manage global context memory",
            #[cfg(debug_assertions)]
            SlashCommand::TestApproval => "test approval request",
        }
    }

    /// Command string without the leading '/'. Provided for compatibility with
    /// existing code that expects a method named `command()`.
    pub fn command(self) -> &'static str {
        self.into()
    }

    /// Whether this command can be run while a task is in progress.
    pub fn available_during_task(self) -> bool {
        match self {
            SlashCommand::New
            | SlashCommand::Init
            | SlashCommand::Compact
            | SlashCommand::Undo
            | SlashCommand::Model
            | SlashCommand::Byok
            | SlashCommand::Approvals
            | SlashCommand::Review
            | SlashCommand::Logout
            | SlashCommand::IndexBuild => false,
            SlashCommand::Diff
            | SlashCommand::SearchCode
            | SlashCommand::MemorySuggest
            | SlashCommand::Mention
            | SlashCommand::Status
            | SlashCommand::Mcp
            | SlashCommand::Feedback
            | SlashCommand::Memory
            | SlashCommand::Quit => true,

            #[cfg(debug_assertions)]
            SlashCommand::TestApproval => true,
        }
    }

    pub fn accepts_args(self) -> bool {
        matches!(self, SlashCommand::SearchCode | SlashCommand::MemorySuggest)
    }
}

/// Return all built-in commands in a Vec paired with their command string.
pub fn built_in_slash_commands() -> Vec<(&'static str, SlashCommand)> {
    let show_beta_features = beta_features_enabled();

    SlashCommand::iter()
        .filter(|cmd| {
            if *cmd == SlashCommand::Undo {
                show_beta_features
            } else {
                true
            }
        })
        .map(|c| (c.command(), c))
        .collect()
}

fn beta_features_enabled() -> bool {
    std::env::var_os("BETA_FEATURE").is_some()
}
