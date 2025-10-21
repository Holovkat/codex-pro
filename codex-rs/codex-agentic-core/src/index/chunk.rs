use std::cmp::min;

use blake3::Hasher;
use serde::Deserialize;
use serde::Serialize;
use textwrap::Options as WrapOptions;
use textwrap::wrap;

pub const DEFAULT_LINES_PER_CHUNK: usize = 120;
pub const DEFAULT_OVERLAP: usize = 20;
pub const DEFAULT_BATCH_SIZE: usize = 24;

#[derive(Debug, Clone)]
pub struct ChunkInput {
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkRecord {
    pub chunk_id: usize,
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub checksum: String,
    pub snippet: String,
    pub embedding: Vec<f32>,
}

impl ChunkRecord {
    pub fn with_embedding(mut self, embedding: Vec<f32>) -> Self {
        self.embedding = embedding;
        self
    }
}

pub fn chunk_file(content: &str, lines_per_chunk: usize, overlap: usize) -> Vec<ChunkInput> {
    let mut result = Vec::new();
    let mut start = 0usize;
    if lines_per_chunk == 0 {
        return result;
    }
    let lines: Vec<&str> = content.lines().collect();
    while start < lines.len() {
        let end = min(start + lines_per_chunk, lines.len());
        let slice = &lines[start..end];
        let joined = slice.join(
            "
",
        );
        let snippet = wrap_snippet(&joined);
        result.push(ChunkInput {
            start_line: start + 1,
            end_line: end,
            text: joined,
            snippet,
        });
        if end == lines.len() {
            break;
        }
        start = end.saturating_sub(overlap.min(lines_per_chunk.saturating_sub(1)));
    }
    result
}

pub fn wrap_snippet(text: &str) -> String {
    let options = WrapOptions::new(80);
    wrap(text, options)
        .into_iter()
        .take(3)
        .collect::<Vec<_>>()
        .join(
            "
",
        )
}

pub fn checksum_for(text: &str) -> String {
    let mut hasher = Hasher::new();
    hasher.update(text.as_bytes());
    hasher.finalize().to_hex().to_string()
}
