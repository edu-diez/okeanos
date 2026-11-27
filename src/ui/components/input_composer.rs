//! Input composer component — multi-line text area per tui-design.md §5.9.
//!
//! Multi-line editing, Enter to submit, Shift+Enter for newline,
//! Ctrl+C to cancel in-flight turn, Ctrl+D to exit.
//! Slash commands inline: /undo, /redo, /help, /todos, /logs, /approve, /deny.

use crate::ui::theme::{SemanticRole, Theme};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect as RRect},
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Input composer state.
#[derive(Debug)]
pub struct InputComposer {
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
    theme: Arc<Theme>,
    reduced_motion: bool,
    visible: bool,
    disabled: bool, // During in-flight turn
    status: ComposerStatus,
    on_submit: Option<mpsc::Sender<String>>,
    placeholder: String,
    show_key_hints: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerStatus {
    Ready,
    Running,
    Cancelled,
    Error(String),
}

impl InputComposer {
    pub fn new(theme: Arc<Theme>) -> Self {
        let reduced_motion = theme.reduced_motion.enabled;
        Self {
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
            theme,
            reduced_motion,
            visible: true,
            disabled: false,
            status: ComposerStatus::Ready,
            on_submit: None,
            placeholder: "Describe what you want to do…".to_string(),
            show_key_hints: true,
        }
    }

    /// Set the theme (for runtime theme switching).
    pub fn set_theme(&mut self, theme: Arc<Theme>) {
        self.reduced_motion = theme.reduced_motion.enabled;
        self.theme = theme;
    }

    /// Set reduced motion mode.
    pub fn set_reduced_motion(&mut self, enabled: bool) {
        self.reduced_motion = enabled;
    }

    /// Set the callback for when a prompt is submitted.
    pub fn on_submit(&mut self, tx: mpsc::Sender<String>) {
        self.on_submit = Some(tx);
    }

    /// Set the placeholder text.
    pub fn set_placeholder(&mut self, placeholder: String) {
        self.placeholder = placeholder;
    }

    /// Show/hide the composer.
    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    /// Check if visible.
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Set max width for layout calculations.
    pub fn set_max_width(&mut self, _width: u16) {
        // Could be used for placeholder truncation or other layout adjustments
    }

    /// Update animation tick (for cursor blink).
    pub fn tick(&mut self) {
        // Cursor blink is handled in render using Instant::now()
        // This method exists for consistency with other components
    }

    /// Handle events from the event stream.
    pub fn handle_event(&mut self, _event: &crate::events::Event) {
        // Could handle events like Token to update streaming status
    }

    /// Disable/enable the composer (during in-flight turn).
    pub fn set_disabled(&mut self, disabled: bool) {
        self.disabled = disabled;
        if disabled {
            self.status = ComposerStatus::Running;
        } else {
            self.status = ComposerStatus::Ready;
        }
    }

    /// Check if disabled.
    pub fn is_disabled(&self) -> bool {
        self.disabled
    }

    /// Set error status.
    pub fn set_error(&mut self, error: String) {
        self.status = ComposerStatus::Error(error);
        self.disabled = false;
    }

    /// Get the current input as a single string.
    pub fn get_input(&self) -> String {
        self.lines.join("\n")
    }

    /// Clear the input.
    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor_line = 0;
        self.cursor_col = 0;
    }

    /// Handle keyboard input.
    /// Returns true if the key was handled.
    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        if !self.visible || self.disabled {
            return false;
        }

        use crossterm::event::{KeyCode, KeyModifiers};

