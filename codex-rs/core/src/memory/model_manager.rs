use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use chrono::DateTime;
use chrono::Utc;
use futures::StreamExt;
use llama_cpp::LlamaModel;
use llama_cpp::LlamaParams;
use llama_cpp::SessionParams;
use llama_cpp::standard_sampler::StandardSampler;
use reqwest::Client;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use textwrap::wrap;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::task::{self};
use tokio::time::sleep;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::memory::clean_summary;

const MODELS_DIRNAME: &str = "models";
const MINICPM_DIRNAME: &str = "minicpm";
const MANIFEST_FILENAME: &str = "manifest.json";
const MODEL_VERSION: &str = "MiniCPM-Llama3-V2.5-Q4_K_M";
const SUMMARY_MAX_CHARS: usize = 320;
const SUMMARY_WRAP_WIDTH: usize = 96;
const SUMMARY_MAX_TOKENS: usize = 160;
const SUMMARY_CONTEXT_TOKENS: usize = 4096;
const APPROX_CHARS_PER_TOKEN: usize = 4;
const SUMMARY_RETRY_ATTEMPTS: usize = 3;
const SUMMARY_RETRY_BACKOFF_MS: u64 = 250;
const SUMMARY_END_MARKER: &str = "<END>";

#[derive(Debug, Clone)]
pub struct MiniCpmSummary {
    pub text: String,
    pub confidence: f32,
    pub model_version: String,
}

#[derive(Debug, Clone)]
pub enum MiniCpmStatus {
    Ready {
        version: String,
        last_updated: DateTime<Utc>,
    },
    Missing {
        version: String,
        last_updated: DateTime<Utc>,
        missing: Vec<String>,
    },
}

#[derive(Debug)]
pub struct MiniCpmManager {
    _root: PathBuf,
    model_dir: PathBuf,
    manifest_path: PathBuf,
    client: Client,
    manifest: tokio::sync::RwLock<ModelManifest>,
    runner: Mutex<Option<Arc<MiniCpmRunner>>>,
    download_state: Mutex<MiniCpmDownloadState>,
    diagnostics: Mutex<MiniCpmDiagnostics>,
}

