use anyhow::Context;
use anyhow::Result;
use codex_core::config::Config;
use codex_core::config::OPENAI_DEFAULT_MODEL;
use codex_core::model_family::derive_default_model_family;
use once_cell::sync::OnceCell;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

type OverlaySources = Vec<PathBuf>;

#[derive(Clone, Debug, Default)]
struct OverlayData {
    text: String,
    sources: OverlaySources,
}

#[derive(Clone, Debug, Default)]
struct PromptHints {
    search_hint: Option<String>,
}

impl PromptHints {
    fn for_overlay(sources: &[PathBuf], confidence_percent: u8) -> Self {
        let mut candidates = Vec::new();
        for source in sources {
            if let Some(root) = project_root_from_path(source) {
                candidates.push(root);
            }
        }
        if let Ok(cwd) = env::current_dir() {
            candidates.push(cwd);
        }
        Self::from_roots(confidence_percent, candidates)
    }

    fn for_config(config: &Config) -> Self {
        let confidence_percent = settings::global().search_confidence_min_percent();
        let mut candidates = vec![config.cwd.clone()];
        if let Ok(cwd) = env::current_dir() {
            candidates.push(cwd);
        }
        Self::from_roots(confidence_percent, candidates)
    }

    fn from_roots(confidence_percent: u8, candidates: Vec<PathBuf>) -> Self {
        let mut seen = HashSet::new();
        for root in candidates {
            if root.as_os_str().is_empty() {
                continue;
            }
            if !seen.insert(root.clone()) {
                continue;
            }
            if let Some(hint) = build_search_hint(&root, confidence_percent) {
                return Self {
                    search_hint: Some(hint),
                };
            }
        }
        Self::default()
    }

    fn merge<'a>(&self, original: &'a str) -> Cow<'a, str> {
        let Some(hint) = &self.search_hint else {
            return Cow::Borrowed(original);
        };
        if original.contains(hint) {
            return Cow::Borrowed(original);
        }
        if original.trim().is_empty() {
            return Cow::Owned(hint.clone());
        }
        Cow::Owned(format!("{original}\n\n{hint}"))
    }
}

use crate::index::analytics::load_analytics;
use crate::index::analytics::load_manifest;
use crate::index::paths::IndexPaths;
use crate::settings;
use crate::settings::Settings;

static OVERLAY_PROMPT: OnceCell<String> = OnceCell::new();
static PROMPT_SOURCES: OnceCell<OverlaySources> = OnceCell::new();

pub fn default_prompt_path() -> PathBuf {
    PathBuf::from(settings::DEFAULT_PROMPT_PATH)
}

pub fn overlay_from_settings(settings: &Settings) -> Result<String> {
    let data = load_overlay_data(settings)?;
    let _ = PROMPT_SOURCES.set(data.sources.clone());
    Ok(data.text)
}

pub fn init_global_from_settings(settings: &Settings) -> Result<String> {
    let data = load_overlay_data(settings)?;
    let text = data.text.clone();
    let _ = OVERLAY_PROMPT.set(text.clone());
    let _ = PROMPT_SOURCES.set(data.sources);
    Ok(text)
}

pub fn global_prompt() -> String {
    OVERLAY_PROMPT.get().cloned().unwrap_or_else(|| {
        let settings = settings::global();
        load_overlay_data(&settings)
            .map(|data| {
                let text = data.text.clone();
                let _ = OVERLAY_PROMPT.set(text.clone());
                let _ = PROMPT_SOURCES.set(data.sources);
                text
            })
            .unwrap_or_else(|_| default_overlay_fallback())
    })
}

pub fn global_prompt_sources() -> Vec<PathBuf> {
    if let Some(sources) = PROMPT_SOURCES.get() {
        return sources.clone();
    }

    let settings = settings::global();
    load_overlay_data(&settings)
        .map(|data| {
            let _ = PROMPT_SOURCES.set(data.sources.clone());
            data.sources
        })
        .unwrap_or_default()
}

