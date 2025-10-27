use std::collections::HashSet;
use std::io::Stdout;
use std::io::{self};
use std::time::Duration;

use anyhow::Result;
use chrono::DateTime;
use chrono::Local;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use crossterm::event::{self};
use crossterm::execute;
use crossterm::terminal;
use crossterm::terminal::disable_raw_mode;
use crossterm::terminal::enable_raw_mode;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::prelude::*;
use ratatui::style::Stylize;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Cell;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Row;
use ratatui::widgets::Table;
use ratatui::widgets::Wrap;

fn main() -> Result<()> {
    let mut stdout = io::stdout();
    init_terminal(&mut stdout)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut app = MemoryManagerState::sample();

    loop {
        terminal.draw(|frame| app.draw(frame))?;
        if !app.handle_event()? {
            break;
        }
    }

    terminal.show_cursor()?;
    restore_terminal()?;
    Ok(())
}

fn init_terminal(stdout: &mut Stdout) -> Result<()> {
    enable_raw_mode()?;
    execute!(
        stdout,
        terminal::EnterAlternateScreen,
        event::EnableMouseCapture
    )?;
    Ok(())
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        terminal::LeaveAlternateScreen,
        event::DisableMouseCapture
    )?;
    Ok(())
}

struct MemoryManagerState {
    rows: Vec<MemoryRow>,
    query: String,
    user_select_mode: bool,
    editing_query: bool,
    min_confidence: f32,
    hits: usize,
    misses: usize,
    cursor: usize,
    manual_selected: HashSet<usize>,
    manual_deselected: HashSet<usize>,
    show_help: bool,
}

impl MemoryManagerState {
    fn sample() -> Self {
        Self {
            rows: vec![
                MemoryRow::new(
                    Local::now(),
                    vec!["project:codex".to_string(), "persona:dev".to_string()],
                    "Discussed LightMem-style global memory, including JSONL manifest and HNSW graph plans.",
                    0.87,
                ),
                MemoryRow::new(
                    Local::now() - chrono::Duration::minutes(14),
                    vec!["tooling:index".to_string()],
                    "Captured requirement to expose /memory manager with preview toggle and min CF% slider.",
                    0.91,
                ),
                MemoryRow::new(
                    Local::now() - chrono::Duration::hours(1),
                    vec!["planning".to_string()],
                    "Agreed to log retrieval hits vs. misses for debugging future analytics workflows.",
                    0.79,
                ),
                MemoryRow::new(
                    Local::now() - chrono::Duration::hours(5),
                    vec!["archive".to_string()],
                    "Legacy memory below threshold to illustrate CF filtering behaviour.",
                    0.62,
                ),
            ],
            query: String::new(),
            user_select_mode: true,
            editing_query: false,
            min_confidence: 0.75,
            hits: 12,
            misses: 3,
            cursor: 0,
            manual_selected: HashSet::new(),
            manual_deselected: HashSet::new(),
            show_help: false,
        }
    }

