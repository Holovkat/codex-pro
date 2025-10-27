use std::cmp::Ordering;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use chrono::Utc;
use fslock::LockFile;
use hnsw_rs::prelude::*;
use tokio::fs as async_fs;
use tokio::task;
use uuid::Uuid;

use super::types::MemoryHit;
use super::types::MemoryMetrics;
use super::types::MemoryRecord;
use super::types::MemoryRecordUpdate;
use super::types::MemoryStats;

const MANIFEST_FILENAME: &str = "manifest.jsonl";
const HNSW_DIRNAME: &str = "hnsw";
const HNSW_BASENAME: &str = "memory";
const LOCK_FILENAME: &str = "lock";
const METRICS_FILENAME: &str = "metrics.json";

const HNSW_MAX_CONNECTIONS: usize = 32;
const HNSW_EF_CONSTRUCTION: usize = 200;
const HNSW_MAX_LAYER: usize = 16;

#[derive(Debug)]
pub struct GlobalMemoryStore {
    root: PathBuf,
    manifest: PathBuf,
    index_dir: PathBuf,
    lock_path: PathBuf,
    metrics_path: PathBuf,
    records: Vec<MemoryRecord>,
    metrics: MemoryMetrics,
    last_rebuild_at: Option<chrono::DateTime<Utc>>,
}

impl GlobalMemoryStore {
    pub async fn open(root: PathBuf) -> Result<Self> {
        async_fs::create_dir_all(&root)
            .await
            .with_context(|| format!("unable to create memory root at {}", root.display()))?;

        let manifest = root.join(MANIFEST_FILENAME);
        let index_dir = root.join(HNSW_DIRNAME);
        async_fs::create_dir_all(&index_dir)
            .await
            .with_context(|| {
                format!(
                    "unable to create memory index dir at {}",
                    index_dir.display()
                )
            })?;

        let lock_path = root.join(LOCK_FILENAME);
        if async_fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .await
            .is_err()
        {
            File::create(&lock_path).with_context(|| {
                format!("unable to create lock file at {}", lock_path.display())
            })?;
        }

        let metrics_path = root.join(METRICS_FILENAME);

        let manifest_clone = manifest.clone();
        let metrics_clone = metrics_path.clone();

        let records = task::spawn_blocking(move || load_records(&manifest_clone)).await??;
        let metrics = task::spawn_blocking(move || load_metrics(&metrics_clone)).await??;

        let mut store = Self {
            root,
            manifest,
            index_dir,
            lock_path,
            metrics_path,
            records,
            metrics,
            last_rebuild_at: None,
        };

        if !store.records.is_empty() {
            store.rebuild()?;
        } else {
            store.clear_index_dir()?;
        }

        Ok(store)
    }

    pub fn append(&mut self, mut record: MemoryRecord) -> Result<()> {
        let _lock = self.acquire_lock()?;
        self.align_embedding(&mut record.embedding)?;
        append_manifest_record(&self.manifest, &record)?;
        self.records.push(record);
        self.rebuild_index_unlocked()
    }

    pub fn update(&mut self, record_id: Uuid, update: MemoryRecordUpdate) -> Result<MemoryRecord> {
        let _lock = self.acquire_lock()?;
        let position = self
            .records
            .iter_mut()
            .position(|record| record.record_id == record_id)
            .ok_or_else(|| anyhow!("memory record {record_id} not found"))?;
        let mut current = self.records[position].clone();
        if let Some(summary) = update.summary {
            current.summary = summary;
        }
        if let Some(mut embedding) = update.embedding {
            self.align_embedding(&mut embedding)?;
            current.embedding = embedding;
        } else {
            self.align_embedding(&mut current.embedding)?;
        }
        if let Some(metadata) = update.metadata {
            current.metadata = metadata;
        }
        if let Some(confidence) = update.confidence {
            current.confidence = confidence;
        }
        if let Some(source) = update.source {
            current.source = source;
        }
        current.updated_at = Utc::now();
        self.records[position] = current.clone();
        write_all_records(&self.manifest, &self.records)?;
        self.rebuild_index_unlocked()?;
        Ok(current)
    }

    pub fn delete(&mut self, record_id: Uuid) -> Result<Option<MemoryRecord>> {
        let _lock = self.acquire_lock()?;
        if let Some(position) = self
            .records
            .iter()
            .position(|record| record.record_id == record_id)
        {
            let removed = self.records.remove(position);
            write_all_records(&self.manifest, &self.records)?;
            self.rebuild_index_unlocked()?;
            return Ok(Some(removed));
        }
        Ok(None)
    }

