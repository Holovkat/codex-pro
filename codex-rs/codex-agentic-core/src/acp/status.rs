use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use chrono::DateTime;
use chrono::Local;
use codex_common::create_config_summary_entries;
// rollback: avoid WireApi usage
use codex_core::auth::get_auth_file;
use codex_core::auth::try_read_auth_json;
use codex_core::config::Config;
use codex_core::project_doc::discover_project_doc_paths;
use codex_core::protocol::RateLimitSnapshot;
use codex_core::protocol::RateLimitWindow;
use codex_core::protocol::TokenUsageInfo;
use codex_protocol::ConversationId;
use dunce::simplified;
use pathdiff::diff_paths;

use crate::CommandContext;
use crate::global_prompt_sources;

const TITLE_INDENT: &str = "  ";
const LABEL_WIDTH: usize = 16;
const RATE_LIMIT_BAR_SEGMENTS: usize = 20;
const RATE_LIMIT_BAR_FILLED: &str = "█";
const RATE_LIMIT_BAR_EMPTY: &str = "░";

pub fn render_status_card(
    config: &Config,
    ctx: &CommandContext,
    usage: Option<&TokenUsageInfo>,
    rate_limits: Option<&RateLimitSnapshot>,
    rate_limits_captured_at: Option<&DateTime<Local>>,
    conversation_id: Option<&ConversationId>,
) -> String {
    let entries = create_config_summary_entries(config);
    let entry_map: HashMap<&str, &str> = entries.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let display_name = if ctx.binary_name.eq_ignore_ascii_case("codex-agentic") {
        "OpenAI Codex".to_string()
    } else {
        ctx.binary_name.clone()
    };
    let title = format!(
        "{TITLE_INDENT}>_ {display_name} (v{})",
        env!("CARGO_PKG_VERSION")
    );

    let model_value = format_model_value(config, &entry_map);
    let directory_value = format_directory(&config.cwd);
    let approval_value = entry_map
        .get("approval")
        .map(|value| (*value).to_string())
        .unwrap_or_else(|| config.approval_policy.to_string());
    let agents_value = format_agents_summary(config);
    let prompts = format_prompts_list(config);

    let account_info = account_summary(config);
    let hide_token_usage = matches!(account_info.as_ref(), Some(AccountDisplay::ChatGpt { .. }));
    let account_value = account_info.as_ref().map(|info| match info {
        AccountDisplay::ChatGpt { email, plan } => match (email.as_deref(), plan.as_deref()) {
            (Some(email), Some(plan)) => format!("{email} ({plan})"),
            (Some(email), None) => email.to_string(),
            (None, Some(plan)) => plan.to_string(),
            (None, None) => "ChatGPT".to_string(),
        },
        AccountDisplay::ApiKey => "API key configured (run codex login to use ChatGPT)".into(),
    });

    let session_value = conversation_id.map(std::string::ToString::to_string);

    let mut lines = vec![
        title,
        String::new(),
        format_field("Model", &model_value),
        format_field("Provider", &format_provider_value(config)),
        format_field("Directory", &directory_value),
        format_field("Approval", &approval_value),
        format_field("Sandbox", summarize_sandbox_policy(&config.sandbox_policy)),
        format_field("Agents.md", &agents_value),
    ];

    if let Some((first, rest)) = prompts.split_first() {
        lines.push(format_field("Prompts", first));
        for prompt in rest {
            lines.push(format_continuation(prompt));
        }
    } else {
        lines.push(format_field("Prompts", "<none>"));
    }

    if let Some(value) = account_value {
        lines.push(format_field("Account", &value));
    }

    if let Some(value) = session_value {
        lines.push(format_field("Session", &value));
    }

    lines.push(String::new());

    if let Some(info) = usage {
        if !hide_token_usage {
            lines.push(format_field("Token usage", &format_token_usage(info)));
        }

        if let Some(context_line) = format_context_window(info) {
            lines.push(format_field("Context window", &context_line));
        }
    }

    lines.extend(format_rate_limit_lines(
        rate_limits,
        rate_limits_captured_at,
    ));

    wrap_with_border(lines)
}

