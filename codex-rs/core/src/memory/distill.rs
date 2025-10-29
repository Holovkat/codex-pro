use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::thread::JoinHandle as ThreadJoinHandle;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use fastembed::TextEmbedding;
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use tokio::sync::OnceCell;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::mpsc::{self};
use tokio::task;
use tokio::task::JoinHandle;
use tracing::info;
use tracing::warn;
use uuid::Uuid;

use super::model_manager::MiniCpmManager;
use super::model_manager::MiniCpmStatus;
use super::settings::MemorySettingsManager;
use super::store::GlobalMemoryStore;
use super::types::MemoryEvent;
use super::types::MemoryHit;
use super::types::MemoryMetadata;
use super::types::MemoryMetrics;
use super::types::MemoryRecord;
use super::types::MemoryRecordUpdate;
use super::types::MemorySource;
use super::types::clean_summary;

pub type EmbedderSlot = Arc<OnceCell<Arc<Mutex<TextEmbedding>>>>;

pub async fn get_embedder(slot: &EmbedderSlot) -> Result<Arc<Mutex<TextEmbedding>>> {
    let embedder = slot
        .get_or_try_init(|| async {
            task::spawn_blocking(|| TextEmbedding::try_new(Default::default()))
                .await
                .map_err(|err| anyhow!("embedder join error: {err}"))?
                .map(|embedder| Arc::new(Mutex::new(embedder)))
                .map_err(|err| anyhow!("failed to init embedder: {err}"))
        })
        .await?;
    info!("memory embedder ready");
    Ok(Arc::clone(embedder))
}

#[derive(Debug)]
enum DistillerHandle {
    Async(JoinHandle<()>),
    Thread(ThreadJoinHandle<()>),
}

impl DistillerHandle {
    fn shutdown(self) {
        match self {
            DistillerHandle::Async(handle) => handle.abort(),
            DistillerHandle::Thread(handle) => {
                let _ = handle.join();
            }
        }
    }
}

#[derive(Clone)]
pub struct MemoryRuntime {
    pub store: Arc<Mutex<GlobalMemoryStore>>,
    pub settings: Arc<MemorySettingsManager>,
    pub model: Arc<MiniCpmManager>,
    pub embedder: EmbedderSlot,
}

#[derive(Debug)]
pub struct MemoryDistiller {
    tx: UnboundedSender<MemoryEvent>,
    handle: Option<DistillerHandle>,
    store: Option<Arc<Mutex<GlobalMemoryStore>>>,
    settings: Option<Arc<MemorySettingsManager>>,
    model: Option<Arc<MiniCpmManager>>,
}

struct WorkerContext {
    store: Arc<Mutex<GlobalMemoryStore>>,
    model: Arc<MiniCpmManager>,
    settings: Arc<MemorySettingsManager>,
    embedder: EmbedderSlot,
}

impl MemoryDistiller {
    pub async fn spawn(root: PathBuf) -> Result<(Self, MemoryRuntime)> {
        let store = Arc::new(Mutex::new(GlobalMemoryStore::open(root.clone()).await?));
        let settings = Arc::new(MemorySettingsManager::load(root.clone()).await?);
        let current_settings = settings.get().await;
        let model = Arc::new(MiniCpmManager::load(root.clone()).await?);
        let embedder: EmbedderSlot = Arc::new(OnceCell::new());
        if !current_settings.enabled {
            return Ok((
                MemoryDistiller::noop(),
                MemoryRuntime {
                    store,
                    settings,
                    model,
                    embedder,
                },
            ));
        }

        ensure_model_cache(&model).await;

        let context = Arc::new(WorkerContext {
            store: store.clone(),
            model: model.clone(),
            settings: settings.clone(),
            embedder: embedder.clone(),
        });

        let (tx, rx) = mpsc::unbounded_channel();
        let worker_context = context;
        let handle = tokio::spawn(async move {
            if let Err(err) = run_worker(worker_context, rx).await {
                warn!("memory distiller worker exited: {err:#}");
            }
        });

        Ok((
            Self {
                tx,
                handle: Some(DistillerHandle::Async(handle)),
                store: Some(store.clone()),
                settings: Some(settings.clone()),
                model: Some(model.clone()),
            },
            MemoryRuntime {
                store,
                settings,
                model,
                embedder,
            },
        ))
    }

