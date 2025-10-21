use anyhow::Context;
use anyhow::Result;
use codex_core::WireApi;
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::RwLock;

static SETTINGS: Lazy<RwLock<Settings>> = Lazy::new(|| RwLock::new(load()));
pub const SETTINGS_PATH: &str = "settings.json";
pub const DEFAULT_PROMPT_PATH: &str = ".codex/.custom-system-prompts";
static SETTINGS_PATH_CACHE: OnceLock<PathBuf> = OnceLock::new();
pub const DEFAULT_SEARCH_CONFIDENCE_MIN: f32 = 0.60;

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct Settings {
    pub updates: Option<Updates>,
    pub index: Option<Index>,
    pub acp: Option<Acp>,
    pub model: Option<Model>,
    pub providers: Option<Providers>,
    pub prompts: Option<Prompts>,
}

impl Settings {
    pub fn prompt_path(&self) -> PathBuf {
        self.prompts
            .as_ref()
            .and_then(|p| p.default.as_ref())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_PROMPT_PATH))
    }

    pub fn overlay_enabled(&self) -> bool {
        self.index
            .as_ref()
            .and_then(|idx| idx.overlay)
            .unwrap_or(true)
    }

    pub fn post_turn_refresh_enabled(&self) -> bool {
        self.index
            .as_ref()
            .and_then(|idx| idx.post_turn_refresh)
            .unwrap_or(true)
    }

    pub fn refresh_min_secs(&self) -> i64 {
        self.index
            .as_ref()
            .and_then(|idx| idx.refresh_min_secs)
            .unwrap_or(300)
    }

    pub fn search_confidence_min(&self) -> f32 {
        self.index
            .as_ref()
            .and_then(|idx| idx.search_confidence_min)
            .unwrap_or(DEFAULT_SEARCH_CONFIDENCE_MIN)
            .clamp(0.0, 1.0)
    }

    pub fn search_confidence_min_percent(&self) -> u8 {
        (self.search_confidence_min() * 100.0)
            .round()
            .clamp(0.0, 100.0) as u8
    }

    pub fn update_search_confidence_min(&mut self, min: Option<f32>) {
        let normalized = min.map(|value| value.clamp(0.0, 1.0));
        let index = self.index.get_or_insert_with(Index::default);
        index.search_confidence_min = normalized;
    }
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct Updates {
    pub repo: Option<String>,
    pub latest_url: Option<String>,
    pub upgrade_cmd: Option<String>,
    pub disable_check: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct Index {
    pub overlay: Option<bool>,
    pub refresh_min_secs: Option<i64>,
    pub post_turn_refresh: Option<bool>,
    pub retrieval_enabled: Option<bool>,
    pub retrieval_threshold: Option<f32>,
    pub context_tokens: Option<u32>,
    pub search_confidence_min: Option<f32>,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct Acp {
    pub yolo_with_search: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct Model {
    pub provider: Option<String>,
    pub default: Option<String>,
    pub reasoning_effort: Option<String>,
    pub reasoning_view: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct Providers {
    pub oss: Option<Oss>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub custom: BTreeMap<String, CustomProvider>,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct Oss {
    pub endpoint: Option<String>,
}

fn default_wire_api() -> WireApi {
    WireApi::Responses
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CustomProvider {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default = "default_wire_api")]
    pub wire_api: WireApi,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_models: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_model_refresh: Option<String>,
    #[serde(default)]
    pub plan_tool_enabled: bool,
}

impl Default for CustomProvider {
    fn default() -> Self {
        Self {
            name: String::new(),
            base_url: None,
            wire_api: default_wire_api(),
            default_model: None,
            added_at: None,
            cached_models: None,
            last_model_refresh: None,
            plan_tool_enabled: false,
        }
    }
}

impl CustomProvider {
    pub fn wire_api(&self) -> WireApi {
        self.wire_api
    }

    pub fn plan_tool_enabled(&self) -> bool {
        self.plan_tool_enabled
    }
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct Prompts {
    pub default: Option<String>,
}

fn resolve_settings_path() -> PathBuf {
    if let Some(path) = SETTINGS_PATH_CACHE.get() {
        return path.clone();
    }

    if let Ok(env_path) = std::env::var("CODEX_SETTINGS_PATH") {
        let path = PathBuf::from(env_path);
        if path.is_file() {
            let _ = SETTINGS_PATH_CACHE.set(path.clone());
            return path;
        }
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    if let Ok(cwd) = std::env::current_dir() {
        for ancestor in cwd.ancestors() {
            if ancestor.as_os_str().is_empty() {
                continue;
            }
            candidates.push(ancestor.join(".codex").join(SETTINGS_PATH));
            candidates.push(ancestor.join(SETTINGS_PATH));
            candidates.push(ancestor.join("codex-rs").join(SETTINGS_PATH));
            candidates.push(
                ancestor
                    .join("openai-codex")
                    .join("codex-rs")
                    .join(SETTINGS_PATH),
            );
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors() {
            if ancestor.as_os_str().is_empty() {
                continue;
            }
            candidates.push(ancestor.join(".codex").join(SETTINGS_PATH));
            candidates.push(ancestor.join(SETTINGS_PATH));
            candidates.push(ancestor.join("codex-rs").join(SETTINGS_PATH));
            candidates.push(
                ancestor
                    .join("openai-codex")
                    .join("codex-rs")
                    .join(SETTINGS_PATH),
            );
        }
    }

    if let Ok(codex_home) = std::env::var("CODEX_HOME") {
        let path = PathBuf::from(codex_home);
        candidates.push(path.join(SETTINGS_PATH));
        candidates.push(path.join(".codex").join(SETTINGS_PATH));
    }

    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join(".codex").join(SETTINGS_PATH));
    }

    candidates.push(Path::new(".codex").join(SETTINGS_PATH));
    candidates.push(Path::new(SETTINGS_PATH).to_path_buf());

    let cwd = std::env::current_dir().ok();
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));

    let mut best: Option<(PathBuf, i32)> = None;
    let mut fallback: Option<(PathBuf, i32)> = None;

    for candidate in candidates {
        let path = candidate;
        if !seen.insert(path.clone()) {
            continue;
        }

        let score = score_candidate(&path, cwd.as_deref(), exe_dir.as_deref());

        if path.exists() {
            let canonical = path.canonicalize().unwrap_or(path.clone());
            let score = score_candidate(&canonical, cwd.as_deref(), exe_dir.as_deref());

            if best
                .as_ref()
                .map(|(_, best_score)| score > *best_score)
                .unwrap_or(true)
            {
                best = Some((canonical, score));
            }
        } else if fallback
            .as_ref()
            .map(|(_, best_score)| score > *best_score)
            .unwrap_or(true)
        {
            fallback = Some((path.clone(), score));
        }
    }

    if let Some((best_path, _)) = best {
        let _ = SETTINGS_PATH_CACHE.set(best_path.clone());
        eprintln!("[codex settings] using {}", best_path.display());
        return best_path;
    }

    if let Some((preferred, _)) = fallback {
        let _ = SETTINGS_PATH_CACHE.set(preferred.clone());
        return preferred;
    }

    let default_path = Path::new(".codex").join(SETTINGS_PATH);
    let _ = SETTINGS_PATH_CACHE.set(default_path.clone());
    default_path
}

fn score_candidate(path: &Path, cwd: Option<&Path>, exe_dir: Option<&Path>) -> i32 {
    let mut score = 0;
    let mut depth = 0;
    let mut has_codex_rs = false;
    let mut has_openai_codex = false;
    let mut has_dot_codex = false;

    for component in path.components() {
        depth += 1;
        let value = component.as_os_str();
        if value == "codex-rs" {
            has_codex_rs = true;
        }
        if value == "openai-codex" {
            has_openai_codex = true;
        }
        if value == ".codex" {
            has_dot_codex = true;
        }
    }

    score += depth;

    if has_codex_rs {
        score += 1_000;
    }

    if has_openai_codex {
        score += 250;
    }

    if has_dot_codex {
        score += 500;
    }

    if path
        .parent()
        .and_then(|parent| parent.file_name())
        .map(|name| name == "codex-rs")
        .unwrap_or(false)
    {
        score += 2_000;
    }

    if path
        .parent()
        .and_then(|parent| parent.parent())
        .and_then(|grand| grand.file_name())
        .map(|name| name == "openai-codex")
        .unwrap_or(false)
    {
        score += 500;
    }

    if let Some(cwd) = cwd
        && path.starts_with(cwd)
    {
        score += 100;
    }

    if let Some(exe_dir) = exe_dir
        && path.starts_with(exe_dir)
    {
        score += 100;
    }

    score
}

pub fn load() -> Settings {
    let path = resolve_settings_path();
    if path.exists() {
        load_from_path(&path)
    } else {
        Settings::default()
    }
}

pub fn load_from_path(path: &Path) -> Settings {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save_to_path(path: &Path, settings: &Settings) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create settings parent {}", parent.display()))?;
    }
    let serialized = serde_json::to_string_pretty(settings)
        .with_context(|| format!("failed to serialize settings to {}", path.display()))?;
    fs::write(path, serialized)
        .with_context(|| format!("failed to write settings to {}", path.display()))?;
    Ok(())
}

pub fn init_global(settings: Settings) -> Settings {
    if let Ok(mut guard) = SETTINGS.write() {
        *guard = settings.clone();
    }
    settings
}

pub fn global() -> Settings {
    SETTINGS
        .read()
        .map(|guard| guard.clone())
        .unwrap_or_else(|_| load())
}

pub fn persist_default_model_selection(model: &str, provider: Option<&str>) -> Result<Settings> {
    let mut updated = global();
    let model_settings = updated.model.get_or_insert_with(Model::default);
    model_settings.default = Some(model.to_string());
    if let Some(provider) = provider {
        model_settings.provider = Some(provider.to_string());
    }
    let path = resolve_settings_path();
    save_to_path(&path, &updated)?;
    init_global(updated.clone());
    Ok(updated)
}

pub fn persist_search_confidence_min(min: Option<f32>) -> Result<Settings> {
    let mut updated = global();
    updated.update_search_confidence_min(min);
    let path = resolve_settings_path();
    save_to_path(&path, &updated)?;
    init_global(updated.clone());
    Ok(updated)
}

pub fn persist(settings: Settings) -> Result<Settings> {
    let path = resolve_settings_path();
    save_to_path(&path, &settings)?;
    init_global(settings.clone());
    Ok(settings)
}

impl Settings {
    fn providers_mut(&mut self) -> &mut Providers {
        self.providers.get_or_insert_with(Providers::default)
    }

    pub fn custom_provider(&self, id: &str) -> Option<&CustomProvider> {
        self.providers
            .as_ref()
            .and_then(|providers| providers.custom.get(id))
    }

    pub fn custom_providers(&self) -> impl Iterator<Item = (&String, &CustomProvider)> {
        self.providers
            .as_ref()
            .map(|providers| providers.custom.iter())
            .into_iter()
            .flatten()
    }

    pub fn custom_providers_mut(&mut self) -> &mut BTreeMap<String, CustomProvider> {
        &mut self.providers_mut().custom
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_prefers_codex_settings_over_workspace_root() {
        let codex_settings = Path::new("/Users/dev/workspace/openai-codex/codex-rs/settings.json");
        let workspace_settings = Path::new("/Users/dev/workspace/settings.json");
        let cwd = Some(Path::new("/Users/dev/workspace"));
        let exe_dir = Some(Path::new(
            "/Users/dev/workspace/openai-codex/codex-rs/target/debug",
        ));

        assert!(
            score_candidate(codex_settings, cwd, exe_dir)
                > score_candidate(workspace_settings, cwd, exe_dir)
        );
    }

    #[test]
    fn search_confidence_defaults_to_constant() {
        let settings = Settings::default();
        assert!(
            (settings.search_confidence_min() - DEFAULT_SEARCH_CONFIDENCE_MIN).abs() < f32::EPSILON
        );
        assert_eq!(settings.search_confidence_min_percent(), 60);
    }

    #[test]
    fn update_search_confidence_min_clamps_to_bounds() {
        let mut settings = Settings::default();
        settings.update_search_confidence_min(Some(1.5));
        assert!((settings.search_confidence_min() - 1.0).abs() < f32::EPSILON);
        settings.update_search_confidence_min(Some(-0.5));
        assert!((settings.search_confidence_min() - 0.0).abs() < f32::EPSILON);
        settings.update_search_confidence_min(None);
        assert!(
            (settings.search_confidence_min() - DEFAULT_SEARCH_CONFIDENCE_MIN).abs() < f32::EPSILON
        );
    }
}
