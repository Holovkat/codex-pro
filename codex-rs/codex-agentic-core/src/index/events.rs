use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IndexEvent {
    Started {
        total_files: usize,
    },
    Progress {
        processed_files: usize,
        total_files: usize,
        processed_chunks: usize,
        total_chunks: usize,
        current_path: String,
    },
    Completed(IndexSummary),
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexSummary {
    pub project_root: PathBuf,
    pub total_files: usize,
    pub total_chunks: usize,
    pub embedding_model: String,
    pub embedding_dim: usize,
    pub duration_ms: u128,
    pub reused_chunks: usize,
    pub new_chunks: usize,
}