pub fn default_base_prompt() -> String {
    derive_default_model_family(OPENAI_DEFAULT_MODEL).base_instructions
}

pub fn apply_overlay_to_config(config: &mut Config, overlay: &str) {
    let hints = PromptHints::for_config(config);
    let overlay_with_hint = hints.merge(overlay);
    if let Some(merged) = merged_user_instructions(
        config.user_instructions.as_deref(),
        overlay_with_hint.as_ref(),
    ) {
        config.user_instructions = Some(merged);
    }
}

fn merged_user_instructions(current: Option<&str>, overlay: &str) -> Option<String> {
    let overlay_trimmed = overlay.trim();
    if overlay_trimmed.is_empty() {
        return current.map(std::string::ToString::to_string);
    }

    match current {
        Some(existing) if !existing.trim().is_empty() => {
            if existing.contains(overlay_trimmed) {
                Some(existing.to_string())
            } else {
                Some(format!("{overlay_trimmed}\n\n{existing}"))
            }
        }
        _ => Some(overlay_trimmed.to_string()),
    }
}

fn read_prompt(path: PathBuf) -> Result<OverlayData> {
    let mut last_err = None;
    let mut seen = HashSet::new();

    for candidate in candidate_paths(&path) {
        if !seen.insert(candidate.clone()) {
            continue;
        }
        match read_prompt_from(&candidate) {
            Ok(Some(data)) => return Ok(data),
            Ok(None) => continue,
            Err(err) => last_err = Some(err),
        }
    }

    if let Some(err) = last_err {
        Err(err)
    } else {
        Ok(OverlayData::default())
    }
}