    pub fn noop() -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<MemoryEvent>();
        let handle = match Handle::try_current() {
            Ok(runtime) => DistillerHandle::Async(runtime.spawn(async move {
                while rx.recv().await.is_some() {
                    // drop events
                }
            })),
            Err(_) => DistillerHandle::Thread(thread::spawn(
                move || {
                    while rx.blocking_recv().is_some() {}
                },
            )),
        };
        Self {
            tx,
            handle: Some(handle),
            store: None,
            settings: None,
            model: None,
        }
    }

    pub fn sender(&self) -> UnboundedSender<MemoryEvent> {
        self.tx.clone()
    }

    pub fn store(&self) -> Option<Arc<Mutex<GlobalMemoryStore>>> {
        self.store.clone()
    }

    pub fn settings_manager(&self) -> Option<Arc<MemorySettingsManager>> {
        self.settings.clone()
    }

    pub fn model_manager(&self) -> Option<Arc<MiniCpmManager>> {
        self.model.clone()
    }
}

async fn run_worker(
    context: Arc<WorkerContext>,
    mut rx: UnboundedReceiver<MemoryEvent>,
) -> Result<()> {
    while let Some(event) = rx.recv().await {
        if let Err(err) = process_event(context.clone(), event).await {
            warn!("memory distillation failed: {err:#}");
        }
    }
    Ok(())
}

async fn process_event(context: Arc<WorkerContext>, event: MemoryEvent) -> Result<()> {
    let summary = context
        .model
        .summarise(&event.text)
        .await
        .context("summarise memory event")?;
    let summary_text = clean_summary(&summary.text);
    if !context.settings.get().await.enabled {
        return Ok(());
    }
    let embedder_arc = match get_embedder(&context.embedder).await {
        Ok(embedder) => embedder,
        Err(err) => {
            warn!("memory embedder unavailable; dropping event: {err:#}");
            return Ok(());
        }
    };
    let embedding = {
        let mut embedder = embedder_arc.lock().await;
        let embeddings = embedder
            .embed(vec![summary_text.clone()], None)
            .map_err(|err| anyhow!("failed to compute embeddings: {err}"))?;
        embeddings
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("embedding result missing"))?
    };
    let record = MemoryRecord::from_event(&event, summary_text, embedding, summary.confidence);
    context
        .store
        .lock()
        .await
        .append(record)
        .context("append record to manifest")?;
    Ok(())
}

async fn ensure_model_cache(manager: &MiniCpmManager) {
    match manager.status().await {
        Ok(MiniCpmStatus::Ready { .. }) => {
            info!("MiniCPM model cache ready");
        }
        Ok(MiniCpmStatus::Missing { missing, .. }) => {
            if missing.is_empty() {
                info!("MiniCPM model cache ready");
                return;
            }
            warn!(
                "MiniCPM artifacts missing: {:?}. Attempting download to warm cache.",
                missing
            );
            match manager.download().await {
                Ok(MiniCpmStatus::Ready { .. }) => {
                    info!("MiniCPM model cache downloaded");
                }
                Ok(MiniCpmStatus::Missing { missing, .. }) => {
                    warn!(
                        "MiniCPM artifacts still missing after download: {:?}. Falling back to summariser stub until downloads succeed.",
                        missing
                    );
                }
                Err(err) => {
                    warn!("MiniCPM download failed; using fallback summariser: {err:#}");
                }
            }
        }
        Err(err) => {
            warn!("unable to determine MiniCPM status: {err:#}");
        }
    }
}

impl MemoryRuntime {
    pub fn model_manager(&self) -> Option<Arc<MiniCpmManager>> {
        Some(self.model.clone())
    }

    pub async fn load(root: PathBuf) -> Result<Self> {
        let store = Arc::new(Mutex::new(GlobalMemoryStore::open(root.clone()).await?));
        let settings = Arc::new(MemorySettingsManager::load(root.clone()).await?);
        let current_settings = settings.get().await;
        let model = Arc::new(MiniCpmManager::load(root).await?);
        if current_settings.enabled {
            ensure_model_cache(&model).await;
        }
        let embedder: EmbedderSlot = Arc::new(OnceCell::new());
        Ok(Self {
            store,
            settings,
            model,
            embedder,
        })
    }