fn format_model_value(config: &Config, entries: &HashMap<&str, &str>) -> String {
    let model = entries
        .get("model")
        .copied()
        .unwrap_or(config.model.as_str());

    let mut details: Vec<String> = Vec::new();
    if let Some(effort) = entries.get("reasoning effort") {
        let effort = effort.trim();
        if !effort.is_empty() {
            details.push(format!("reasoning {}", effort.to_ascii_lowercase()));
        }
    }
    if let Some(summary) = entries.get("reasoning summaries") {
        let summary = summary.trim();
        if summary.eq_ignore_ascii_case("none") || summary.eq_ignore_ascii_case("off") {
            details.push("summaries off".to_string());
        } else if !summary.is_empty() {
            details.push(format!("summaries {}", summary.to_ascii_lowercase()));
        }
    }

    if details.is_empty() {
        model.to_string()
    } else {
        format!("{model} ({})", details.join(", "))
    }
}

fn format_provider_value(config: &Config) -> String {
    if let Some(base) = config.model_provider.base_url.as_deref() {
        format!("{} @ {}", config.model_provider_id, base)
    } else {
        config.model_provider_id.clone()
    }
}

// removed Endpoint formatter (rollback)

fn format_directory(path: &Path) -> String {
    let simplified = simplified(path);
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && let Ok(stripped) = simplified.strip_prefix(&home)
    {
        if stripped.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~{}{}", std::path::MAIN_SEPARATOR, stripped.display());
    }
    simplified.display().to_string()
}

fn format_agents_summary(config: &Config) -> String {
    match discover_project_doc_paths(config) {
        Ok(paths) => format_paths_summary(config, &paths),
        Err(_) => "<none>".to_string(),
    }
}

fn format_paths_summary(config: &Config, paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        return "<none>".to_string();
    }

    let mut entries: Vec<String> = paths
        .iter()
        .map(|path| format_path_for_display(config, path))
        .collect();
    entries.sort();
    entries.dedup();

    if entries.is_empty() {
        "<none>".to_string()
    } else {
        entries.join(", ")
    }
}

fn format_path_for_display(config: &Config, path: &Path) -> String {
    let simplified = simplified(path);
    if let Some(rel) = diff_paths(simplified, &config.cwd) {
        if !rel.as_os_str().is_empty() {
            return normalized_display(&rel);
        }
        return ".".to_string();
    }

    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && let Ok(rel) = simplified.strip_prefix(home)
    {
        if rel.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~{}{}", std::path::MAIN_SEPARATOR, rel.display());
    }

    normalized_display(simplified)
}

fn normalized_display(path: &Path) -> String {
    simplified(path).display().to_string()
}

fn format_prompts_list(config: &Config) -> Vec<String> {
    let sources = global_prompt_sources();
    if sources.is_empty() {
        return vec!["<none>".to_string()];
    }
    sources
        .into_iter()
        .map(|path| format_prompt_entry(config, &path))
        .collect()
}

fn format_prompt_entry(config: &Config, path: &Path) -> String {
    let simplified = simplified(path);
    if let Ok(stripped) = simplified.strip_prefix(&config.codex_home)
        && !stripped.as_os_str().is_empty()
    {
        return format!(
            ".codex/{}",
            normalized_display(stripped).trim_start_matches(std::path::MAIN_SEPARATOR)
        );
    }

    if let Ok(stripped) = simplified.strip_prefix(&config.cwd)
        && !stripped.as_os_str().is_empty()
    {
        return normalized_display(stripped);
    }

    if let Some(rel) = diff_paths(simplified, &config.codex_home)
        && !rel.as_os_str().is_empty()
    {
        return normalized_display(&rel);
    }
    if let Some(rel) = diff_paths(simplified, &config.cwd)
        && !rel.as_os_str().is_empty()
    {
        return normalized_display(&rel);
    }

    normalized_display(simplified)
}

enum AccountDisplay {
    ChatGpt {
        email: Option<String>,
        plan: Option<String>,
    },
    ApiKey,
}

