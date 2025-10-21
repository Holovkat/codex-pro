use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

use super::chunk::ChunkRecord;

pub const INDEX_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IndexManifest {
    pub version: u32,
    pub embedding_model: String,
    pub embedding_dim: usize,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub total_files: usize,
    pub total_chunks: usize,
    pub lines_per_chunk: usize,
    pub overlap: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IndexAnalytics {
    pub last_attempt_ts: Option<DateTime<Utc>>,
    pub last_success_ts: Option<DateTime<Utc>>,
    pub last_duration_ms: Option<u128>,
    pub build_count: u64,
    pub last_error: Option<String>,
}

pub fn load_manifest(path: &Path) -> Result<IndexManifest> {
    let reader = File::open(path).with_context(|| format!("unable to open {}", path.display()))?;
    serde_json::from_reader(reader).with_context(|| format!("unable to parse {}", path.display()))
}

pub fn write_manifest(path: &Path, manifest: &IndexManifest) -> Result<()> {
    let writer =
        File::create(path).with_context(|| format!("unable to write {}", path.display()))?;
    serde_json::to_writer_pretty(writer, manifest)
        .with_context(|| format!("unable to serialize manifest at {}", path.display()))
}

pub fn load_analytics(path: &Path) -> Result<IndexAnalytics> {
    let reader = File::open(path).with_context(|| format!("unable to open {}", path.display()))?;
    serde_json::from_reader(reader).with_context(|| format!("unable to parse {}", path.display()))
}

pub fn update_analytics(path: &Path, mut update: impl FnMut(&mut IndexAnalytics)) -> Result<()> {
    let mut analytics = load_analytics(path).unwrap_or_default();
    update(&mut analytics);
    let writer =
        File::create(path).with_context(|| format!("unable to write {}", path.display()))?;
    serde_json::to_writer_pretty(writer, &analytics)
        .with_context(|| format!("unable to serialize analytics at {}", path.display()))
}

pub fn load_chunk_records(path: &Path) -> Result<Vec<ChunkRecord>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(path).with_context(|| format!("unable to open {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let record: ChunkRecord = serde_json::from_str(&line)
            .with_context(|| format!("invalid chunk entry in {}", path.display()))?;
        records.push(record);
    }
    Ok(records)
}

pub fn write_chunk_records(path: &Path, records: &[ChunkRecord]) -> Result<()> {
    let file = File::create(path).with_context(|| format!("unable to write {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    for record in records {
        let line = serde_json::to_string(record)?;
        writeln!(writer, "{line}")?;
    }
    Ok(())
}