    pub async fn list_records(&self) -> Result<Vec<MemoryRecord>> {
        let store = self.store.lock().await;
        store.load_all().context("load memory records from store")
    }

    pub async fn create_record(
        &self,
        mut summary: String,
        metadata: MemoryMetadata,
        confidence: f32,
        source: MemorySource,
    ) -> Result<MemoryRecord> {
        summary = clean_summary(&summary);
        let embedding = self.embed_text(&summary).await?;
        let mut record = MemoryRecord::new(
            summary,
            embedding,
            metadata,
            confidence.clamp(0.0, 1.0),
            source,
        );
        record.updated_at = record.created_at;
        let mut store = self.store.lock().await;
        store
            .append(record.clone())
            .context("append manual memory record")?;
        Ok(record)
    }

    pub async fn update_record(
        &self,
        record_id: Uuid,
        mut update: MemoryRecordUpdate,
    ) -> Result<MemoryRecord> {
        if let Some(summary) = update.summary.as_mut() {
            *summary = clean_summary(summary);
            if update.embedding.is_none() {
                let embedding = self.embed_text(summary).await?;
                update.embedding = Some(embedding);
            }
        }
        if let Some(confidence) = update.confidence.as_mut() {
            *confidence = confidence.clamp(0.0, 1.0);
        }
        let mut store = self.store.lock().await;
        store
            .update(record_id, update)
            .context("update memory record")
    }

    pub async fn delete_record(&self, record_id: Uuid) -> Result<Option<MemoryRecord>> {
        let mut store = self.store.lock().await;
        store.delete(record_id).context("delete memory record")
    }

    pub async fn fetch_records(&self, ids: &[Uuid]) -> Result<Vec<MemoryRecord>> {
        let mut store = self.store.lock().await;
        store.fetch(ids).context("fetch memory records")
    }

    pub async fn search_records(
        &self,
        query: &str,
        limit: usize,
        min_confidence: Option<f32>,
    ) -> Result<Vec<MemoryHit>> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        let embedding = self.embed_text(trimmed).await?;
        let store = self.store.lock().await;
        let hits = store
            .query(&embedding, limit.max(1))
            .context("query memory store")?;
        let threshold = min_confidence.unwrap_or(0.0);
        Ok(hits
            .into_iter()
            .filter(|hit| hit.record.confidence >= threshold)
            .collect())
    }

    pub async fn metrics(&self) -> Result<MemoryMetrics> {
        let store = self.store.lock().await;
        Ok(store.metrics().clone())
    }

    async fn embed_text(&self, text: &str) -> Result<Vec<f32>> {
        if !self.settings.get().await.enabled {
            return Err(anyhow!(
                "memory runtime is disabled; run `codex memory enable` to turn it back on"
            ));
        }
        let embedder_arc = get_embedder(&self.embedder).await?;
        let mut embedder = embedder_arc.lock().await;
        let embeddings = embedder
            .embed(vec![text.to_string()], None)
            .map_err(|err| anyhow!("failed to compute embedding: {err}"))?;
        embeddings
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("embedding result missing"))
    }
}

impl Drop for MemoryDistiller {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::types::MemoryMetadata;
    use crate::memory::types::MemorySource;

    #[tokio::test]
    async fn distills_event_into_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (distiller, runtime) = MemoryDistiller::spawn(dir.path().to_path_buf())
            .await
            .expect("spawn");
        let metadata = MemoryMetadata::default();
        let event = MemoryEvent::new(MemorySource::UserMessage, "hello world", metadata);
        distiller.sender().send(event).expect("send");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let records = runtime.store.lock().await.load_all().expect("load");
        assert!(!records.is_empty());
    }

    #[tokio::test]
    async fn spawn_returns_noop_when_disabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = MemorySettingsManager::load(dir.path().to_path_buf())
            .await
            .expect("load manager");
        manager
            .update(|settings| settings.enabled = false)
            .await
            .expect("disable runtime");

        let (distiller, runtime) = MemoryDistiller::spawn(dir.path().to_path_buf())
            .await
            .expect("spawn");
        assert!(distiller.store().is_none());
        let error = runtime
            .search_records("ping", 5, None)
            .await
            .expect_err("search should fail when disabled");
        assert!(error.to_string().contains("memory runtime is disabled"));
    }
}