fn read_prompt_from(path: &Path) -> Result<Option<OverlayData>> {
    match fs::metadata(path) {
        Ok(meta) => {
            if meta.is_dir() {
                read_prompt_dir(path)
            } else if meta.is_file() {
                read_prompt_file(path)
            } else {
                Ok(None)
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn read_prompt_dir(dir: &Path) -> Result<Option<OverlayData>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)
        .with_context(|| format!("unable to read prompt directory at {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_file()
            && is_markdown(&path)
            && let Some(name) = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(std::string::ToString::to_string)
        {
            entries.push((name, path));
        }
    }

    entries.sort_by(|(a_name, _), (b_name, _)| natural_cmp(a_name, b_name));

    use std::collections::HashSet;

    let mut sections = Vec::new();
    let mut sources = Vec::new();
    let mut seen = HashSet::new();
    for (_, path) in entries {
        if let Some(text) = read_prompt_file_text(&path)? {
            let key = text.trim().to_string();
            if key.is_empty() {
                continue;
            }
            let is_new = seen.insert(key);
            if is_new {
                sections.push(text);
            }
            sources.push(path);
        }
    }

    if sections.is_empty() {
        Ok(None)
    } else {
        let text = dedupe_overlay_sections(sections.join("\n\n"));
        Ok(Some(OverlayData { text, sources }))
    }
}

fn read_prompt_file(path: &Path) -> Result<Option<OverlayData>> {
    if let Some(text) = read_prompt_file_text(path)? {
        Ok(Some(OverlayData {
            text,
            sources: vec![path.to_path_buf()],
        }))
    } else {
        Ok(None)
    }
}

fn read_prompt_file_text(path: &Path) -> Result<Option<String>> {
    if !is_markdown(path) {
        return Ok(None);
    }
    let contents = fs::read_to_string(path)
        .with_context(|| format!("unable to read prompt file at {}", path.display()))?;
    if contents.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(contents))
    }
}

fn is_markdown(path: &Path) -> bool {
    matches!(path.extension().and_then(OsStr::to_str), Some(ext) if ext.eq_ignore_ascii_case("md"))
}

fn natural_cmp(left: &str, right: &str) -> Ordering {
    left.to_ascii_lowercase()
        .cmp(&right.to_ascii_lowercase())
        .then_with(|| left.cmp(right))
}

fn default_overlay_fallback() -> String {
    String::new()
}

fn candidate_paths(path: &Path) -> Vec<PathBuf> {
    if path.is_absolute() {
        return vec![path.to_path_buf()];
    }

    let mut candidates = Vec::new();
    candidates.push(path.to_path_buf());

    if let Ok(current_dir) = env::current_dir() {
        candidates.push(current_dir.join(path));
    }

    if let Some(workspace_dir) = option_env!("CARGO_WORKSPACE_DIR") {
        candidates.push(PathBuf::from(workspace_dir).join(path));
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for ancestor in manifest_dir.ancestors() {
        candidates.push(ancestor.join(path));
    }

    candidates
}

fn dedupe_overlay_sections(text: String) -> String {
    use std::collections::HashSet;

    let mut seen = HashSet::new();
    let mut deduped: Vec<String> = Vec::new();

    for chunk in text.split("\n\n") {
        let trimmed = chunk.trim();
        if trimmed.is_empty() {
            if deduped
                .last()
                .map(std::string::String::is_empty)
                .unwrap_or(false)
            {
                continue;
            }
            deduped.push(String::new());
            continue;
        }

        if seen.insert(trimmed.to_string()) {
            deduped.push(chunk.to_string());
        }
    }

    // collapse consecutive empties and rebuild with double newlines
    let mut filtered = Vec::new();
    for entry in deduped {
        if entry.is_empty() {
            if filtered
                .last()
                .map(|s: &String| s.is_empty())
                .unwrap_or(false)
            {
                continue;
            }
            filtered.push(String::new());
        } else {
            filtered.push(entry);
        }
    }

    filtered
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn load_overlay_data(settings: &Settings) -> Result<OverlayData> {
    let mut data = read_prompt(settings.prompt_path())?;
    let hints = PromptHints::for_overlay(&data.sources, settings.search_confidence_min_percent());
    data.text = hints.merge(&data.text).into_owned();
    Ok(data)
}

fn build_search_hint(root: &Path, confidence_percent: u8) -> Option<String> {
    let paths = IndexPaths::from_root(root.to_path_buf());
    if !paths.manifest_path.exists() {
        return None;
    }
    let manifest = load_manifest(&paths.manifest_path).ok()?;
    if manifest.total_chunks == 0 || manifest.total_files == 0 {
        return None;
    }
    if let Ok(analytics) = load_analytics(&paths.analytics_path)
        && analytics.last_success_ts.is_none()
        && analytics.build_count == 0
    {
        return None;
    }
    Some(format!(
        "Call the `search_code` tool (or `/search-code \"<keywords>\"`) whenever you need to reference project code. It matches by meaning (not regex) and hides hits below {confidence_percent}% confidence.\n\nWhen you need stored context, call `memory_suggest` (optionally with a \"query\" argument). Then call `memory_fetch` with the IDs you plan to quote before incorporating memory content into your response."
    ))
}

fn project_root_from_path(path: &Path) -> Option<PathBuf> {
    for ancestor in path.ancestors() {
        if ancestor
            .file_name()
            .and_then(OsStr::to_str)
            .map(|name| name == ".codex")
            .unwrap_or(false)
        {
            return ancestor.parent().map(PathBuf::from);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::analytics::IndexAnalytics;
    use crate::index::analytics::IndexManifest;
    use crate::index::paths::IndexPaths;
    use chrono::Utc;
    use serde_json::to_string_pretty;
    use tempfile::tempdir;

    #[test]
    fn candidate_paths_include_manifest_ancestors() {
        let rel = PathBuf::from(".codex/.custom-system-prompts");
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let expected = manifest_dir.parent().unwrap().join(&rel);
        let candidates = candidate_paths(&rel);
        assert!(candidates.contains(&expected));
    }

    #[test]
    fn merged_user_instructions_adds_overlay() {
        let result = merged_user_instructions(None, "keep it tidy").unwrap();
        assert_eq!(result, "keep it tidy");
    }

    #[test]
    fn merged_user_instructions_appends_when_existing() {
        let existing = "prior instructions";
        let overlay = "new overlay";
        let result = merged_user_instructions(Some(existing), overlay).unwrap();
        assert!(result.starts_with(overlay));
        assert!(result.contains(existing));
    }

    #[test]
    fn directory_overlay_concatenates_markdown_only() {
        let temp = tempdir().unwrap();
        let prompts_dir = temp.path().join(".custom-system-prompts");
        fs::create_dir_all(&prompts_dir).unwrap();
        fs::write(prompts_dir.join("b.md"), "Second overlay").unwrap();
        fs::write(prompts_dir.join("a.md"), "First overlay").unwrap();
        fs::write(prompts_dir.join("notes.txt"), "ignored").unwrap();
        fs::write(prompts_dir.join("z.md.disabled"), "also ignored").unwrap();

        let data = read_prompt_from(&prompts_dir).unwrap().unwrap();
        assert!(data.text.contains("First overlay"));
        assert!(data.text.contains("Second overlay"));
        assert_eq!(data.sources.len(), 2);
        assert!(
            data.text.find("First overlay").unwrap() < data.text.find("Second overlay").unwrap()
        );
    }

    #[test]
    fn missing_directory_falls_back() {
        let path = PathBuf::from("/no/such/directory/.custom-system-prompts");
        let overlay = read_prompt(path).unwrap();
        assert!(overlay.text.is_empty());
        assert!(overlay.sources.is_empty());
    }

    #[test]
    fn duplicate_sections_are_removed() {
        let temp = tempdir().unwrap();
        let prompts_dir = temp.path().join(".custom-system-prompts");
        fs::create_dir_all(&prompts_dir).unwrap();
        fs::write(prompts_dir.join("a.md"), "Repeat me").unwrap();
        fs::write(prompts_dir.join("b.md"), "Repeat me").unwrap();

        let data = read_prompt_from(&prompts_dir).unwrap().unwrap();
        assert_eq!(data.text.trim(), "Repeat me");
        assert_eq!(data.sources.len(), 2);
    }

    #[test]
    fn prompt_hints_append_when_index_present() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let prompts_dir = root.join(".codex/.custom-system-prompts");
        fs::create_dir_all(&prompts_dir).unwrap();
        let overlay_path = prompts_dir.join("prompt.md");
        fs::write(&overlay_path, "Existing overlay").unwrap();

        let paths = IndexPaths::from_root(root.to_path_buf());
        fs::create_dir_all(&paths.index_dir).unwrap();
        let manifest = IndexManifest {
            version: 1,
            embedding_model: "test-model".to_string(),
            embedding_dim: 128,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            total_files: 1,
            total_chunks: 1,
            lines_per_chunk: 128,
            overlap: 32,
        };
        fs::write(&paths.manifest_path, to_string_pretty(&manifest).unwrap()).unwrap();
        let analytics = IndexAnalytics {
            last_success_ts: Some(Utc::now()),
            build_count: 1,
            ..IndexAnalytics::default()
        };
        fs::write(&paths.analytics_path, to_string_pretty(&analytics).unwrap()).unwrap();

        let sources = vec![overlay_path];
        let hints = PromptHints::for_overlay(&sources, 72);
        let merged = hints.merge("Existing overlay instructions").into_owned();
        assert!(merged.contains("Existing overlay instructions"));
        assert!(merged.contains("search_code"));
        assert!(merged.contains("72% confidence"));
        let no_duplicate = hints.merge(&merged);
        assert_eq!(no_duplicate.as_ref(), merged);
    }

    #[test]
    fn prompt_hints_skip_without_manifest() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let prompts_dir = root.join(".codex/.custom-system-prompts");
        fs::create_dir_all(&prompts_dir).unwrap();
        let overlay_path = prompts_dir.join("prompt.md");
        fs::write(&overlay_path, "Existing overlay").unwrap();

        let sources = vec![overlay_path];
        let hints = PromptHints::for_overlay(&sources, 50);
        let merged = hints.merge("Existing overlay instructions").into_owned();
        assert_eq!(merged, "Existing overlay instructions");
    }
}