impl MiniCpmManager {
    pub async fn load(root: PathBuf) -> Result<Self> {
        let model_dir = root.join(MODELS_DIRNAME).join(MINICPM_DIRNAME);
        tokio::fs::create_dir_all(&model_dir)
            .await
            .with_context(|| {
                format!("unable to create MiniCPM model dir {}", model_dir.display())
            })?;
        let manifest_path = model_dir.join(MANIFEST_FILENAME);
        let manifest = if let Ok(bytes) = tokio::fs::read(&manifest_path).await {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| ModelManifest::fresh())
        } else {
            ModelManifest::fresh()
        };
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .context("unable to build download client")?;
        Ok(Self {
            _root: root,
            model_dir,
            manifest_path,
            client,
            manifest: tokio::sync::RwLock::new(manifest),
            runner: Mutex::new(None),
            download_state: Mutex::new(MiniCpmDownloadState::default()),
            diagnostics: Mutex::new(MiniCpmDiagnostics::default()),
        })
    }

    pub async fn download_state(&self) -> MiniCpmDownloadState {
        self.download_state.lock().await.clone()
    }

    pub async fn diagnostics(&self) -> MiniCpmDiagnostics {
        self.diagnostics.lock().await.clone()
    }

    async fn ensure_runner(&self, version: &str) -> Result<Option<Arc<MiniCpmRunner>>> {
        {
            let guard = self.runner.lock().await;
            if let Some(runner) = guard.as_ref() {
                return Ok(Some(runner.clone()));
            }
        }

        if !MODEL_ARTIFACTS
            .iter()
            .all(|artifact| self.model_dir.join(artifact.filename).exists())
        {
            return Ok(None);
        }

        let model_dir = self.model_dir.clone();
        let version = version.to_string();
        let runner = MiniCpmRunner::load(model_dir, version).await?;
        let runner = Arc::new(runner);
        let mut guard = self.runner.lock().await;
        if let Some(existing) = guard.as_ref() {
            return Ok(Some(existing.clone()));
        }
        *guard = Some(runner.clone());
        Ok(Some(runner))
    }

    async fn record_success(&self) {
        let mut diagnostics = self.diagnostics.lock().await;
        diagnostics.last_success_at = Some(Utc::now());
    }

    async fn record_failure(&self, attempt: usize, err: &anyhow::Error) {
        let mut diagnostics = self.diagnostics.lock().await;
        diagnostics.last_failure = Some(MiniCpmFailure {
            attempt,
            occurred_at: Utc::now(),
            message: format!("{err:#}"),
        });
    }

    async fn reset_runner(&self) {
        let mut guard = self.runner.lock().await;
        *guard = None;
    }

    async fn begin_download(&self) {
        let mut state = self.download_state.lock().await;
        state.started_at = Some(Utc::now());
        state.completed_at = None;
        state.artifacts.clear();
        for artifact in MODEL_ARTIFACTS.iter() {
            state.artifacts.insert(
                artifact.filename.to_string(),
                MiniCpmArtifactState::new(artifact.filename),
            );
        }
    }

    async fn mark_download_complete(&self) {
        let mut state = self.download_state.lock().await;
        state.completed_at = Some(Utc::now());
    }

    async fn note_artifact_status(&self, filename: &str, status: MiniCpmArtifactStatus) {
        let mut state = self.download_state.lock().await;
        let entry = state
            .artifacts
            .entry(filename.to_string())
            .or_insert_with(|| MiniCpmArtifactState::new(filename));
        entry.status = status;
        entry.last_updated_at = Utc::now();
        if matches!(status, MiniCpmArtifactStatus::Ready)
            && let Some(total) = entry.total_bytes
        {
            entry.downloaded_bytes = total;
        }
        if !matches!(status, MiniCpmArtifactStatus::Failed) {
            entry.error = None;
        }
    }

    async fn note_artifact_progress(&self, filename: &str, downloaded: u64, total: Option<u64>) {
        let mut state = self.download_state.lock().await;
        let entry = state
            .artifacts
            .entry(filename.to_string())
            .or_insert_with(|| MiniCpmArtifactState::new(filename));
        entry.status = MiniCpmArtifactStatus::Downloading;
        entry.downloaded_bytes = downloaded;
        if let Some(total) = total {
            entry.total_bytes = Some(total);
        }
        entry.last_updated_at = Utc::now();
        entry.error = None;
    }

    async fn note_artifact_verified(&self, filename: &str, size: u64, checksum: Option<String>) {
        let mut state = self.download_state.lock().await;
        let entry = state
            .artifacts
            .entry(filename.to_string())
            .or_insert_with(|| MiniCpmArtifactState::new(filename));
        entry.status = MiniCpmArtifactStatus::Ready;
        entry.downloaded_bytes = size;
        entry.total_bytes = Some(size);
        if let Some(checksum) = checksum {
            entry.checksum = Some(checksum);
        }
        entry.error = None;
        entry.last_updated_at = Utc::now();
    }

    async fn note_artifact_error(&self, filename: &str, err: &anyhow::Error) {
        let mut state = self.download_state.lock().await;
        let entry = state
            .artifacts
            .entry(filename.to_string())
            .or_insert_with(|| MiniCpmArtifactState::new(filename));
        entry.status = MiniCpmArtifactStatus::Failed;
        entry.error = Some(format!("{err:#}"));
        entry.last_updated_at = Utc::now();
    }

    pub fn model_dir(&self) -> &Path {
        &self.model_dir
    }

    pub async fn status(&self) -> Result<MiniCpmStatus> {
        let manifest = self.manifest.read().await.clone();
        let missing = self
            .missing_artifacts(&manifest)
            .await
            .context("unable to determine MiniCPM artifact status")?;
        if missing.is_empty() {
            Ok(MiniCpmStatus::Ready {
                version: manifest.version,
                last_updated: manifest.last_updated,
            })
        } else {
            Ok(MiniCpmStatus::Missing {
                version: manifest.version,
                last_updated: manifest.last_updated,
                missing,
            })
        }
    }

    pub async fn download(&self) -> Result<MiniCpmStatus> {
        self.begin_download().await;

        let mut manifest = self.manifest.read().await.clone();
        let mut updated = false;

        for artifact in MODEL_ARTIFACTS.iter() {
            let filename = artifact.filename;
            self.note_artifact_status(filename, MiniCpmArtifactStatus::Verifying)
                .await;
            let path = self.model_dir.join(filename);
            let mut needs_download = true;
            if path.exists() {
                match file_sha256(&path).await {
                    Ok(checksum) => {
                        if manifest
                            .artifacts
                            .get(filename)
                            .map(|known| known == &checksum)
                            .unwrap_or(false)
                        {
                            let size = tokio::fs::metadata(&path)
                                .await
                                .map(|meta| meta.len())
                                .unwrap_or(0);
                            self.note_artifact_verified(filename, size, Some(checksum.clone()))
                                .await;
                            needs_download = false;
                        } else {
                            debug!(
                                "MiniCPM artifact {} checksum mismatch (manifest {:?}, actual {})",
                                filename,
                                manifest.artifacts.get(filename),
                                checksum
                            );
                        }
                    }
                    Err(err) => {
                        warn!(
                            "MiniCPM artifact {} checksum verification failed: {err:#}",
                            filename
                        );
                        self.note_artifact_error(filename, &err).await;
                    }
                }
            }

            if needs_download {
                self.note_artifact_status(filename, MiniCpmArtifactStatus::Downloading)
                    .await;
                info!("downloading MiniCPM artifact {}", filename);
                match self.download_artifact(artifact).await {
                    Ok((checksum, bytes)) => {
                        self.note_artifact_verified(filename, bytes, Some(checksum.clone()))
                            .await;
                        manifest.artifacts.insert(filename.to_string(), checksum);
                        updated = true;
                    }
                    Err(err) => {
                        self.note_artifact_error(filename, &err).await;
                        warn!("MiniCPM artifact {} failed to download: {err:#}", filename);
                    }
                }
            }
        }

        if updated {
            manifest.last_updated = Utc::now();
            self.persist_manifest(&manifest).await?;
            {
                let mut manifest_lock = self.manifest.write().await;
                *manifest_lock = manifest.clone();
            }
            self.reset_runner().await;
        }

        self.mark_download_complete().await;
        self.status().await
    }

    async fn missing_artifacts(&self, manifest: &ModelManifest) -> Result<Vec<String>> {
        let mut missing = Vec::new();
        for artifact in MODEL_ARTIFACTS.iter() {
            let path = self.model_dir.join(artifact.filename);
            if !path.exists() {
                missing.push(artifact.filename.to_string());
                continue;
            }
            let checksum = file_sha256(&path).await?;
            if manifest
                .artifacts
                .get(artifact.filename)
                .map(|known| known != &checksum)
                .unwrap_or(true)
            {
                missing.push(artifact.filename.to_string());
            }
        }
        Ok(missing)
    }

    async fn download_artifact(&self, artifact: &ModelArtifact) -> Result<(String, u64)> {
        let response = self
            .client
            .get(artifact.url)
            .send()
            .await
            .with_context(|| format!("failed to start download for {}", artifact.url))?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "MiniCPM download for {} failed with HTTP {}",
                artifact.filename,
                response.status()
            ));
        }
        let tmp_path = self.model_dir.join(format!("{}.part", artifact.filename));
        let mut file = tokio::fs::File::create(&tmp_path)
            .await
            .with_context(|| format!("unable to create {}", tmp_path.display()))?;
        let total = response.content_length();
        let mut stream = response.bytes_stream();
        let mut hasher = Sha256::new();
        let mut downloaded: u64 = 0;
        if let Some(total) = total {
            self.note_artifact_progress(artifact.filename, 0, Some(total))
                .await;
        }
        while let Some(chunk) = stream.next().await {
            let data = chunk.context("download stream interrupted")?;
            hasher.update(&data);
            file.write_all(&data)
                .await
                .with_context(|| format!("unable to write {}", tmp_path.display()))?;
            downloaded += data.len() as u64;
            self.note_artifact_progress(artifact.filename, downloaded, total)
                .await;
        }
        file.flush().await.ok();
        let checksum = hex_encode(&hasher.finalize());
        tokio::fs::rename(&tmp_path, self.model_dir.join(artifact.filename))
            .await
            .with_context(|| {
                format!(
                    "unable to move {} into place",
                    self.model_dir.join(artifact.filename).display()
                )
            })?;
        let final_size = if let Some(total) = total {
            total
        } else {
            downloaded
        };
        self.note_artifact_progress(artifact.filename, final_size, Some(final_size))
            .await;
        Ok((checksum, final_size))
    }

    async fn persist_manifest(&self, manifest: &ModelManifest) -> Result<()> {
        let data = serde_json::to_vec_pretty(manifest).context("serialize MiniCPM manifest")?;
        let tmp = self.manifest_path.with_extension("json.tmp");
        tokio::fs::write(&tmp, data)
            .await
            .with_context(|| format!("unable to write {}", tmp.display()))?;
        tokio::fs::rename(&tmp, &self.manifest_path)
            .await
            .with_context(|| format!("unable to persist {}", self.manifest_path.display()))?;
        Ok(())
    }

    pub async fn summarise(&self, text: &str) -> Result<MiniCpmSummary> {
        let trimmed = text.trim();
        let manifest = self.manifest.read().await.clone();
        let version = manifest.version.clone();
        drop(manifest);

        if trimmed.is_empty() {
            return Ok(MiniCpmSummary {
                text: String::new(),
                confidence: 0.0,
                model_version: version,
            });
        }

        let runner = match self.ensure_runner(&version).await {
            Ok(Some(runner)) => runner,
            Ok(None) => return Ok(fallback_summarise(trimmed, &version)),
            Err(err) => {
                warn!("MiniCPM runner unavailable: {err:#}");
                self.record_failure(0, &err).await;
                return Ok(fallback_summarise(trimmed, &version));
            }
        };

        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..SUMMARY_RETRY_ATTEMPTS {
            match runner.summarise(trimmed).await {
                Ok(summary) => {
                    self.record_success().await;
                    return Ok(summary);
                }
                Err(err) => {
                    warn!(
                        "MiniCPM summarisation attempt {} failed: {err:#}",
                        attempt + 1
                    );
                    self.record_failure(attempt + 1, &err).await;
                    last_err = Some(err);
                    if attempt + 1 < SUMMARY_RETRY_ATTEMPTS {
                        let delay = SUMMARY_RETRY_BACKOFF_MS * (attempt as u64 + 1);
                        sleep(Duration::from_millis(delay)).await;
                    }
                }
            }
        }

        if let Some(err) = last_err {
            warn!("MiniCPM summarisation falling back after retries: {err:#}");
        }
        Ok(fallback_summarise(trimmed, &version))
    }
}

