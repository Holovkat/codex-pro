use std::io::Read;
use std::io::Write;
use std::io::{self};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::anyhow;
use chrono::Local;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use codex_common::CliConfigOverrides;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_core::memory::GlobalMemoryStore;
use codex_core::memory::MemoryHit;
use codex_core::memory::MemoryMetadata;
use codex_core::memory::MemoryRecord;
use codex_core::memory::MemoryRecordUpdate;
use codex_core::memory::MemoryRuntime;
use codex_core::memory::MemorySettingsManager;
use codex_core::memory::MemorySource;
use codex_core::memory::MemoryStats;
use codex_core::memory::MiniCpmArtifactStatus;
use codex_core::memory::MiniCpmDownloadState;
use codex_core::memory::MiniCpmManager;
use codex_core::memory::MiniCpmStatus;
use codex_core::memory::clean_summary;
use serde::Serialize;
use uuid::Uuid;

#[derive(Debug, Parser)]
pub struct MemoryCli {
    #[command(subcommand)]
    action: MemoryAction,
}

#[derive(Debug, Subcommand)]
enum MemoryAction {
    /// Initialise the global memory store and default settings.
    Init,
    /// Rebuild the memory vector index.
    Rebuild,
    /// Reset the global memory store (destructive).
    Reset {
        /// Skip the interactive confirmation prompt.
        #[arg(long = "yes", short = 'y')]
        force: bool,
    },
    /// Show memory statistics (hits, misses, disk usage).
    Stats,
    /// List stored memories.
    List(ListArgs),
    /// Create a manual memory record.
    Create(CreateArgs),
    /// Edit an existing memory record.
    Edit(EditArgs),
    /// Delete a memory record.
    Delete(DeleteArgs),
    /// Search the memory store semantically.
    Search(SearchArgs),
}

#[derive(Debug, Parser)]
struct ListArgs {
    /// Output as JSON for scripting.
    #[arg(long)]
    json: bool,
    /// Maximum number of records to display.
    #[arg(long)]
    limit: Option<usize>,
    /// Include entries below the configured minimum confidence threshold.
    #[arg(long)]
    all: bool,
    /// Override the confidence filter (percentage or 0-1).
    #[arg(long = "min-confidence")]
    min_confidence: Option<f32>,
}

#[derive(Debug, Parser)]
struct CreateArgs {
    /// Summary text for the memory record.
    #[arg(long)]
    summary: Option<String>,
    /// Read the summary from STDIN.
    #[arg(long)]
    stdin: bool,
    /// Confidence score (percent or 0-1 range).
    #[arg(long = "confidence")]
    confidence: Option<f32>,
    /// Tags to associate; repeat for multiple.
    #[arg(long = "tag", value_name = "TAG")]
    tags: Vec<String>,
    /// Source attribution for the memory.
    #[arg(long = "source", default_value_t = MemorySourceArg::User)]
    source: MemorySourceArg,
    /// Output the created record as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Parser)]
struct EditArgs {
    /// Memory record identifier (UUID).
    id: String,
    /// Replace the summary text.
    #[arg(long)]
    summary: Option<String>,
    /// Read the new summary from STDIN.
    #[arg(long)]
    stdin: bool,
    /// Update the confidence score.
    #[arg(long = "confidence")]
    confidence: Option<f32>,
    /// Replace tags with the provided list.
    #[arg(long = "tag", value_name = "TAG")]
    tags: Vec<String>,
    /// Remove all tags.
    #[arg(long = "clear-tags")]
    clear_tags: bool,
    /// Override the memory source.
    #[arg(long = "source")]
    source: Option<MemorySourceArg>,
    /// Output the updated record as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Parser)]
struct DeleteArgs {
    /// Memory record identifier (UUID).
    id: String,
    /// Skip the interactive confirmation prompt.
    #[arg(long = "yes", short = 'y')]
    force: bool,
}

#[derive(Debug, Parser)]
struct SearchArgs {
    /// Query text to embed and search for.
    query: String,
    /// Maximum number of matches.
    #[arg(long)]
    limit: Option<usize>,
    /// Override the confidence filter (percent or 0-1).
    #[arg(long = "min-confidence")]
    min_confidence: Option<f32>,
    /// Output results as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize, Default)]
enum MemorySourceArg {
    #[default]
    User,
    Assistant,
    Tool,
    FileDiff,
    System,
}

impl From<MemorySourceArg> for MemorySource {
    fn from(value: MemorySourceArg) -> Self {
        match value {
            MemorySourceArg::User => MemorySource::UserMessage,
            MemorySourceArg::Assistant => MemorySource::AssistantMessage,
            MemorySourceArg::Tool => MemorySource::ToolOutput,
            MemorySourceArg::FileDiff => MemorySource::FileDiff,
            MemorySourceArg::System => MemorySource::SystemMessage,
        }
    }
}

impl std::fmt::Display for MemorySourceArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            MemorySourceArg::User => "user",
            MemorySourceArg::Assistant => "assistant",
            MemorySourceArg::Tool => "tool",
            MemorySourceArg::FileDiff => "file-diff",
            MemorySourceArg::System => "system",
        };
        f.write_str(value)
    }
}

