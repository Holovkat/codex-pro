use async_trait::async_trait;
use serde::Deserialize;

use crate::function_tool::FunctionCallError;
use crate::search;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;

pub struct SearchCodeHandler;

#[derive(Debug, Deserialize)]
struct SearchCodeArgs {
    query: String,
    #[serde(default)]
    top_k: Option<usize>,
    #[serde(default)]
    model: Option<String>,
}

#[async_trait]
impl ToolHandler for SearchCodeHandler {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError> {
        let ToolInvocation { turn, payload, .. } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "search_code only supports function-call payloads".to_string(),
                ));
            }
        };

        let args: SearchCodeArgs = serde_json::from_str(&arguments).map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to parse search_code arguments: {err}"
            ))
        })?;

        let query = args.query.trim();
        if query.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "search_code requires a non-empty \"query\"".to_string(),
            ));
        }

        let cwd = turn.cwd.clone();
        let top_k = args.top_k.unwrap_or(5).clamp(1, 20);
        let hits = search::search_index(&cwd, query, top_k, args.model.as_deref())
            .map_err(|err| FunctionCallError::RespondToModel(format!("{err:#}")))?;

        if hits.is_empty() {
            return Ok(ToolOutput::Function {
                content: format!("No indexed results matched \"{query}\"."),
                success: Some(true),
            });
        }

        let mut lines = Vec::new();
        lines.push(format!(
            "Search results for \"{query}\" (showing up to {top_k}):"
        ));
        for hit in hits {
            lines.push(format!(
                "{rank}. {path}:{start}-{end} · score {score:.2}",
                rank = hit.rank,
                path = hit.file_path,
                start = hit.start_line,
                end = hit.end_line,
                score = hit.score
            ));
            if !hit.snippet.trim().is_empty() {
                for snippet_line in hit.snippet.lines() {
                    lines.push(format!("    {snippet_line}"));
                }
            }
        }
        Ok(ToolOutput::Function {
            content: lines.join("\n"),
            success: Some(true),
        })
    }
}
