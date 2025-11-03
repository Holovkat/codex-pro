use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use clap::Parser;
use serde_json::json;
use uuid::Uuid;

use crate::commands::CommandContext;
use crate::commands::CommandResult;
use crate::settings::DEFAULT_SEARCH_CONFIDENCE_MIN;
use crate::settings::Settings;
use crate::settings::persist_search_confidence_min;
use codex_core::config::OPENAI_DEFAULT_MODEL;
use codex_otel::otel_event_manager::OtelEventManager;
use codex_protocol::ConversationId;

use super::analytics::load_analytics;
use super::analytics::load_chunk_records;
use super::analytics::load_manifest;
use super::builder::BuildOptions;
use super::builder::build_with_progress;
use super::chunk::DEFAULT_BATCH_SIZE;
use super::chunk::DEFAULT_LINES_PER_CHUNK;
use super::chunk::DEFAULT_OVERLAP;
use super::events::IndexEvent;
use super::paths::IndexPaths;
use super::query::QueryResponse;
use super::query::query_index;

const DEFAULT_QUERY_TOP_K: usize = 8;

fn build_cli_telemetry(settings: &Settings, binary_name: &str) -> OtelEventManager {
    let model = settings
        .model
        .as_ref()
        .and_then(|m| m.default.as_ref())
        .cloned()
        .unwrap_or_else(|| OPENAI_DEFAULT_MODEL.to_string());
    OtelEventManager::new(
        ConversationId::new(),
        &model,
        &model,
        None,
        None,
        false,
        format!("codex-cli:{binary_name}"),
    )
}

#[derive(Debug, Clone, Parser)]
#[command(name = "index.build", disable_help_flag = true)]
struct BuildCli {
    #[arg(long = "dir", value_name = "PATH")]
    dir: Option<PathBuf>,
    #[arg(long = "lines", default_value_t = DEFAULT_LINES_PER_CHUNK)]
    lines: usize,
    #[arg(long = "overlap", default_value_t = DEFAULT_OVERLAP)]
    overlap: usize,
    #[arg(long = "batch", default_value_t = DEFAULT_BATCH_SIZE)]
    batch: usize,
    #[arg(long = "model")]
    model: Option<String>,
    #[arg(long = "json", default_value_t = false)]
    json: bool,
}

#[derive(Debug, Clone, Parser)]
#[command(name = "index.query", disable_help_flag = true)]
struct QueryCli {
    #[arg(long = "dir", value_name = "PATH")]
    dir: Option<PathBuf>,
    #[arg(long = "top", default_value_t = DEFAULT_QUERY_TOP_K)]
    top: usize,
    #[arg(long = "model")]
    model: Option<String>,
    #[arg(long = "json", default_value_t = true)]
    json: bool,
    #[arg(value_name = "QUERY", required = true)]
    query: Vec<String>,
}

#[derive(Debug, Clone, Parser)]
#[command(name = "index.status", disable_help_flag = true)]
struct StatusCli {
    #[arg(long = "dir", value_name = "PATH")]
    dir: Option<PathBuf>,
    #[arg(long = "json", default_value_t = false)]
    json: bool,
}

#[derive(Debug, Clone, Parser)]
#[command(name = "index.verify", disable_help_flag = true)]
struct VerifyCli {
    #[arg(long = "dir", value_name = "PATH")]
    dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Parser)]
#[command(name = "index.clean", disable_help_flag = true)]
struct CleanCli {
    #[arg(long = "dir", value_name = "PATH")]
    dir: Option<PathBuf>,
    #[arg(long = "yes", default_value_t = false)]
    yes: bool,
}

#[derive(Debug, Clone, Parser)]
#[command(name = "index.ignore", disable_help_flag = true)]
struct IgnoreCli {
    #[arg(long = "dir", value_name = "PATH")]
    dir: Option<PathBuf>,
    #[arg(long = "print", default_value_t = false)]
    print: bool,
    #[arg(long = "add")]
    add: Vec<String>,
}