pub async fn run(memory_cli: MemoryCli, root_overrides: CliConfigOverrides) -> anyhow::Result<()> {
    let overrides = root_overrides
        .parse_overrides()
        .map_err(|err| anyhow!("invalid config override: {err}"))?;
    let config = Config::load_with_cli_overrides(overrides, ConfigOverrides::default()).await?;
    let memory_root = config.codex_home.join("memory");

    match memory_cli.action {
        MemoryAction::Init => init(memory_root.clone()).await?,
        MemoryAction::Rebuild => rebuild(memory_root.clone()).await?,
        MemoryAction::Reset { force } => reset(memory_root.clone(), force).await?,
        MemoryAction::Stats => stats(memory_root.clone()).await?,
        MemoryAction::List(args) => list(memory_root.clone(), args).await?,
        MemoryAction::Create(args) => create(memory_root.clone(), args).await?,
        MemoryAction::Edit(args) => edit(memory_root.clone(), args).await?,
        MemoryAction::Delete(args) => delete(memory_root.clone(), args).await?,
        MemoryAction::Search(args) => search(memory_root.clone(), args).await?,
    }

    Ok(())
}

async fn init(memory_root: PathBuf) -> anyhow::Result<()> {
    let store = GlobalMemoryStore::open(memory_root.clone())
        .await
        .context("failed to initialise memory store")?;
    drop(store);
    MemorySettingsManager::load(memory_root)
        .await
        .context("failed to initialise memory settings")?;
    println!("Memory store initialised at ~/.codex/memory");
    Ok(())
}

async fn rebuild(memory_root: PathBuf) -> anyhow::Result<()> {
    let mut store = GlobalMemoryStore::open(memory_root)
        .await
        .context("failed to open memory store")?;
    store.rebuild().context("failed to rebuild memory index")?;
    println!("Memory index rebuilt");
    Ok(())
}

async fn reset(memory_root: PathBuf, force: bool) -> anyhow::Result<()> {
    if !force && !confirm_destructive_action()? {
        println!("Aborted");
        return Ok(());
    }
    let mut store = GlobalMemoryStore::open(memory_root)
        .await
        .context("failed to open memory store")?;
    store.reset().context("failed to reset memory store")?;
    println!("Memory store reset");
    Ok(())
}

async fn stats(memory_root: PathBuf) -> anyhow::Result<()> {
    let runtime = load_runtime(memory_root).await?;
    let stats = {
        let store = runtime.store.lock().await;
        store.stats().context("failed to read memory statistics")?
    };
    print_stats(stats);
    if let Some(manager) = runtime.model_manager() {
        print_model_status(manager).await?;
    }
    Ok(())
}

async fn list(memory_root: PathBuf, args: ListArgs) -> anyhow::Result<()> {
    let runtime = load_runtime(memory_root).await?;
    let settings = runtime.settings.get().await;
    let mut records = runtime
        .list_records()
        .await
        .context("failed to list memory records")?;
    records.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    let threshold = args
        .min_confidence
        .map(normalise_confidence)
        .unwrap_or(settings.min_confidence);
    if !args.all {
        records.retain(|record| record.confidence >= threshold);
    }
    if let Some(limit) = args.limit {
        records.truncate(limit);
    }
    if args.json {
        print_records_json(&records)?;
    } else {
        print_records_table(&records, Some(threshold));
    }
    Ok(())
}

