use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
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
    dedupe_keys: HashSet<String>,
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

        let mut records = task::spawn_blocking(move || load_records(&manifest_clone)).await??;
        let removed_duplicates = prune_records(&mut records);
        let metrics = task::spawn_blocking(move || load_metrics(&metrics_clone)).await??;

        let dedupe_keys = records.iter().map(dedupe_key).collect();

        let mut store = Self {
            root,
            manifest,
            index_dir,
            lock_path,
            metrics_path,
            records,
            metrics,
            last_rebuild_at: None,
            dedupe_keys,
        };

        if removed_duplicates > 0 {
            write_all_records(&store.manifest, &store.records)?;
        }

        if !store.records.is_empty() {
            store.rebuild()?;
        } else {
            store.clear_index_dir()?;
        }

        Ok(store)
    }

    pub fn append(&mut self, mut record: MemoryRecord) -> Result<()> {
        let _lock = self.acquire_lock()?;
        let key = dedupe_key(&record);
        if !self.dedupe_keys.insert(key) {
            return Ok(());
        }
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
        let old_key = dedupe_key(&current);
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
        self.dedupe_keys.remove(&old_key);
        self.dedupe_keys.insert(dedupe_key(&current));
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
            self.dedupe_keys.remove(&dedupe_key(&removed));
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
        self.dedupe_keys.clear();
        Ok(())
    }

    pub fn rebuild(&mut self) -> Result<()> {
        let _lock = self.acquire_lock()?;
        self.rebuild_index_unlocked()
    }

    pub fn fetch(&mut self, ids: &[Uuid]) -> Result<Vec<MemoryRecord>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let _lock = self.acquire_lock()?;
        let mut results = Vec::new();
        let mut updated = false;

        for id in ids {
            if let Some(record) = self
                .records
                .iter_mut()
                .find(|record| &record.record_id == id)
            {
                record.tool_last_fetched_at = Some(Utc::now());
                results.push(record.clone());
                updated = true;
            }
        }

        if updated {
            write_all_records(&self.manifest, &self.records)?;
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
            suggest_invocations: self.metrics.suggest_invocations,
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

    pub fn record_suggest_invocation(&mut self) -> Result<()> {
        let _lock = self.acquire_lock()?;
        self.metrics.suggest_invocations = self.metrics.suggest_invocations.saturating_add(1);
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

fn dedupe_key(record: &MemoryRecord) -> String {
    let summary = record.summary.trim().to_ascii_lowercase();
    let conversation = record
        .metadata
        .conversation_id
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let role = record
        .metadata
        .role
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let source = format!("{:?}", record.source);
    format!("{summary}::{conversation}::{role}::{source}")
}

fn prune_records(records: &mut Vec<MemoryRecord>) -> usize {
    if records.is_empty() {
        return 0;
    }

    let mut newest_indices: HashMap<String, usize> = HashMap::new();

    for (idx, record) in records.iter().enumerate() {
        let key = dedupe_key(record);
        match newest_indices.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(idx);
            }
            Entry::Occupied(mut entry) => {
                let current_idx = *entry.get();
                if should_replace(&records[current_idx], record) {
                    entry.insert(idx);
                }
            }
        }
    }

    if newest_indices.len() == records.len() {
        return 0;
    }

    let mut keep_flags = vec![false; records.len()];
    for idx in newest_indices.into_values() {
        keep_flags[idx] = true;
    }
    let removed = keep_flags.iter().filter(|flag| !**flag).count();
    let mut cursor = 0;
    records.retain(|_| {
        let keep = keep_flags[cursor];
        cursor += 1;
        keep
    });
    removed
}

