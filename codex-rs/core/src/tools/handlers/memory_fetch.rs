use async_trait::async_trait;
use serde::Deserialize;
use uuid::Uuid;

use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;

pub struct MemoryFetchHandler;

#[derive(Deserialize, Default)]
struct MemoryFetchArgs {
    #[serde(default)]
    ids: Vec<String>,
    #[serde(default)]
    id: Option<String>,
}

#[async_trait]
impl ToolHandler for MemoryFetchHandler {
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
                    "memory_fetch only supports function-call payloads".to_string(),
                ));
            }
        };

        let args: MemoryFetchArgs = serde_json::from_str(&arguments).map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to parse memory_fetch arguments: {err}"
            ))
        })?;

        let mut requested_ids = args.ids;
        if let Some(single) = args.id {
            requested_ids.push(single);
        }

        if requested_ids.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "memory_fetch requires one or more memory IDs in the \"ids\" array".to_string(),
            ));
        }

        let runtime = session.memory_runtime().await.ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "memory runtime is not available in this session".to_string(),
            )
        })?;

        let uuids = parse_ids(&requested_ids)?;
        let records = runtime
            .fetch_records(&uuids)
            .await
            .map_err(|err| FunctionCallError::RespondToModel(format!("{err:#}")))?;

        let mut missing_ids = Vec::new();
        let mut fetched = Vec::new();
        for id in &uuids {
            if let Some(record) = records.iter().find(|rec| rec.record_id == *id) {
                fetched.push(record);
            } else {
                missing_ids.push(id.to_string());
            }
        }

        let mut output = String::new();
        for record in fetched {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&format!(
                "Memory [{}]\nSummary: {}\nConfidence: {:.0}%\nSource: {:?}",
                record.record_id,
                record.summary.trim(),
                record.confidence * 100.0,
                record.source
            ));
            if let Some(conversation) = record.metadata.conversation_id.as_deref() {
                output.push_str(&format!("\nConversation: {conversation}"));
            }
            if !record.metadata.tags.is_empty() {
                output.push_str(&format!("\nTags: {}", record.metadata.tags.join(", ")));
            }
            output.push('\n');
        }

        if !missing_ids.is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str("Missing memory IDs:\n");
            for id in missing_ids {
                output.push_str("- ");
                output.push_str(&id);
                output.push('\n');
            }
        }

        if output.is_empty() {
            output.push_str("No matching memory records were found.");
        }

        Ok(ToolOutput::Function {
            content: output,
            success: Some(true),
        })
    }
}

fn parse_ids(ids: &[String]) -> Result<Vec<Uuid>, FunctionCallError> {
    ids.iter()
        .map(|raw| {
            Uuid::parse_str(raw).map_err(|_| {
                FunctionCallError::RespondToModel(format!(
                    "memory_fetch received an invalid UUID: {raw}"
                ))
            })
        })
        .collect()
}