    pub fn reset(&mut self) -> Result<()> {
        let _lock = self.acquire_lock()?;
        if self.manifest.exists() {
            fs::remove_file(&self.manifest).with_context(|| {
                format!("unable to remove manifest at {}", self.manifest.display())
            })?;
        }
        self.clear_index_dir()?;
        self.records.clear();
        self.metrics = MemoryMetrics::default();
        self.persist_metrics_unlocked()?;
        self.last_rebuild_at = None;
        Ok(())
    }

    pub fn rebuild(&mut self) -> Result<()> {
        let _lock = self.acquire_lock()?;
        self.rebuild_index_unlocked()
    }

    pub fn fetch(&self, ids: &[Uuid]) -> Result<Vec<MemoryRecord>> {
        let mut results = Vec::new();
        for id in ids {
            if let Some(record) = self.records.iter().find(|record| &record.record_id == id) {
                results.push(record.clone());
            }
        }
        Ok(results)
    }

    pub fn load_all(&self) -> Result<Vec<MemoryRecord>> {
        Ok(self.records.clone())
    }

    pub fn query(&self, embedding: &[f32], top_k: usize) -> Result<Vec<MemoryHit>> {
        if embedding.is_empty() || self.records.is_empty() || top_k == 0 {
            return Ok(Vec::new());
        }
        let mut io = HnswIo::new(&self.index_dir, HNSW_BASENAME);
        let graph: Hnsw<f32, DistCosine> = match io.load_hnsw::<f32, DistCosine>() {
            Ok(graph) => graph,
            Err(_) => return Ok(Vec::new()),
        };
        let ef = (top_k.max(1) * 4).max(64);
        let neighbours = graph.search(embedding, top_k.max(1), ef);
        let mut hits = Vec::new();
        for neighbour in neighbours {
            if let Some(record) = self.records.get(neighbour.d_id) {
                hits.push(MemoryHit {
                    score: 1.0 - neighbour.distance,
                    record: record.clone(),
                });
            }
        }
        Ok(hits)
    }

    pub fn stats(&self) -> Result<MemoryStats> {
        let disk_usage_bytes =
            compute_disk_usage(&self.manifest, &self.index_dir, &self.metrics_path)?;
        Ok(MemoryStats {
            total_records: self.records.len(),
            hits: self.metrics.hits,
            misses: self.metrics.misses,
            preview_accepted: self.metrics.preview_accepted,
            preview_skipped: self.metrics.preview_skipped,
            disk_usage_bytes,
            last_rebuild_at: self.last_rebuild_at,
        })
    }

    pub fn metrics(&self) -> &MemoryMetrics {
        &self.metrics
    }

    pub fn manifest_path(&self) -> &Path {
        &self.manifest
    }