async fn create(memory_root: PathBuf, args: CreateArgs) -> anyhow::Result<()> {
    let runtime = load_runtime(memory_root).await?;
    let settings = runtime.settings.get().await;
    let summary = read_summary(args.summary, args.stdin, "Enter memory summary:")?;
    if summary.trim().is_empty() {
        return Err(anyhow!("summary cannot be empty"));
    }
    let metadata = MemoryMetadata {
        tags: args.tags.clone(),
        ..Default::default()
    };
    let confidence = args
        .confidence
        .map(normalise_confidence)
        .unwrap_or(settings.min_confidence);
    let record = runtime
        .create_record(
            summary.trim().to_string(),
            metadata,
            confidence,
            args.source.into(),
        )
        .await
        .context("failed to create memory record")?;
    if args.json {
        print_single_record_json(&record)?;
    } else {
        println!("Created memory {}", record.record_id);
        print_records_table(std::slice::from_ref(&record), None);
    }
    Ok(())
}

async fn edit(memory_root: PathBuf, args: EditArgs) -> anyhow::Result<()> {
    let runtime = load_runtime(memory_root).await?;
    let id = parse_uuid(&args.id)?;
    let summary = if args.summary.is_some() || args.stdin {
        Some(read_summary(
            args.summary,
            args.stdin,
            "Enter updated summary:",
        )?)
    } else {
        None
    };
    let mut existing = runtime
        .fetch_records(&[id])
        .await
        .context("failed to fetch memory record")?;
    let current = existing
        .pop()
        .ok_or_else(|| anyhow!("memory {id} not found"))?;
    let mut update = MemoryRecordUpdate::default();
    if let Some(summary) = summary {
        if summary.trim().is_empty() {
            return Err(anyhow!("summary cannot be empty"));
        }
        update.summary = Some(summary.trim().to_string());
    }
    if let Some(confidence) = args.confidence {
        update.confidence = Some(normalise_confidence(confidence));
    }
    if args.clear_tags {
        let mut metadata = current.metadata.clone();
        metadata.tags.clear();
        update.metadata = Some(metadata);
    } else if !args.tags.is_empty() {
        let mut metadata = current.metadata.clone();
        metadata.tags = args.tags.clone();
        update.metadata = Some(metadata);
    }
    if let Some(source) = args.source {
        update.source = Some(source.into());
    }
    let record = runtime
        .update_record(id, update)
        .await
        .context("failed to update memory record")?;
    if args.json {
        print_single_record_json(&record)?;
    } else {
        println!("Updated memory {}", record.record_id);
        print_records_table(std::slice::from_ref(&record), None);
    }
    Ok(())
}

async fn delete(memory_root: PathBuf, args: DeleteArgs) -> anyhow::Result<()> {
    let runtime = load_runtime(memory_root).await?;
    let id = parse_uuid(&args.id)?;
    if !args.force && !confirm_single_delete(id)? {
        println!("Aborted");
        return Ok(());
    }
    let deleted = runtime
        .delete_record(id)
        .await
        .context("failed to delete memory record")?;
    if deleted.is_some() {
        println!("Deleted memory {id}");
    } else {
        println!("Memory {id} not found");
    }
    Ok(())
}

async fn search(memory_root: PathBuf, args: SearchArgs) -> anyhow::Result<()> {
    let runtime = load_runtime(memory_root).await?;
    let settings = runtime.settings.get().await;
    let threshold = args
        .min_confidence
        .map(normalise_confidence)
        .unwrap_or(settings.min_confidence);
    let limit = args.limit.unwrap_or(10);
    let hits = runtime
        .search_records(&args.query, limit, Some(threshold))
        .await
        .context("memory search failed")?;
    if args.json {
        print_hits_json(&hits)?;
    } else {
        print_hits_table(&hits, threshold);
    }
    Ok(())
}

