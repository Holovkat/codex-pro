use anyhow::Result;
use anyhow::anyhow;
use hnsw_rs::prelude::*;
use serde::Serialize;
use std::path::Path;

use super::analytics::load_chunk_records;
use super::analytics::load_manifest;
use super::embedder::EmbeddingHandle;
use super::embedder::parse_model;
use super::paths::IndexPaths;

#[derive(Debug, Clone, Serialize)]
pub struct QueryHit {
    pub rank: usize,
    pub score: f32,
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryResponse {
    pub query: String,
    pub hits: Vec<QueryHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence_min: Option<f32>,
}

pub fn query_index(
    project_root: &Path,
    query: &str,
    top_k: usize,
    model_override: Option<&str>,
) -> Result<QueryResponse> {
    let paths = IndexPaths::from_root(project_root.to_path_buf());
    if !paths.manifest_path.exists() {
        return Err(anyhow!(
            "index manifest missing at {}",
            paths.manifest_path.display()
        ));
    }
    let manifest = load_manifest(&paths.manifest_path)?;
    let chunks = load_chunk_records(&paths.meta_path)?;
    if chunks.is_empty() {
        return Err(anyhow!("no indexed chunks available"));
    }
    let mut embedder = EmbeddingHandle::new(
        model_override
            .and_then(parse_model)
            .or_else(|| parse_model(&manifest.embedding_model)),
    )?;
    let embeddings = embedder.embed(vec![query.to_string()])?;
    let query_vec = embeddings
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
        if let Some(record) = chunks.iter().find(|chunk| chunk.chunk_id == neighbour.d_id) {
            hits.push(QueryHit {
                rank: rank + 1,
                score: 1.0 - neighbour.distance,
                file_path: record.file_path.clone(),
                start_line: record.start_line,
                end_line: record.end_line,
                snippet: record.snippet.clone(),
            });
        }
    }
    Ok(QueryResponse {
        query: query.to_string(),
        hits,
        confidence_min: None,
    })
}

pub fn filter_hits_by_confidence(mut hits: Vec<QueryHit>, min: f32) -> Vec<QueryHit> {
    let threshold = min.clamp(0.0, 1.0);
    hits.retain(|hit| hit.score >= threshold);
    for (index, hit) in hits.iter_mut().enumerate() {
        hit.rank = index + 1;
    }
    hits
}

impl QueryResponse {
    pub fn with_confidence_min(mut self, min: f32) -> Self {
        self.confidence_min = Some(min.clamp(0.0, 1.0));
        self.hits = filter_hits_by_confidence(self.hits, min);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(rank: usize, score: f32) -> QueryHit {
        QueryHit {
            rank,
            score,
            file_path: "path".to_string(),
            start_line: 1,
            end_line: 2,
            snippet: "snippet".to_string(),
        }
    }

    #[test]
    fn filter_hits_reindexes_remaining_results() {
        let hits = vec![hit(1, 0.9), hit(2, 0.5), hit(3, 0.7)];
        let filtered = filter_hits_by_confidence(hits, 0.6);
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].rank, 1);
        assert_eq!(filtered[1].rank, 2);
        assert!((filtered[0].score - 0.9).abs() < f32::EPSILON);
        assert!((filtered[1].score - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn response_with_confidence_sets_field() {
        let hits = vec![hit(1, 0.9)];
        let response = QueryResponse {
            query: "q".to_string(),
            hits,
            confidence_min: None,
        }
        .with_confidence_min(0.8);
        assert_eq!(response.hits.len(), 1);
        assert!((response.hits[0].score - 0.9).abs() < f32::EPSILON);
        assert!((response.confidence_min.expect("confidence") - 0.8).abs() < f32::EPSILON);
    }
}