#[derive(Debug, Clone, Parser)]
#[command(name = "search-code", disable_help_flag = true)]
struct SearchCli {
    #[arg(long = "dir", value_name = "PATH")]
    dir: Option<PathBuf>,
    #[arg(long = "top", default_value_t = DEFAULT_QUERY_TOP_K)]
    top: usize,
    #[arg(long = "min-confidence")]
    min_confidence: Option<f32>,
    #[arg(value_name = "QUERY", required = true)]
    query: Vec<String>,
}

#[derive(Debug, Clone, Parser)]
#[command(name = "search.confidence", disable_help_flag = true)]
struct SearchConfidenceCli {
    #[arg(long = "set", value_name = "VALUE", conflicts_with = "reset")]
    set: Option<f32>,
    #[arg(long = "reset", default_value_t = false)]
    reset: bool,
    #[arg(long = "json", default_value_t = false)]
    json: bool,
}

pub fn build_command(ctx: &CommandContext, args: &[String]) -> Result<CommandResult> {
    let cli = parse::<BuildCli>("index.build", args)?;
    let mut options = build_options(ctx, &cli, &ctx.settings);
    options.batch_size = cli.batch.max(1);
    options.lines_per_chunk = cli.lines.max(1);
    options.overlap = cli.overlap.min(options.lines_per_chunk.saturating_sub(1));
    options.requested_model = cli.model;
    let mut events = Vec::new();
    let summary = build_with_progress(options, |event| {
        if matches!(
            event,
            IndexEvent::Progress { .. } | IndexEvent::Started { .. } | IndexEvent::Completed(_)
        ) {
            events.push(event);
        }
    })?;
    if cli.json {
        Ok(CommandResult::Json(json!({
            "summary": summary,
            "events": events,
        })))
    } else {
        Ok(CommandResult::Text(format!(
            "Indexed {} files into {} chunks using {} (dim {}) in {}ms",
            summary.total_files,
            summary.total_chunks,
            summary.embedding_model,
            summary.embedding_dim,
            summary.duration_ms,
        )))
    }
}

pub fn query_command(ctx: &CommandContext, args: &[String]) -> Result<CommandResult> {
    let cli = parse::<QueryCli>("index.query", args)?;
    let project_root = resolve_root(ctx, cli.dir);
    let query = cli.query.join(" ");
    let response = query_index(&project_root, &query, cli.top.max(1), cli.model.as_deref())?;
    if cli.json {
        Ok(CommandResult::Json(json!(response)))
    } else {
        Ok(CommandResult::Text(render_hits(&response)))
    }
}

pub fn status_command(ctx: &CommandContext, args: &[String]) -> Result<CommandResult> {
    let cli = parse::<StatusCli>("index.status", args)?;
    let project_root = resolve_root(ctx, cli.dir);
    let paths = IndexPaths::from_root(project_root);
    if !paths.manifest_path.exists() {
        return Ok(CommandResult::Text(
            "Index has not been built yet".to_string(),
        ));
    }
    let manifest = load_manifest(&paths.manifest_path)?;
    let analytics = load_analytics(&paths.analytics_path).unwrap_or_default();
    if cli.json {
        Ok(CommandResult::Json(json!({
            "manifest": manifest,
            "analytics": analytics,
        })))
    } else {
        let last_success = analytics
            .last_success_ts
            .map(|ts| ts.to_rfc3339())
            .unwrap_or_else(|| "never".to_string());
        Ok(CommandResult::Text(format!(
            "Index model {} dim {} • files {} • chunks {} • last success {}",
            manifest.embedding_model,
            manifest.embedding_dim,
            manifest.total_files,
            manifest.total_chunks,
            last_success
        )))
    }
}

