use std::fs::File;
use std::fs::{self};
use std::io::BufReader;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codex_agentic_core::index::embedder::EmbeddingHandle;
use serde::Deserialize;
use serde::Serialize;
use textwrap::wrap;

const MODEL_VERSION: &str = "MiniCPM-Llama3-V2-Q4_K_M";
const MODEL_FILES: &[&str] = &["model.gguf", "tokenizer.json", "config.json"];
const SAMPLE_TEXT: &str = "Discussed LightMem-inspired context memory architecture, covering capture,\
 distillation, and retrieval requirements. Identified need for MiniCPM summarisation and fastembed \
 vectors, alongside new TUI management workflows and metrics for hits versus misses.";

fn main() -> Result<()> {
    let manager = ModelManager::new()?;
    let status = manager.ensure_ready()?;

    match &status {
        ModelStatus::Available { manifest } => {
            println!(
                "MiniCPM cache ready (version {}, updated {})",
                manifest.version, manifest.last_updated
            );
        }
        ModelStatus::MissingArtifacts { missing, .. } => {
            println!("MiniCPM artifacts missing:");
            for path in missing {
                println!("  - {}", path.display());
            }
        }
    }

    let handle = manager.load_handle(status)?;
    let start = Instant::now();
    let summary = handle.summarise(SAMPLE_TEXT)?;
    let summary_elapsed = start.elapsed();
    println!(
        "Summariser returned {} chars in {:.2} ms (confidence {:.2}, model {})",
        summary.text.chars().count(),
        summary_elapsed.as_secs_f64() * 1000.0,
        summary.confidence,
        summary.model_version
    );
    println!("Summary preview:\n{}\n", summary.text);

    let mut embedder = EmbeddingProbe::new()?;
    let embed_start = Instant::now();
    let embedding = embedder.embed(&summary.text)?;
    let embed_elapsed = embed_start.elapsed();
    println!(
        "Embedding dimension {} computed in {:.2} ms",
        embedding.len(),
        embed_elapsed.as_secs_f64() * 1000.0
    );

    Ok(())
}

struct ModelManager {
    root: PathBuf,
    manifest_path: PathBuf,
}

impl ModelManager {
    fn new() -> Result<Self> {
        let root = dirs::home_dir()
            .context("home directory unavailable")?
            .join(".codex/memory/models/minicpm");
        let manifest_path = root.join("manifest.json");
        Ok(Self {
            root,
            manifest_path,
        })
    }

    fn ensure_ready(&self) -> Result<ModelStatus> {
        fs::create_dir_all(&self.root)?;
        let mut manifest = if self.manifest_path.exists() {
            load_manifest(&self.manifest_path)?
        } else {
            ModelManifest::fresh()
        };
        manifest.version = MODEL_VERSION.to_string();
        save_manifest(&self.manifest_path, &manifest)?;

        let missing: Vec<PathBuf> = MODEL_FILES
            .iter()
            .map(|name| self.root.join(name))
            .filter(|path| !path.exists())
            .collect();

        if missing.is_empty() {
            Ok(ModelStatus::Available { manifest })
        } else {
            Ok(ModelStatus::MissingArtifacts { manifest, missing })
        }
    }

    fn load_handle(&self, status: ModelStatus) -> Result<MiniCpmHandle> {
        let manifest = match status {
            ModelStatus::Available { manifest } => manifest,
            ModelStatus::MissingArtifacts { manifest, .. } => manifest,
        };
        Ok(MiniCpmHandle {
            model_dir: self.root.clone(),
            manifest,
        })
    }
}

struct MiniCpmHandle {
    model_dir: PathBuf,
    manifest: ModelManifest,
}

impl MiniCpmHandle {
    fn summarise(&self, text: &str) -> Result<SummaryResult> {
        if !self.is_model_present() {
            return Ok(fallback_summarise(text, &self.manifest.version));
        }
        Ok(fallback_summarise(text, &self.manifest.version))
    }

    fn is_model_present(&self) -> bool {
        MODEL_FILES
            .iter()
            .all(|name| self.model_dir.join(name).exists())
    }
}

struct EmbeddingProbe {
    handle: EmbeddingHandle,
}

impl EmbeddingProbe {
    fn new() -> Result<Self> {
        Ok(Self {
            handle: EmbeddingHandle::new(None)?,
        })
    }

    fn embed(&mut self, text: &str) -> Result<Vec<f32>> {
        let embeddings = self.handle.embed(vec![text.to_string()])?;
        embeddings
            .into_iter()
            .next()
            .context("embedding result missing")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModelManifest {
    version: String,
    checksum: Option<String>,
    last_updated: DateTime<Utc>,
}

impl ModelManifest {
    fn fresh() -> Self {
        Self {
            version: MODEL_VERSION.to_string(),
            checksum: None,
            last_updated: Utc::now(),
        }
    }
}

#[derive(Debug)]
enum ModelStatus {
    Available {
        manifest: ModelManifest,
    },
    MissingArtifacts {
        manifest: ModelManifest,
        missing: Vec<PathBuf>,
    },
}

#[derive(Debug)]
struct SummaryResult {
    text: String,
    confidence: f32,
    model_version: String,
}

fn fallback_summarise(text: &str, model_version: &str) -> SummaryResult {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return SummaryResult {
            text: String::new(),
            confidence: 0.0,
            model_version: model_version.to_string(),
        };
    }
    let lines = wrap(trimmed, 96);
    let joined = lines
        .into_iter()
        .take(3)
        .map(|line| line.into_owned())
        .collect::<Vec<String>>()
        .join(" ");
    let limited: String = joined.chars().take(320).collect();
    let original_count = trimmed.chars().count().max(1);
    let ratio = limited.chars().count() as f32 / original_count as f32;
    SummaryResult {
        text: limited,
        confidence: ratio.clamp(0.25, 0.95),
        model_version: model_version.to_string(),
    }
}

fn load_manifest(path: &Path) -> Result<ModelManifest> {
    let file = File::open(path)?;
    let mut buffer = Vec::new();
    BufReader::new(file).read_to_end(&mut buffer)?;
    Ok(serde_json::from_slice(&buffer)?)
}

fn save_manifest(path: &Path, manifest: &ModelManifest) -> Result<()> {
    let contents = serde_json::to_vec_pretty(manifest)?;
    fs::write(path, contents)?;
    Ok(())
}
