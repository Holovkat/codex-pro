use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;
use std::time::Instant;

use chrono::DateTime;
use chrono::Local;
use codex_core::memory::GlobalMemoryStore;
use codex_core::memory::MemoryMetadata;
use codex_core::memory::MemoryMetrics;
use codex_core::memory::MemoryPreviewMode;
use codex_core::memory::MemoryPreviewModeExt;
use codex_core::memory::MemoryRecord;
use codex_core::memory::MemoryRecordUpdate;
use codex_core::memory::MemoryRuntime;
use codex_core::memory::MemorySource;
use codex_core::memory::MemoryStats;
use codex_core::memory::MiniCpmArtifactStatus;
use codex_core::memory::MiniCpmDiagnostics;
use codex_core::memory::MiniCpmDownloadState;
use codex_core::memory::MiniCpmStatus;
use codex_core::memory::clean_summary;
use color_eyre::eyre::Result;
use color_eyre::eyre::eyre;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use ratatui::layout::Alignment;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use tokio_stream::StreamExt;
use uuid::Uuid;

use crate::custom_terminal::Frame as TerminalFrame;
use crate::text_formatting::truncate_text;
use crate::tui::FrameRequester;
use crate::tui::Tui;
use crate::tui::TuiEvent;

const CF_STEP: f32 = 0.05;
const STATUS_DURATION: Duration = Duration::from_secs(4);
const MAX_VISIBLE_ROWS: usize = 500;

pub async fn run_memory_manager(tui: &mut Tui, codex_home: &Path) -> Result<()> {
    let alt = AltScreenGuard::enter(tui)?;
    let runtime = MemoryRuntime::load(codex_home.join("memory"))
        .await
        .map_err(|err| eyre!("failed to initialise memory runtime: {err:#}"))?;
    let mut state = MemoryManagerState::load(runtime).await?;

    let mut events = alt.tui.event_stream().fuse();
    state.request_redraw(alt.tui);
    loop {
        tokio::select! {
            Some(event) = events.next() => {
                match event {
                    TuiEvent::Key(key) => {
                        if matches!(key.kind, KeyEventKind::Release) {
                            continue;
                        }
                        if state.handle_key(alt.tui, key).await? {
                            break;
                        }
                    }
                    TuiEvent::Draw => {
                        state.draw(alt.tui)?;
                    }
                    _ => {}
                }
            }
            else => break,
        }
    }
    Ok(())
}

struct AltScreenGuard<'a> {
    tui: &'a mut Tui,
}

impl<'a> AltScreenGuard<'a> {
    fn enter(tui: &'a mut Tui) -> Result<Self> {
        tui.enter_alt_screen()
            .map_err(|err| eyre!("failed to enter alternate screen: {err:#}"))?;
        Ok(Self { tui })
    }
}

impl Drop for AltScreenGuard<'_> {
    fn drop(&mut self) {
        let _ = self.tui.leave_alt_screen();
    }
}

#[derive(Clone)]
struct MemoryRow {
    record: MemoryRecord,
    score: Option<f32>,
    duplicate_count: usize,
}

impl MemoryRow {
    fn new(record: MemoryRecord) -> Self {
        Self {
            record,
            score: None,
            duplicate_count: 1,
        }
    }