    pub fn index_dir(&self) -> &Path {
        &self.index_dir
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn record_hit(&mut self) -> Result<()> {
        let _lock = self.acquire_lock()?;
        self.metrics.hits = self.metrics.hits.saturating_add(1);
        self.persist_metrics_unlocked()
    }

    pub fn record_miss(&mut self) -> Result<()> {
        let _lock = self.acquire_lock()?;
        self.metrics.misses = self.metrics.misses.saturating_add(1);
        self.persist_metrics_unlocked()
    }

    pub fn record_preview_accept(&mut self) -> Result<()> {
        let _lock = self.acquire_lock()?;
        self.metrics.preview_accepted = self.metrics.preview_accepted.saturating_add(1);
        self.persist_metrics_unlocked()
    }

    pub fn record_preview_skip(&mut self) -> Result<()> {
        let _lock = self.acquire_lock()?;
        self.metrics.preview_skipped = self.metrics.preview_skipped.saturating_add(1);
        self.persist_metrics_unlocked()
    }

    fn acquire_lock(&self) -> Result<LockFile> {
        let mut lock = LockFile::open(&self.lock_path)
            .with_context(|| format!("unable to open lock file at {}", self.lock_path.display()))?;
        lock.lock()
            .with_context(|| format!("unable to lock {}", self.lock_path.display()))?;
        Ok(lock)
    }

    fn rebuild_index_unlocked(&mut self) -> Result<()> {
        let embedding_dim = self.ensure_embeddings_ready()?;
        if embedding_dim == 0 {
            self.clear_index_dir()?;
            self.last_rebuild_at = Some(Utc::now());
            return Ok(());
        }

        self.clear_index_dir()?;

        let graph = Hnsw::<f32, DistCosine>::new(
            HNSW_MAX_CONNECTIONS,
            self.records.len(),
            HNSW_MAX_LAYER,
            HNSW_EF_CONSTRUCTION,
            DistCosine {},
        );
        let data: Vec<(&Vec<f32>, usize)> = self
            .records
            .iter()
            .enumerate()
            .map(|(idx, record)| (&record.embedding, idx))
            .collect();
        graph.parallel_insert(&data);
        graph
            .file_dump(&self.index_dir, HNSW_BASENAME)
            .with_context(|| {
                format!("failed to dump HNSW graph to {}", self.index_dir.display())
            })?;

        self.last_rebuild_at = Some(Utc::now());
        Ok(())
    }

    fn clear_index_dir(&self) -> Result<()> {
        if self.index_dir.exists() {
            fs::remove_dir_all(&self.index_dir).with_context(|| {
                format!(
                    "unable to clear memory index dir at {}",
                    self.index_dir.display()
                )
            })?;
        }
        fs::create_dir_all(&self.index_dir).with_context(|| {
            format!(
                "unable to recreate memory index dir at {}",
                self.index_dir.display()
            )
        })
    }

    fn persist_metrics_unlocked(&self) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.metrics_path)
            .with_context(|| {
                format!(
                    "unable to open metrics file at {}",
                    self.metrics_path.display()
                )
            })?;
        serde_json::to_writer(&mut file, &self.metrics)
            .context("unable to serialize memory metrics")?;
        file.write_all(b"\n")
            .context("unable to write newline to memory metrics")?;
        file.flush().context("unable to flush memory metrics")?;
        Ok(())
    }
}

fn append_manifest_record(path: &Path, record: &MemoryRecord) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create(true).append(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("unable to open {}", path.display()))?;
    serde_json::to_writer(&mut file, record).context("unable to serialize memory record")?;
    file.write_all(b"\n")
        .context("unable to write memory record newline")?;
    file.flush().context("unable to flush memory record")?;
    Ok(())
}

fn write_all_records(path: &Path, records: &[MemoryRecord]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("unable to open {}", path.display()))?;
    for record in records {
        serde_json::to_writer(&mut file, record).context("unable to serialize memory record")?;
        file.write_all(b"\n")
            .context("unable to write memory record newline")?;
    }
    file.flush().context("unable to flush memory manifest")?;
    Ok(())
}

fn load_records(path: &Path) -> Result<Vec<MemoryRecord>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(path).with_context(|| format!("unable to read {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let record: MemoryRecord =
            serde_json::from_str(&line).context("unable to parse memory record")?;
        records.push(record);
    }
    Ok(records)
}

fn load_metrics(path: &Path) -> Result<MemoryMetrics> {
    if !path.exists() {
        return Ok(MemoryMetrics::default());
    }
    let file = File::open(path).with_context(|| format!("unable to open {}", path.display()))?;
    let metrics: MemoryMetrics =
        serde_json::from_reader(file).context("unable to parse memory metrics from disk")?;
    Ok(metrics)
}

