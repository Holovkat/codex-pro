use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::StatefulWidgetRef;
use ratatui::widgets::Widget;
use std::cell::RefCell;

use crate::key_hint;
use crate::render::renderable::Renderable;

use super::popup_consts::standard_popup_hint_line;

use super::CancellationEvent;
use super::bottom_pane_view::BottomPaneView;
use super::textarea::TextArea;
use super::textarea::TextAreaState;

/// Callback invoked when the user submits a custom prompt.
pub(crate) type PromptSubmitted = Box<dyn Fn(String) + Send + Sync>;
pub(crate) type PromptCancelled = Box<dyn Fn() + Send + Sync>;

/// Minimal multi-line text input view to collect custom review instructions.
pub(crate) struct CustomPromptView {
    title: String,
    placeholder: String,
    context_label: Option<String>,
    on_submit: PromptSubmitted,
    on_cancel: Option<PromptCancelled>,
    full_height: bool,

    // UI state
    textarea: TextArea,
    textarea_state: RefCell<TextAreaState>,
    complete: bool,
    allow_empty_submit: bool,
}

impl CustomPromptView {
    pub(crate) fn new(
        title: String,
        placeholder: String,
        context_label: Option<String>,
        on_submit: PromptSubmitted,
    ) -> Self {
        Self {
            title,
            placeholder,
            context_label,
            on_submit,
            on_cancel: None,
            full_height: false,
            textarea: TextArea::new(),
            textarea_state: RefCell::new(TextAreaState::default()),
            complete: false,
            allow_empty_submit: false,
        }
    }

    pub(crate) fn with_allow_empty_submit(mut self, allow: bool) -> Self {
        self.allow_empty_submit = allow;
        self
    }

    pub(crate) fn with_on_cancel(mut self, on_cancel: PromptCancelled) -> Self {
        self.on_cancel = Some(on_cancel);
        self
    }

    pub(crate) fn with_initial_text(mut self, value: String) -> Self {
        if !value.is_empty() {
            self.textarea.set_text(&value);
            let len = self.textarea.text().len();
            self.textarea.set_cursor(len);
            if let Ok(mut state) = self.textarea_state.try_borrow_mut() {
                *state = TextAreaState::default();
            }
        }
        self
    }

    pub(crate) fn with_full_height(mut self, full_height: bool) -> Self {
        self.full_height = full_height;
        self
    }
}

impl BottomPaneView for CustomPromptView {
    fn handle_key_event(&mut self, key_event: KeyEvent) {
        match key_event {
            KeyEvent {
                code: KeyCode::Esc, ..
            } => {
                self.on_ctrl_c();
            }
            KeyEvent {
                code: KeyCode::Char('j' | 'J'),
                modifiers,
                ..
            } if modifiers.contains(KeyModifiers::CONTROL) => {
                self.textarea.insert_str("\n");
            }
            KeyEvent {
                code: KeyCode::Enter,
                modifiers,
                ..
            } => {
                let should_submit = modifiers.is_empty()
                    || modifiers.contains(KeyModifiers::CONTROL)
                    || modifiers.contains(KeyModifiers::SUPER);
                if should_submit {
                    let text = self.textarea.text().trim().to_string();
                    if !text.is_empty() || self.allow_empty_submit {
                        (self.on_submit)(text);
                        self.complete = true;
                    }
                } else {
                    self.textarea.input(key_event);
                }
            }
            other => {
                self.textarea.input(other);
            }
        }
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.complete = true;
        if let Some(callback) = self.on_cancel.as_ref() {
            callback();
        }
        CancellationEvent::Handled
    }

    fn is_complete(&self) -> bool {
        self.complete
    }

    fn handle_paste(&mut self, pasted: String) -> bool {
        if pasted.is_empty() {
            return false;
        }
        self.textarea.insert_str(&pasted);
        true
    }
}

impl Renderable for CustomPromptView {
    fn desired_height(&self, width: u16) -> u16 {
        let extra_top: u16 = if self.context_label.is_some() { 1 } else { 0 };
        let input_height = if self.full_height {
            self.full_height_guess(width)
        } else {
            self.input_height(width)
        };
        1u16 + extra_top + input_height + 3u16
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let input_height = self.computed_input_height(area.width, area.height);

        // Title line
        let title_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        };
        let title_spans: Vec<Span<'static>> = vec![gutter(), self.title.clone().bold()];
        Paragraph::new(Line::from(title_spans)).render(title_area, buf);

