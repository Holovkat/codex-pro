use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::Context;
use anyhow::Result;
use tokio::sync::RwLock;
use tracing::warn;

use super::types::MemorySettings;

const SETTINGS_FILENAME: &str = "settings.json";

#[derive(Debug)]
pub struct MemorySettingsManager {
    path: PathBuf,
    state: RwLock<MemorySettings>,
    modified: RwLock<Option<SystemTime>>,
}

impl MemorySettingsManager {
    pub async fn load(root: PathBuf) -> Result<Self> {
        let path = root.join(SETTINGS_FILENAME);
        let (settings, modified) = if let Ok(bytes) = tokio::fs::read(&path).await {
            let settings = serde_json::from_slice::<MemorySettings>(&bytes)
                .unwrap_or_else(|_| MemorySettings::default());
            let modified = file_modified(&path).await;
            (settings, modified)
        } else {
            (MemorySettings::default(), None)
        };
        Ok(Self {
            path,
            state: RwLock::new(settings),
            modified: RwLock::new(modified),
        })
    }

    pub async fn get(&self) -> MemorySettings {
        if let Err(err) = self.refresh().await {
            warn!("failed to refresh memory settings from disk: {err:#}");
        }
        self.state.read().await.clone()
    }

    pub async fn set(&self, new_settings: MemorySettings) -> Result<MemorySettings> {
        let mut guard = self.state.write().await;
        *guard = new_settings;
        self.persist_locked(&guard).await?;
        Ok(guard.clone())
    }

    pub async fn update<F>(&self, mutate: F) -> Result<MemorySettings>
    where
        F: FnOnce(&mut MemorySettings),
    {
        let mut guard = self.state.write().await;
        mutate(&mut guard);
        self.persist_locked(&guard).await?;
        Ok(guard.clone())
    }

    async fn persist_locked(&self, settings: &MemorySettings) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("unable to create settings dir {}", parent.display()))?;
        }
        let tmp_path = self.path.with_extension("json.tmp");
        let raw = serde_json::to_vec_pretty(settings).context("serialize memory settings")?;
        tokio::fs::write(&tmp_path, &raw)
            .await
            .with_context(|| format!("unable to write {}", tmp_path.display()))?;
        tokio::fs::rename(&tmp_path, &self.path)
            .await
            .with_context(|| format!("unable to move {} into place", self.path.display()))?;
        let timestamp = file_modified(&self.path).await;
        let mut modified = self.modified.write().await;
        *modified = timestamp;
        Ok(())
    }

    async fn refresh(&self) -> Result<()> {
        let timestamp = file_modified(&self.path).await;
        {
            let current = self.modified.read().await;
            if timestamp.is_none() || *current == timestamp {
                return Ok(());
            }
        }
        let bytes = tokio::fs::read(&self.path)
            .await
            .with_context(|| format!("unable to read {}", self.path.display()))?;
        let parsed = serde_json::from_slice::<MemorySettings>(&bytes)
            .context("unable to parse memory settings")?;
        {
            let mut guard = self.state.write().await;
            *guard = parsed;
        }
        let mut modified = self.modified.write().await;
        *modified = timestamp;
        Ok(())
    }
}

async fn file_modified(path: &Path) -> Option<SystemTime> {
    tokio::fs::metadata(path)
        .await
        .ok()
        .and_then(|metadata| metadata.modified().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryPreviewMode;

    #[tokio::test]
    async fn loads_default_settings_when_missing() {
        let dir = tempfile::tempdir().expect("tmp dir");
        let manager = MemorySettingsManager::load(dir.path().to_path_buf())
            .await
            .expect("load");
        let settings = manager.get().await;
        assert!(!settings.enabled);
        assert_eq!(settings.min_confidence, 0.75);
    }

    #[tokio::test]
    async fn persists_updates() {
        let dir = tempfile::tempdir().expect("tmp dir");
        let manager = MemorySettingsManager::load(dir.path().to_path_buf())
            .await
            .expect("load");
        manager
            .update(|settings| {
                settings.min_confidence = 0.9;
            })
            .await
            .expect("update");
        let reloaded = MemorySettingsManager::load(dir.path().to_path_buf())
            .await
            .expect("reload");
        let settings = reloaded.get().await;
        assert!((settings.min_confidence - 0.9).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn reloads_changes_from_disk() {
        let dir = tempfile::tempdir().expect("tmp dir");
        let manager = MemorySettingsManager::load(dir.path().to_path_buf())
            .await
            .expect("load");
        manager
            .update(|settings| settings.enabled = true)
            .await
            .expect("enable");

        let mut external = manager.get().await;
        external.enabled = false;
        external.preview_mode = MemoryPreviewMode::Disabled;
        external.min_confidence = 0.8;
        external.max_tokens = 512;
        external.retention_days = 45;
        external.prefer_pull_suggestions = false;
        let raw = serde_json::to_vec_pretty(&external).expect("serialize settings");
        let path = dir.path().join(SETTINGS_FILENAME);
        tokio::fs::write(&path, raw).await.expect("write");

        let refreshed = manager.get().await;
        assert_eq!(refreshed, external);
    }
}