    fn bump_duplicate(&mut self) {
        self.duplicate_count = self.duplicate_count.saturating_add(1);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModalKind {
    Create,
    Edit,
    ConfirmDelete,
    ConfirmReset,
    ConfirmRebuild,
}

#[derive(Clone)]
enum ModalState {
    Form(MemoryFormState),
    Confirm(ConfirmState),
    ConfirmUnsaved(UnsavedPromptState),
}

#[derive(Clone)]
struct ConfirmState {
    kind: ModalKind,
    target: Option<Uuid>,
    message: String,
}

#[derive(Clone)]
struct MemoryFormState {
    kind: ModalKind,
    record_id: Option<Uuid>,
    original_summary: String,
    summary: String,
    original_tags: String,
    tags: String,
    confidence: f32,
    metadata: MemoryMetadata,
    source: MemorySource,
    active_field: FormField,
    summary_cursor: CursorPosition,
    tags_cursor: usize,
    error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FormField {
    Summary,
    Tags,
}

#[derive(Clone, Copy, Debug, Default)]
struct CursorPosition {
    line: usize,
    column: usize,
    desired_column: usize,
}

#[derive(Clone)]
struct UnsavedPromptState {
    form: MemoryFormState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FormOutcome {
    Continue,
    Cancel,
    PromptSave,
}

impl MemoryFormState {
    fn for_create(default_confidence: f32) -> Self {
        Self {
            kind: ModalKind::Create,
            record_id: None,
            original_summary: String::new(),
            summary: String::new(),
            original_tags: String::new(),
            tags: String::new(),
            confidence: default_confidence,
            metadata: MemoryMetadata::default(),
            source: MemorySource::UserMessage,
            active_field: FormField::Summary,
            summary_cursor: CursorPosition::default(),
            tags_cursor: 0,
            error: None,
        }
    }

    fn for_edit(record: &MemoryRecord) -> Self {
        let summary = clean_summary(&record.summary);
        let tags = record.metadata.tags.join(", ");
        let tags_cursor = tags.chars().count();
        let lines = summary.split('\n').count().max(1);
        let last_line_len = summary
            .split('\n')
            .next_back()
            .map(|line| line.chars().count())
            .unwrap_or(0);
        Self {
            kind: ModalKind::Edit,
            record_id: Some(record.record_id),
            original_summary: summary.clone(),
            summary,
            original_tags: tags.clone(),
            tags,
            confidence: record.confidence,
            metadata: record.metadata.clone(),
            source: record.source.clone(),
            active_field: FormField::Summary,
            summary_cursor: CursorPosition {
                line: lines.saturating_sub(1),
                column: last_line_len,
                desired_column: last_line_len,
            },
            tags_cursor,
            error: None,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> FormOutcome {
        match key.code {
            KeyCode::Esc => {
                if self.is_dirty() {
                    FormOutcome::PromptSave
                } else {
                    FormOutcome::Cancel
                }
            }
            KeyCode::Tab => {
                self.toggle_field();
                FormOutcome::Continue
            }
            KeyCode::BackTab => {
                self.toggle_field();
                FormOutcome::Continue
            }
            _ => match self.active_field {
                FormField::Summary => self.handle_summary_key(key),
                FormField::Tags => self.handle_tags_key(key),
            },
        }
    }

    fn is_dirty(&self) -> bool {
        self.summary != self.original_summary || self.tags != self.original_tags
    }

    fn toggle_field(&mut self) {
        self.active_field = match self.active_field {
            FormField::Summary => FormField::Tags,
            FormField::Tags => FormField::Summary,
        };
        match self.active_field {
            FormField::Summary => {
                let total_lines = self.summary_lines_len();
                if self.summary_cursor.line >= total_lines {
                    self.summary_cursor.line = total_lines.saturating_sub(1);
                    self.summary_cursor.column = self.summary_line_len(self.summary_cursor.line);
                    self.summary_cursor.desired_column = self.summary_cursor.column;
                }
            }
            FormField::Tags => {
                self.tags_cursor = self.tags_char_len().min(self.tags_cursor);
            }
        }
    }

    fn handle_summary_key(&mut self, key: KeyEvent) -> FormOutcome {
        match key.code {
            KeyCode::Left => {
                self.move_summary_left();
                FormOutcome::Continue
            }
            KeyCode::Right => {
                self.move_summary_right();
                FormOutcome::Continue
            }
            KeyCode::Up => {
                self.move_summary_vertical(-1);
                FormOutcome::Continue
            }
            KeyCode::Down => {
                self.move_summary_vertical(1);
                FormOutcome::Continue
            }
            KeyCode::Home => {
                self.summary_cursor.column = 0;
                self.summary_cursor.desired_column = 0;
                FormOutcome::Continue
            }
            KeyCode::End => {
                let len = self.summary_line_len(self.summary_cursor.line);
                self.summary_cursor.column = len;
                self.summary_cursor.desired_column = len;
                FormOutcome::Continue
            }
            KeyCode::Backspace => {
                self.remove_summary_before_cursor();
                FormOutcome::Continue
            }
            KeyCode::Delete => {
                self.remove_summary_at_cursor();
                FormOutcome::Continue
            }
            KeyCode::Enter => {
                self.insert_summary_char('\n');
                FormOutcome::Continue
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    FormOutcome::Continue
                } else {
                    self.insert_summary_char(c);
                    FormOutcome::Continue
                }
            }
            _ => FormOutcome::Continue,
        }
    }

    fn handle_tags_key(&mut self, key: KeyEvent) -> FormOutcome {
        match key.code {
            KeyCode::Left => {
                if self.tags_cursor > 0 {
                    self.tags_cursor -= 1;
                }
                FormOutcome::Continue
            }
            KeyCode::Right => {
                if self.tags_cursor < self.tags_char_len() {
                    self.tags_cursor += 1;
                }
                FormOutcome::Continue
            }
            KeyCode::Home => {
                self.tags_cursor = 0;
                FormOutcome::Continue
            }
            KeyCode::End => {
                self.tags_cursor = self.tags_char_len();
                FormOutcome::Continue
            }
            KeyCode::Backspace => {
                if self.tags_cursor > 0 {
                    let idx = self.tags_byte_offset(self.tags_cursor);
                    let prev = self.tags_byte_offset(self.tags_cursor - 1);
                    self.tags.replace_range(prev..idx, "");
                    self.tags_cursor -= 1;
                }
                FormOutcome::Continue
            }
            KeyCode::Delete => {
                if self.tags_cursor < self.tags_char_len() {
                    let idx = self.tags_byte_offset(self.tags_cursor);
                    let next = self.tags_byte_offset(self.tags_cursor + 1);
                    self.tags.replace_range(idx..next, "");
                }
                FormOutcome::Continue
            }
            KeyCode::Enter => FormOutcome::PromptSave,
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    FormOutcome::Continue
                } else {
                    let idx = self.tags_byte_offset(self.tags_cursor);
                    self.tags.insert(idx, c);
                    self.tags_cursor += 1;
                    FormOutcome::Continue
                }
            }
            _ => FormOutcome::Continue,
        }
    }

    fn summary_lines(&self) -> Vec<&str> {
        if self.summary.is_empty() {
            vec![""]
        } else {
            self.summary.split('\n').collect()
        }
    }

    fn summary_lines_len(&self) -> usize {
        self.summary_lines().len()
    }

    fn summary_line_len(&self, line: usize) -> usize {
        self.summary
            .split('\n')
            .nth(line)
            .map(|l| l.chars().count())
            .unwrap_or(0)
    }

    fn tags_char_len(&self) -> usize {
        self.tags.chars().count()
    }

    fn summary_byte_offset(&self, line: usize, column: usize) -> usize {
        let lines = self.summary_lines();
        let mut offset = 0usize;
        for (idx, current) in lines.iter().enumerate() {
            if idx == line {
                let idx_in_line = byte_index_for_column(current, column);
                offset += idx_in_line;
                break;
            }
            offset += current.len();
            if idx + 1 < lines.len() {
                offset = offset.saturating_add(1);
            }
        }
        offset
    }

    fn tags_byte_offset(&self, column: usize) -> usize {
        byte_index_for_column(&self.tags, column)
    }

    fn move_summary_left(&mut self) {
        if self.summary_cursor.column > 0 {
            self.summary_cursor.column -= 1;
        } else if self.summary_cursor.line > 0 {
            self.summary_cursor.line -= 1;
            self.summary_cursor.column = self.summary_line_len(self.summary_cursor.line);
        }
        self.summary_cursor.desired_column = self.summary_cursor.column;
    }

    fn move_summary_right(&mut self) {
        let len = self.summary_line_len(self.summary_cursor.line);
        if self.summary_cursor.column < len {
            self.summary_cursor.column += 1;
        } else if self.summary_cursor.line + 1 < self.summary_lines_len() {
            self.summary_cursor.line += 1;
            self.summary_cursor.column = 0;
        }
        self.summary_cursor.desired_column = self.summary_cursor.column;
    }

    fn move_summary_vertical(&mut self, delta: isize) {
        let total_lines = self.summary_lines_len();
        let new_line = clamp_isize(
            self.summary_cursor.line as isize + delta,
            0,
            total_lines.saturating_sub(1) as isize,
        ) as usize;
        self.summary_cursor.line = new_line;
        let len = self.summary_line_len(new_line);
        self.summary_cursor.column = len.min(self.summary_cursor.desired_column);
    }

    fn insert_summary_char(&mut self, ch: char) {
        let idx = self.summary_byte_offset(self.summary_cursor.line, self.summary_cursor.column);
        self.summary.insert(idx, ch);
        if ch == '\n' {
            self.summary_cursor.line += 1;
            self.summary_cursor.column = 0;
        } else {
            self.summary_cursor.column += 1;
        }
        self.summary_cursor.desired_column = self.summary_cursor.column;
    }

    fn remove_summary_before_cursor(&mut self) {
        if self.summary_cursor.column > 0 {
            let idx =
                self.summary_byte_offset(self.summary_cursor.line, self.summary_cursor.column);
            let prev =
                self.summary_byte_offset(self.summary_cursor.line, self.summary_cursor.column - 1);
            self.summary.replace_range(prev..idx, "");
            self.summary_cursor.column -= 1;
        } else if self.summary_cursor.line > 0 {
            let idx = self.summary_byte_offset(self.summary_cursor.line, 0);
            if idx > 0 {
                self.summary.remove(idx - 1);
                self.summary_cursor.line -= 1;
                self.summary_cursor.column = self.summary_line_len(self.summary_cursor.line);
                self.summary_cursor.desired_column = self.summary_cursor.column;
            }
        }
        self.summary_cursor.desired_column = self.summary_cursor.column;
    }

    fn remove_summary_at_cursor(&mut self) {
        let line_len = self.summary_line_len(self.summary_cursor.line);
        if self.summary_cursor.column < line_len {
            let idx =
                self.summary_byte_offset(self.summary_cursor.line, self.summary_cursor.column);
            let next =
                self.summary_byte_offset(self.summary_cursor.line, self.summary_cursor.column + 1);
            self.summary.replace_range(idx..next, "");
        } else if self.summary_cursor.line + 1 < self.summary_lines_len() {
            let idx = self.summary_byte_offset(self.summary_cursor.line + 1, 0);
            if idx > 0 {
                self.summary.remove(idx - 1);
            }
        }
        self.summary_cursor.desired_column = self.summary_cursor.column;
    }

    fn validate(&mut self) -> Result<()> {
        if self.summary.trim().is_empty() {
            self.error = Some("Summary cannot be empty".to_string());
            return Err(eyre!("summary required"));
        }
        self.error = None;
        Ok(())
    }

    fn parsed_tags(&self) -> Vec<String> {
        self.tags
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(std::string::ToString::to_string)
            .collect()
    }
}

fn clamp_isize(value: isize, min: isize, max: isize) -> isize {
    value.max(min).min(max)
}

fn byte_index_for_column(line: &str, column: usize) -> usize {
    if column == 0 {
        return 0;
    }
    let mut result = line.len();
    for (idx, (byte, _)) in line.char_indices().enumerate() {
        if idx == column {
            result = byte;
            break;
        }
    }
    if column >= line.chars().count() {
        line.len()
    } else {
        result
    }
}

#[derive(Clone)]
struct StatusLine {
    text: String,
    level: StatusLevel,
    expires_at: Option<Instant>,
}

#[derive(Clone, Copy)]
enum StatusLevel {
    Success,
    Error,
}

impl StatusLine {
    fn success(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            level: StatusLevel::Success,
            expires_at: Some(Instant::now() + STATUS_DURATION),
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            level: StatusLevel::Error,
            expires_at: None,
        }
    }
}

struct MemoryManagerState {
    runtime: MemoryRuntime,
    rows: Vec<MemoryRow>,
    index_by_id: HashMap<Uuid, usize>,
    filtered: Vec<usize>,
    cursor: usize,
    scroll_top: usize,
    query: String,
    editing_query: bool,
    semantic_hits: Option<Vec<(Uuid, f32)>>,
    min_confidence: f32,
    preview_mode: MemoryPreviewMode,
    manual_selected: HashSet<Uuid>,
    manual_deselected: HashSet<Uuid>,
    model_status: Option<MiniCpmStatus>,
    download_state: MiniCpmDownloadState,
    diagnostics: MiniCpmDiagnostics,
    metrics: MemoryMetrics,
    stats: MemoryStats,
    status: Option<StatusLine>,
    modal: Option<ModalState>,
    requester: Option<FrameRequester>,
}

impl MemoryManagerState {
    async fn load(runtime: MemoryRuntime) -> Result<Self> {
        let mut guard = runtime.store.lock().await;
        let rows = Self::load_rows(&mut guard)?;
        let metrics = guard.metrics().clone();
        let stats = guard
            .stats()
            .map_err(|err| eyre!("failed to read memory stats: {err:#}"))?;
        drop(guard);
        let settings = runtime.settings.get().await;
        let index_by_id = rows
            .iter()
            .enumerate()
            .map(|(idx, row)| (row.record.record_id, idx))
            .collect();
        let mut state = Self {
            runtime,
            rows,
            index_by_id,
            filtered: Vec::new(),
            cursor: 0,
            scroll_top: 0,
            query: String::new(),
            editing_query: false,
            semantic_hits: None,
            min_confidence: settings.min_confidence,
            preview_mode: settings.preview_mode,
            manual_selected: HashSet::new(),
            manual_deselected: HashSet::new(),
            model_status: None,
            download_state: MiniCpmDownloadState::default(),
            diagnostics: MiniCpmDiagnostics::default(),
            metrics,
            stats,
            status: None,
            modal: None,
            requester: None,
        };
        state.refresh_model_state().await?;
        state.update_filtered();
        Ok(state)
    }

    fn request_redraw(&mut self, tui: &Tui) {
        self.requester = Some(tui.frame_requester());
        if let Some(requester) = &self.requester {
            requester.schedule_frame();
        }
    }

    async fn refresh(&mut self) -> Result<()> {
        let mut guard = self.runtime.store.lock().await;
        self.rows = Self::load_rows(&mut guard)?;
        self.index_by_id = self
            .rows
            .iter()
            .enumerate()
            .map(|(idx, row)| (row.record.record_id, idx))
            .collect();
        self.metrics = guard.metrics().clone();
        self.stats = guard
            .stats()
            .map_err(|err| eyre!("failed to read memory stats: {err:#}"))?;
        drop(guard);
        self.refresh_model_state().await?;
        self.update_filtered();
        Ok(())
    }

    fn load_rows(store: &mut GlobalMemoryStore) -> Result<Vec<MemoryRow>> {
        let records = store
            .load_all()
            .map_err(|err| eyre!("failed to load memory records: {err:#}"))?;
        Ok(Self::collapse_records(records))
    }

    fn collapse_records(mut records: Vec<MemoryRecord>) -> Vec<MemoryRow> {
        let mut deduped: Vec<MemoryRow> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();

        records.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

        for record in records {
            let key = Self::memory_key(&record);
            if let Some(&idx) = index.get(&key) {
                deduped[idx].bump_duplicate();
            } else {
                index.insert(key, deduped.len());
                deduped.push(MemoryRow::new(record));
            }
        }

        if deduped.len() > MAX_VISIBLE_ROWS {
            deduped.truncate(MAX_VISIBLE_ROWS);
        }
        deduped
    }

    fn memory_key(record: &MemoryRecord) -> String {
        let summary = clean_summary(&record.summary)
            .to_ascii_lowercase()
            .trim()
            .to_string();
        let conversation = record
            .metadata
            .conversation_id
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let role = record
            .metadata
            .role
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let source = format!("{:?}", record.source);
        format!("{summary}::{conversation}::{role}::{source}")
    }
    fn update_filtered(&mut self) {
        self.filtered.clear();
        if self.semantic_hits.is_none() {
            for row in &mut self.rows {
                row.score = None;
            }
        }
        if let Some(hits) = &self.semantic_hits {
            for (id, score) in hits {
                if let Some(&idx) = self.index_by_id.get(id) {
                    let record = &self.rows[idx].record;
                    if record.confidence >= self.min_confidence {
                        if let Some(row) = self.rows.get_mut(idx) {
                            row.score = Some(*score);
                        }
                        self.filtered.push(idx);
                    }
                }
            }
        } else {
            for (idx, row) in self.rows.iter().enumerate() {
                if row.record.confidence >= self.min_confidence
                    && self.matches_query(&row.record, &self.query)
                {
                    self.filtered.push(idx);
                }
            }
        }
        if self.cursor >= self.filtered.len() {
            self.cursor = self.filtered.len().saturating_sub(1);
        }
    }

    async fn refresh_model_state(&mut self) -> Result<()> {
        if let Some(manager) = self.runtime.model_manager() {
            match manager.status().await {
                Ok(status) => {
                    self.model_status = Some(status);
                }
                Err(err) => {
                    let message = format!("MiniCPM status error: {err:#}");
                    if self
                        .status
                        .as_ref()
                        .map(|current| current.text != message)
                        .unwrap_or(true)
                    {
                        self.status = Some(StatusLine::error(message));
                    }
                    self.model_status = None;
                }
            }
            self.download_state = manager.download_state().await;
            self.diagnostics = manager.diagnostics().await;
        } else {
            self.model_status = None;
            self.download_state = MiniCpmDownloadState::default();
            self.diagnostics = MiniCpmDiagnostics::default();
        }
        Ok(())
    }

    fn format_download_progress(&self) -> Option<String> {
        let mut artifacts: Vec<_> = self.download_state.artifacts.iter().collect();
        artifacts.sort_by(|a, b| a.0.cmp(b.0));
        for (name, artifact) in artifacts {
            match artifact.status {
                MiniCpmArtifactStatus::Downloading => {
                    if let Some(total) = artifact.total_bytes
                        && total > 0
                    {
                        let percent = (artifact.downloaded_bytes as f64 * 100.0) / total as f64;
                        return Some(format!(
                            "downloading {} ({:.0}% – {}/{total} bytes)",
                            name, percent, artifact.downloaded_bytes
                        ));
                    }
                    return Some(format!(
                        "downloading {} ({} bytes)",
                        name, artifact.downloaded_bytes
                    ));
                }
                MiniCpmArtifactStatus::Verifying => {
                    return Some(format!("verifying {name}"));
                }
                MiniCpmArtifactStatus::Failed => {
                    let msg = artifact
                        .error
                        .as_deref()
                        .map(|text| truncate_text(text, 32))
                        .unwrap_or_else(|| "unknown error".to_string());
                    return Some(format!("{name} failed: {msg}"));
                }
                _ => {}
            }
        }
        None
    }

    fn model_status_line(&self) -> Line<'static> {
        let mut spans: Vec<Span<'static>> = vec![Span::from("MiniCPM: ").dim()];
        match &self.model_status {
            Some(MiniCpmStatus::Ready {
                version,
                last_updated,
            }) => {
                let ts = last_updated
                    .with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string();
                spans.push(format!("ready ({version}, updated {ts})").green());
            }
            Some(MiniCpmStatus::Missing {
                version, missing, ..
            }) => {
                if let Some(message) = self.format_download_progress() {
                    spans.push(message.cyan());
                } else if missing.is_empty() {
                    spans.push(format!("verifying cache (version {version})").cyan());
                } else {
                    spans
                        .push(format!("missing {} (version {version})", missing.join(", ")).cyan());
                }
            }
            None => spans.push("status unavailable".red()),
        }

        if let Some(failure) = self.diagnostics.last_failure.as_ref() {
            let when = failure
                .occurred_at
                .with_timezone(&Local)
                .format("%H:%M:%S")
                .to_string();
            let message = truncate_text(&failure.message, 40);
            spans.push("   ".into());
            spans.push(format!("last error {message} @ {when}").red());
        }

        Line::from(spans)
    }

    fn matches_query(&self, record: &MemoryRecord, query: &str) -> bool {
        if query.trim().is_empty() {
            return true;
        }
        let q = query.to_lowercase();
        record.summary.to_lowercase().contains(&q)
            || record
                .metadata
                .tags
                .iter()
                .any(|tag| tag.to_lowercase().contains(&q))
    }

    async fn run_semantic_search(&mut self) -> Result<()> {
        if self.query.trim().is_empty() {
            self.semantic_hits = None;
            self.update_filtered();
            return Ok(());
        }
        let hits = self
            .runtime
            .search_records(
                &self.query,
                self.rows.len().max(8),
                Some(self.min_confidence),
            )
            .await
            .map_err(|err| eyre!("memory query failed: {err:#}"))?;
        self.semantic_hits = Some(
            hits.into_iter()
                .map(|hit| (hit.record.record_id, hit.score))
                .collect(),
        );
        self.update_filtered();
        Ok(())
    }

    fn current_row(&self) -> Option<&MemoryRow> {
        self.filtered
            .get(self.cursor)
            .and_then(|idx| self.rows.get(*idx))
    }

    fn toggle_selection(&mut self) {
        let (id, was_selected) = match self.current_row() {
            Some(row) => (row.record.record_id, self.is_selected(&row.record)),
            None => return,
        };
        if self.manual_selected.remove(&id) {
            self.manual_deselected.insert(id);
        } else if self.manual_deselected.remove(&id) {
            self.manual_selected.insert(id);
        } else if was_selected {
            self.manual_deselected.insert(id);
        } else {
            self.manual_selected.insert(id);
        }
    }

    fn is_selected(&self, record: &MemoryRecord) -> bool {
        if self.manual_selected.contains(&record.record_id) {
            return true;
        }
        if self.manual_deselected.contains(&record.record_id) {
            return false;
        }
        record.confidence >= self.min_confidence
    }

    async fn handle_key(&mut self, tui: &mut Tui, key: KeyEvent) -> Result<bool> {
        if let Some(modal) = self.modal.take() {
            self.handle_modal_key(modal, key).await?;
            self.request_redraw(tui);
            return Ok(false);
        }
        match key.code {
            KeyCode::Esc => {
                if self.editing_query {
                    self.editing_query = false;
                    self.request_redraw(tui);
                    return Ok(false);
                }
                return Ok(true);
            }
            KeyCode::Up => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                }
            }
            KeyCode::Down => {
                if self.cursor + 1 < self.filtered.len() {
                    self.cursor += 1;
                }
            }
            KeyCode::PageUp => {
                self.cursor = self.cursor.saturating_sub(10);
            }
            KeyCode::PageDown => {
                self.cursor = (self.cursor + 10).min(self.filtered.len().saturating_sub(1));
            }
            KeyCode::Char(c) if self.editing_query && key.modifiers.is_empty() => {
                self.query.push(c);
                if let Err(err) = self.run_semantic_search().await {
                    self.status = Some(StatusLine::error(err.to_string()));
                }
            }
            KeyCode::Char('s') if key.modifiers.is_empty() && !self.editing_query => {
                self.editing_query = true;
            }
            KeyCode::Backspace if self.editing_query => {
                self.query.pop();
                if let Err(err) = self.run_semantic_search().await {
                    self.status = Some(StatusLine::error(err.to_string()));
                }
            }
            KeyCode::Char('m') if key.modifiers.is_empty() => {
                let new_mode = match self.preview_mode {
                    MemoryPreviewMode::Enabled => MemoryPreviewMode::Disabled,
                    MemoryPreviewMode::Disabled => MemoryPreviewMode::Enabled,
                };
                if let Err(err) = self
                    .runtime
                    .settings
                    .update(|settings| settings.preview_mode = new_mode)
                    .await
                {
                    self.status = Some(StatusLine::error(format!(
                        "Failed to update preview mode: {err:#}"
                    )));
                } else {
                    self.preview_mode = new_mode;
                    self.status = Some(StatusLine::success(match new_mode {
                        MemoryPreviewMode::Enabled => {
                            "Preview mode enabled (user selection required)"
                        }
                        MemoryPreviewMode::Disabled => "Preview mode disabled (auto selection)",
                    }));
                }
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.adjust_confidence(CF_STEP).await?;
            }
            KeyCode::Char('-') | KeyCode::Char('_') => {
                self.adjust_confidence(-CF_STEP).await?;
            }
            KeyCode::Enter => {
                if let Some(row) = self.current_row() {
                    self.modal = Some(ModalState::Form(MemoryFormState::for_edit(&row.record)));
                }
            }
            KeyCode::Char(' ') if self.preview_mode.requires_user_confirmation() => {
                self.toggle_selection();
            }
            KeyCode::Char('a') if self.preview_mode.requires_user_confirmation() => {
                let all_selected = self
                    .visible_records()
                    .all(|record| self.is_selected(record));
                self.manual_selected.clear();
                self.manual_deselected.clear();
                if !all_selected {
                    let ids: Vec<Uuid> = self
                        .visible_records()
                        .map(|record| record.record_id)
                        .collect();
                    for id in ids {
                        self.manual_selected.insert(id);
                    }
                }
            }
            KeyCode::Char('n') => {
                self.modal = Some(ModalState::Form(MemoryFormState::for_create(
                    self.min_confidence.max(0.5),
                )));
            }
            KeyCode::Char('d') => {
                if let Some(row) = self.current_row() {
                    self.modal = Some(ModalState::Confirm(ConfirmState {
                        kind: ModalKind::ConfirmDelete,
                        target: Some(row.record.record_id),
                        message: format!(
                            "Delete memory “{}”?",
                            truncate_text(&row.record.summary, 60)
                        ),
                    }));
                }
            }
            KeyCode::Char('r') => {
                self.modal = Some(ModalState::Confirm(ConfirmState {
                    kind: ModalKind::ConfirmRebuild,
                    target: None,
                    message: "Rebuild memory index from existing records?".to_string(),
                }));
            }
            KeyCode::Char('R') => {
                self.modal = Some(ModalState::Confirm(ConfirmState {
                    kind: ModalKind::ConfirmReset,
                    target: None,
                    message: "Reset memory store? This deletes manifest, index, and metrics."
                        .to_string(),
                }));
            }
            _ => {}
        }
        self.request_redraw(tui);
        Ok(false)
    }

    async fn adjust_confidence(&mut self, delta: f32) -> Result<()> {
        let mut new_conf = (self.min_confidence + delta).clamp(0.0, 1.0);
        // Snap to 2 decimal places to avoid floating drift.
        new_conf = (new_conf * 100.0).round() / 100.0;
        if (new_conf - self.min_confidence).abs() < f32::EPSILON {
            return Ok(());
        }
        self.runtime
            .settings
            .update(|settings| settings.min_confidence = new_conf)
            .await
            .map_err(|err| eyre!("failed to persist confidence setting: {err:#}"))?;
        self.min_confidence = new_conf;
        self.manual_selected
            .retain(|id| self.rows.iter().any(|row| row.record.record_id == *id));
        self.manual_deselected
            .retain(|id| self.rows.iter().any(|row| row.record.record_id == *id));
        self.update_filtered();
        self.status = Some(StatusLine::success(format!(
            "Minimum confidence set to {:.0}%",
            new_conf * 100.0
        )));
        Ok(())
    }

    fn visible_records(&self) -> impl Iterator<Item = &MemoryRecord> {
        self.filtered
            .iter()
            .filter_map(|idx| self.rows.get(*idx))
            .map(|row| &row.record)
    }

    fn draw(&mut self, tui: &mut Tui) -> Result<()> {
        let rows_clone = self.rows.clone();
        let filtered = self.filtered.clone();
        let cursor = self.cursor;
        let query = self.query.clone();
        let editing_query = self.editing_query;
        let min_conf = self.min_confidence;
        let preview_mode = self.preview_mode;
        let hits = self.metrics.hits;
        let misses = self.metrics.misses;
        let stats = self.stats.clone();
        let status = self.status.clone();
        let manual_selected = self.manual_selected.clone();
        let manual_deselected = self.manual_deselected.clone();
        let modal = self.modal.clone();
        let semantic = self.semantic_hits.clone();

        tui.draw(u16::MAX, |frame| {
            let size = frame.area();
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(6),
                    Constraint::Min(8),
                    Constraint::Length(3),
                ])
                .split(size);
            self.draw_header(
                frame,
                layout[0],
                &query,
                editing_query,
                min_conf,
                preview_mode,
                hits,
                misses,
            );
            self.draw_table(
                frame,
                layout[1],
                &rows_clone,
                &filtered,
                cursor,
                &manual_selected,
                &manual_deselected,
                semantic.as_ref(),
            );
            self.draw_footer(frame, layout[2], status.as_ref(), stats.total_records);
            if let Some(modal) = &modal {
                self.draw_modal(frame, size, modal);
            }
        })?;
        self.expire_status();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_header(
        &self,
        frame: &mut TerminalFrame,
        area: Rect,
        query: &str,
        editing: bool,
        min_conf: f32,
        preview_mode: MemoryPreviewMode,
        hits: u64,
        misses: u64,
    ) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Memory Manager".cyan().bold());
        let inner = block.inner(area);
        block.render(area, frame.buffer_mut());

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(inner);

        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
            .split(rows[0]);

        let mut query_display = if query.is_empty() {
            "type to filter…".dim().to_string()
        } else {
            query.to_string()
        };
        if editing {
            query_display.push('▌');
        }
        let search_line = Line::from(vec!["[S]earch: ".cyan().bold(), query_display.into()]);
        Paragraph::new(search_line).render(columns[0], frame.buffer_mut());

        let mode_line = format!(
            "Select [m]ode: {}",
            match preview_mode {
                MemoryPreviewMode::Enabled => "user",
                MemoryPreviewMode::Disabled => "auto",
            }
        )
        .cyan()
        .bold();
        Paragraph::new(Line::from(vec![mode_line]))
            .alignment(ratatui::layout::Alignment::Right)
            .render(columns[1], frame.buffer_mut());

        Paragraph::new(self.model_status_line()).render(rows[1], frame.buffer_mut());

        let confidence_line = Line::from(vec![
            format!("CF ≥ {:>3}%", (min_conf * 100.0).round() as i32)
                .cyan()
                .bold(),
            " [+/-]".dim(),
        ]);
        Paragraph::new(confidence_line).render(rows[2], frame.buffer_mut());

        let metrics_line = Line::from(vec![
            format!("Hits: {hits}").into(),
            "   ".into(),
            format!("Misses: {misses}").into(),
        ])
        .dim();
        Paragraph::new(metrics_line).render(rows[3], frame.buffer_mut());
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_table(
        &mut self,
        frame: &mut TerminalFrame,
        area: Rect,
        rows: &[MemoryRow],
        filtered: &[usize],
        cursor: usize,
        manual_selected: &HashSet<Uuid>,
        manual_deselected: &HashSet<Uuid>,
        semantic: Option<&Vec<(Uuid, f32)>>,
    ) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Recent Memories".cyan().bold());
        let inner = block.inner(area);
        block.render(area, frame.buffer_mut());
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let mut lines: Vec<Line<'static>> = Vec::new();
        let summary_width = inner.width.saturating_sub(56).max(8) as usize;
        for (position, idx) in filtered.iter().enumerate() {
            if let Some(row) = rows.get(*idx) {
                let record = &row.record;
                let selected = if manual_selected.contains(&record.record_id) {
                    true
                } else if manual_deselected.contains(&record.record_id) {
                    false
                } else {
                    record.confidence >= self.min_confidence
                };
                let marker = if selected { "[x]" } else { "[ ]" };
                let timestamp = format_time(record.updated_at);
                let confidence = format!("{:>3}%", (record.confidence * 100.0).round() as i32);
                let tags = if record.metadata.tags.is_empty() {
                    "-".to_string()
                } else {
                    record.metadata.tags.join(", ")
                };
                let mut summary_text = clean_summary(&record.summary);
                if row.duplicate_count > 1 {
                    summary_text = format!("{summary_text} (×{})", row.duplicate_count);
                }
                if let Some(semantic) = semantic
                    && let Some((_, score)) =
                        semantic.iter().find(|(id, _)| id == &record.record_id)
                {
                    summary_text = format!("{summary_text}  (score {score:.2})");
                }
                let summary = truncate_text(&summary_text, summary_width);
                let tool_marker: Span<'static> = if record.tool_last_fetched_at.is_some() {
                    "[t]".cyan()
                } else {
                    "[ ]".dim()
                };
                let mut line = Line::from(vec![
                    marker.to_string().into(),
                    tool_marker,
                    " ".into(),
                    timestamp.into(),
                    "  ".into(),
                    confidence.into(),
                    "  ".into(),
                    tags.clone().dim(),
                    "  ".into(),
                    summary.into(),
                ]);
                if position == cursor {
                    line = line.style(Modifier::REVERSED);
                }
                lines.push(line);
            }
        }

        if lines.is_empty() {
            lines.push(Line::from(
                "No memories above the minimum confidence."
                    .dim()
                    .to_string(),
            ));
        }

        let height = inner.height as usize;
        if height > 0 {
            if cursor < self.scroll_top {
                self.scroll_top = cursor;
            } else if cursor >= self.scroll_top + height {
                self.scroll_top = cursor + 1 - height;
            }
            let max_scroll = filtered.len().saturating_sub(height);
            self.scroll_top = self.scroll_top.min(max_scroll);
        } else {
            self.scroll_top = 0;
        }

        Paragraph::new(lines)
            .scroll((self.scroll_top as u16, 0))
            .wrap(ratatui::widgets::Wrap { trim: true })
            .render(inner, frame.buffer_mut());

        if height > 0 && filtered.len() > height {
            let top_index = self.scroll_top + 1;
            let bottom_index = (self.scroll_top + height).min(filtered.len());
            if top_index <= bottom_index {
                let mut arrows = String::new();
                if self.scroll_top > 0 {
                    arrows.push('▲');
                }
                if bottom_index < filtered.len() {
                    arrows.push('▼');
                }
                let mut hint = String::new();
                if !arrows.is_empty() {
                    hint.push_str(&arrows);
                    hint.push(' ');
                }
                hint.push_str(&format!(
                    "{}-{} of {}",
                    top_index,
                    bottom_index,
                    filtered.len()
                ));
                let hint_line = Line::from(hint).dim();
                let hint_area = Rect::new(
                    inner.x,
                    inner.y + inner.height.saturating_sub(1),
                    inner.width,
                    1,
                );
                Paragraph::new(hint_line)
                    .alignment(Alignment::Right)
                    .render(hint_area, frame.buffer_mut());
            }
        }
    }

    fn draw_footer(
        &self,
        frame: &mut TerminalFrame,
        area: Rect,
        status: Option<&StatusLine>,
        total: usize,
    ) {
        let hints_line = Line::from(vec![
            "[+/-] CF".into(),
            "   ".into(),
            "[Space] toggle".into(),
            "   ".into(),
            "[t] tool call".into(),
            "   ".into(),
            "[A] select all".into(),
            "   ".into(),
            "[N] new".into(),
            "   ".into(),
            "[Enter] edit".into(),
            "   ".into(),
            "[D] delete".into(),
            "   ".into(),
            "[R] rebuild".into(),
            "   ".into(),
            "[Shift+R] reset".into(),
            "   ".into(),
            "[Esc] close".into(),
        ])
        .dim();
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(area);
        Paragraph::new(hints_line).render(layout[0], frame.buffer_mut());
        let mut status_spans: Vec<Span<'static>> = Vec::new();
        status_spans.push(format!("Memories: {total}").dim());
        if let Some(status) = status {
            status_spans.push("   ".into());
            let styled = match status.level {
                StatusLevel::Success => status.text.clone().green(),
                StatusLevel::Error => status.text.clone().red(),
            };
            status_spans.push(styled);
        }
        Paragraph::new(Line::from(status_spans)).render(layout[1], frame.buffer_mut());
    }

    fn draw_modal(&self, frame: &mut TerminalFrame, area: Rect, modal: &ModalState) {
        let horizontal_inset: u16 = if area.width > 6 { 2 } else { 0 };
        let vertical_inset: u16 = if area.height > 6 { 1 } else { 0 };
        let width = area
            .width
            .saturating_sub(horizontal_inset.saturating_mul(2))
            .max(40u16);
        let height = area
            .height
            .saturating_sub(vertical_inset.saturating_mul(2))
            .max(10u16);
        let popup_area = Rect::new(
            area.x + horizontal_inset,
            area.y + vertical_inset,
            width.min(area.width),
            height.min(area.height),
        );
        Clear.render(popup_area, frame.buffer_mut());
        match modal {
            ModalState::Form(form) => self.draw_form_modal(frame, popup_area, form),
            ModalState::Confirm(confirm) => self.draw_confirm_modal(frame, popup_area, confirm),
            ModalState::ConfirmUnsaved(prompt) => {
                self.draw_unsaved_prompt(frame, popup_area, prompt)
            }
        }
    }

    fn draw_form_modal(&self, frame: &mut TerminalFrame, area: Rect, form: &MemoryFormState) {
        let title = match form.kind {
            ModalKind::Create => "Create Memory",
            ModalKind::Edit => "Edit Memory",
            _ => "Memory",
        };
        let title_line = Line::from(format!(
            "{} · CF {:>3}%",
            title,
            (form.confidence * 100.0).round() as i32
        ))
        .cyan()
        .bold();
        let block = Block::default().borders(Borders::ALL).title(title_line);
        let inner = block.inner(area);
        block.render(area, frame.buffer_mut());
        let mut y = inner.y;
        let mut remaining = inner.height;

        let summary_header_area = Rect::new(inner.x, y, inner.width, 1);
        y = y.saturating_add(1);
        remaining = remaining.saturating_sub(1);

        let summary_height = remaining.saturating_sub(3).max(3);
        let summary_area = Rect::new(inner.x, y, inner.width, summary_height);
        y = y.saturating_add(summary_height);
        remaining = remaining.saturating_sub(summary_height);

        let tags_header_area = Rect::new(inner.x, y, inner.width, 1);
        y = y.saturating_add(1);
        remaining = remaining.saturating_sub(1);

        let tags_area = Rect::new(inner.x, y, inner.width, 1);
        y = y.saturating_add(1);
        remaining = remaining.saturating_sub(1);

        let footer_area = Rect::new(inner.x, y, inner.width, remaining.max(1));

        self.render_field_header(
            frame,
            summary_header_area,
            "Summary",
            matches!(form.active_field, FormField::Summary),
        );
        self.render_summary_field(frame, summary_area, form);
        self.render_field_header(
            frame,
            tags_header_area,
            "Tags (comma separated)",
            matches!(form.active_field, FormField::Tags),
        );
        self.render_single_line_field(
            frame,
            tags_area,
            &form.tags,
            matches!(form.active_field, FormField::Tags),
            form.tags_cursor,
        );
        let mut footer = vec![
            "[Esc] done".into(),
            "   ".into(),
            "[Tab] switch field".into(),
            "   ".into(),
            "[Shift+Enter] newline".into(),
        ];
        if let Some(error) = &form.error {
            footer.push("   ".into());
            footer.push(error.clone().red());
        }
        Paragraph::new(Line::from(footer))
            .wrap(ratatui::widgets::Wrap { trim: true })
            .render(footer_area, frame.buffer_mut());
    }

    fn render_field_header(
        &self,
        frame: &mut TerminalFrame,
        area: Rect,
        label: &str,
        active: bool,
    ) {
        let mut spans = vec![label.cyan().bold()];
        if active {
            spans.push("  (editing)".dim());
        }
        Paragraph::new(Line::from(spans)).render(area, frame.buffer_mut());
    }

    fn render_summary_field(&self, frame: &mut TerminalFrame, area: Rect, form: &MemoryFormState) {
        let height = area.height.max(1) as usize;
        let mut lines = form.summary_lines();
        if lines.is_empty() {
            lines.push("");
        }
        let total_lines = lines.len();
        let cursor_line = form.summary_cursor.line.min(total_lines.saturating_sub(1));
        let start = (cursor_line + 1).saturating_sub(height);
        let end = (start + height).min(total_lines);
        let active = matches!(form.active_field, FormField::Summary);
        let mut rendered: Vec<Line<'static>> = Vec::new();
        for idx in start..end {
            let text = lines.get(idx).copied().unwrap_or("");
            if active && idx == cursor_line {
                let caret_col = form.summary_cursor.column.min(text.chars().count());
                let split_idx = byte_index_for_column(text, caret_col);
                let (before, after) = text.split_at(split_idx);
                let spans: Vec<Span<'static>> = vec![
                    before.to_string().into(),
                    "▌".cyan().bold(),
                    after.to_string().into(),
                ];
                rendered.push(Line::from(spans));
            } else {
                rendered.push(Line::from(text.to_string()));
            }
        }
        if rendered.is_empty() {
            if active {
                rendered.push(Line::from("▌"));
            } else {
                rendered.push(Line::from(""));
            }
        }
        Paragraph::new(rendered)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .render(area, frame.buffer_mut());
    }

    fn render_single_line_field(
        &self,
        frame: &mut TerminalFrame,
        area: Rect,
        value: &str,
        active: bool,
        cursor: usize,
    ) {
        let spans: Vec<Span<'static>> = if active {
            let caret_col = cursor.min(value.chars().count());
            let split_idx = byte_index_for_column(value, caret_col);
            let (before, after) = value.split_at(split_idx);
            vec![
                before.to_string().into(),
                "▌".cyan().bold(),
                after.to_string().into(),
            ]
        } else {
            vec![value.to_string().into()]
        };
        Paragraph::new(Line::from(spans)).render(area, frame.buffer_mut());
    }

    fn draw_confirm_modal(&self, frame: &mut TerminalFrame, area: Rect, confirm: &ConfirmState) {
        let title = match confirm.kind {
            ModalKind::ConfirmDelete => "Confirm Delete",
            ModalKind::ConfirmReset => "Confirm Reset",
            ModalKind::ConfirmRebuild => "Confirm Rebuild",
            _ => "Confirm",
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title.cyan().bold());
        let inner = block.inner(area);
        block.render(area, frame.buffer_mut());
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(2), Constraint::Length(1)])
            .split(inner);
        Paragraph::new(Line::from(vec![confirm.message.clone().into()]))
            .wrap(ratatui::widgets::Wrap { trim: true })
            .render(layout[0], frame.buffer_mut());
        let footer = Line::from(vec![
            "[Enter] confirm".bold(),
            "   ".into(),
            "[Esc] cancel".into(),
        ]);
        Paragraph::new(footer).render(layout[1], frame.buffer_mut());
    }

    fn draw_unsaved_prompt(
        &self,
        frame: &mut TerminalFrame,
        area: Rect,
        _prompt: &UnsavedPromptState,
    ) {
        let width = area.width.clamp(30, 60);
        let height = 5;
        let x = area.x + area.width.saturating_sub(width) / 2;
        let y = area.y + area.height.saturating_sub(height) / 2;
        let popup_area = Rect::new(x, y, width, height);
        Clear.render(popup_area, frame.buffer_mut());
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Save Changes?".cyan().bold());
        let inner = block.inner(popup_area);
        block.render(popup_area, frame.buffer_mut());
        let lines = vec![
            Line::from("Save changes to this memory?"),
            Line::from(vec![
                "[Y] Yes".into(),
                "   ".into(),
                "[N] No".into(),
                "   ".into(),
                "[Esc] continue editing".dim(),
            ]),
        ];
        Paragraph::new(lines)
            .wrap(ratatui::widgets::Wrap { trim: true })
            .render(inner, frame.buffer_mut());
    }

    fn expire_status(&mut self) {
        if let Some(status) = &self.status
            && let Some(expiry) = status.expires_at
            && Instant::now() >= expiry
        {
            self.status = None;
        }
    }

    async fn handle_modal_key(&mut self, modal: ModalState, key: KeyEvent) -> Result<()> {
        match modal {
            ModalState::Form(mut form) => match form.handle_key(key) {
                FormOutcome::Continue => {
                    self.modal = Some(ModalState::Form(form));
                }
                FormOutcome::Cancel => {
                    self.modal = None;
                }
                FormOutcome::PromptSave => {
                    if let Err(err) = form.validate() {
                        self.status = Some(StatusLine::error(err.to_string()));
                        self.modal = Some(ModalState::Form(form));
                    } else {
                        self.modal = Some(ModalState::ConfirmUnsaved(UnsavedPromptState { form }));
                    }
                }
            },
            ModalState::Confirm(confirm) => match key.code {
                KeyCode::Enter => {
                    self.handle_confirmation(confirm).await?;
                    self.modal = None;
                }
                KeyCode::Esc => {
                    self.modal = None;
                }
                _ => {
                    self.modal = Some(ModalState::Confirm(confirm));
                }
            },
            ModalState::ConfirmUnsaved(prompt) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let mut form = prompt.form;
                    if let Err(err) = form.validate() {
                        self.status = Some(StatusLine::error(err.to_string()));
                        self.modal = Some(ModalState::Form(form));
                    } else {
                        self.submit_form(form).await?;
                        self.modal = None;
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.modal = None;
                }
                KeyCode::Esc => {
                    self.modal = Some(ModalState::Form(prompt.form));
                }
                _ => {
                    self.modal = Some(ModalState::ConfirmUnsaved(prompt));
                }
            },
        }
        Ok(())
    }

    async fn submit_form(&mut self, form: MemoryFormState) -> Result<()> {
        match form.kind {
            ModalKind::Create => self.create_memory(form).await,
            ModalKind::Edit => self.update_memory(form).await,
            _ => Ok(()),
        }
    }

    async fn create_memory(&mut self, form: MemoryFormState) -> Result<()> {
        let summary = clean_summary(&form.summary);
        let tags = form.parsed_tags();
        let mut metadata = form.metadata.clone();
        metadata.tags = tags;
        self.runtime
            .create_record(summary, metadata, form.confidence, form.source.clone())
            .await
            .map_err(|err| eyre!("failed to create memory: {err:#}"))?;
        self.refresh().await?;
        self.status = Some(StatusLine::success("Memory created"));
        Ok(())
    }

    async fn update_memory(&mut self, form: MemoryFormState) -> Result<()> {
        let record_id = form
            .record_id
            .ok_or_else(|| eyre!("missing record ID for edit"))?;
        let summary = clean_summary(&form.summary);
        let tags = form.parsed_tags();
        let mut metadata = form.metadata.clone();
        metadata.tags = tags;
        let update = MemoryRecordUpdate {
            summary: Some(summary.clone()),
            metadata: Some(metadata),
            source: Some(form.source.clone()),
            ..MemoryRecordUpdate::default()
        };
        self.runtime
            .update_record(record_id, update)
            .await
            .map_err(|err| eyre!("failed to update memory record: {err:#}"))?;
        self.refresh().await?;
        self.status = Some(StatusLine::success("Memory updated"));
        Ok(())
    }

    async fn handle_confirmation(&mut self, confirm: ConfirmState) -> Result<()> {
        match confirm.kind {
            ModalKind::ConfirmDelete => {
                if let Some(id) = confirm.target {
                    self.runtime
                        .delete_record(id)
                        .await
                        .map_err(|err| eyre!("failed to delete memory: {err:#}"))?
                        .ok_or_else(|| eyre!("memory not found"))?;
                    self.refresh().await?;
                    self.status = Some(StatusLine::success("Memory deleted"));
                }
            }
            ModalKind::ConfirmReset => {
                let mut store = self.runtime.store.lock().await;
                store
                    .reset()
                    .map_err(|err| eyre!("failed to reset memory store: {err:#}"))?;
                drop(store);
                self.refresh().await?;
                self.status = Some(StatusLine::success("Memory store reset"));
            }
            ModalKind::ConfirmRebuild => {
                let mut store = self.runtime.store.lock().await;
                store
                    .rebuild()
                    .map_err(|err| eyre!("failed to rebuild memory index: {err:#}"))?;
                drop(store);
                self.refresh().await?;
                self.status = Some(StatusLine::success("Memory index rebuilt"));
            }
            _ => {}
        }
        Ok(())
    }
}