fn account_summary(config: &Config) -> Option<AccountDisplay> {
    let auth_file = get_auth_file(&config.codex_home);
    let auth = try_read_auth_json(&auth_file).ok()?;

    if let Some(tokens) = auth.tokens.as_ref() {
        let info = &tokens.id_token;
        let email = info.email.clone();
        let plan = info
            .get_chatgpt_plan_type()
            .map(|plan| title_case(plan.as_str()));
        return Some(AccountDisplay::ChatGpt { email, plan });
    }

    if let Some(key) = auth.openai_api_key.as_ref()
        && !key.trim().is_empty()
    {
        return Some(AccountDisplay::ApiKey);
    }

    None
}

fn format_token_usage(info: &TokenUsageInfo) -> String {
    let total = format_tokens_compact(info.total_token_usage.blended_total());
    let input = format_tokens_compact(info.total_token_usage.non_cached_input());
    let output = format_tokens_compact(info.total_token_usage.output_tokens);
    format!("{total} total ({input} input + {output} output)")
}

fn format_context_window(info: &TokenUsageInfo) -> Option<String> {
    let window = info.model_context_window?;
    let usage = &info.last_token_usage;
    let percent = usage.percent_of_context_window_remaining(window);
    let used = format_tokens_compact(usage.tokens_in_context_window());
    let window_fmt = format_tokens_compact(window);
    Some(format!("{percent}% left ({used} used / {window_fmt})"))
}

fn format_rate_limit_lines(
    snapshot: Option<&RateLimitSnapshot>,
    captured_at: Option<&DateTime<Local>>,
) -> Vec<String> {
    let Some(snapshot) = snapshot else {
        return vec![format_field("Limits", "send a message to load usage data")];
    };

    let mut rows = Vec::new();
    if let Some(primary) = snapshot.primary.as_ref() {
        rows.push(format_rate_limit_window("Primary", primary, captured_at));
    }
    if let Some(secondary) = snapshot.secondary.as_ref() {
        rows.push(format_rate_limit_window(
            "Secondary",
            secondary,
            captured_at,
        ));
    }

    if rows.is_empty() {
        vec![format_field("Limits", "data not available yet")]
    } else {
        rows
    }
}

fn format_rate_limit_window(
    label: &str,
    window: &RateLimitWindow,
    captured_at: Option<&DateTime<Local>>,
) -> String {
    let resets = captured_at.and_then(|ts| format_reset(window.resets_at.as_deref(), ts));
    let label = match window.window_minutes {
        Some(minutes) => format!("{label} ({})", describe_window(minutes)),
        None => label.to_string(),
    };
    format_rate_limit_row(label, window.used_percent, resets)
}

fn describe_window(minutes: u64) -> String {
    const MINUTES_PER_HOUR: u64 = 60;
    const MINUTES_PER_DAY: u64 = 24 * MINUTES_PER_HOUR;
    const MINUTES_PER_WEEK: u64 = 7 * MINUTES_PER_DAY;
    const MINUTES_PER_MONTH: u64 = 30 * MINUTES_PER_DAY;
    const MINUTES_PER_YEAR: u64 = 365 * MINUTES_PER_DAY;

    if minutes < MINUTES_PER_HOUR {
        format!("{minutes}m")
    } else if minutes <= MINUTES_PER_DAY {
        let hours = minutes.div_ceil(MINUTES_PER_HOUR);
        format!("{hours}h")
    } else if minutes <= MINUTES_PER_WEEK {
        let days = minutes.div_ceil(MINUTES_PER_DAY);
        format!("{days}d")
    } else if minutes <= MINUTES_PER_MONTH {
        "Weekly".to_string()
    } else if minutes <= MINUTES_PER_YEAR {
        "Monthly".to_string()
    } else {
        "Annual".to_string()
    }
}