        // Optional context line
        let mut input_y = area.y.saturating_add(1);
        if let Some(context_label) = &self.context_label {
            let context_area = Rect {
                x: area.x,
                y: input_y,
                width: area.width,
                height: 1,
            };
            let spans: Vec<Span<'static>> = vec![gutter(), context_label.clone().cyan()];
            Paragraph::new(Line::from(spans)).render(context_area, buf);
            input_y = input_y.saturating_add(1);
        }

        // Input line
        let input_area = Rect {
            x: area.x,
            y: input_y,
            width: area.width,
            height: input_height,
        };
        if input_area.width >= 2 {
            for row in 0..input_area.height {
                Paragraph::new(Line::from(vec![gutter()])).render(
                    Rect {
                        x: input_area.x,
                        y: input_area.y.saturating_add(row),
                        width: 2,
                        height: 1,
                    },
                    buf,
                );
            }

            let text_area_height = input_area.height.saturating_sub(1);
            if text_area_height > 0 {
                if input_area.width > 2 {
                    let blank_rect = Rect {
                        x: input_area.x.saturating_add(2),
                        y: input_area.y,
                        width: input_area.width.saturating_sub(2),
                        height: 1,
                    };
                    Clear.render(blank_rect, buf);
                }
                let textarea_rect = Rect {
                    x: input_area.x.saturating_add(2),
                    y: input_area.y.saturating_add(1),
                    width: input_area.width.saturating_sub(2),
                    height: text_area_height,
                };
                let mut state = self.textarea_state.borrow_mut();
                StatefulWidgetRef::render_ref(&(&self.textarea), textarea_rect, buf, &mut state);
                if self.textarea.text().is_empty() {
                    Paragraph::new(Line::from(self.placeholder.clone().dim()))
                        .render(textarea_rect, buf);
                }
            }
        }

        let hint_blank_y = input_area.y.saturating_add(input_height);
        if hint_blank_y < area.y.saturating_add(area.height) {
            let blank_area = Rect {
                x: area.x,
                y: hint_blank_y,
                width: area.width,
                height: 1,
            };
            Clear.render(blank_area, buf);
        }

        let hint_y = hint_blank_y.saturating_add(1);
        if hint_y < area.y.saturating_add(area.height) {
            let hint_line = if self.full_height {
                multiline_hint_line()
            } else {
                standard_popup_hint_line()
            };
            Paragraph::new(hint_line).render(
                Rect {
                    x: area.x,
                    y: hint_y,
                    width: area.width,
                    height: 1,
                },
                buf,
            );
        }
    }
}

impl CustomPromptView {
    fn input_height(&self, width: u16) -> u16 {
        let usable_width = width.saturating_sub(2);
        let text_height = self.textarea.desired_height(usable_width).clamp(1, 8);
        text_height.saturating_add(1).min(9)
    }

    fn computed_input_height(&self, width: u16, area_height: u16) -> u16 {
        if self.full_height {
            self.input_height_full(area_height)
        } else {
            self.input_height(width)
        }
    }

    fn input_height_full(&self, area_height: u16) -> u16 {
        let header = 1 + if self.context_label.is_some() { 1 } else { 0 };
        let hint_rows = 2;
        area_height
            .saturating_sub(header + hint_rows)
            .max(3)
            .min(area_height)
    }

    fn full_height_guess(&self, width: u16) -> u16 {
        let usable_width = width.saturating_sub(2);
        self.textarea
            .desired_height(usable_width)
            .saturating_add(12)
            .clamp(6, 60)
    }
}

fn gutter() -> Span<'static> {
    "▌ ".cyan()
}

fn multiline_hint_line() -> Line<'static> {
    Line::from(vec![
        key_hint::plain(KeyCode::Enter).into(),
        " saves · ".into(),
        key_hint::ctrl(KeyCode::Char('J')).into(),
        " adds newline · ".into(),
        key_hint::plain(KeyCode::Esc).into(),
        " cancels".into(),
    ])
}
