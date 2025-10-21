use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use codex_agentic_core::index::analytics::IndexAnalytics;
use codex_agentic_core::index::analytics::IndexManifest;
use codex_agentic_core::index::analytics::load_analytics;
use codex_agentic_core::index::analytics::load_manifest;
use codex_agentic_core::index::paths::IndexPaths;

#[derive(Debug, Clone)]
pub(crate) struct IndexStatusSnapshot {
    pub manifest: IndexManifest,
    pub analytics: IndexAnalytics,
}

impl IndexStatusSnapshot {
    pub(crate) fn load(root: &Path) -> Result<Option<Self>> {
        let paths = IndexPaths::from_root(root.to_path_buf());
        if !paths.manifest_path.exists() {
            return Ok(None);
        }
        let manifest = load_manifest(&paths.manifest_path).with_context(|| {
            format!(
                "failed to read index manifest at {}",
                paths.manifest_path.display()
            )
        })?;
        let analytics = load_analytics(&paths.analytics_path).unwrap_or_default();
        Ok(Some(Self {
            manifest,
            analytics,
        }))
    }
}

pub(crate) fn format_age(prefix: &str, timestamp: DateTime<Utc>) -> String {
    let now = Utc::now();
    let delta = now.signed_duration_since(timestamp);
    let label = human_duration(delta);
    format!("{prefix} {label}")
}

fn human_duration(delta: Duration) -> String {
    if delta.num_seconds() < 0 {
        return "in the future".to_string();
    }
    let secs = delta.num_seconds();
    if secs < 60 {
        return format!("{secs}s ago");
    }
    if secs < 3600 {
        let minutes = secs / 60;
        return format!("{minutes}m ago");
    }
    if secs < 86_400 {
        let hours = secs / 3600;
        return format!("{hours}h ago");
    }
    let days = secs / 86_400;
    if days < 30 {
        return format!("{days}d ago");
    }
    let months = days / 30;
    if months < 12 {
        return format!("{months}mo ago");
    }
    let years = months / 12;
    format!("{years}y ago")
}