fn compute_disk_usage(manifest: &Path, index_dir: &Path, metrics_path: &Path) -> Result<u64> {
    let mut total = 0u64;
    if let Ok(meta) = fs::metadata(manifest) {
        total = total.saturating_add(meta.len());
    }
    if let Ok(entries) = fs::read_dir(index_dir) {
        for entry in entries {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                total = total.saturating_add(dir_size(&entry.path())?);
            } else {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    if let Ok(meta) = fs::metadata(metrics_path) {
        total = total.saturating_add(meta.len());
    }
    Ok(total)
}

fn dir_size(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                total = total.saturating_add(dir_size(&entry.path())?);
            } else {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(total)
}

fn adjust_embedding_dimension(values: &mut Vec<f32>, target: usize) {
    match values.len().cmp(&target) {
        Ordering::Less => values.resize(target, 0.0),
        Ordering::Greater => values.truncate(target),
        Ordering::Equal => {}
    }
}

impl GlobalMemoryStore {
    fn align_embedding(&mut self, embedding: &mut Vec<f32>) -> Result<usize> {
        if embedding.is_empty() {
            bail!("memory record embedding dimension must be non-zero");
        }
        let target = self
            .records
            .iter()
            .map(|record| record.embedding.len())
            .max()
            .unwrap_or(embedding.len())
            .max(embedding.len());
        self.normalize_embeddings_to(target)?;
        adjust_embedding_dimension(embedding, target);
        Ok(target)
    }

    fn ensure_embeddings_ready(&mut self) -> Result<usize> {
        if self.records.is_empty() {
            return Ok(0);
        }
        let target = self
            .records
            .iter()
            .map(|record| record.embedding.len())
            .max()
            .unwrap_or(0);
        self.normalize_embeddings_to(target)?;
        Ok(target)
    }

    fn normalize_embeddings_to(&mut self, target: usize) -> Result<()> {
        if target == 0 {
            return Ok(());
        }
        let mut changed = false;
        let now = Utc::now();
        for record in &mut self.records {
            if record.embedding.len() != target {
                adjust_embedding_dimension(&mut record.embedding, target);
                record.updated_at = now;
                changed = true;
            }
        }
        if changed {
            write_all_records(&self.manifest, &self.records)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::types::MemoryMetadata;
    use crate::memory::types::MemoryRecord;
    use crate::memory::types::MemoryRecordUpdate;
    use crate::memory::types::MemorySource;

    fn sample_record(summary: &str) -> MemoryRecord {
        MemoryRecord::new(
            summary.to_string(),
            vec![0.5, 0.1],
            MemoryMetadata {
                tags: vec!["test".into()],
                ..MemoryMetadata::default()
            },
            0.9,
            MemorySource::UserMessage,
        )
    }

    #[tokio::test]
    async fn append_and_load_round_trip() {
        let tmp = tempfile::tempdir().expect("tmp dir");
        let mut store = GlobalMemoryStore::open(tmp.path().join("memory"))
            .await
            .expect("open");
        let record = sample_record("First summary");
        store.append(record).expect("append");
        let loaded = store.load_all().expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].summary, "First summary");
        let mut entries = store.index_dir().read_dir().expect("read index dir");
        assert!(entries.next().is_some());
    }

    #[tokio::test]
    async fn update_overwrites_record() {
        let tmp = tempfile::tempdir().expect("tmp dir");
        let mut store = GlobalMemoryStore::open(tmp.path().join("memory"))
            .await
            .expect("open");
        let record = sample_record("Original");
        let record_id = record.record_id;
        store.append(record).expect("append");
        store
            .update(
                record_id,
                MemoryRecordUpdate {
                    summary: Some("Edited summary".into()),
                    ..MemoryRecordUpdate::default()
                },
            )
            .expect("update");
        let records = store.load_all().expect("load");
        assert_eq!(records[0].summary, "Edited summary");
    }

    #[tokio::test]
    async fn delete_removes_record() {
        let tmp = tempfile::tempdir().expect("tmp dir");
        let mut store = GlobalMemoryStore::open(tmp.path().join("memory"))
            .await
            .expect("open");
        let first = sample_record("First");
        let second = sample_record("Second");
        let first_id = first.record_id;
        store.append(first).expect("append first");
        store.append(second).expect("append second");
        let removed = store.delete(first_id).expect("delete");
        assert!(removed.is_some());
        let records = store.load_all().expect("load");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].summary, "Second");
    }

    #[tokio::test]
    async fn reset_clears_store() {
        let tmp = tempfile::tempdir().expect("tmp dir");
        let mut store = GlobalMemoryStore::open(tmp.path().join("memory"))
            .await
            .expect("open");
        store.append(sample_record("First")).expect("append");
        store.append(sample_record("Second")).expect("append");
        store.reset().expect("reset");
        let records = store.load_all().expect("load");
        assert!(records.is_empty());
        assert!(!store.manifest_path().exists());
        let mut entries = store.index_dir().read_dir().expect("read index dir");
        assert!(entries.next().is_none());
    }

    #[tokio::test]
    async fn appending_resizes_existing_embeddings() {
        let tmp = tempfile::tempdir().expect("tmp dir");
        let mut store = GlobalMemoryStore::open(tmp.path().join("memory"))
            .await
            .expect("open");
        store.append(sample_record("First")).expect("append first");
        let mut second = sample_record("Second");
        second.embedding = vec![0.1, 0.2, 0.3, 0.4];
        store.append(second).expect("append second");
        let records = store.load_all().expect("load");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].embedding.len(), 4);
        assert_eq!(records[1].embedding.len(), 4);
        store.rebuild().expect("rebuild");
    }
}
