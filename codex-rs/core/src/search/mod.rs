use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use fastembed::EmbeddingModel;
use fastembed::InitOptions;
use fastembed::TextEmbedding;
use hnsw_rs::prelude::*;
use serde::Deserialize;

const INDEX_DIR_NAME: &str = ".codex/index";
const MANIFEST_FILE: &str = "manifest.json";
const META_FILE: &str = "meta.jsonl";
const VECTORS_BASENAME: &str = "vectors";

#[derive(Debug, Clone)]
struct IndexPaths {
    index_dir: PathBuf,
    manifest_path: PathBuf,
    meta_path: PathBuf,
}

impl IndexPaths {
    fn from_root(root: PathBuf) -> Self {
        let index_dir = root.join(INDEX_DIR_NAME);
        Self {
            index_dir: index_dir.clone(),
            manifest_path: index_dir.join(MANIFEST_FILE),
            meta_path: index_dir.join(META_FILE),
        }
    }

    fn basename(&self) -> &'static str {
        VECTORS_BASENAME
    }
}

#[derive(Debug, Clone, Deserialize)]
struct IndexManifest {
    embedding_model: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ChunkRecord {
    chunk_id: usize,
    file_path: String,
    start_line: usize,
    end_line: usize,
    snippet: String,
}

fn load_manifest(path: &Path) -> Result<IndexManifest> {
    let reader = File::open(path).with_context(|| format!("unable to open {}", path.display()))?;
    serde_json::from_reader(reader).with_context(|| format!("unable to parse {}", path.display()))
}

fn load_chunk_records(path: &Path) -> Result<Vec<ChunkRecord>> {
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

fn parse_model(name: &str) -> Option<EmbeddingModel> {
    name.parse::<EmbeddingModel>().ok()
}

fn build_embedder(manifest: &IndexManifest, override_model: Option<&str>) -> Result<TextEmbedding> {
    let selected = override_model
        .and_then(parse_model)
        .or_else(|| parse_model(&manifest.embedding_model))
        .unwrap_or_default();
    TextEmbedding::try_new(InitOptions::new(selected))
        .or_else(|_| TextEmbedding::try_new(Default::default()))
        .map_err(|err| anyhow!("failed to initialise embedding model: {err}"))
}

pub fn search_index(
    project_root: &Path,
    query: &str,
    top_k: usize,
    model_override: Option<&str>,
) -> Result<Vec<SearchHit>> {
    let paths = IndexPaths::from_root(project_root.to_path_buf());
    if !paths.manifest_path.exists() {
        return Err(anyhow!(
            "index manifest missing at {}",
            paths.manifest_path.display()
        ));
    }

    let manifest = load_manifest(&paths.manifest_path)?;
    let mut embedder = build_embedder(&manifest, model_override)?;
    let chunk_records = load_chunk_records(&paths.meta_path)?;
    if chunk_records.is_empty() {
        return Err(anyhow!("no indexed chunks available"));
    }

    let query_vec = embedder
        .embed(vec![query.to_string()], None)
        .map_err(|err| anyhow!("failed to embed query: {err}"))?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("query embedding missing"))?;

    let mut hnswio = HnswIo::new(&paths.index_dir, paths.basename());
    let hnsw: Hnsw<f32, DistCosine> = hnswio
        .load_hnsw::<f32, DistCosine>()
        .map_err(|err| anyhow!("failed to load HNSW graph: {err}"))?;
    let ef = (top_k.max(1) * 4).max(64);
    let neighbours = hnsw.search(&query_vec, top_k.max(1), ef);

    let mut hits = Vec::new();
    for (rank, neighbour) in neighbours.into_iter().enumerate() {
        if let Some(record) = chunk_records
            .iter()
            .find(|chunk| chunk.chunk_id == neighbour.d_id)
        {
            hits.push(SearchHit {
                rank: rank + 1,
                score: 1.0 - neighbour.distance,
                file_path: record.file_path.clone(),
                start_line: record.start_line,
                end_line: record.end_line,
                snippet: record.snippet.clone(),
            });
        }
    }
    Ok(hits)
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub rank: usize,
    pub score: f32,
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub snippet: String,
}
