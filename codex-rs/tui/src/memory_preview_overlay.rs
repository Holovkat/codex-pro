use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::tui;
use crate::tui::TuiEvent;
use codex_core::protocol::MemoryPreviewEntry;
use codex_core::protocol::MemoryPreviewEvent;
use codex_core::protocol::MemoryPreviewMode;
use codex_core::protocol::Op;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

#[derive(Clone)]
struct MemoryPreviewRow {
    record_id: String,
    summary: String,
    confidence: f32,
    score: f32,
    selected: bool,
}

pub(crate) struct MemoryPreviewOverlay {
    rows: Vec<MemoryPreviewRow>,
    cursor: usize,
    min_confidence: f32,
    preview_mode: MemoryPreviewMode,
    app_event_tx: AppEventSender,
    is_done: bool,
}

impl MemoryPreviewOverlay {
    pub(crate) fn new(event: MemoryPreviewEvent, app_event_tx: AppEventSender) -> Self {
        let rows = event
            .entries
            .into_iter()
            .map(MemoryPreviewRow::from_entry)
            .collect();
        Self {
            rows,
            cursor: 0,
            min_confidence: event.min_confidence,
            preview_mode: event.preview_mode,
            app_event_tx,
            is_done: false,
        }
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        Clear.render(area, buf);
        let mut lines: Vec<Line<'static>> = Vec::new();
        let header = format!(
            "Select memories (CF ≥ {:>3}%) – preview mode: {}",
            (self.min_confidence * 100.0).round() as i32,
            match self.preview_mode {
                MemoryPreviewMode::Enabled => "manual",
                MemoryPreviewMode::Disabled => "auto",
            }
        );
        lines.push(Line::from(header).bold());
        lines.push(Line::from("".to_string()));
        if self.rows.is_empty() {
            lines.push(Line::from("No matching memories found.").dim());
        } else {
            for (idx, row) in self.rows.iter_mut().enumerate() {
                let marker = if row.selected { "[x]" } else { "[ ]" };
                let text = format!(
                    "{} {:>3}% · score {:>5.2}  {}",
                    marker,
                    (row.confidence * 100.0).round() as i32,
                    row.score,
                    row.summary
                );
                let mut line = Line::from(text);
                if idx == self.cursor {
                    line = line.style(Modifier::REVERSED);
                }
                lines.push(line);
            }
        }
        lines.push(Line::from("".to_string()));
        lines.push(self.instructions_line());

        Paragraph::new(lines).render(area, buf);
    }

    fn instructions_line(&self) -> Line<'static> {
        Line::from(vec![
            Span::from("Enter").bold(),
            " accept  ".into(),
            Span::from("Space").bold(),
            " toggle  ".into(),
            Span::from("A").bold(),
            " select all  ".into(),
            Span::from("Esc").bold(),
            " skip  ".into(),
            Span::from("M").bold(),
            " manager".into(),
        ])
        .dim()
    }

    pub(crate) fn handle_event(&mut self, tui: &mut tui::Tui, event: TuiEvent) {
        match event {
            TuiEvent::Key(key) => self.handle_key_event(tui, key),
            TuiEvent::Draw => {
                let _ = tui.draw(u16::MAX, |frame| {
                    self.render(frame.area(), frame.buffer);
                });
            }
            _ => {}
        }
    }

    fn handle_key_event(&mut self, tui: &mut tui::Tui, key: KeyEvent) {
        if self.rows.is_empty() {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.send_decision(Vec::new());
                }
                _ => {}
            }
            return;
        }
        match key {
            KeyEvent {
                code: KeyCode::Up,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            } => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                }
            }
            KeyEvent {
                code: KeyCode::Down,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            } => {
                if self.cursor + 1 < self.rows.len() {
                    self.cursor += 1;
                }
            }
            KeyEvent {
                code: KeyCode::Char(' '),
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            } => {
                if let Some(row) = self.rows.get_mut(self.cursor) {
                    row.selected = !row.selected;
                }
            }
            KeyEvent {
                code: KeyCode::Enter,
                kind: KeyEventKind::Press,
                ..
            } => {
                let accepted: Vec<String> = self
                    .rows
                    .iter()
                    .filter(|row| row.selected)
                    .map(|row| row.record_id.clone())
                    .collect();
                self.send_decision(accepted);
            }
            KeyEvent {
                code: KeyCode::Char('a'),
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let all_selected = self.rows.iter().all(|row| row.selected);
                for row in &mut self.rows {
                    row.selected = !all_selected;
                }
            }
            KeyEvent {
                code: KeyCode::Esc,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('q'),
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            } => {
                self.send_decision(Vec::new());
            }
            KeyEvent {
                code: KeyCode::Char('m'),
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            } => {
                self.app_event_tx.send(AppEvent::OpenMemoryManager);
            }
            _ => {}
        }
        tui.frame_requester().schedule_frame();
    }

    fn send_decision(&mut self, accepted_ids: Vec<String>) {
        if self.is_done {
            return;
        }
        self.app_event_tx
            .send(AppEvent::CodexOp(Op::MemoryPreviewDecision {
                accepted_ids,
            }));
        self.is_done = true;
    }

    pub(crate) fn is_done(&self) -> bool {
        self.is_done
    }
}

impl MemoryPreviewRow {
    fn from_entry(entry: MemoryPreviewEntry) -> Self {
        Self {
            record_id: entry.record_id,
            summary: entry.summary,
            confidence: entry.confidence,
            score: entry.score,
            selected: true,
        }
    }
}
