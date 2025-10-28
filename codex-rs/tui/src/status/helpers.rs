use crate::exec_command::relativize_to_home;
use crate::text_formatting;
use chrono::DateTime;
use chrono::Local;
use codex_agentic_core::global_prompt_sources;
use codex_core::auth::load_auth_dot_json;
use codex_core::config::Config;
use codex_core::project_doc::discover_project_doc_paths;
use pathdiff::diff_paths;
use std::path::Path;
use std::path::PathBuf;
use unicode_width::UnicodeWidthStr;

use super::account::StatusAccountDisplay;

fn normalize_agents_display_path(path: &Path) -> String {
    dunce::simplified(path).display().to_string()
}

fn format_path_for_display(config: &Config, path: &Path) -> String {
    if let Some(rel) = diff_paths(path, &config.cwd) {
        let simplified = dunce::simplified(&rel);
        if !simplified.as_os_str().is_empty() {
            return normalize_agents_display_path(simplified);
        }
        return ".".to_string();
    }

    if let Some(home_rel) = relativize_to_home(path) {
        if home_rel.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~{}{}", std::path::MAIN_SEPARATOR, home_rel.display());
    }

    if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
        return file_name.to_string();
    }

    normalize_agents_display_path(path)
}

fn format_paths_summary(config: &Config, paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        return "<none>".to_string();
    }
    let mut rels: Vec<String> = paths
        .iter()
        .map(|path| format_path_for_display(config, path))
        .collect();
    rels.sort();
    rels.dedup();
    if rels.is_empty() {
        "<none>".to_string()
    } else {
        rels.join(", ")
    }
}

pub(crate) fn compose_model_display(
    config: &Config,
    entries: &[(&str, String)],
) -> (String, Vec<String>) {
    let mut details: Vec<String> = Vec::new();
    if let Some((_, effort)) = entries.iter().find(|(k, _)| *k == "reasoning effort") {
        details.push(format!("reasoning {}", effort.to_ascii_lowercase()));
    }
    if let Some((_, summary)) = entries.iter().find(|(k, _)| *k == "reasoning summaries") {
        let summary = summary.trim();
        if summary.eq_ignore_ascii_case("none") || summary.eq_ignore_ascii_case("off") {
            details.push("summaries off".to_string());
        } else if !summary.is_empty() {
            details.push(format!("summaries {}", summary.to_ascii_lowercase()));
        }
    }

    (config.model.clone(), details)
}

pub(crate) fn compose_agents_summary(config: &Config) -> String {
    match discover_project_doc_paths(config) {
        Ok(paths) => format_paths_summary(config, &paths),
        Err(_) => "<none>".to_string(),
    }
}

pub(crate) fn compose_prompts_list(config: &Config) -> Vec<String> {
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
    let simplified_path = dunce::simplified(path);

    if let Ok(stripped) = simplified_path.strip_prefix(&config.codex_home) {
        let simplified = dunce::simplified(stripped);
        if !simplified.as_os_str().is_empty() {
            return format!(
                ".codex/{}",
                normalize_agents_display_path(simplified)
                    .trim_start_matches(std::path::MAIN_SEPARATOR)
            );
        }
    }

    if let Ok(stripped) = simplified_path.strip_prefix(&config.cwd) {
        let simplified = dunce::simplified(stripped);
        if !simplified.as_os_str().is_empty() {
            return normalize_agents_display_path(simplified);
        }
    }

    if let Some(rel) = diff_paths(simplified_path, &config.codex_home) {
        let simplified = dunce::simplified(&rel);
        if !simplified.as_os_str().is_empty() {
            return normalize_agents_display_path(simplified);
        }
    }

    if let Some(rel) = diff_paths(simplified_path, &config.cwd) {
        let simplified = dunce::simplified(&rel);
        if !simplified.as_os_str().is_empty() {
            return normalize_agents_display_path(simplified);
        }
    }

    if let Some(name) = simplified_path.file_name().and_then(|n| n.to_str()) {
        return name.to_string();
    }

    normalize_agents_display_path(simplified_path)
}

pub(crate) fn compose_account_display(config: &Config) -> Option<StatusAccountDisplay> {
    let auth =
        load_auth_dot_json(&config.codex_home, config.cli_auth_credentials_store_mode).ok()??;

    if let Some(tokens) = auth.tokens.as_ref() {
        let info = &tokens.id_token;
        let email = info.email.clone();
        let plan = info.get_chatgpt_plan_type().map(|plan| title_case(&plan));
        return Some(StatusAccountDisplay::ChatGpt { email, plan });
    }

    if let Some(key) = auth.openai_api_key
        && !key.is_empty()
    {
        return Some(StatusAccountDisplay::ApiKey);
    }

    None
}

pub(crate) fn format_tokens_compact(value: i64) -> String {
    let value = value.max(0);
    if value == 0 {
        return "0".to_string();
    }
    if value < 1_000 {
        return value.to_string();
    }

    let value_f64 = value as f64;
    let (scaled, suffix) = if value >= 1_000_000_000_000 {
        (value_f64 / 1_000_000_000_000.0, "T")
    } else if value >= 1_000_000_000 {
        (value_f64 / 1_000_000_000.0, "B")
    } else if value >= 1_000_000 {
        (value_f64 / 1_000_000.0, "M")
    } else {
        (value_f64 / 1_000.0, "K")
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

pub(crate) fn format_directory_display(directory: &Path, max_width: Option<usize>) -> String {
    let formatted = if let Some(rel) = relativize_to_home(directory) {
        if rel.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~{}{}", std::path::MAIN_SEPARATOR, rel.display())
        }
    } else {
        directory.display().to_string()
    };

    if let Some(max_width) = max_width {
        if max_width == 0 {
            return String::new();
        }
        if UnicodeWidthStr::width(formatted.as_str()) > max_width {
            return text_formatting::center_truncate_path(&formatted, max_width);
        }
    }

    formatted
}

pub(crate) fn format_reset_timestamp(dt: DateTime<Local>, captured_at: DateTime<Local>) -> String {
    let time = dt.format("%H:%M").to_string();
    if dt.date_naive() == captured_at.date_naive() {
        time
    } else {
        format!("{time} on {}", dt.format("%-d %b"))
    }
}

pub(crate) fn title_case(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let mut chars = s.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return String::new(),
    };
    let rest: String = chars.as_str().to_ascii_lowercase();
    first.to_uppercase().collect::<String>() + &rest
}