fn print_stats(stats: MemoryStats) {
    println!("Total records : {}", stats.total_records);
    println!("Hits          : {}", stats.hits);
    println!("Misses        : {}", stats.misses);
    println!("Preview accept: {}", stats.preview_accepted);
    println!("Preview skip  : {}", stats.preview_skipped);
    let mb = stats.disk_usage_bytes as f64 / (1024.0 * 1024.0);
    println!("Disk usage    : {mb:.2} MiB");
    if let Some(ts) = stats.last_rebuild_at {
        println!("Last rebuild  : {ts}");
    } else {
        println!("Last rebuild  : never");
    }
}

async fn print_model_status(manager: Arc<MiniCpmManager>) -> anyhow::Result<()> {
    let status = match manager.status().await {
        Ok(status) => status,
        Err(err) => {
            println!();
            println!("MiniCPM cache   : {}", manager.model_dir().display());
            println!("MiniCPM status  : error ({err:#})");
            return Ok(());
        }
    };

    println!();
    println!("MiniCPM cache   : {}", manager.model_dir().display());
    match &status {
        MiniCpmStatus::Ready {
            version,
            last_updated,
        } => {
            println!(
                "MiniCPM status  : ready ({}; updated {})",
                version,
                last_updated
                    .with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M:%S")
            );
        }
        MiniCpmStatus::Missing {
            version, missing, ..
        } => {
            println!("MiniCPM status  : cache incomplete (version {version})");
            if !missing.is_empty() {
                println!("MiniCPM missing : {}", missing.join(", "));
            }
        }
    }

    let download_state = manager.download_state().await;
    if let Some(progress) = format_download_state(&download_state) {
        println!("MiniCPM download: {progress}");
    }

    let diagnostics = manager.diagnostics().await;
    if let Some(failure) = diagnostics.last_failure {
        let when = failure
            .occurred_at
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S");
        println!(
            "MiniCPM last err: {} ({when})",
            truncate_cli(&failure.message, 80)
        );
    }

    Ok(())
}

fn format_download_state(state: &MiniCpmDownloadState) -> Option<String> {
    let mut artifacts: Vec<_> = state.artifacts.iter().collect();
    artifacts.sort_by(|a, b| a.0.cmp(b.0));
    for (name, artifact) in artifacts {
        match artifact.status {
            MiniCpmArtifactStatus::Downloading => {
                if let Some(total) = artifact.total_bytes
                    && total > 0
                {
                    let percent = (artifact.downloaded_bytes as f64 * 100.0) / total as f64;
                    return Some(format!(
                        "downloading {name} ({:.0}% – {}/{total} bytes)",
                        percent, artifact.downloaded_bytes
                    ));
                }
                return Some(format!(
                    "downloading {name} ({} bytes)",
                    artifact.downloaded_bytes
                ));
            }
            MiniCpmArtifactStatus::Verifying => {
                return Some(format!("verifying {name}"));
            }
            MiniCpmArtifactStatus::Failed => {
                let msg = artifact
                    .error
                    .as_deref()
                    .map(|text| truncate_cli(text, 60))
                    .unwrap_or_else(|| "unknown error".to_string());
                return Some(format!("{name} failed: {msg}"));
            }
            _ => {}
        }
    }
    None
}

fn truncate_cli(message: &str, max: usize) -> String {
    if message.chars().count() > max {
        let mut truncated: String = message.chars().take(max.saturating_sub(1)).collect();
        truncated.push('…');
        truncated
    } else {
        message.to_string()
    }
}

fn confirm_destructive_action() -> anyhow::Result<bool> {
    print!("This will delete all stored memories. Type 'yes' to continue: ");
    std::io::stdout().flush().ok();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim().eq_ignore_ascii_case("yes"))
}

fn confirm_single_delete(id: Uuid) -> anyhow::Result<bool> {
    print!("Delete memory {id}? Type 'yes' to confirm: ");
    io::stdout().flush().ok();
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().eq_ignore_ascii_case("yes"))
}

async fn load_runtime(memory_root: PathBuf) -> anyhow::Result<MemoryRuntime> {
    MemoryRuntime::load(memory_root)
        .await
        .context("failed to load memory runtime")
}

