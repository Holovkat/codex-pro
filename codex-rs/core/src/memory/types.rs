use chrono::DateTime;
use chrono::Utc;
use codex_protocol::protocol::MemoryPreviewMode;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

/// Origin metadata for events captured by the recorder.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemorySource {
    UserMessage,
    AssistantMessage,
    ToolOutput,
    FileDiff,
    SystemMessage,
}

/// Extensible metadata bag attached to each memory event.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryMetadata {
    pub conversation_id: Option<String>,
    pub session_source: Option<String>,
    pub role: Option<String>,
    pub tool_name: Option<String>,
    pub call_id: Option<String>,
    pub file_path: Option<String>,
    pub tags: Vec<String>,
}

impl MemoryMetadata {
    pub fn with_conversation(id: impl Into<String>) -> Self {
        Self {
            conversation_id: Some(id.into()),
            ..Self::default()
        }
    }
}

/// Atomic event awaiting distillation into long-term memory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryEvent {
    pub event_id: Uuid,
    pub source: MemorySource,
    pub text: String,
    pub metadata: MemoryMetadata,
    pub timestamp: DateTime<Utc>,
}

impl MemoryEvent {
    pub fn new(source: MemorySource, text: impl Into<String>, metadata: MemoryMetadata) -> Self {
        Self {
            event_id: Uuid::now_v7(),
            source,
            text: text.into(),
            metadata,
            timestamp: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryRecord {
    pub record_id: Uuid,
    pub summary: String,
    pub embedding: Vec<f32>,
    pub metadata: MemoryMetadata,
    pub confidence: f32,
    pub source: MemorySource,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub tool_last_fetched_at: Option<DateTime<Utc>>,
}

impl MemoryRecord {
    pub fn new(
        summary: String,
        embedding: Vec<f32>,
        metadata: MemoryMetadata,
        confidence: f32,
        source: MemorySource,
    ) -> Self {
        let now = Utc::now();
        Self {
            record_id: Uuid::now_v7(),
            summary,
            embedding,
            metadata,
            confidence,
            source,
            created_at: now,
            updated_at: now,
            tool_last_fetched_at: None,
        }
    }

    pub fn from_event(
        event: &MemoryEvent,
        summary: String,
        embedding: Vec<f32>,
        confidence: f32,
    ) -> Self {
        let mut metadata = event.metadata.clone();
        metadata.tags.push(event.source_tag());
        Self::new(
            summary,
            embedding,
            metadata,
            confidence,
            event.source.clone(),
        )
    }
}

pub trait MemoryPreviewModeExt {
    fn requires_user_confirmation(self) -> bool;
}

impl MemoryPreviewModeExt for MemoryPreviewMode {
    fn requires_user_confirmation(self) -> bool {
        matches!(self, MemoryPreviewMode::Enabled)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemorySettings {
    pub enabled: bool,
    pub min_confidence: f32,
    pub preview_mode: MemoryPreviewMode,
    pub max_tokens: u32,
    pub retention_days: u32,
    #[serde(default = "default_prefer_pull_suggestions")]
    pub prefer_pull_suggestions: bool,
}

impl Default for MemorySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            min_confidence: 0.75,
            preview_mode: MemoryPreviewMode::Enabled,
            max_tokens: 400,
            retention_days: 30,
            prefer_pull_suggestions: default_prefer_pull_suggestions(),
        }
    }
}

const fn default_prefer_pull_suggestions() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryHit {
    pub score: f32,
    pub record: MemoryRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryStats {
    pub total_records: usize,
    pub hits: u64,
    pub misses: u64,
    pub preview_accepted: u64,
    pub preview_skipped: u64,
    #[serde(default)]
    pub suggest_invocations: u64,
    pub disk_usage_bytes: u64,
    pub last_rebuild_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryMetrics {
    pub hits: u64,
    pub misses: u64,
    pub preview_accepted: u64,
    pub preview_skipped: u64,
    #[serde(default)]
    pub suggest_invocations: u64,
    pub last_reset_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MemoryRecordUpdate {
    pub summary: Option<String>,
    pub embedding: Option<Vec<f32>>,
    pub metadata: Option<MemoryMetadata>,
    pub confidence: Option<f32>,
    pub source: Option<MemorySource>,
}

impl MemoryEvent {
    fn source_tag(&self) -> String {
        match self.source {
            MemorySource::UserMessage => "user".into(),
            MemorySource::AssistantMessage => "assistant".into(),
            MemorySource::ToolOutput => "tool".into(),
            MemorySource::FileDiff => "file_diff".into(),
            MemorySource::SystemMessage => "system".into(),
        }
    }
}

pub fn clean_summary(raw: &str) -> String {
    raw.replace("<user_instructions>", "")
        .replace("</user_instructions>", "")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_event_round_trips_json() {
        let metadata = MemoryMetadata {
            conversation_id: Some("conv-1".into()),
            session_source: Some("tui".into()),
            role: Some("user".into()),
            tool_name: None,
            call_id: Some("call-1".into()),
            file_path: Some("src/lib.rs".into()),
            tags: vec!["context".into(), "lightmem".into()],
        };
        let event = MemoryEvent::new(
            MemorySource::UserMessage,
            "Discussed LightMem architecture",
            metadata.clone(),
        );

        let json = serde_json::to_string(&event).expect("serialize");
        let round_trip: MemoryEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_trip.source, MemorySource::UserMessage);
        assert_eq!(round_trip.text, "Discussed LightMem architecture");
        assert_eq!(round_trip.metadata, metadata);
        assert_eq!(round_trip.event_id, event.event_id);
    }
}
