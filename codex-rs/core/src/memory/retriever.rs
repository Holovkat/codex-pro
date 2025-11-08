use std::cmp::Ordering;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use tokio::sync::Mutex;

use super::MemoryRuntime;
use super::distill::EmbedderSlot;
use super::store::GlobalMemoryStore;
use super::types::MemoryHit;
use super::types::MemoryPreviewModeExt;
use super::types::MemorySettings;
use codex_protocol::protocol::MemoryPreviewMode;

const DEFAULT_MAX_RESULTS: usize = 5;

#[derive(Clone)]
pub struct MemoryRetriever {
    store: Arc<Mutex<GlobalMemoryStore>>,
    settings: Arc<super::settings::MemorySettingsManager>,
    embedder: EmbedderSlot,
}

impl MemoryRetriever {
    pub fn new(runtime: MemoryRuntime) -> Self {
        Self {
            store: Arc::clone(&runtime.store),
            settings: Arc::clone(&runtime.settings),
            embedder: runtime.embedder.clone(),
        }
    }

    pub async fn retrieve_for_text<S: AsRef<str>>(
        &self,
        text: S,
        max_results: Option<usize>,
    ) -> Result<MemoryRetrieval> {
        let query = text.as_ref().trim();
        let settings = self.settings.get().await;
        if !settings.enabled || query.is_empty() {
            return Ok(MemoryRetrieval::new(settings, Vec::new()));
        }
        let embedder_arc = match super::distill::get_embedder(&self.embedder).await {
            Ok(embedder) => embedder,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "memory embedder unavailable during retrieval; returning no hits"
                );
                return Ok(MemoryRetrieval::new(settings, Vec::new()));
            }
        };
        let embedding = {
            let mut embedder = embedder_arc.lock().await;
            let embeddings = embedder
                .embed(vec![query.to_string()], None)
                .map_err(|err| anyhow!("failed to embed memory query: {err}"))?;
            embeddings
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("memory query embedding missing result"))?
        };
        self.retrieve_with_embedding(settings, embedding, max_results)
            .await
    }

    pub async fn retrieve_for_embedding(
        &self,
        embedding: Vec<f32>,
        max_results: Option<usize>,
    ) -> Result<MemoryRetrieval> {
        let settings = self.settings.get().await;
        if !settings.enabled || embedding.is_empty() {
            return Ok(MemoryRetrieval::new(settings, Vec::new()));
        }
        self.retrieve_with_embedding(settings, embedding, max_results)
            .await
    }

    pub async fn record_preview_outcome(&self, accepted: bool) -> Result<()> {
        let mut store = self.store.lock().await;
        if accepted {
            store.record_preview_accept()?;
        } else {
            store.record_preview_skip()?;
        }
        Ok(())
    }

    async fn retrieve_with_embedding(
        &self,
        settings: MemorySettings,
        embedding: Vec<f32>,
        max_results: Option<usize>,
    ) -> Result<MemoryRetrieval> {
        let mut store = self.store.lock().await;
        let hits = store
            .query(&embedding, max_results.unwrap_or(DEFAULT_MAX_RESULTS))
            .context("memory query failed")?;
        let mut filtered: Vec<MemoryHit> = hits
            .into_iter()
            .filter(|hit| hit.record.confidence >= settings.min_confidence)
            .collect();
        filtered.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
        store.record_suggest_invocation()?;
        if filtered.is_empty() {
            store.record_miss()?;
        } else {
            store.record_hit()?;
        }
        Ok(MemoryRetrieval::new(settings, filtered))
    }
}

#[derive(Debug, Clone)]
pub struct MemoryRetrieval {
    pub settings: MemorySettings,
    pub candidates: Vec<MemoryHit>,
}

impl MemoryRetrieval {
    fn new(settings: MemorySettings, candidates: Vec<MemoryHit>) -> Self {
        Self {
            settings,
            candidates,
        }
    }

    pub fn has_candidates(&self) -> bool {
        !self.candidates.is_empty()
    }

    pub fn auto_selected(&self) -> Vec<MemoryHit> {
        if !self.has_candidates() {
            return Vec::new();
        }
        if self.settings.preview_mode.requires_user_confirmation() {
            // Manual preview: nothing auto-selected.
            return Vec::new();
        }
        self.candidates
            .iter()
            .cloned()
            .max_by(|a, b| {
                let confidence = a
                    .record
                    .confidence
                    .partial_cmp(&b.record.confidence)
                    .unwrap_or(Ordering::Equal);
                if confidence != Ordering::Equal {
                    confidence
                } else {
                    a.score.partial_cmp(&b.score).unwrap_or(Ordering::Equal)
                }
            })
            .into_iter()
            .collect()
    }

