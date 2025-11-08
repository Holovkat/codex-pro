use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use tracing::Level;
use tracing::event;

const AGENTS_DIR: &str = "agents";
const PROFILE_FILE: &str = "profile.json";
const INSTANCES_DIR: &str = "instances";
const RUN_FILE: &str = "run.json";

/// Primary data structure describing an agent persona.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
#[derive(Default)]
pub struct AgentProfile {
    pub name: String,
    pub slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priming_prompt: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub default_command: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub enabled_tools: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_flags: Option<HashMap<String, bool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_summary: Option<String>,
}

impl AgentProfile {
    /// Returns the priming prompt or an empty string if unset.
    pub fn priming_prompt(&self) -> &str {
        self.priming_prompt.as_deref().unwrap_or_default()
    }

    pub fn touch_updated_at(&mut self) {
        let now = Utc::now().to_rfc3339();
        if self.created_at.is_none() {
            self.created_at = Some(now.clone());
        }
        self.updated_at = Some(now);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum AgentRunStatus {
    #[default]
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct AgentRunContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub command_line: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub enabled_tools: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_flags: Option<HashMap<String, bool>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dangerous_flags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct AgentRunRecord {
    pub run_id: String,
    pub agent_slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<AgentRunContext>,
    pub status: AgentRunStatus,
}

#[derive(Serialize)]
pub struct AgentRunLogRecord<'a> {
    timestamp: String,
    stream: &'a str,
    line: &'a str,
}

pub fn serialize_agent_log_record(stream: &str, line: &str) -> Result<String> {
    let record = AgentRunLogRecord {
        timestamp: Utc::now().to_rfc3339(),
        stream,
        line,
    };
    serde_json::to_string(&record).context("failed to serialize agent log entry")
}

impl AgentRunRecord {
    pub fn begin(
        agent_slug: &str,
        run_id: String,
        prompt: Option<String>,
        context: Option<AgentRunContext>,
    ) -> Self {
        Self {
            run_id,
            agent_slug: agent_slug.to_string(),
            prompt,
            started_at: Some(Utc::now().to_rfc3339()),
            context,
            status: AgentRunStatus::Running,
            ..Self::default()
        }
    }

    pub fn mark_completed(&mut self, exit_code: Option<i32>) {
        self.status = AgentRunStatus::Completed;
        self.exit_code = exit_code;
        self.completed_at = Some(Utc::now().to_rfc3339());
    }

    pub fn mark_failed(&mut self, exit_code: Option<i32>) {
        self.status = AgentRunStatus::Failed;
        self.exit_code = exit_code;
        self.completed_at = Some(Utc::now().to_rfc3339());
    }

    pub fn mark_cancelled(&mut self) {
        self.status = AgentRunStatus::Cancelled;
        self.completed_at = Some(Utc::now().to_rfc3339());
    }
}

impl From<&AgentProfile> for AgentRunContext {
    fn from(profile: &AgentProfile) -> Self {
        Self {
            agent_name: Some(profile.name.clone()),
            command_line: profile.default_command.clone(),
            enabled_tools: profile.enabled_tools.clone(),
            approval_mode: profile.approval_mode.clone(),
            sandbox_mode: profile.sandbox_mode.clone(),
            default_flags: profile.default_flags.clone(),
            dangerous_flags: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct AgentStore {
    root: PathBuf,
}

impl AgentStore {
    pub fn new() -> Result<Self> {
        let root = resolve_agents_root();
        fs::create_dir_all(&root).with_context(|| {
            format!(
                "failed to create agents root directory at {}",
                root.display()
            )
        })?;
        Ok(Self { root })
    }

    pub fn with_root<P: Into<PathBuf>>(root: P) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).with_context(|| {
            format!(
                "failed to create agents root directory at {}",
                root.display()
            )
        })?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn list_profiles(&self) -> Result<Vec<AgentProfile>> {
        let mut profiles: Vec<AgentProfile> = Vec::new();
        if !self.root.exists() {
            return Ok(profiles);
        }
        for entry in fs::read_dir(&self.root)
            .with_context(|| format!("failed to iterate agents dir {}", self.root.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let profile_path = path.join(PROFILE_FILE);
            if !profile_path.exists() {
                continue;
            }
            let profile = read_profile(&profile_path)?;
            profiles.push(profile);
        }
        profiles.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(profiles)
    }

    pub fn load_profile(&self, slug: &str) -> Result<AgentProfile> {
        let path = self.profile_path(slug);
        read_profile(&path)
    }

    pub fn load_profile_by_selector(&self, selector: &str) -> Result<AgentProfile> {
        if let Ok(profile) = self.load_profile(selector) {
            return Ok(profile);
        }

        let slug = slug_for_name(selector);
        if let Ok(profile) = self.load_profile(&slug) {
            return Ok(profile);
        }

        let selector_lower = selector.to_ascii_lowercase();
        let mut matches: Vec<AgentProfile> = self
            .list_profiles()?
            .into_iter()
            .filter(|profile| profile.name.to_ascii_lowercase() == selector_lower)
            .collect();

        if matches.is_empty() {
            return Err(anyhow!("agent '{selector}' not found"));
        }
        if matches.len() > 1 {
            return Err(anyhow!(
                "agent selector '{selector}' matched multiple entries"
            ));
        }
        Ok(matches.remove(0))
    }

    pub fn upsert_profile(&self, profile: AgentProfile) -> Result<AgentProfile> {
        self.persist_profile(profile, true)
    }

    fn persist_profile(&self, mut profile: AgentProfile, emit_event: bool) -> Result<AgentProfile> {
        let is_new = profile.slug.is_empty();
        if profile.slug.is_empty() {
            profile.slug = slugify(&profile.name);
        }

        if is_new {
            let agent_dir = self.agent_dir(&profile.slug);
            if agent_dir.exists() {
                return Err(anyhow!("agent with slug '{}' already exists", profile.slug));
            }
        }

        profile.touch_updated_at();

        let agent_dir = self.agent_dir(&profile.slug);
        fs::create_dir_all(&agent_dir)
            .with_context(|| format!("failed to create agent directory {}", agent_dir.display()))?;

        let profile_path = agent_dir.join(PROFILE_FILE);
        let serialized =
            serde_json::to_string_pretty(&profile).context("failed to serialize agent profile")?;
        fs::write(&profile_path, serialized).with_context(|| {
            format!(
                "failed to write agent profile to {}",
                profile_path.display()
            )
        })?;
        if emit_event {
            emit_agent_profile_event(if is_new { "create" } else { "update" }, &profile);
        }
        Ok(profile)
    }

    pub fn delete_profile(&self, slug: &str) -> Result<()> {
        let profile_snapshot = self.load_profile(slug).ok();
        let path = self.agent_dir(slug);
        if path.exists() {
            fs::remove_dir_all(&path)
                .with_context(|| format!("failed to delete agent directory {}", path.display()))?;
        }
        if let Some(profile) = profile_snapshot.as_ref() {
            emit_agent_profile_event("delete", profile);
        }
        Ok(())
    }

    pub fn begin_run(
        &self,
        agent_slug: &str,
        run_id: &str,
        prompt: Option<String>,
        context: Option<AgentRunContext>,
    ) -> Result<AgentRunRecord> {
        let run_dir = self.instance_dir(agent_slug, run_id);
        fs::create_dir_all(&run_dir).with_context(|| {
            format!("failed to create agent run directory {}", run_dir.display())
        })?;

        let record = AgentRunRecord::begin(agent_slug, run_id.to_string(), prompt, context);
        self.write_run_record(agent_slug, run_id, &record)?;

        let profile_path = self.profile_path(agent_slug);
        if profile_path.exists() {
            let mut profile = read_profile(&profile_path)?;
            profile.last_run_at = record.started_at.clone();
            self.persist_profile(profile, false)?;
        }
        emit_agent_run_event("start", &record);

        Ok(record)
    }

    pub fn complete_run(
        &self,
        agent_slug: &str,
        run_id: &str,
        exit_code: Option<i32>,
        failed: bool,
    ) -> Result<AgentRunRecord> {
        let mut record = self.load_run(agent_slug, run_id)?;
        if failed {
            record.mark_failed(exit_code);
        } else {
            record.mark_completed(exit_code);
        }
        self.write_run_record(agent_slug, run_id, &record)?;
        emit_agent_run_event(if failed { "failed" } else { "completed" }, &record);
        Ok(record)
    }

    pub fn cancel_run(&self, agent_slug: &str, run_id: &str) -> Result<AgentRunRecord> {
        let mut record = self.load_run(agent_slug, run_id)?;
        record.mark_cancelled();
        self.write_run_record(agent_slug, run_id, &record)?;
        emit_agent_run_event("cancelled", &record);
        Ok(record)
    }

    pub fn load_run(&self, agent_slug: &str, run_id: &str) -> Result<AgentRunRecord> {
        let path = self.instance_dir(agent_slug, run_id).join(RUN_FILE);
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read agent run {}", path.display()))?;
        let record = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse agent run {}", path.display()))?;
        Ok(record)
    }

    pub fn write_run_record(
        &self,
        agent_slug: &str,
        run_id: &str,
        record: &AgentRunRecord,
    ) -> Result<()> {
        let path = self.instance_dir(agent_slug, run_id).join(RUN_FILE);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create run parent directory {}", parent.display())
            })?;
        }
        let serialized =
            serde_json::to_string_pretty(record).context("failed to serialize agent run record")?;
        fs::write(&path, serialized)
            .with_context(|| format!("failed to write agent run {}", path.display()))?;
        Ok(())
    }

    pub fn agent_dir(&self, slug: &str) -> PathBuf {
        self.root.join(slug)
    }

    pub fn profile_path(&self, slug: &str) -> PathBuf {
        self.agent_dir(slug).join(PROFILE_FILE)
    }

    pub fn instance_dir(&self, slug: &str, run_id: &str) -> PathBuf {
        self.agent_dir(slug).join(INSTANCES_DIR).join(run_id)
    }

    pub fn update_profile_summary(
        &self,
        slug: &str,
        summary: Option<String>,
    ) -> Result<AgentProfile> {
        let mut profile = self.load_profile(slug)?;
        profile.last_run_summary = summary;
        let updated = self.upsert_profile(profile)?;
        Ok(updated)
    }

    pub fn annotate_run_summary(
        &self,
        agent_slug: &str,
        run_id: &str,
        summary: Option<String>,
    ) -> Result<AgentRunRecord> {
        let mut record = self.load_run(agent_slug, run_id)?;
        record.summary = summary;
        self.write_run_record(agent_slug, run_id, &record)?;
        Ok(record)
    }
}

fn emit_agent_profile_event(action: &str, profile: &AgentProfile) {
    let default_command = join_with(&profile.default_command, " ");
    let enabled_tools = join_with(&profile.enabled_tools, ", ");
    event!(
        Level::INFO,
        event.name = "codex.agent_profile_change",
        action = action,
        agent.slug = profile.slug.as_str(),
        agent.name = profile.name.as_str(),
        approval_mode = profile.approval_mode.as_deref().unwrap_or_default(),
        sandbox_mode = profile.sandbox_mode.as_deref().unwrap_or_default(),
        default_command = default_command.as_deref().unwrap_or_default(),
        enabled_tools = enabled_tools.as_deref().unwrap_or_default(),
    );
}

fn emit_agent_run_event(action: &str, record: &AgentRunRecord) {
    let context = record.context.as_ref();
    let command_line = context
        .and_then(|ctx| join_with(&ctx.command_line, " "))
        .unwrap_or_default();
    let enabled_tools = context
        .and_then(|ctx| join_with(&ctx.enabled_tools, ", "))
        .unwrap_or_default();
    let dangerous_flags = context
        .and_then(|ctx| join_with(&ctx.dangerous_flags, ", "))
        .unwrap_or_default();
    let default_flags_json = context
        .and_then(|ctx| ctx.default_flags.as_ref())
        .map(|flags| serde_json::to_string(flags).unwrap_or_default())
        .unwrap_or_default();
    event!(
        Level::INFO,
        event.name = "codex.agent_run",
        action = action,
        agent.slug = record.agent_slug.as_str(),
        run.id = record.run_id.as_str(),
        status = ?record.status,
        exit_code = record.exit_code,
        command_line = command_line.as_str(),
        enabled_tools = enabled_tools.as_str(),
        approval_mode = context
            .and_then(|ctx| ctx.approval_mode.as_deref())
            .unwrap_or_default(),
        sandbox_mode = context
            .and_then(|ctx| ctx.sandbox_mode.as_deref())
            .unwrap_or_default(),
        default_flags = default_flags_json.as_str(),
        dangerous_flags = dangerous_flags.as_str(),
    );
}

fn join_with(values: &[String], separator: &str) -> Option<String> {
    if values.is_empty() {
        None
    } else {
        Some(values.join(separator))
    }
}

fn resolve_agents_root() -> PathBuf {
    if let Ok(env_path) = env::var("CODEX_AGENTS_PATH")
        && !env_path.is_empty()
    {
        return PathBuf::from(env_path);
    }

    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(settings_env) = env::var("CODEX_SETTINGS_PATH") {
        let path = PathBuf::from(settings_env);
        if let Some(parent) = path.parent() {
            if parent.file_name().is_some_and(|name| name == ".codex") {
                candidates.push(parent.to_path_buf());
            } else {
                candidates.push(parent.join(".codex"));
            }
        }
    }

    if let Ok(cwd) = env::current_dir() {
        for ancestor in cwd.ancestors() {
            if ancestor.as_os_str().is_empty() {
                continue;
            }
            candidates.push(ancestor.join(".codex"));
            candidates.push(ancestor.join("codex-rs").join(".codex"));
            candidates.push(
                ancestor
                    .join("openai-codex")
                    .join("codex-rs")
                    .join(".codex"),
            );
        }
    }

    if let Ok(exe) = env::current_exe() {
        for ancestor in exe.ancestors() {
            if ancestor.as_os_str().is_empty() {
                continue;
            }
            candidates.push(ancestor.join(".codex"));
            candidates.push(ancestor.join("codex-rs").join(".codex"));
            candidates.push(
                ancestor
                    .join("openai-codex")
                    .join("codex-rs")
                    .join(".codex"),
            );
        }
    }

    for candidate in &candidates {
        let agents_dir = candidate.join(AGENTS_DIR);
        if agents_dir.exists() {
            return agents_dir;
        }
    }

    let fallback_base = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    candidates
        .into_iter()
        .next()
        .unwrap_or_else(|| fallback_base.join(".codex"))
        .join(AGENTS_DIR)
}

fn read_profile(path: &Path) -> Result<AgentProfile> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut profile: AgentProfile = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    if profile.slug.is_empty() {
        profile.slug = slugify(&profile.name);
    }
    Ok(profile)
}

fn slugify(input: &str) -> String {
    let mut slug = String::with_capacity(input.len());
    let mut last_was_dash = false;
    for ch in input.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            slug.push(lower);
            last_was_dash = false;
        } else if matches!(lower, ' ' | '-' | '_' | '.') && !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }
    if slug.is_empty() {
        slug = "agent".to_string();
    }
    slug.trim_matches('-').to_string()
}

