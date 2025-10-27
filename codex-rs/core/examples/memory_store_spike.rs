use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use hnsw_rs::prelude::*;
use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

const BASENAME: &str = "memory_store";
const EMBEDDING_DIM: usize = 16;
const MAX_CONNECTIONS: usize = 32;
const EF_CONSTRUCTION: usize = 200;
const NB_LAYER: usize = 16;

fn main() -> Result<()> {
    let root = dirs::home_dir()
        .context("home directory unavailable")?
        .join(".codex/memory/spike");
    let mut store = MemoryStore::open(root)?;

    println!(
        "Loaded {} existing memories from manifest.jsonl",
        store.record_count()
    );

    let mut rng = StdRng::seed_from_u64(23);
    let mut append_latencies = Vec::new();

    for idx in 0..5 {
        let summary = format!("Spike record #{idx}");
        let embedding = random_embedding(EMBEDDING_DIM, &mut rng);
        let record = MemoryRecord::new(summary.clone(), embedding, 0.85);
        let start = Instant::now();
        store.append(record)?;
        let elapsed = start.elapsed();
        append_latencies.push(elapsed);
        println!("Appended \"{}\" in {:.2} ms", summary, duration_ms(elapsed));
    }

    if let Some(reference) = store.latest_record().map(|record| record.clone()) {
        let query_start = Instant::now();
        let hits = store.query(&reference.embedding, 3)?;
        let query_elapsed = query_start.elapsed();
        println!(
            "Query returned {} hits in {:.2} ms",
            hits.len(),
            duration_ms(query_elapsed)
        );
        for hit in hits {
            println!("  score {:.3} → {}", hit.score, hit.record.summary);
        }
    }

    if !append_latencies.is_empty() {
        println!(
            "Append latency median {:.2} ms ({} inserts)",
            median_ms(&append_latencies),
            append_latencies.len()
        );
    }

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemoryRecord {
    record_id: Uuid,
    summary: String,
    embedding: Vec<f32>,
    confidence: f32,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
struct MemoryHit {
    score: f32,
    record: MemoryRecord,
}

struct MemoryStore {
    manifest_path: PathBuf,
    index_dir: PathBuf,
    records: Vec<MemoryRecord>,
}

impl MemoryStore {
    fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        let manifest_path = root.join("manifest.jsonl");
        let index_dir = root.join("hnsw");
        fs::create_dir_all(&index_dir)?;
        let records = load_records(&manifest_path)?;

        let mut store = Self {
            manifest_path,
            index_dir,
            records,
        };
        store.rebuild_graph()?;
        Ok(store)
    }

    fn append(&mut self, record: MemoryRecord) -> Result<()> {
        append_manifest_line(&self.manifest_path, &record)?;
        self.records.push(record);
        self.rebuild_graph()
    }

    fn query(&self, embedding: &[f32], top_k: usize) -> Result<Vec<MemoryHit>> {
        if self.records.is_empty() {
            return Ok(Vec::new());
        }
        let mut io = HnswIo::new(&self.index_dir, BASENAME);
        let graph: Hnsw<f32, DistCosine> =
            io.load_hnsw::<f32, DistCosine>().with_context(|| {
                format!(
                    "unable to load HNSW graph from {}",
                    self.index_dir.display()
                )
            })?;
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

    fn latest_record(&self) -> Option<&MemoryRecord> {
        self.records.last()
    }

    fn record_count(&self) -> usize {
        self.records.len()
    }

    fn rebuild_graph(&mut self) -> Result<()> {
        if self.records.is_empty() {
            return Ok(());
        }
        let graph = Hnsw::<f32, DistCosine>::new(
            MAX_CONNECTIONS,
            self.records.len(),
            NB_LAYER,
            EF_CONSTRUCTION,
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
            .file_dump(&self.index_dir, BASENAME)
            .with_context(|| {
                format!("failed to dump HNSW graph to {}", self.index_dir.display())
            })?;
        Ok(())
    }
}

fn load_records(manifest_path: &Path) -> Result<Vec<MemoryRecord>> {
    if !manifest_path.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(manifest_path)?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let record: MemoryRecord = serde_json::from_str(&line)?;
        records.push(record);
    }
    Ok(records)
}

fn append_manifest_line(path: &Path, record: &MemoryRecord) -> Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, record)?;
    file.write_all(b"\n")?;
    Ok(())
}

impl MemoryRecord {
    fn new(summary: String, embedding: Vec<f32>, confidence: f32) -> Self {
        let now = Utc::now();
        Self {
            record_id: Uuid::new_v4(),
            summary,
            embedding,
            confidence,
            created_at: now,
            updated_at: now,
        }
    }
}

fn random_embedding(dim: usize, rng: &mut StdRng) -> Vec<f32> {
    let mut values: Vec<f32> = (0..dim).map(|_| rng.random::<f32>()).collect();
    let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for value in &mut values {
            *value /= norm;
        }
    }
    values
}

fn median_ms(values: &[Duration]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut samples: Vec<u128> = values.iter().map(Duration::as_nanos).collect();
    samples.sort_unstable();
    let mid = samples.len() / 2;
    let nanos = if samples.len() % 2 == 0 {
        (samples[mid - 1] + samples[mid]) / 2
    } else {
        samples[mid]
    };
    nanos as f64 / 1_000_000.0
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_nanos() as f64 / 1_000_000.0
}