pub fn verify_command(ctx: &CommandContext, args: &[String]) -> Result<CommandResult> {
    let cli = parse::<VerifyCli>("index.verify", args)?;
    let project_root = resolve_root(ctx, cli.dir);
    let paths = IndexPaths::from_root(project_root);
    let manifest = load_manifest(&paths.manifest_path)?;
    let chunks = load_chunk_records(&paths.meta_path)?;
    let graph_path = paths
        .index_dir
        .join(format!("{}.hnsw.graph", super::paths::VECTORS_BASENAME));
    let data_path = paths
        .index_dir
        .join(format!("{}.hnsw.data", super::paths::VECTORS_BASENAME));
    let ok = manifest.total_chunks == chunks.len() && graph_path.exists() && data_path.exists();
    Ok(CommandResult::Json(json!({
        "ok": ok,
        "manifest_chunks": manifest.total_chunks,
        "meta_chunks": chunks.len(),
        "graph_present": graph_path.exists(),
        "data_present": data_path.exists(),
    })))
}

pub fn clean_command(ctx: &CommandContext, args: &[String]) -> Result<CommandResult> {
    let cli = parse::<CleanCli>("index.clean", args)?;
    let project_root = resolve_root(ctx, cli.dir);
    let paths = IndexPaths::from_root(project_root);
    if !paths.index_dir.exists() {
        return Ok(CommandResult::Text("Index cache already clean".to_string()));
    }
    if !cli.yes {
        return Ok(CommandResult::Text(
            "Refusing to remove index without --yes".to_string(),
        ));
    }
    fs::remove_dir_all(&paths.index_dir)
        .with_context(|| format!("failed to remove {}", paths.index_dir.display()))?;
    Ok(CommandResult::Text("Removed index cache".to_string()))
}

pub fn ignore_command(ctx: &CommandContext, args: &[String]) -> Result<CommandResult> {
    let cli = parse::<IgnoreCli>("index.ignore", args)?;
    let project_root = resolve_root(ctx, cli.dir);
    let ignore_path = project_root.join(".index-ignore");
    if cli.print {
        if !ignore_path.exists() {
            return Ok(CommandResult::Text(".index-ignore is empty".to_string()));
        }
        let contents = std::fs::read_to_string(&ignore_path).unwrap_or_default();
        return Ok(CommandResult::Text(contents));
    }
    if !cli.add.is_empty() {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&ignore_path)?;
        for pattern in cli.add {
            writeln!(file, "{pattern}")?;
        }
    }
    Ok(CommandResult::Text(format!(
        "Ignore file located at {}",
        ignore_path.display()
    )))
}

pub fn search_command(ctx: &CommandContext, args: &[String]) -> Result<CommandResult> {
    let cli = parse::<SearchCli>("search-code", args)?;
    let project_root = resolve_root(ctx, cli.dir);
    let query = cli.query.join(" ");
    let telemetry = build_cli_telemetry(&ctx.settings, &ctx.binary_name);
    let call_id = format!("cli-search-code-{}", Uuid::now_v7());
    let mut args_json = json!({
        "query": query,
        "top": cli.top,
        "min_confidence_arg": cli.min_confidence,
    });
    let started = Instant::now();
    let response = match query_index(&project_root, &query, cli.top.max(1), None) {
        Ok(resp) => resp,
        Err(err) => {
            telemetry.tool_result(
                "search_code_cli",
                &call_id,
                &args_json.to_string(),
                started.elapsed(),
                false,
                &format!("{err:#}"),
            );
            return Err(err);
        }
    };
    let confidence = cli
        .min_confidence
        .and_then(normalize_confidence)
        .unwrap_or_else(|| ctx.settings.search_confidence_min());
    args_json["resolved_min_confidence"] = json!(confidence);
    let response = response.with_confidence_min(confidence);
    telemetry.tool_result(
        "search_code_cli",
        &call_id,
        &args_json.to_string(),
        started.elapsed(),
        true,
        &format!("{} hits", response.hits.len()),
    );
    Ok(CommandResult::Json(json!(response)))
}

