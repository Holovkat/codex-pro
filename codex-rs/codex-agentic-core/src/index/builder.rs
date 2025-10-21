use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use chrono::Utc;
use fslock::LockFile;
use hnsw_rs::prelude::*;

use super::analytics::INDEX_VERSION;
use super::analytics::IndexManifest;
use super::analytics::load_chunk_records;
use super::analytics::load_manifest;
use super::analytics::update_analytics;
use super::analytics::write_chunk_records;
use super::analytics::write_manifest;
use super::chunk::ChunkRecord;
use super::chunk::DEFAULT_BATCH_SIZE;
use super::chunk::DEFAULT_LINES_PER_CHUNK;
use super::chunk::DEFAULT_OVERLAP;
use super::chunk::checksum_for;
use super::chunk::chunk_file;
use super::embedder::EmbeddingHandle;
use super::embedder::parse_model;
use super::events::IndexEvent;
use super::events::IndexSummary;
use super::files::collect_indexable_files;
use super::paths::IndexPaths;

const HNSW_MAX_LAYER: usize = 16;

#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub project_root: PathBuf,
    pub batch_size: usize,
    pub lines_per_chunk: usize,
    pub overlap: usize,
    pub requested_model: Option<String>,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            project_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            batch_size: DEFAULT_BATCH_SIZE,
            lines_per_chunk: DEFAULT_LINES_PER_CHUNK,
            overlap: DEFAULT_OVERLAP,
            requested_model: None,
        }
    }
}

pub fn build_with_progress<F>(options: BuildOptions, mut callback: F) -> Result<IndexSummary>
where
    F: FnMut(IndexEvent),
{
    let paths = IndexPaths::from_root(options.project_root.clone());
    paths.ensure_dirs()?;

    let mut lock = LockFile::open(&paths.lock_path)
        .with_context(|| format!("unable to open lock file at {}", paths.lock_path.display()))?;
    lock.lock()
        .with_context(|| format!("unable to lock {}", paths.lock_path.display()))?;

    let start = Instant::now();
    let previous_manifest = load_manifest(&paths.manifest_path).ok();
    let previous_chunks = load_chunk_records(&paths.meta_path).unwrap_or_default();
    let previous_by_checksum: HashMap<String, ChunkRecord> = previous_chunks
        .into_iter()
        .map(|record| (record.checksum.clone(), record))
        .collect();

    let files = collect_indexable_files(&paths.project_root, &paths.index_dir)?;
    callback(IndexEvent::Started {
        total_files: files.len(),
    });

    let requested_model = options
        .requested_model
        .as_deref()
        .and_then(parse_model)
        .or_else(|| {
            previous_manifest
                .as_ref()
                .and_then(|m| parse_model(&m.embedding_model))
        });
    let mut embedder = EmbeddingHandle::new(requested_model)?;

    let mut chunk_records = Vec::new();
    let mut new_chunk_indices = Vec::new();
    let mut reused_chunks = 0usize;

    for (file_idx, entry) in files.iter().enumerate() {
        let rel_path = paths
            .strip_to_relative(entry)
            .to_string_lossy()
            .into_owned();
        let raw = match fs::read_to_string(entry) {
            Ok(contents) => contents,
            Err(_) => continue,
        };
        let chunks = chunk_file(
            &raw,
            options.lines_per_chunk.max(1),
            options
                .overlap
                .min(options.lines_per_chunk.saturating_sub(1)),
        );
        for chunk in chunks {
            let checksum = checksum_for(&chunk.text);
            if let Some(prev) = previous_by_checksum.get(&checksum) {
                chunk_records.push(ChunkRecord {
                    chunk_id: chunk_records.len(),
                    file_path: rel_path.clone(),
                    start_line: chunk.start_line,
                    end_line: chunk.end_line,
                    checksum,
                    snippet: chunk.snippet,
                    embedding: prev.embedding.clone(),
                });
                reused_chunks += 1;
            } else {
                let chunk_id = chunk_records.len();
                chunk_records.push(ChunkRecord {
                    chunk_id,
                    file_path: rel_path.clone(),
                    start_line: chunk.start_line,
                    end_line: chunk.end_line,
                    checksum,
                    snippet: chunk.snippet,
                    embedding: Vec::new(),
                });
                new_chunk_indices.push(chunk_id);
            }
        }

        callback(IndexEvent::Progress {
            processed_files: file_idx + 1,
            total_files: files.len(),
            processed_chunks: chunk_records.len(),
            total_chunks: chunk_records.len(),
            current_path: rel_path,
        });
    }

    if chunk_records.is_empty() {
        update_analytics(&paths.analytics_path, |analytics| {
            analytics.last_attempt_ts = Some(Utc::now());
            analytics.last_error = Some("no indexable files found".to_string());
        })?;
        callback(IndexEvent::Error {
            message: "no indexable files found".to_string(),
        });
        return Err(anyhow!("index build produced no chunks"));
    }

    embed_pending_chunks(
        &mut embedder,
        &mut chunk_records,
        new_chunk_indices,
        options.batch_size.max(1),
    )?;

    let embedding_dim = chunk_records
        .first()
        .map(|record| record.embedding.len())
        .unwrap_or(0);
    if embedding_dim == 0 {
        return Err(anyhow!("embedding dimension is zero"));
    }

    let hnsw = build_hnsw(&chunk_records)?;
    hnsw.file_dump(&paths.index_dir, paths.basename())
        .with_context(|| format!("failed to dump HNSW graph to {}", paths.index_dir.display()))?;

    write_chunk_records(&paths.meta_path, &chunk_records)?;

    let now = Utc::now();
    let manifest = IndexManifest {
        version: INDEX_VERSION,
        embedding_model: embedder.model_name().to_string(),
        embedding_dim,
        created_at: previous_manifest
            .as_ref()
            .map(|m| m.created_at)
            .unwrap_or(now),
        updated_at: now,
        total_files: files.len(),
        total_chunks: chunk_records.len(),
        lines_per_chunk: options.lines_per_chunk,
        overlap: options.overlap,
    };
    write_manifest(&paths.manifest_path, &manifest)?;

    update_analytics(&paths.analytics_path, |analytics| {
        analytics.last_attempt_ts = Some(now);
        analytics.last_success_ts = Some(now);
        analytics.last_duration_ms = Some(start.elapsed().as_millis());
        analytics.last_error = None;
        analytics.build_count = analytics.build_count.saturating_add(1);
    })?;

    let summary = IndexSummary {
        project_root: paths.project_root,
        total_files: files.len(),
        total_chunks: chunk_records.len(),
        embedding_model: manifest.embedding_model.clone(),
        embedding_dim: manifest.embedding_dim,
        duration_ms: start.elapsed().as_millis(),
        reused_chunks,
        new_chunks: chunk_records.len().saturating_sub(reused_chunks),
    };

    callback(IndexEvent::Completed(summary.clone()));
    Ok(summary)
}

