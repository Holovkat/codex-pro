use async_trait::async_trait;
use serde::Deserialize;

use crate::codex::latest_user_message_text;
use crate::function_tool::FunctionCallError;
use crate::memory::MemoryPreviewModeExt;
use crate::memory::MemoryRetriever;
use crate::memory::MemoryRuntime;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;

pub struct MemorySuggestHandler;

#[derive(Debug, Default, Deserialize)]
struct MemorySuggestArgs {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    top_k: Option<u8>,
}

#[async_trait]
impl ToolHandler for MemorySuggestHandler {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError> {
        let ToolInvocation {
            session, payload, ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "memory_suggest only supports function-call payloads".to_string(),
                ));
            }
        };

        let args: MemorySuggestArgs = serde_json::from_str(&arguments).map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to parse memory_suggest arguments: {err}"
            ))
        })?;

        let top_k = args.top_k.unwrap_or(5).clamp(1, 10) as usize;

        let mut query = args.query.unwrap_or_default();
        if query.trim().is_empty() {
            let mut history = session.clone_history().await;
            let items = history.get_history();
            if let Some(latest) = latest_user_message_text(&items) {
                query = latest;
            }
        }
        let query = query.trim().to_string();
        if query.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "Provide a non-empty \"query\" or ask a clarifying question before calling memory_suggest."
                    .to_string(),
            ));
        }

        let runtime: MemoryRuntime = session.memory_runtime().ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "memory runtime is not available in this session".to_string(),
            )
        })?;

        let retriever = MemoryRetriever::new(runtime.clone());
        let retrieval = retriever
            .retrieve_for_text(&query, Some(top_k))
            .await
            .map_err(|err| FunctionCallError::RespondToModel(format!("{err:#}")))?;

        if !retrieval.has_candidates() {
            return Ok(ToolOutput::Function {
                content: format!(
                    "No stored memories matched \"{query}\" above the configured confidence threshold."
                ),
                success: Some(true),
            });
        }

        if retrieval.settings.preview_mode.requires_user_confirmation() {
            return Ok(ToolOutput::Function {
                content: "Memory preview mode requires user confirmation. Ask the user to review memories in the Memory Manager before continuing."
                    .to_string(),
                success: Some(false),
            });
        }

        let confidence_percent = (retrieval.settings.min_confidence * 100.0).round() as i32;
        let mut lines = Vec::new();
        lines.push(format!(
            "Memory suggestions for \"{query}\" (showing up to {top_k} above {confidence_percent}% confidence):"
        ));

        for hit in retrieval.candidates.iter().take(top_k) {
            let summary = hit.record.summary.trim();
            lines.push(format!(
                "- [{}] {} (confidence {:.0}% · score {:.2})",
                hit.record.record_id,
                summary,
                hit.record.confidence * 100.0,
                hit.score
            ));
        }

        lines.push(
            "Call memory_fetch with one or more IDs above before quoting or relying on those memories."
                .to_string(),
        );

        Ok(ToolOutput::Function {
            content: lines.join("\n"),
            success: Some(true),
        })
    }
}