fn format_rate_limit_row(label: String, percent_used: f64, resets_at: Option<String>) -> String {
    let bar = render_status_limit_progress_bar(percent_used);
    let summary = format!("{percent_used:.0}% used");
    let mut value = format!("{bar} {summary}");
    if let Some(reset) = resets_at {
        value.push(' ');
        value.push_str(&format!("(resets {reset})"));
    }
    format_field(&format!("{label} limit"), &value)
}

fn render_status_limit_progress_bar(percent_used: f64) -> String {
    let ratio = (percent_used / 100.0).clamp(0.0, 1.0);
    let filled = (ratio * RATE_LIMIT_BAR_SEGMENTS as f64).round() as usize;
    let filled = filled.min(RATE_LIMIT_BAR_SEGMENTS);
    let empty = RATE_LIMIT_BAR_SEGMENTS.saturating_sub(filled);
    format!(
        "[{}{}]",
        RATE_LIMIT_BAR_FILLED.repeat(filled),
        RATE_LIMIT_BAR_EMPTY.repeat(empty)
    )
}

fn format_reset(resets_at: Option<&str>, captured_at: &DateTime<Local>) -> Option<String> {
    let reset_at = resets_at?;
    let timestamp = chrono::DateTime::parse_from_rfc3339(reset_at)
        .ok()?
        .with_timezone(&Local);
    Some(format_reset_timestamp(timestamp, *captured_at))
}

fn format_reset_timestamp(dt: DateTime<Local>, captured_at: DateTime<Local>) -> String {
    let time = dt.format("%H:%M").to_string();
    if dt.date_naive() == captured_at.date_naive() {
        time
    } else {
        format!("{time} on {}", dt.format("%-d %b"))
    }
}

fn wrap_with_border(lines: Vec<String>) -> String {
    let inner_width = lines
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0);
    let horizontal = "─".repeat(inner_width + 2);
    let mut output = Vec::with_capacity(lines.len() + 2);
    output.push(format!("╭{horizontal}╮"));
    for line in lines {
        let line_width = line.chars().count();
        let padding = inner_width.saturating_sub(line_width);
        output.push(format!("│ {line}{} │", " ".repeat(padding)));
    }
    output.push(format!("╰{horizontal}╯"));
    output.join("\n")
}

fn format_field(label: &str, value: &str) -> String {
    let label = format!("{label}:");
    format!("{TITLE_INDENT}{label:<LABEL_WIDTH$}{value}")
}

fn format_continuation(value: &str) -> String {
    format!("{TITLE_INDENT}{:<LABEL_WIDTH$}{value}", "", value = value)
}

fn title_case(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let mut chars = input.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut result = String::new();
    result.extend(first.to_uppercase());
    let rest = chars.as_str().to_ascii_lowercase();
    result.push_str(&rest);
    result
}

fn format_tokens_compact(value: u64) -> String {
    if value == 0 {
        return "0".to_string();
    }
    if value < 1_000 {
        return value.to_string();
    }

    let (scaled, suffix) = if value >= 1_000_000_000_000 {
        (value as f64 / 1_000_000_000_000.0, "T")
    } else if value >= 1_000_000_000 {
        (value as f64 / 1_000_000_000.0, "B")
    } else if value >= 1_000_000 {
        (value as f64 / 1_000_000.0, "M")
    } else {
        (value as f64 / 1_000.0, "K")
    };

    let decimals = if scaled < 10.0 {
        2
    } else if scaled < 100.0 {
        1
    } else {
        0
    };

    let mut formatted = format!("{scaled:.decimals$}");
    if formatted.contains('.') {
        while formatted.ends_with('0') {
            formatted.pop();
        }
        if formatted.ends_with('.') {
            formatted.pop();
        }
    }

    format!("{formatted}{suffix}")
}

fn summarize_sandbox_policy(policy: &codex_core::protocol::SandboxPolicy) -> &'static str {
    match policy {
        codex_core::protocol::SandboxPolicy::DangerFullAccess => "danger-full-access",
        codex_core::protocol::SandboxPolicy::ReadOnly => "read-only",
        codex_core::protocol::SandboxPolicy::WorkspaceWrite { .. } => "workspace-write",
    }
}
