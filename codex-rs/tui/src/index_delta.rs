use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use codex_agentic_core::index::collect_indexable_files;
use codex_agentic_core::index::paths::IndexPaths;
use tokio::task;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;

#[derive(Clone, Debug, Default)]
pub(crate) struct FileSnapshot {
    entries: HashMap<String, FileSignature>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FileSignature {
    modified_secs: u64,
    len: u64,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SnapshotDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub modified: Vec<String>,
}

impl FileSnapshot {
    pub(crate) fn capture(root: &Path) -> Result<Self> {
        let paths = IndexPaths::from_root(root.to_path_buf());
        let files = collect_indexable_files(root, &paths.index_dir)?;
        let mut entries = HashMap::new();
        for file in files {
            let rel = paths.strip_to_relative(&file).to_string_lossy().to_string();
            let metadata = match std::fs::metadata(&file) {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            let modified_secs = metadata
                .modified()
                .ok()
                .and_then(|ts| ts.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|dur| dur.as_secs())
                .unwrap_or_default();
            let len = metadata.len();
            entries.insert(rel, FileSignature { modified_secs, len });
        }
        Ok(Self { entries })
    }

    pub(crate) fn diff(&self, other: &Self) -> SnapshotDiff {
        let mut diff = SnapshotDiff::default();
        for (path, sig) in &other.entries {
            match self.entries.get(path) {
                None => diff.added.push(path.clone()),
                Some(existing) if existing != sig => diff.modified.push(path.clone()),
                _ => {}
            }
        }
        for path in self.entries.keys() {
            if !other.entries.contains_key(path) {
                diff.removed.push(path.clone());
            }
        }
        diff
    }
}

impl SnapshotDiff {
    pub(crate) fn has_changes(&self) -> bool {
        !(self.added.is_empty() && self.removed.is_empty() && self.modified.is_empty())
    }
}

pub(crate) fn spawn_delta_monitor(root: PathBuf, sender: AppEventSender, interval: Duration) {
    tokio::spawn(async move {
        let mut snapshot = capture_snapshot(root.clone()).await.unwrap_or_default();
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            match capture_snapshot(root.clone()).await {
                Ok(next) => {
                    let diff = snapshot.diff(&next);
                    if diff.has_changes() {
                        snapshot = next;
                        sender.send(AppEvent::IndexDeltaDetected(diff));
                    }
                }
                Err(err) => {
                    tracing::debug!(error = %err, "index delta scan failed");
                }
            }
        }
    });
}

async fn capture_snapshot(root: PathBuf) -> Result<FileSnapshot> {
    task::spawn_blocking(move || FileSnapshot::capture(&root)).await?
}