fn format_time(ts: DateTime<chrono::Utc>) -> String {
    let local: DateTime<Local> = ts.into();
    local.format("%Y-%m-%d %H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::custom_terminal::Terminal;
    use crate::test_backend::VT100Backend;
    use chrono::Duration;
    use chrono::TimeZone;
    use chrono::Utc;
    use codex_core::memory::MemorySettingsManager;
    use codex_core::memory::MiniCpmManager;
    use fastembed::TextEmbedding;
    use insta::assert_snapshot;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::Mutex;

    async fn build_runtime(root: &TempDir) -> MemoryRuntime {
        let store = Arc::new(Mutex::new(
            GlobalMemoryStore::open(root.path().to_path_buf())
                .await
                .expect("open store"),
        ));
        let settings = Arc::new(
            MemorySettingsManager::load(root.path().to_path_buf())
                .await
                .expect("load settings"),
        );
        let model = Arc::new(
            MiniCpmManager::load(root.path().to_path_buf())
                .await
                .expect("load model"),
        );
        let embedder = Arc::new(Mutex::new(
            TextEmbedding::try_new(Default::default()).expect("init embedder"),
        ));
        MemoryRuntime {
            store,
            settings,
            model,
            embedder,
        }
    }

    #[test]
    fn collapse_records_merges_duplicates() {
        let mut first = MemoryRecord::new(
            "Greeted the assistant with hi".to_string(),
            vec![0.1, 0.2],
            MemoryMetadata {
                conversation_id: Some("thread-1".to_string()),
                ..MemoryMetadata::default()
            },
            0.75,
            MemorySource::UserMessage,
        );
        let now = Utc.with_ymd_and_hms(2025, 1, 10, 9, 0, 0).unwrap();
        first.created_at = now;
        first.updated_at = now;

        let mut duplicate = first.clone();
        duplicate.record_id = Uuid::now_v7();
        duplicate.updated_at = now + Duration::seconds(30);

        let mut other = MemoryRecord::new(
            "Captured a follow-up question".to_string(),
            vec![0.3, 0.4],
            MemoryMetadata::default(),
            0.8,
            MemorySource::AssistantMessage,
        );
        other.created_at = now + Duration::seconds(60);
        other.updated_at = other.created_at;

        let rows = MemoryManagerState::collapse_records(vec![first.clone(), duplicate, other]);
        assert_eq!(rows.len(), 2);
        let primary = &rows[0];
        assert_eq!(
            primary.record.summary, "Captured a follow-up question",
            "Most recent record should stay first in the list"
        );
        let greeting = &rows[1];
        assert_eq!(greeting.duplicate_count, 2);
        assert_eq!(
            clean_summary(&greeting.record.summary),
            clean_summary(&first.summary)
        );
    }

    async fn seed_records(runtime: &MemoryRuntime) -> MemoryRecord {
        let mut store = runtime.store.lock().await;
        let ts_primary = Utc.with_ymd_and_hms(2025, 1, 1, 12, 0, 0).unwrap();
        let ts_secondary = Utc.with_ymd_and_hms(2025, 1, 2, 8, 30, 0).unwrap();

        let mut record_a = MemoryRecord::new(
            "Review onboarding flow for regression notes".to_string(),
            vec![0.1, 0.2],
            MemoryMetadata {
                tags: vec!["product".into(), "onboarding".into()],
                ..MemoryMetadata::default()
            },
            0.82,
            MemorySource::UserMessage,
        );
        record_a.created_at = ts_primary;
        record_a.updated_at = ts_primary;

        let mut record_b = MemoryRecord::new(
            "Summarise CLI install steps for offline docs".to_string(),
            vec![0.3, 0.7],
            MemoryMetadata {
                tags: vec!["cli".into(), "docs".into()],
                ..MemoryMetadata::default()
            },
            0.91,
            MemorySource::AssistantMessage,
        );
        record_b.created_at = ts_secondary;
        record_b.updated_at = ts_secondary;

        store.append(record_a).expect("append primary record");
        store
            .append(record_b.clone())
            .expect("append secondary record");
        store.record_hit().expect("increment hits");
        store.record_miss().expect("increment misses");
        drop(store);
        record_b
    }

    #[tokio::test]
    async fn renders_memory_manager_list() {
        let temp = TempDir::new().expect("temp dir");
        let runtime = build_runtime(&temp).await;
        seed_records(&runtime).await;
        let mut state = MemoryManagerState::load(runtime).await.expect("load state");

        let backend = VT100Backend::new(120, 30);
        let mut terminal = Terminal::with_options(backend).expect("terminal");
        terminal.set_viewport_area(ratatui::layout::Rect::new(0, 0, 120, 30));
        let rows_clone = state.rows.clone();
        let filtered = state.filtered.clone();
        let cursor = state.cursor;
        let query = state.query.clone();
        let editing_query = state.editing_query;
        let min_conf = state.min_confidence;
        let preview_mode = state.preview_mode;
        let hits = state.metrics.hits;
        let misses = state.metrics.misses;
        let stats = state.stats.clone();
        let status = state.status.clone();
        let manual_selected = state.manual_selected.clone();
        let manual_deselected = state.manual_deselected.clone();
        let modal = state.modal.clone();
        let semantic = state.semantic_hits.clone();
        terminal
            .draw(|frame| {
                let size = frame.area();
                let layout = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(4),
                        Constraint::Min(10),
                        Constraint::Length(2),
                    ])
                    .split(size);
                state.draw_header(
                    frame,
                    layout[0],
                    &query,
                    editing_query,
                    min_conf,
                    preview_mode,
                    hits,
                    misses,
                );
                state.draw_table(
                    frame,
                    layout[1],
                    &rows_clone,
                    &filtered,
                    cursor,
                    &manual_selected,
                    &manual_deselected,
                    semantic.as_ref(),
                );
                state.draw_footer(frame, layout[2], status.as_ref(), stats.total_records);
                if let Some(modal) = &modal {
                    state.draw_modal(frame, size, modal);
                }
            })
            .expect("draw manager");
        let screen = terminal.backend().vt100().screen().contents();
        assert_snapshot!("memory_manager_list", screen);
    }

    #[tokio::test]
    async fn renders_memory_manager_edit_modal() {
        let temp = TempDir::new().expect("temp dir");
        let runtime = build_runtime(&temp).await;
        let record = seed_records(&runtime).await;
        let mut state = MemoryManagerState::load(runtime).await.expect("load state");
        state.modal = Some(ModalState::Form(MemoryFormState::for_edit(&record)));

        let backend = VT100Backend::new(120, 30);
        let mut terminal = Terminal::with_options(backend).expect("terminal");
        terminal.set_viewport_area(ratatui::layout::Rect::new(0, 0, 120, 30));
        let rows_clone = state.rows.clone();
        let filtered = state.filtered.clone();
        let cursor = state.cursor;
        let query = state.query.clone();
        let editing_query = state.editing_query;
        let min_conf = state.min_confidence;
        let preview_mode = state.preview_mode;
        let hits = state.metrics.hits;
        let misses = state.metrics.misses;
        let stats = state.stats.clone();
        let status = state.status.clone();
        let manual_selected = state.manual_selected.clone();
        let manual_deselected = state.manual_deselected.clone();
        let modal = state.modal.clone();
        let semantic = state.semantic_hits.clone();
        terminal
            .draw(|frame| {
                let size = frame.area();
                let layout = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(4),
                        Constraint::Min(10),
                        Constraint::Length(2),
                    ])
                    .split(size);
                state.draw_header(
                    frame,
                    layout[0],
                    &query,
                    editing_query,
                    min_conf,
                    preview_mode,
                    hits,
                    misses,
                );
                state.draw_table(
                    frame,
                    layout[1],
                    &rows_clone,
                    &filtered,
                    cursor,
                    &manual_selected,
                    &manual_deselected,
                    semantic.as_ref(),
                );
                state.draw_footer(frame, layout[2], status.as_ref(), stats.total_records);
                if let Some(modal) = &modal {
                    state.draw_modal(frame, size, modal);
                }
            })
            .expect("draw manager");
        let screen = terminal.backend().vt100().screen().contents();
        assert_snapshot!("memory_manager_edit_modal", screen);
    }

    #[tokio::test]
    async fn renders_memory_manager_confirm_modal() {
        let temp = TempDir::new().expect("temp dir");
        let runtime = build_runtime(&temp).await;
        let record = seed_records(&runtime).await;
        let mut state = MemoryManagerState::load(runtime).await.expect("load state");
        state.modal = Some(ModalState::Confirm(ConfirmState {
            kind: ModalKind::ConfirmDelete,
            target: Some(record.record_id),
            message: "Delete memory “Summarise CLI install steps for offline docs”?".to_string(),
        }));

        let backend = VT100Backend::new(120, 30);
        let mut terminal = Terminal::with_options(backend).expect("terminal");
        terminal.set_viewport_area(ratatui::layout::Rect::new(0, 0, 120, 30));
        let rows_clone = state.rows.clone();
        let filtered = state.filtered.clone();
        let cursor = state.cursor;
        let query = state.query.clone();
        let editing_query = state.editing_query;
        let min_conf = state.min_confidence;
        let preview_mode = state.preview_mode;
        let hits = state.metrics.hits;
        let misses = state.metrics.misses;
        let stats = state.stats.clone();
        let status = state.status.clone();
        let manual_selected = state.manual_selected.clone();
        let manual_deselected = state.manual_deselected.clone();
        let modal = state.modal.clone();
        let semantic = state.semantic_hits.clone();
        terminal
            .draw(|frame| {
                let size = frame.area();
                let layout = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(4),
                        Constraint::Min(10),
                        Constraint::Length(2),
                    ])
                    .split(size);
                state.draw_header(
                    frame,
                    layout[0],
                    &query,
                    editing_query,
                    min_conf,
                    preview_mode,
                    hits,
                    misses,
                );
                state.draw_table(
                    frame,
                    layout[1],
                    &rows_clone,
                    &filtered,
                    cursor,
                    &manual_selected,
                    &manual_deselected,
                    semantic.as_ref(),
                );
                state.draw_footer(frame, layout[2], status.as_ref(), stats.total_records);
                if let Some(modal) = &modal {
                    state.draw_modal(frame, size, modal);
                }
            })
            .expect("draw manager");
        let screen = terminal.backend().vt100().screen().contents();
        assert_snapshot!("memory_manager_confirm_delete", screen);
    }
}