        match key.code {
            KeyCode::Enter => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    // Shift+Enter: newline
                    self.insert_newline();
                    true
                } else {
                    // Enter: submit
                    self.submit();
                    true
                }
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+C: cancel in-flight turn or clear input
                if self.status == ComposerStatus::Running {
                    self.status = ComposerStatus::Cancelled;
                    // Emit a cancel event through the channel if needed
                } else {
                    self.clear();
                }
                true
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+D: exit (handled by main loop)
                false // Let main loop handle exit
            }
            KeyCode::Backspace => {
                self.handle_backspace();
                true
            }
            KeyCode::Delete => {
                self.handle_delete();
                true
            }
            KeyCode::Left => {
                self.move_cursor_left();
                true
            }
            KeyCode::Right => {
                self.move_cursor_right();
                true
            }
            KeyCode::Up => {
                self.move_cursor_up();
                true
            }
            KeyCode::Down => {
                self.move_cursor_down();
                true
            }
            KeyCode::Home => {
                self.cursor_col = 0;
                true
            }
            KeyCode::End => {
                if let Some(line) = self.lines.get(self.cursor_line) {
                    self.cursor_col = line.len();
                }
                true
            }
            KeyCode::Tab => {
                // Insert 2 spaces for tab
                self.insert_text("  ");
                true
            }
            KeyCode::Char(c) => {
                self.insert_text(&c.to_string());
                true
            }
            KeyCode::PageUp | KeyCode::PageDown => {
                // Could implement history navigation
                true
            }
            _ => false,
        }
    }

    fn insert_text(&mut self, text: &str) {
        if let Some(line) = self.lines.get_mut(self.cursor_line) {
            line.insert_str(self.cursor_col, text);
            self.cursor_col += text.chars().count();
        }
    }

    fn insert_newline(&mut self) {
        if let Some(line) = self.lines.get_mut(self.cursor_line) {
            let new_line = line.split_off(self.cursor_col);
            self.lines.insert(self.cursor_line + 1, new_line);
            self.cursor_line += 1;
            self.cursor_col = 0;
        }
    }

    fn handle_backspace(&mut self) {
        if self.cursor_col > 0 {
            if let Some(line) = self.lines.get_mut(self.cursor_line) {
                let _char_width = line
                    .chars()
                    .nth(self.cursor_col - 1)
                    .map(|c| c.len_utf8())
                    .unwrap_or(1);
                let new_col = self.cursor_col.saturating_sub(1);
                line.drain(new_col..self.cursor_col);
                self.cursor_col = new_col;
            }
        } else if self.cursor_line > 0 {
            // Merge with previous line
            let current_line = self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            if let Some(prev_line) = self.lines.get_mut(self.cursor_line) {
                self.cursor_col = prev_line.len();
                prev_line.push_str(&current_line);
            }
        }
    }

    fn handle_delete(&mut self) {
        if self.cursor_line >= self.lines.len() {
            return;
        }
        if self.cursor_col < self.lines[self.cursor_line].len() {
            // Delete character at cursor
            let line = &mut self.lines[self.cursor_line];
            let char_width = line
                .chars()
                .nth(self.cursor_col)
                .map(|c| c.len_utf8())
                .unwrap_or(1);
            line.drain(self.cursor_col..self.cursor_col + char_width);
        } else if self.cursor_line + 1 < self.lines.len() {
            // Merge with next line
            let next_line = self.lines.remove(self.cursor_line + 1);
            self.lines[self.cursor_line].push_str(&next_line);
        }
    }

    fn move_cursor_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_line > 0 {
            self.cursor_line -= 1;
            if let Some(line) = self.lines.get(self.cursor_line) {
                self.cursor_col = line.len();
            }
        }
    }

    fn move_cursor_right(&mut self) {
        if let Some(line) = self.lines.get(self.cursor_line) {
            if self.cursor_col < line.len() {
                self.cursor_col += 1;
            } else if self.cursor_line + 1 < self.lines.len() {
                self.cursor_line += 1;
                self.cursor_col = 0;
            }
        }
    }

    fn move_cursor_up(&mut self) {
        if self.cursor_line > 0 {
            self.cursor_line -= 1;
            if let Some(line) = self.lines.get(self.cursor_line) {
                self.cursor_col = self.cursor_col.min(line.len());
            }
        }
    }

    fn move_cursor_down(&mut self) {
        if self.cursor_line + 1 < self.lines.len() {
            self.cursor_line += 1;
            if let Some(line) = self.lines.get(self.cursor_line) {
                self.cursor_col = self.cursor_col.min(line.len());
            }
        }
    }

    fn submit(&mut self) {
        let input = self.get_input().trim().to_string();
        if !input.is_empty() {
            if let Some(tx) = &self.on_submit {
                let _ = tx.try_send(input.clone());
            }
            self.clear();
        }
    }

    /// Get the height needed for the composer (1-4 lines + borders).
    pub fn required_height(&self) -> u16 {
        let content_height = self.lines.len().clamp(1, 10) as u16;
        content_height + 2 // borders
    }

    /// Render the input composer.
    pub fn render(&mut self, area: RRect, buf: &mut Buffer) {
        if !self.visible {
            return;
        }

        // Build the input text with cursor
        let mut display_lines = Vec::new();
        let max_display_lines = area.height.saturating_sub(2) as usize;

        for (idx, line) in self.lines.iter().enumerate().take(max_display_lines) {
            if idx == self.cursor_line && !self.disabled {
                // Show cursor on current line
                let before = line.chars().take(self.cursor_col).collect::<String>();
                let after = line.chars().skip(self.cursor_col).collect::<String>();

                let mut spans = vec![];
                spans.push(Span::styled(
                    before,
                    self.theme.style(SemanticRole::CanvasFg),
                ));

                // Blinking cursor when not disabled
                if !self.reduced_motion
                    && (std::time::Instant::now().elapsed().as_millis() / 500).is_multiple_of(2)
                {
                    spans.push(Span::styled(
                        "█",
                        self.theme
                            .style(SemanticRole::CanvasFg)
                            .add_modifier(Modifier::SLOW_BLINK),
                    ));
                } else if self.reduced_motion {
                    spans.push(Span::styled("|", self.theme.style(SemanticRole::CanvasFg)));
                }

                if !after.is_empty() {
                    spans.push(Span::styled(
                        after,
                        self.theme.style(SemanticRole::CanvasFg),
                    ));
                }

                display_lines.push(Line::from(spans));
            } else {
                display_lines.push(Line::from(vec![Span::styled(
                    line.clone(),
                    self.theme.style(SemanticRole::CanvasFg),
                )]));
            }
        }

        // Show placeholder if empty
        if self.lines.len() == 1 && self.lines[0].is_empty() && !self.disabled {
            display_lines.push(Line::from(vec![Span::styled(
                self.placeholder.clone(),
                self.theme.style(SemanticRole::CanvasMuted),
            )]));
        }

        // Build key hints
        let hints = if self.show_key_hints && !self.disabled {
            match self.status {
                ComposerStatus::Ready => {
                    " Enter: send · Shift+Enter: newline · Ctrl+C: cancel · Ctrl+D: exit "
                }
                ComposerStatus::Running => " Running… Ctrl+C to cancel ",
                ComposerStatus::Cancelled => " Cancelled ",
                ComposerStatus::Error(_) => " Error ",
            }
        } else {
            ""
        };

        let block = Block::default()
            .title(" Input ")
            .borders(Borders::ALL)
            .border_style(if self.disabled {
                self.theme.style(SemanticRole::Warn)
            } else {
                self.theme.style(SemanticRole::CanvasBorder)
            })
            .style(self.theme.style(SemanticRole::CanvasBg));

        let inner = RRect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        };

        // Render the block
        block.render(area, buf);

        // Layout: input text + hints at bottom-right
        if inner.height > 1 {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(1),
                    Constraint::Length(1), // hints
                ])
                .split(inner);

            Paragraph::new(display_lines)
                .style(self.theme.style(SemanticRole::CanvasBg))
                .render(layout[0], buf);

            if !hints.is_empty() {
                let hints_line = Line::from(vec![Span::styled(
                    hints,
                    self.theme.style(SemanticRole::CanvasMuted),
                )]);
                Paragraph::new(hints_line)
                    .alignment(ratatui::layout::Alignment::Right)
                    .render(layout[1], buf);
            }
        }
    }
}