fn embed_pending_chunks(
    embedder: &mut EmbeddingHandle,
    records: &mut [ChunkRecord],
    indices: Vec<usize>,
    batch_size: usize,
) -> Result<()> {
    if indices.is_empty() {
        return Ok(());
    }
    let mut batch = Vec::with_capacity(batch_size);
    for chunk_id in indices {
        if let Some(record) = records.get(chunk_id) {
            batch.push((chunk_id, record.snippet.clone()));
        }
        if batch.len() == batch_size {
            flush_batch(embedder, records, &mut batch)?;
        }
    }
    if !batch.is_empty() {
        flush_batch(embedder, records, &mut batch)?;
    }
    Ok(())
}

fn flush_batch(
    embedder: &mut EmbeddingHandle,
    records: &mut [ChunkRecord],
    batch: &mut Vec<(usize, String)>,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let texts: Vec<String> = batch.iter().map(|(_, text)| text.clone()).collect();
    let embeddings = embedder.embed(texts)?;
    for (offset, (chunk_id, _)) in batch.iter().enumerate() {
        if let Some(record) = records.get_mut(*chunk_id) {
            record.embedding = embeddings[offset].clone();
        }
    }
    batch.clear();
    Ok(())
}

#[allow(mismatched_lifetime_syntaxes)]
fn build_hnsw(records: &[ChunkRecord]) -> Result<Hnsw<f32, DistCosine>> {
    let total = records.len().max(1);
    let max_nb_connection = 32;
    let ef_c = 200.max(max_nb_connection);
    let nb_layer = HNSW_MAX_LAYER;
    let hnsw =
        Hnsw::<f32, DistCosine>::new(max_nb_connection, total, nb_layer, ef_c, DistCosine {});
    let data: Vec<(&Vec<f32>, usize)> = records
        .iter()
        .map(|record| (&record.embedding, record.chunk_id))
        .collect();
    hnsw.parallel_insert(&data);
    Ok(hnsw)
}