    fn draw(&self, frame: &mut Frame) {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(3),
            ])
            .split(frame.area());

        self.draw_query(frame, layout[0]);
        self.draw_table(frame, layout[1]);
        self.draw_footer(frame, layout[2]);

        if self.show_help {
            self.draw_help(frame);
        }
    }

    fn draw_query(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Memory Manager".cyan().bold());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(inner);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
            .split(rows[0]);

        let mut query_display = if self.query.is_empty() {
            "type to filter…".dim().to_string()
        } else {
            self.query.clone()
        };
        if self.editing_query {
            query_display.push('▌');
        }

        let query_line = Line::from(vec![
            "[S]earch: ".cyan().bold().into(),
            query_display.into(),
        ]);
        let query_paragraph = Paragraph::new(query_line);
        frame.render_widget(query_paragraph, columns[0]);

        let mode_str = if self.user_select_mode {
            "user"
        } else {
            "auto"
        };
        let mode_line = Line::from(vec![
            format!("Select [m]ode: {mode_str}").cyan().bold().into(),
        ]);
        let mode_paragraph = Paragraph::new(mode_line).alignment(Alignment::Right);
        frame.render_widget(mode_paragraph, columns[1]);

        let hint_line = Line::from(vec!["(semantic search • press s to edit)".dim().into()]);
        frame.render_widget(Paragraph::new(hint_line), rows[1]);
    }

    fn draw_table(&self, frame: &mut Frame, area: Rect) {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            let block = Block::default()
                .borders(Borders::ALL)
                .title("Recent Memories".cyan().bold());
            let paragraph = Paragraph::new("No memories match the current filters.")
                .block(block)
                .wrap(Wrap { trim: true });
            frame.render_widget(paragraph, area);
            return;
        }

        let header = Row::new(vec![
            Cell::from("Pick".cyan().bold()),
            Cell::from("Time".cyan().bold()),
            Cell::from("Tags".cyan().bold()),
            Cell::from("Summary".cyan().bold()),
            Cell::from("CF%".cyan().bold()),
        ]);

        let highlight = filtered
            .get(self.cursor.min(filtered.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0);

        let rows = filtered.into_iter().map(|index| {
            let row = &self.rows[index];
            let should_select = self.is_selected(index);
            let marker = if should_select { "●" } else { "○" };
            let mut data_row = Row::new(vec![
                Cell::from(marker.cyan()),
                Cell::from(row.time.clone()),
                Cell::from(row.tags_display()),
                Cell::from(row.summary_preview()),
                Cell::from(format!("{:.0}", row.confidence * 100.0)),
            ]);
            if index == highlight {
                data_row = data_row.style(Style::default().add_modifier(Modifier::REVERSED));
            }
            data_row
        });

        let widths = [
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(24),
            Constraint::Percentage(60),
            Constraint::Length(6),
        ];

        let table = Table::new(rows, widths).header(header).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Recent Memories".cyan().bold()),
        );

        frame.render_widget(table, area);
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let footer_line = Line::from(vec![
            "Min CF%: ".into(),
            format!("{:.0}", self.min_confidence * 100.0).bold().into(),
            " ".into(),
            "[+/-]".dim().into(),
            "  Hits: ".into(),
            self.hits.to_string().green().into(),
            "  Misses: ".into(),
            self.misses.to_string().red().into(),
            "  [↑/↓] move  [Space] pick  [h] help  [esc] quit"
                .dim()
                .into(),
        ]);
        let paragraph = Paragraph::new(footer_line)
            .block(Block::default().borders(Borders::ALL))
            .wrap(Wrap { trim: true });
        frame.render_widget(paragraph, area);
    }

    fn draw_help(&self, frame: &mut Frame) {
        let area = centered_rect(60, 40, frame.area());
        let block = Block::default()
            .title("Key Bindings".cyan().bold())
            .borders(Borders::ALL);
        let lines = vec![
            Line::from("↑/↓ Navigate memories"),
            Line::from("Space/Enter Toggle selection"),
            Line::from("m Toggle memory select mode"),
            Line::from("s Start semantic search"),
            Line::from("+ Raise min confidence (+5%)"),
            Line::from("- Lower min confidence (-5%)"),
            Line::from("Ctrl+R Rotate sample data"),
            Line::from("Esc Close manager / cancel input"),
            Line::from("h Toggle this help overlay"),
        ];

        let paragraph = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
        frame.render_widget(Clear, area);
        frame.render_widget(paragraph, area);
    }

    fn handle_event(&mut self) -> Result<bool> {
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind == event::KeyEventKind::Press {
                        if self.editing_query {
                            self.handle_query_input(key);
                        } else {
                            match key.code {
                                KeyCode::Esc => return Ok(false),
                                KeyCode::Char('h') => self.show_help = !self.show_help,
                                KeyCode::Char('m') => {
                                    self.user_select_mode = !self.user_select_mode
                                }
                                KeyCode::Char('s') => {
                                    self.editing_query = true;
                                    self.query.clear();
                                }
                                KeyCode::Up => self.move_cursor(-1),
                                KeyCode::Down => self.move_cursor(1),
                                KeyCode::Char(' ') | KeyCode::Enter => {
                                    if let Some(index) = self.current_row_index() {
                                        self.toggle_selection(index);
                                    }
                                }
                                KeyCode::Char('+') => {
                                    self.min_confidence = (self.min_confidence + 0.05).min(0.95);
                                    self.normalize_cursor();
                                }
                                KeyCode::Char('-') => {
                                    self.min_confidence = (self.min_confidence - 0.05).max(0.0);
                                    self.normalize_cursor();
                                }
                                KeyCode::Char('r')
                                    if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                {
                                    self.rows.rotate_left(1);
                                    self.normalize_cursor();
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Event::Mouse(_) | Event::Resize(_, _) | Event::FocusGained | Event::FocusLost => {}
                Event::Paste(_) => {}
            }
        }
        Ok(true)
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let needle = self.query.to_lowercase();
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.confidence >= self.min_confidence)
            .filter(|(_, row)| needle.is_empty() || row.matches_query(&needle))
            .map(|(index, _)| index)
            .collect()
    }

    fn current_row_index(&self) -> Option<usize> {
        let filtered = self.filtered_indices();
        filtered.get(self.cursor).copied()
    }

    fn move_cursor(&mut self, delta: isize) {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            self.cursor = 0;
            return;
        }
        let len = filtered.len();
        let mut position = self.cursor.min(len.saturating_sub(1));
        if delta < 0 && position > 0 {
            position -= 1;
        } else if delta > 0 && position + 1 < len {
            position += 1;
        }
        self.cursor = position;
    }

    fn normalize_cursor(&mut self) {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            self.cursor = 0;
        } else if self.cursor >= filtered.len() {
            self.cursor = filtered.len().saturating_sub(1);
        }
    }

    fn handle_query_input(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.editing_query = false,
            KeyCode::Enter => self.editing_query = false,
            KeyCode::Backspace => {
                self.query.pop();
            }
            KeyCode::Delete => {
                self.query.clear();
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.push(ch);
            }
            _ => {}
        }
        self.normalize_cursor();
    }

    fn is_selected(&self, index: usize) -> bool {
        if self.manual_deselected.contains(&index) {
            return false;
        }
        if self.manual_selected.contains(&index) {
            return true;
        }
        self.rows
            .get(index)
            .map(|row| row.confidence >= self.min_confidence)
            .unwrap_or(false)
    }

    fn toggle_selection(&mut self, index: usize) {
        if self.is_selected(index) {
            self.manual_selected.remove(&index);
            self.manual_deselected.insert(index);
        } else {
            self.manual_deselected.remove(&index);
            self.manual_selected.insert(index);
        }
    }
}

