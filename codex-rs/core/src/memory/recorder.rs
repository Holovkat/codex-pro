use codex_protocol::ConversationId;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::LocalShellAction;
use codex_protocol::models::LocalShellStatus;
use codex_protocol::models::ResponseItem;
use tokio::sync::mpsc::UnboundedSender;

use super::types::MemoryEvent;
use super::types::MemoryMetadata;
use super::types::MemorySource;
use crate::codex::compact::content_items_to_text;

#[derive(Clone)]
pub struct MemoryRecorderConfig {
    pub conversation_id: ConversationId,
    pub session_source: Option<String>,
    pub sink: Option<UnboundedSender<MemoryEvent>>,
}

impl MemoryRecorderConfig {
    pub fn disabled(conversation_id: ConversationId) -> Self {
        Self {
            conversation_id,
            session_source: None,
            sink: None,
        }
    }
}

#[derive(Clone)]
pub struct MemoryRecorder {
    conversation_id: ConversationId,
    session_source: Option<String>,
    sink: Option<UnboundedSender<MemoryEvent>>,
}

impl MemoryRecorder {
    pub fn new(config: MemoryRecorderConfig) -> Self {
        Self {
            conversation_id: config.conversation_id,
            session_source: config.session_source,
            sink: config.sink,
        }
    }

    pub fn disabled(conversation_id: ConversationId) -> Self {
        Self::new(MemoryRecorderConfig::disabled(conversation_id))
    }

    pub fn record_response_items(&self, items: &[ResponseItem]) {
        for item in items {
            if let Some(event) = self.event_for_response_item(item) {
                self.publish(event);
            }
        }
    }

    pub fn record_file_diff(&self, call_id: &str, unified_diff: &str) {
        if unified_diff.trim().is_empty() {
            return;
        }
        let mut metadata = MemoryMetadata::with_conversation(self.conversation_id.to_string());
        metadata.call_id = Some(call_id.to_string());
        metadata.tags.push("file_diff".to_string());
        let event = MemoryEvent::new(MemorySource::FileDiff, unified_diff.to_string(), metadata);
        self.publish(event);
    }

    fn event_for_response_item(&self, item: &ResponseItem) -> Option<MemoryEvent> {
        match item {
            ResponseItem::Message { role, content, .. } => {
                let text = content_items_to_text(content)?;
                let source = classify_role(role.as_str());
                let mut metadata =
                    MemoryMetadata::with_conversation(self.conversation_id.to_string());
                metadata.role = Some(role.clone());
                Some(MemoryEvent::new(source, text, metadata))
            }
            ResponseItem::FunctionCallOutput { call_id, output } => {
                let text = render_function_output(output);
                let mut metadata =
                    MemoryMetadata::with_conversation(self.conversation_id.to_string());
                metadata.call_id = Some(call_id.clone());
                metadata.tags.push("tool".to_string());
                Some(MemoryEvent::new(MemorySource::ToolOutput, text, metadata))
            }
            ResponseItem::CustomToolCallOutput { call_id, output } => {
                let mut metadata =
                    MemoryMetadata::with_conversation(self.conversation_id.to_string());
                metadata.call_id = Some(call_id.clone());
                metadata.tags.push("tool".to_string());
                Some(MemoryEvent::new(
                    MemorySource::ToolOutput,
                    output.clone(),
                    metadata,
                ))
            }
            ResponseItem::LocalShellCall { action, status, .. } => {
                shell_action_text(action, status).map(|(text, mut metadata)| {
                    metadata.conversation_id = Some(self.conversation_id.to_string());
                    MemoryEvent::new(MemorySource::ToolOutput, text, metadata)
                })
            }
            ResponseItem::WebSearchCall { action, .. } => {
                let mut metadata =
                    MemoryMetadata::with_conversation(self.conversation_id.to_string());
                metadata.tool_name = Some("web_search".to_string());
                let text = format!("Triggered web search: {action:?}");
                Some(MemoryEvent::new(MemorySource::ToolOutput, text, metadata))
            }
            _ => None,
        }
    }

    fn publish(&self, mut event: MemoryEvent) {
        if event.metadata.conversation_id.is_none() {
            event.metadata.conversation_id = Some(self.conversation_id.to_string());
        }
        if event.metadata.session_source.is_none() {
            event.metadata.session_source = self.session_source.clone();
        }
        if let Some(sender) = &self.sink {
            let _ = sender.send(event);
        }
    }
}

fn classify_role(role: &str) -> MemorySource {
    match role {
        "user" => MemorySource::UserMessage,
        "assistant" => MemorySource::AssistantMessage,
        _ => MemorySource::SystemMessage,
    }
}

fn render_function_output(output: &FunctionCallOutputPayload) -> String {
    match &output.success {
        Some(success) => {
            if *success {
                output.content.clone()
            } else {
                format!("tool call failed: {}", output.content)
            }
        }
        None => output.content.clone(),
    }
}

fn shell_action_text(
    action: &LocalShellAction,
    status: &LocalShellStatus,
) -> Option<(String, MemoryMetadata)> {
    match action {
        LocalShellAction::Exec(exec) => {
            let metadata = MemoryMetadata {
                tool_name: Some("shell".to_string()),
                ..MemoryMetadata::default()
            };
            let command = shell_command_preview(&exec.command);
            let mut text = format!("Shell command `{command}` reported status {status:?}");
            if let Some(code) = &exec.user {
                text.push_str(&format!(" (user: {code})"));
            }
            Some((text, metadata))
        }
    }
}

fn shell_command_preview(command: &[String]) -> String {
    if command.is_empty() {
        String::new()
    } else {
        Preview(command).to_string()
    }
}

struct Preview<'a>(&'a [String]);

impl<'a> std::fmt::Display for Preview<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (idx, part) in self.0.iter().enumerate() {
            if idx > 0 {
                write!(f, " ")?;
            }
            if part.contains(char::is_whitespace) {
                write!(f, "{part:?}")?;
            } else {
                write!(f, "{part}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::MemorySource;
    use super::*;
    use codex_protocol::models::ContentItem;
    use tokio::sync::mpsc;

    fn recorder_with_channel() -> (MemoryRecorder, mpsc::UnboundedReceiver<MemoryEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let config = MemoryRecorderConfig {
            conversation_id: ConversationId::default(),
            session_source: Some("test".into()),
            sink: Some(tx),
        };
        (MemoryRecorder::new(config), rx)
    }

    #[tokio::test]
    async fn records_user_message() {
        let (recorder, mut rx) = recorder_with_channel();
        let item = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::OutputText {
                text: "hello".to_string(),
            }],
        };
        recorder.record_response_items(&[item]);
        let event = rx.recv().await.expect("event");
        assert_eq!(event.source, MemorySource::UserMessage);
        assert_eq!(event.text, "hello");
        assert_eq!(event.metadata.session_source.as_deref(), Some("test"));
    }

    #[tokio::test]
    async fn records_tool_output() {
        let (recorder, mut rx) = recorder_with_channel();
        let item = ResponseItem::FunctionCallOutput {
            call_id: "call-1".to_string(),
            output: FunctionCallOutputPayload {
                success: Some(true),
                content: "ok".to_string(),
            },
        };
        recorder.record_response_items(&[item]);
        let event = rx.recv().await.expect("event");
        assert_eq!(event.source, MemorySource::ToolOutput);
        assert_eq!(event.metadata.call_id.as_deref(), Some("call-1"));
    }
}