fn normalise_confidence(value: f32) -> f32 {
    if value > 1.0 {
        (value / 100.0).clamp(0.0, 1.0)
    } else {
        value.clamp(0.0, 1.0)
    }
}

fn read_summary(summary: Option<String>, stdin: bool, prompt: &str) -> anyhow::Result<String> {
    if let Some(summary) = summary {
        return Ok(summary);
    }
    if stdin {
        eprintln!("{prompt}");
        let mut buffer = String::new();
        io::stdin().read_to_string(&mut buffer)?;
        return Ok(buffer);
    }
    Err(anyhow!(
        "summary is required; pass --summary or use --stdin"
    ))
}

fn parse_uuid(value: &str) -> anyhow::Result<Uuid> {
    Uuid::parse_str(value).map_err(|err| anyhow!("invalid UUID '{value}': {err}"))
}

fn print_records_table(records: &[MemoryRecord], threshold: Option<f32>) {
    if let Some(threshold) = threshold {
        println!(
            "Showing {} record(s) with confidence ≥ {:.0}%",
            records.len(),
            threshold * 100.0
        );
    } else {
        println!("Showing {} record(s)", records.len());
    }
    println!(
        "{:<36} {:>6} {:<17} {:<18} Summary",
        "Record ID", "CF%", "Updated", "Tags"
    );
    for record in records {
        let updated = record
            .updated_at
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M")
            .to_string();
        let tags = if record.metadata.tags.is_empty() {
            "-".to_string()
        } else {
            record.metadata.tags.join(", ")
        };
        let summary = truncate_summary(&clean_summary(&record.summary), 60);
        println!(
            "{:<36} {:>6.0} {:<17} {:<18} {}",
            record.record_id,
            record.confidence * 100.0,
            updated,
            truncate_summary(&tags, 18),
            summary
        );
    }
}

fn print_hits_table(hits: &[MemoryHit], threshold: f32) {
    println!(
        "{} match(es) (confidence ≥ {:.0}%):",
        hits.len(),
        threshold * 100.0
    );
    println!(
        "{:<4} {:<36} {:>6} {:>6} Summary",
        "#", "Record ID", "Score", "CF%"
    );
    for (idx, hit) in hits.iter().enumerate() {
        let summary = truncate_summary(&clean_summary(&hit.record.summary), 60);
        println!(
            "{:<4} {:<36} {:>6.0} {:>6.0} {}",
            idx + 1,
            hit.record.record_id,
            hit.score * 100.0,
            hit.record.confidence * 100.0,
            summary
        );
    }
}

fn truncate_summary(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let mut result: String = trimmed.chars().take(max.saturating_sub(1)).collect();
    result.push('…');
    result
}

fn print_records_json(records: &[MemoryRecord]) -> anyhow::Result<()> {
    let payload: Vec<_> = records.iter().map(SerializableRecord::from).collect();
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

fn print_single_record_json(record: &MemoryRecord) -> anyhow::Result<()> {
    print_records_json(std::slice::from_ref(record))
}

fn print_hits_json(hits: &[MemoryHit]) -> anyhow::Result<()> {
    let payload: Vec<_> = hits.iter().map(SerializableHit::from).collect();
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

#[derive(Serialize)]
struct SerializableRecord {
    id: String,
    summary: String,
    confidence: f32,
    tags: Vec<String>,
    source: MemorySource,
    created_at: String,
    updated_at: String,
}

impl From<&MemoryRecord> for SerializableRecord {
    fn from(record: &MemoryRecord) -> Self {
        Self {
            id: record.record_id.to_string(),
            summary: record.summary.clone(),
            confidence: record.confidence,
            tags: record.metadata.tags.clone(),
            source: record.source.clone(),
            created_at: record.created_at.to_rfc3339(),
            updated_at: record.updated_at.to_rfc3339(),
        }
    }
}

#[derive(Serialize)]
struct SerializableHit {
    score: f32,
    record: SerializableRecord,
}

impl From<&MemoryHit> for SerializableHit {
    fn from(hit: &MemoryHit) -> Self {
        Self {
            score: hit.score,
            record: SerializableRecord::from(&hit.record),
        }
    }
}