pub fn slug_for_name(name: &str) -> String {
    slugify(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("My Agent"), "my-agent");
        assert_eq!(slugify("My_Agent"), "my-agent");
        assert_eq!(slugify("My  Agent"), "my-agent");
        assert_eq!(slugify("agent"), "agent");
    }

    #[test]
    fn round_trip_profile() {
        let tmp = TempDir::new().unwrap();
        let store = AgentStore::with_root(tmp.path().join(AGENTS_DIR)).unwrap();

        let mut profile = AgentProfile {
            name: "Demo".to_string(),
            slug: String::new(),
            description: Some("Example".to_string()),
            priming_prompt: Some("You are helpful.".to_string()),
            default_command: vec!["codex".to_string(), "exec".to_string()],
            enabled_tools: vec!["web_search_request".to_string()],
            approval_mode: Some("workspace-write".to_string()),
            sandbox_mode: Some("danger-full-access".to_string()),
            default_flags: None,
            created_at: None,
            updated_at: None,
            last_run_at: None,
            last_run_summary: None,
        };

        profile = store.upsert_profile(profile).unwrap();
        assert_eq!(profile.slug, "demo");

        let loaded = store.load_profile("demo").unwrap();
        assert_eq!(loaded.name, "Demo");
        assert_eq!(loaded.priming_prompt(), "You are helpful.");
    }

    #[test]
    fn run_lifecycle() {
        let tmp = TempDir::new().unwrap();
        let store = AgentStore::with_root(tmp.path().join(AGENTS_DIR)).unwrap();

        let profile = AgentProfile {
            name: "Runner".to_string(),
            slug: "runner".to_string(),
            ..AgentProfile::default()
        };
        store.upsert_profile(profile).unwrap();

        let run = store
            .begin_run("runner", "run-1", Some("Test".to_string()), None)
            .unwrap();
        assert!(matches!(run.status, AgentRunStatus::Running));

        let run = store
            .complete_run("runner", "run-1", Some(0), false)
            .unwrap();
        assert!(matches!(run.status, AgentRunStatus::Completed));
        assert_eq!(run.exit_code, Some(0));

        let loaded = store.load_run("runner", "run-1").unwrap();
        assert!(matches!(loaded.status, AgentRunStatus::Completed));
    }

    #[test]
    fn run_metadata_persists_defaults() {
        let tmp = TempDir::new().unwrap();
        let store = AgentStore::with_root(tmp.path().join(AGENTS_DIR)).unwrap();

        let profile = AgentProfile {
            name: "Runner".to_string(),
            slug: "runner".to_string(),
            default_command: vec!["codex-agentic".to_string(), "exec".to_string()],
            enabled_tools: vec!["web_search_request".to_string()],
            approval_mode: Some("never".to_string()),
            sandbox_mode: Some("workspace-write".to_string()),
            ..AgentProfile::default()
        };
        store.upsert_profile(profile.clone()).unwrap();

        let mut context: AgentRunContext = (&profile).into();
        context.enabled_tools = vec!["web_search_request".to_string(), "terminal".to_string()];
        context
            .dangerous_flags
            .push("dangerously_bypass_approvals_and_sandbox".to_string());

        let record = store
            .begin_run(
                "runner",
                "run-meta",
                Some("Prompt".to_string()),
                Some(context.clone()),
            )
            .unwrap();
        assert_eq!(record.context.as_ref(), Some(&context));

        let loaded = store.load_run("runner", "run-meta").unwrap();
        assert_eq!(loaded.context.as_ref(), Some(&context));
    }

    #[test]
    fn update_profile_summary_sets_field() {
        let tmp = TempDir::new().unwrap();
        let store = AgentStore::with_root(tmp.path().join(AGENTS_DIR)).unwrap();

        let profile = AgentProfile {
            name: "Summarizer".to_string(),
            slug: String::new(),
            ..AgentProfile::default()
        };
        let saved = store.upsert_profile(profile).unwrap();
        assert!(saved.last_run_summary.is_none());

        store
            .update_profile_summary(&saved.slug, Some("run summary".to_string()))
            .unwrap();
        let reloaded = store.load_profile(&saved.slug).unwrap();
        assert_eq!(reloaded.last_run_summary.as_deref(), Some("run summary"));
    }

    #[test]
    fn annotate_run_summary_persists() {
        let tmp = TempDir::new().unwrap();
        let store = AgentStore::with_root(tmp.path().join(AGENTS_DIR)).unwrap();

        let profile = AgentProfile {
            name: "Runner".to_string(),
            slug: "runner".to_string(),
            ..AgentProfile::default()
        };
        store.upsert_profile(profile).unwrap();

        store
            .begin_run("runner", "run-42", Some("Prompt".to_string()), None)
            .unwrap();
        store
            .annotate_run_summary("runner", "run-42", Some("Done".to_string()))
            .unwrap();
        let record = store.load_run("runner", "run-42").unwrap();
        assert_eq!(record.summary.as_deref(), Some("Done"));
    }
}