#[derive(Debug, Clone, Default)]
pub struct MiniCpmDiagnostics {
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_failure: Option<MiniCpmFailure>,
}

#[derive(Debug, Clone)]
pub struct MiniCpmFailure {
    pub attempt: usize,
    pub occurred_at: DateTime<Utc>,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct MiniCpmDownloadState {
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub artifacts: HashMap<String, MiniCpmArtifactState>,
}

#[derive(Debug, Clone)]
pub struct MiniCpmArtifactState {
    pub filename: String,
    pub status: MiniCpmArtifactStatus,
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
    pub checksum: Option<String>,
    pub error: Option<String>,
    pub last_updated_at: DateTime<Utc>,
}

impl MiniCpmArtifactState {
    fn new(filename: &str) -> Self {
        Self {
            filename: filename.to_string(),
            status: MiniCpmArtifactStatus::Pending,
            total_bytes: None,
            downloaded_bytes: 0,
            checksum: None,
            error: None,
            last_updated_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MiniCpmArtifactStatus {
    Pending,
    Verifying,
    Downloading,
    Ready,
    Failed,
}

struct MiniCpmRunner {
    model: LlamaModel,
    version: String,
}

impl MiniCpmRunner {
    async fn load(model_dir: PathBuf, version: String) -> Result<Self> {
        let path = model_dir.join("model.gguf");
        if !path.exists() {
            return Err(anyhow!("MiniCPM model file missing at {}", path.display()));
        }
        let model_result = task::spawn_blocking({
            let path = path.clone();
            move || {
                LlamaModel::load_from_file(path, LlamaParams::default())
                    .map_err(|err| anyhow!("failed to load MiniCPM model: {err}"))
            }
        })
        .await
        .context("MiniCPM model load task interrupted")?;
        let model = model_result?;
        Ok(Self { model, version })
    }

    async fn summarise(&self, text: &str) -> Result<MiniCpmSummary> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(anyhow!("empty summarisation input"));
        }
        let input_chars = trimmed.chars().count();
        let (clipped, truncated) = Self::clip_input(trimmed);
        let prompt = Self::build_prompt(&clipped);
        let model = self.model.clone();
        let raw_result = task::spawn_blocking(move || Self::run_inference(model, prompt))
            .await
            .context("MiniCPM summarisation task interrupted")?;
        let raw = raw_result?;
        self.finalize_summary(raw, input_chars, truncated)
    }

    fn run_inference(model: LlamaModel, prompt: String) -> Result<String> {
        let mut session = model
            .create_session(Self::session_params())
            .map_err(|err| anyhow!("failed to create MiniCPM session: {err}"))?;
        session
            .advance_context(prompt.as_bytes())
            .map_err(|err| anyhow!("failed to prime MiniCPM context: {err}"))?;
        let handle = session
            .start_completing_with(StandardSampler::default(), SUMMARY_MAX_TOKENS)
            .map_err(|err| anyhow!("failed to start MiniCPM completion: {err}"))?;
        Ok(handle.into_string())
    }

    fn session_params() -> SessionParams {
        let mut params = SessionParams::default();
        params.n_ctx = SUMMARY_CONTEXT_TOKENS as u32;
        params.n_batch = params.n_ctx.min(512);
        params.n_ubatch = params.n_batch;
        params.seed = 42;
        params
    }

    fn clip_input(text: &str) -> (String, bool) {
        let total_chars = text.chars().count();
        let max_chars = SUMMARY_CONTEXT_TOKENS * APPROX_CHARS_PER_TOKEN;
        if total_chars <= max_chars {
            return (text.to_string(), false);
        }
        let skip = total_chars - max_chars;
        let clipped: String = text.chars().skip(skip).collect();
        (clipped, true)
    }

    fn build_prompt(text: &str) -> String {
        format!(
            "You are Codex's memory summariser. Summarise the following context into at most three concise sentences highlighting actionable details and decisions. Use plain text with no markup. End your response with the marker {SUMMARY_END_MARKER}.\n\nContext:\n{text}\n\nSummary:\n"
        )
    }

    fn finalize_summary(
        &self,
        raw: String,
        input_chars: usize,
        truncated_input: bool,
    ) -> Result<MiniCpmSummary> {
        let marker_index = raw.find(SUMMARY_END_MARKER);
        let missing_marker = marker_index.is_none();
        let trimmed_output = marker_index.map(|idx| &raw[..idx]).unwrap_or(&raw).trim();
        if trimmed_output.is_empty() {
            return Err(anyhow!("MiniCPM returned empty summary output"));
        }
        let limited: String = trimmed_output.chars().take(SUMMARY_MAX_CHARS).collect();
        let cleaned = clean_summary(&limited);
        if cleaned.is_empty() {
            return Err(anyhow!("MiniCPM summary cleaned to empty string"));
        }
        let summary_chars = cleaned.chars().count();
        let mut confidence = (summary_chars as f32 / input_chars.max(1) as f32).clamp(0.35, 0.95);
        if truncated_input {
            confidence = (confidence * 0.85).clamp(0.3, 0.85);
        }
        if missing_marker {
            confidence = (confidence * 0.9).clamp(0.25, 0.9);
        }
        Ok(MiniCpmSummary {
            text: cleaned,
            confidence,
            model_version: self.version.clone(),
        })
    }
}

impl std::fmt::Debug for MiniCpmRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MiniCpmRunner")
            .field("version", &self.version)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ModelManifest {
    version: String,
    last_updated: DateTime<Utc>,
    #[serde(default)]
    artifacts: HashMap<String, String>,
}

impl ModelManifest {
    fn fresh() -> Self {
        Self {
            version: MODEL_VERSION.to_string(),
            last_updated: Utc::now(),
            artifacts: HashMap::new(),
        }
    }
}

struct ModelArtifact {
    filename: &'static str,
    url: &'static str,
}

const MODEL_ARTIFACTS: &[ModelArtifact] = &[
    ModelArtifact {
        filename: "model.gguf",
        url: "https://huggingface.co/openbmb/MiniCPM-Llama3-V2.5/resolve/main/gguf/MiniCPM-Llama3-V2.5-q4_k_m.gguf?download=1",
    },
    ModelArtifact {
        filename: "tokenizer.json",
        url: "https://huggingface.co/openbmb/MiniCPM-Llama3-V2.5/resolve/main/tokenizer.json?download=1",
    },
    ModelArtifact {
        filename: "config.json",
        url: "https://huggingface.co/openbmb/MiniCPM-Llama3-V2.5/resolve/main/config.json?download=1",
    },
];

fn fallback_summarise(text: &str, model_version: &str) -> MiniCpmSummary {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return MiniCpmSummary {
            text: String::new(),
            confidence: 0.0,
            model_version: model_version.to_string(),
        };
    }
    let wrapped = wrap(trimmed, SUMMARY_WRAP_WIDTH);
    let joined = wrapped
        .into_iter()
        .take(3)
        .map(std::borrow::Cow::into_owned)
        .collect::<Vec<String>>()
        .join(" ");
    let limited: String = joined.chars().take(SUMMARY_MAX_CHARS).collect();
    let original_len = trimmed.chars().count().max(1);
    let ratio = limited.chars().count() as f32 / original_len as f32;
    MiniCpmSummary {
        text: limited,
        confidence: ratio.clamp(0.25, 0.95),
        model_version: model_version.to_string(),
    }
}

async fn file_sha256(path: &Path) -> Result<String> {
    let path = path.to_path_buf();
    let handle: JoinHandle<Result<String>> = task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path)
            .with_context(|| format!("unable to open {}", path.display()))?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 8192];
        loop {
            let read = std::io::Read::read(&mut file, &mut buffer)
                .with_context(|| format!("unable to read {}", path.display()))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(hex_encode(&hasher.finalize()))
    });
    handle.await.context("hash task interrupted")?
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_missing_when_no_artifacts() {
        let dir = tempfile::tempdir().expect("tmp dir");
        let manager = MiniCpmManager::load(dir.path().to_path_buf())
            .await
            .expect("load");
        let status = manager.status().await.expect("status");
        match status {
            MiniCpmStatus::Missing { missing, .. } => {
                assert_eq!(missing.len(), MODEL_ARTIFACTS.len());
            }
            _ => panic!("expected missing"),
        }
    }

    #[tokio::test]
    async fn status_ready_when_checksums_match() {
        let dir = tempfile::tempdir().expect("tmp dir");
        let manager = MiniCpmManager::load(dir.path().to_path_buf())
            .await
            .expect("load");
        for artifact in MODEL_ARTIFACTS.iter() {
            let path = manager.model_dir.join(artifact.filename);
            tokio::fs::write(&path, b"placeholder")
                .await
                .expect("write placeholder");
            let checksum = file_sha256(&path).await.expect("checksum");
            {
                let mut manifest = manager.manifest.write().await;
                manifest
                    .artifacts
                    .insert(artifact.filename.to_string(), checksum);
                manager.persist_manifest(&manifest).await.expect("persist");
            }
        }
        let status = manager.status().await.expect("status");
        if let MiniCpmStatus::Missing { .. } = status {
            panic!("expected ready status after placeholder hashes recorded");
        }
    }
}
