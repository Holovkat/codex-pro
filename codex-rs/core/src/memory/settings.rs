use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use tokio::sync::RwLock;

use super::types::MemorySettings;

const SETTINGS_FILENAME: &str = "settings.json";

#[derive(Debug)]
pub struct MemorySettingsManager {
    path: PathBuf,
    state: RwLock<MemorySettings>,
}

impl MemorySettingsManager {
    pub async fn load(root: PathBuf) -> Result<Self> {
        let path = root.join(SETTINGS_FILENAME);
        let settings = if let Ok(bytes) = tokio::fs::read(&path).await {
            serde_json::from_slice::<MemorySettings>(&bytes)
                .unwrap_or_else(|_| MemorySettings::default())
        } else {
            MemorySettings::default()
        };
        Ok(Self {
            path,
            state: RwLock::new(settings),
        })
    }

    pub async fn get(&self) -> MemorySettings {
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn loads_default_settings_when_missing() {
        let dir = tempfile::tempdir().expect("tmp dir");
        let manager = MemorySettingsManager::load(dir.path().to_path_buf())
            .await
            .expect("load");
        let settings = manager.get().await;
        assert!(settings.enabled);
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
}