struct MemoryRow {
    time: String,
    tags: Vec<String>,
    summary: String,
    confidence: f32,
}

impl MemoryRow {
    fn new(timestamp: DateTime<Local>, tags: Vec<String>, summary: &str, confidence: f32) -> Self {
        Self {
            time: timestamp.format("%H:%M:%S").to_string(),
            tags,
            summary: summary.to_string(),
            confidence,
        }
    }

    fn tags_display(&self) -> Line<'_> {
        let spans: Vec<Span> = self
            .tags
            .iter()
            .map(|tag| format!("{tag} ").dim())
            .collect();
        Line::from(spans)
    }

    fn summary_preview(&self) -> String {
        let preview: String = self.summary.chars().take(80).collect();
        if preview.len() < self.summary.len() {
            format!("{preview}…")
        } else {
            preview
        }
    }

    fn matches_query(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        let mut haystack = self.summary.to_lowercase();
        haystack.push(' ');
        haystack.push_str(&self.tags.join(" ").to_lowercase());
        haystack.contains(needle)
    }
}

fn centered_rect(width_pct: u16, height_pct: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_pct) / 2),
            Constraint::Percentage(height_pct),
            Constraint::Percentage((100 - height_pct) / 2),
        ])
        .split(area);

    let vertical = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_pct) / 2),
            Constraint::Percentage(width_pct),
            Constraint::Percentage((100 - width_pct) / 2),
        ])
        .split(popup_layout[1]);

    vertical[1]
}