    pub fn preview_mode(&self) -> MemoryPreviewMode {
        self.settings.preview_mode
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryRuntime;
    use crate::memory::model_manager::MiniCpmManager;
    use crate::memory::settings::MemorySettingsManager;
    use crate::memory::store::GlobalMemoryStore;
    use crate::memory::types::MemoryMetadata;
    use crate::memory::types::MemoryRecord;
    use crate::memory::types::MemorySource;
    use codex_protocol::protocol::MemoryPreviewMode;
    use fastembed::TextEmbedding;
    use fastembed::TextInitOptions;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::Mutex;
    use tokio::sync::OnceCell;

    async fn build_runtime(root: &TempDir) -> MemoryRuntime {
        let store = Arc::new(Mutex::new(
            GlobalMemoryStore::open(root.path().to_path_buf())
                .await
                .expect("open store"),
        ));
        let settings = Arc::new(
            MemorySettingsManager::load(root.path().to_path_buf())
                .await
                .expect("load settings"),
        );
        let model = Arc::new(
            MiniCpmManager::load(root.path().to_path_buf())
                .await
                .expect("load model"),
        );
        let embedder = Arc::new(OnceCell::new());
        let embedder_set = embedder.set(Arc::new(Mutex::new(
            TextEmbedding::try_new(
                TextInitOptions::default().with_cache_dir(root.path().join("fastembed-cache")),
            )
            .expect("init embedder"),
        )));
        if embedder_set.is_err() {
            panic!("embedder already set");
        }
        MemoryRuntime {
            store,
            settings,
            model,
            embedder,
        }
    }

    fn record(summary: &str, embedding: Vec<f32>, confidence: f32) -> MemoryRecord {
        MemoryRecord::new(
            summary.to_string(),
            embedding,
            MemoryMetadata::default(),
            confidence,
            MemorySource::UserMessage,
        )
    }

    #[tokio::test]
    async fn auto_select_prefers_highest_confidence() {
        let temp = TempDir::new().expect("temp dir");
        let runtime = build_runtime(&temp).await;
        let mut store = runtime.store.lock().await;
        let record_a = record("alpha", vec![1.0, 0.0], 0.6);
        let record_b = record("beta", vec![0.0, 1.0], 0.9);
        let record_b_id = record_b.record_id;
        store.append(record_a).expect("append alpha");
        store.append(record_b.clone()).expect("append beta");
        drop(store);

        runtime
            .settings
            .update(|settings| {
                settings.preview_mode = MemoryPreviewMode::Disabled;
                settings.min_confidence = 0.5;
            })
            .await
            .expect("update settings");

        let retriever = MemoryRetriever::new(runtime.clone());
        let retrieval = retriever
            .retrieve_for_embedding(vec![0.7, 0.7], Some(5))
            .await
            .expect("retrieve");
        assert_eq!(retrieval.candidates.len(), 2);
        let selected = retrieval.auto_selected();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].record.record_id, record_b_id);

        let metrics = runtime.metrics().await.expect("metrics");
        assert_eq!(metrics.hits, 1);
        assert_eq!(metrics.misses, 0);
        assert_eq!(metrics.preview_accepted, 0);
        retriever
            .record_preview_outcome(true)
            .await
            .expect("record accept");
        let metrics = runtime.metrics().await.expect("metrics");
        assert_eq!(metrics.preview_accepted, 1);
        assert_eq!(metrics.preview_skipped, 0);
    }

    #[tokio::test]
    async fn preview_skip_records_metric() {
        let temp = TempDir::new().expect("temp dir");
        let runtime = build_runtime(&temp).await;
        let mut store = runtime.store.lock().await;
        let record = record("gamma", vec![1.0, 0.0], 0.8);
        store.append(record).expect("append record");
        drop(store);

        runtime
            .settings
            .update(|settings| {
                settings.preview_mode = MemoryPreviewMode::Enabled;
                settings.min_confidence = 0.5;
            })
            .await
            .expect("update settings");

        let retriever = MemoryRetriever::new(runtime.clone());
        let retrieval = retriever
            .retrieve_for_embedding(vec![1.0, 0.0], Some(3))
            .await
            .expect("retrieve");
        assert!(retrieval.has_candidates());
        retriever
            .record_preview_outcome(false)
            .await
            .expect("record skip");
        let metrics = runtime.metrics().await.expect("metrics");
        assert_eq!(metrics.preview_skipped, 1);
        assert_eq!(metrics.preview_accepted, 0);
    }

    #[tokio::test]
    async fn records_miss_when_no_candidates() {
        let temp = TempDir::new().expect("temp dir");
        let runtime = build_runtime(&temp).await;
        let retriever = MemoryRetriever::new(runtime.clone());
        let retrieval = retriever
            .retrieve_for_embedding(vec![1.0], Some(3))
            .await
            .expect("retrieve");
        assert!(!retrieval.has_candidates());
        let metrics = runtime.metrics().await.expect("metrics");
        assert_eq!(metrics.hits, 0);
        assert_eq!(metrics.misses, 1);
    }
}