pub fn search_confidence_command(ctx: &CommandContext, args: &[String]) -> Result<CommandResult> {
    let cli = parse::<SearchConfidenceCli>("search.confidence", args)?;
    let updated = if cli.reset {
        Some(persist_search_confidence_min(None)?)
    } else if let Some(value) = cli.set {
        let normalized = normalize_confidence(value)
            .ok_or_else(|| anyhow!("min confidence must be a finite number"))?;
        Some(persist_search_confidence_min(Some(normalized))?)
    } else {
        None
    };
    let settings = updated.as_ref().unwrap_or(&ctx.settings);
    let confidence = settings.search_confidence_min();
    let percent = (confidence * 100.0).round();
    if cli.json {
        return Ok(CommandResult::Json(json!({
            "status": if updated.is_some() { "updated" } else { "current" },
            "confidence_min": confidence,
            "confidence_percent": percent,
            "raw_args": args,
        })));
    }
    let status = if updated.is_some() {
        "Updated minimum confidence"
    } else {
        "Current minimum confidence"
    };
    Ok(CommandResult::Text(format!(
        "{status} set to {percent:.0}% (score ≥ {confidence:.3})."
    )))
}

pub fn apply_command(_ctx: &CommandContext, args: &[String]) -> Result<CommandResult> {
    Ok(CommandResult::Json(json!({
        "status": "unimplemented",
        "command": "apply",
        "args": args,
    })))
}

fn parse<T>(name: &str, args: &[String]) -> Result<T>
where
    T: Parser,
{
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(name.to_string());
    argv.extend(args.iter().cloned());
    T::try_parse_from(argv).map_err(|err| anyhow!(err.to_string()))
}

fn build_options(ctx: &CommandContext, cli: &BuildCli, settings: &Settings) -> BuildOptions {
    let root = resolve_root(ctx, cli.dir.clone());
    let mut options = BuildOptions {
        project_root: root,
        batch_size: DEFAULT_BATCH_SIZE,
        lines_per_chunk: DEFAULT_LINES_PER_CHUNK,
        overlap: DEFAULT_OVERLAP,
        requested_model: None,
    };
    if let Some(index) = settings.index.as_ref() {
        if let Some(lines) = index.context_tokens {
            options.lines_per_chunk = lines as usize;
        }
        if let Some(refresh) = index.retrieval_threshold {
            let overlap = refresh.max(0.0) as usize;
            if overlap > 0 {
                options.overlap = overlap.min(options.lines_per_chunk.saturating_sub(1));
            }
        }
    }
    options
}

fn resolve_root(ctx: &CommandContext, override_dir: Option<PathBuf>) -> PathBuf {
    if let Some(dir) = override_dir {
        dir
    } else if let Some(dir) = ctx.working_dir.as_ref() {
        PathBuf::from(dir)
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }
}

fn render_hits(response: &QueryResponse) -> String {
    let confidence = response
        .confidence_min
        .unwrap_or(DEFAULT_SEARCH_CONFIDENCE_MIN);
    if response.hits.is_empty() {
        return format!(
            "No results for '{}' (confidence ≥ {:.0}%).",
            response.query,
            confidence * 100.0
        );
    }
    let mut lines = vec![format!(
        "Results for '{}' (confidence ≥ {:.0}%):",
        response.query,
        confidence * 100.0
    )];
    for hit in &response.hits {
        let snippet = hit.snippet.replace('\n', " ");
        lines.push(format!(
            "#{rank} score={score:.3} {path}:{start}-{end}\n{snippet}",
            rank = hit.rank,
            score = hit.score,
            path = hit.file_path,
            start = hit.start_line,
            end = hit.end_line,
            snippet = snippet,
        ));
    }
    lines.join("\n")
}

fn normalize_confidence(raw: f32) -> Option<f32> {
    if raw.is_nan() {
        return None;
    }
    let normalized = if raw > 1.0 { raw / 100.0 } else { raw };
    Some(normalized.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_percentage_values() {
        assert!((normalize_confidence(75.0).unwrap() - 0.75).abs() < f32::EPSILON);
    }

    #[test]
    fn normalizes_ratio_values() {
        assert!((normalize_confidence(0.65).unwrap() - 0.65).abs() < f32::EPSILON);
    }

    #[test]
    fn clamps_out_of_range_values() {
        assert!((normalize_confidence(150.0).unwrap() - 1.0).abs() < f32::EPSILON);
        assert!((normalize_confidence(-25.0).unwrap() - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn drops_nan_values() {
        assert!(normalize_confidence(f32::NAN).is_none());
    }
}