impl Default for InputComposer {
    fn default() -> Self {
        Self::new(Theme::default_dark())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn test_theme() -> Arc<Theme> {
        Theme::default_dark()
    }

    #[test]
    fn test_composer_new() {
        let composer = InputComposer::new(test_theme());
        assert!(composer.is_visible());
        assert!(!composer.is_disabled());
        assert_eq!(composer.get_input(), "");
    }

    #[test]
    fn test_insert_text() {
        let mut composer = InputComposer::new(test_theme());
        composer.insert_text("hello");
        assert_eq!(composer.get_input(), "hello");
        assert_eq!(composer.cursor_col, 5);
    }

    #[test]
    fn test_newline() {
        let mut composer = InputComposer::new(test_theme());
        composer.insert_text("line1");
        composer.insert_newline();
        composer.insert_text("line2");
        assert_eq!(composer.get_input(), "line1\nline2");
        assert_eq!(composer.cursor_line, 1);
        assert_eq!(composer.cursor_col, 5);
    }

    #[test]
    fn test_backspace() {
        let mut composer = InputComposer::new(test_theme());
        composer.insert_text("hello");
        composer.handle_backspace();
        assert_eq!(composer.get_input(), "hell");
        assert_eq!(composer.cursor_col, 4);
    }

    #[test]
    fn test_backspace_merge_lines() {
        let mut composer = InputComposer::new(test_theme());
        composer.insert_text("line1");
        composer.insert_newline();
        composer.insert_text("line2");
        // Move cursor to start of line2
        composer.cursor_col = 0;
        composer.handle_backspace(); // At start of line2, should merge
        assert_eq!(composer.get_input(), "line1line2");
        assert_eq!(composer.cursor_line, 0);
    }

    #[test]
    fn test_cursor_movement() {
        let mut composer = InputComposer::new(test_theme());
        composer.insert_text("hello");
        composer.move_cursor_left();
        assert_eq!(composer.cursor_col, 4);
        composer.move_cursor_left();
        assert_eq!(composer.cursor_col, 3);
        composer.move_cursor_right();
        assert_eq!(composer.cursor_col, 4);
        composer.move_cursor_right();
        assert_eq!(composer.cursor_col, 5); // End of line
    }

    #[test]
    fn test_clear() {
        let mut composer = InputComposer::new(test_theme());
        composer.insert_text("hello");
        composer.clear();
        assert_eq!(composer.get_input(), "");
        assert_eq!(composer.cursor_line, 0);
        assert_eq!(composer.cursor_col, 0);
    }

    #[test]
    fn test_disabled_state() {
        let mut composer = InputComposer::new(test_theme());
        composer.set_disabled(true);
        assert!(composer.is_disabled());
        assert_eq!(composer.status, ComposerStatus::Running);
        composer.set_disabled(false);
        assert!(!composer.is_disabled());
        assert_eq!(composer.status, ComposerStatus::Ready);
    }

    #[test]
    fn test_submit_callback() {
        let mut composer = InputComposer::new(test_theme());
        let (tx, mut rx) = mpsc::channel(1);
        composer.on_submit(tx);
        composer.insert_text("test prompt");
        composer.submit();
        assert_eq!(composer.get_input(), "");
        let received = rx.try_recv().unwrap();
        assert_eq!(received, "test prompt");
    }
}