fn should_replace(current: &MemoryRecord, candidate: &MemoryRecord) -> bool {
    match candidate.updated_at.cmp(&current.updated_at) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => match candidate.created_at.cmp(&current.created_at) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => true,
        },
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
    use chrono::Duration;

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

    #[tokio::test]
    async fn duplicate_records_are_ignored() {
        let tmp = tempfile::tempdir().expect("tmp dir");
        let mut store = GlobalMemoryStore::open(tmp.path().join("memory"))
            .await
            .expect("open");
        store
            .append(sample_record("Repeated greeting"))
            .expect("append");
        store
            .append(sample_record("Repeated greeting"))
            .expect("append duplicate");
        let records = store.load_all().expect("load");
        assert_eq!(records.len(), 1, "duplicate summaries should be skipped");
    }

    #[tokio::test]
    async fn fetch_updates_tool_last_fetched_at() {
        let tmp = tempfile::tempdir().expect("tmp dir");
        let mut store = GlobalMemoryStore::open(tmp.path().join("memory"))
            .await
            .expect("open");
        let record = sample_record("Fetch me");
        let record_id = record.record_id;
        store.append(record).expect("append");

        let before = store.load_all().expect("load before fetch");
        assert_eq!(before.len(), 1);
        assert!(
            before[0].tool_last_fetched_at.is_none(),
            "tool_last_fetched_at should start empty"
        );

        let fetched = store.fetch(&[record_id]).expect("fetch");
        assert_eq!(fetched.len(), 1);
        assert!(
            fetched[0].tool_last_fetched_at.is_some(),
            "fetch should populate tool_last_fetched_at"
        );

        drop(store);

        let reopened = GlobalMemoryStore::open(tmp.path().join("memory"))
            .await
            .expect("reopen");
        let persisted = reopened.load_all().expect("load after reopen");
        assert_eq!(persisted.len(), 1);
        assert!(
            persisted[0].tool_last_fetched_at.is_some(),
            "tool fetch timestamp should persist in manifest"
        );
    }

    #[test]
    fn prune_records_keeps_newest_entries() {
        let mut older = sample_record("Hello again");
        older.created_at -= Duration::minutes(10);
        older.updated_at = older.created_at;

        let mut newest = sample_record("Hello again");
        newest.created_at = older.created_at + Duration::minutes(5);
        newest.updated_at = newest.created_at + Duration::seconds(30);

        let mut records = vec![older, newest.clone()];
        let removed = prune_records(&mut records);

        assert_eq!(removed, 1);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record_id, newest.record_id);
    }

    #[test]
    fn prune_records_breaks_ties_with_created_at_and_position() {
        let mut first = sample_record("Same summary");
        let mut second = sample_record("Same summary");

        let baseline = first.created_at;
        first.created_at = baseline - Duration::minutes(2);
        first.updated_at = baseline;

        second.created_at = baseline - Duration::minutes(1);
        second.updated_at = baseline;

        let mut records = vec![first, second.clone()];
        let removed = prune_records(&mut records);

        assert_eq!(removed, 1);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record_id, second.record_id);

        // Ensure that when timestamps fully match we still keep the later occurrence.
        let identical_a = sample_record("identical");
        let mut identical_b = identical_a.clone();
        identical_b.record_id = Uuid::now_v7();
        identical_b.created_at = identical_a.created_at;
        identical_b.updated_at = identical_a.updated_at;

        let mut identical_records = vec![identical_a, identical_b.clone()];
        let identical_removed = prune_records(&mut identical_records);

        assert_eq!(identical_removed, 1);
        assert_eq!(identical_records[0].record_id, identical_b.record_id);
    }

    #[tokio::test]
    async fn open_rewrites_manifest_after_pruning_duplicates() {
        let tmp = tempfile::tempdir().expect("tmp dir");
        let root = tmp.path().join("memory");
        std::fs::create_dir_all(&root).expect("create memory dir");
        let manifest_path = root.join(MANIFEST_FILENAME);

        let mut older = sample_record("Repeated hello");
        older.created_at -= Duration::minutes(30);
        older.updated_at = older.created_at;

        let mut newer = sample_record("Repeated hello");
        newer.created_at = older.created_at + Duration::minutes(5);
        newer.updated_at = newer.created_at + Duration::seconds(10);
        let expected_id = newer.record_id;

        {
            let mut file = std::fs::File::create(&manifest_path).expect("create manifest");
            serde_json::to_writer(&mut file, &older).expect("write older");
            file.write_all(b"\n").expect("newline");
            serde_json::to_writer(&mut file, &newer).expect("write newer");
            file.write_all(b"\n").expect("newline");
            file.flush().expect("flush manifest");
        }

        let store = GlobalMemoryStore::open(root.clone())
            .await
            .expect("open store");
        let records = store.load_all().expect("load pruned records");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record_id, expected_id);

        let manifest_contents =
            std::fs::read_to_string(&manifest_path).expect("read manifest after pruning");
        assert_eq!(manifest_contents.lines().count(), 1);
        assert!(
            manifest_contents.contains(&expected_id.to_string()),
            "manifest should retain the newest memory id"
        );
    }
}
