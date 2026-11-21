//! Command palette component — centered overlay per DESIGN.md §5.7.
//!
//! Lists `/undo`, `/redo`, `/help`, `/approve`, `/deny`, `/todos`, `/logs`.
//! Prefix/fuzzy match as you type; Esc closes; Enter runs.

use crate::ui::theme::{SemanticRole, Theme};
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Rect as RRect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Widget},
};
use std::sync::Arc;

/// Command palette action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandAction {
    Help,
    Undo,
    Redo,
    Approve,
    Deny,
    Todos,
    Logs,
    Compact,
    Custom(String),
}

impl CommandAction {
    /// Get the command string for this action.
    pub fn command(&self) -> &str {
        match self {
            CommandAction::Help => "/help",
            CommandAction::Undo => "/undo",
            CommandAction::Redo => "/redo",
            CommandAction::Approve => "/approve",
            CommandAction::Deny => "/deny",
            CommandAction::Todos => "/todos",
            CommandAction::Logs => "/logs",
            CommandAction::Compact => "/compact",
            CommandAction::Custom(s) => s,
        }
    }

    /// Get the description for this action.
    pub fn description(&self) -> &str {
        match self {
            CommandAction::Help => "Show help",
            CommandAction::Undo => "Undo last action",
            CommandAction::Redo => "Redo last undone action",
            CommandAction::Approve => "Approve pending tool call",
            CommandAction::Deny => "Deny pending tool call",
            CommandAction::Todos => "Show/hide todo panel",
            CommandAction::Logs => "Show/hide log panel",
            CommandAction::Compact => "Compact context (summarize history)",
            CommandAction::Custom(_) => "Custom command",
        }
    }

    /// Get all built-in commands.
    pub fn all() -> Vec<CommandAction> {
        vec![
            CommandAction::Help,
            CommandAction::Undo,
            CommandAction::Redo,
            CommandAction::Approve,
            CommandAction::Deny,
            CommandAction::Todos,
            CommandAction::Logs,
            CommandAction::Compact,
        ]
    }
}

/// Command palette state.
pub struct CommandPalette {
    query: String,
    filtered: Vec<CommandAction>,
    list_state: ListState,
    theme: Arc<Theme>,
    reduced_motion: bool,
    visible: bool,
    on_execute: Option<Box<dyn FnMut(CommandAction) + Send + Sync>>,
}

impl CommandPalette {
    pub fn new(theme: Arc<Theme>) -> Self {
        let all = CommandAction::all();
        let reduced_motion = theme.reduced_motion.enabled;
        Self {
            query: String::new(),
            filtered: all.clone(),
            list_state: ListState::default(),
            theme,
            reduced_motion,
            visible: false,
            on_execute: None,
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

    /// Set callback for command execution.
    pub fn on_execute<F>(&mut self, f: F)
    where
        F: FnMut(CommandAction) + Send + Sync + 'static,
    {
        self.on_execute = Some(Box::new(f));
    }

    /// Show the command palette.
    pub fn show(&mut self) {
        self.visible = true;
        self.query.clear();
        self.filtered = CommandAction::all();
        self.list_state.select(Some(0));
    }

    /// Hide the command palette.
    pub fn hide(&mut self) {
        self.visible = false;
        self.query.clear();
        self.filtered = CommandAction::all();
        self.list_state.select(None);
    }

    /// Toggle visibility.
    pub fn toggle(&mut self) {
        if self.visible {
            self.hide();
        } else {
            self.show();
        }
    }

    /// Check if visible.
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Handle keyboard input.
    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        if !self.visible {
            return false;
        }

        use crossterm::event::{KeyCode, KeyModifiers};

        match key.code {
            KeyCode::Esc => {
                self.hide();
                true
            }
            KeyCode::Enter => {
                if let Some(selected) = self.list_state.selected()
                    && let Some(action) = self.filtered.get(selected).cloned()
                {
                    if let Some(mut callback) = self.on_execute.take() {
                        callback(action);
                        self.on_execute = Some(callback);
                    }
                    self.hide();
                }
                true
            }
            KeyCode::Tab | KeyCode::Down => {
                self.select_next();
                true
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.select_prev();
                true
            }
            KeyCode::Char(c) => {
                self.query.push(c);
                self.filter();
                true
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.filter();
                true
            }
            KeyCode::Delete => {
                // Ctrl+Delete: clear query
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    self.query.clear();
                    self.filter();
                }
                true
            }
            _ => false,
        }
    }

    /// Select next item.
    fn select_next(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        let i = self.list_state.selected().unwrap_or(0);
        if i + 1 < self.filtered.len() {
            self.list_state.select(Some(i + 1));
        }
    }

    /// Select previous item.
    fn select_prev(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        let i = self.list_state.selected().unwrap_or(0);
        if i > 0 {
            self.list_state.select(Some(i - 1));
        }
    }

    /// Filter commands based on query.
    fn filter(&mut self) {
        let query = self.query.to_lowercase();
        if query.is_empty() {
            self.filtered = CommandAction::all();
        } else {
            self.filtered = CommandAction::all()
                .into_iter()
                .filter(|a| {
                    let cmd = a.command().to_lowercase();
                    let desc = a.description().to_lowercase();
                    cmd.contains(&query) || desc.contains(&query)
                })
                .collect();
        }
        self.list_state.select(if self.filtered.is_empty() {
            None
        } else {
            Some(0)
        });
    }

    /// Get current query.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Render the command palette.
    pub fn render(&self, area: RRect, buf: &mut Buffer) {
        if !self.visible {
            return;
        }

        Clear.render(area, buf);

        // Palette dimensions
        let palette_width = (area.width as f32 * 0.8).clamp(50.0, 80.0) as u16;
        let item_height = (self.filtered.len() as u16 + 3).min(15); // max 15 items
        let palette_height = item_height + 4; // border + padding

        let palette_area = centered_rect(palette_width, palette_height, area);

        // Background block
        let block = Block::default()
            .title(" Commands ")
            .borders(Borders::ALL)
            .border_style(
                self.theme
                    .style(SemanticRole::Tool)
                    .add_modifier(Modifier::BOLD),
            )
            .style(
                self.theme
                    .style_bg(SemanticRole::CanvasFg, SemanticRole::CanvasBg),
            );

        let inner = RRect {
            x: palette_area.x + 1,
            y: palette_area.y + 1,
            width: palette_area.width.saturating_sub(2),
            height: palette_area.height.saturating_sub(2),
        };

        block.render(palette_area, buf);

        // Layout: query input + list
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // Query line
                Constraint::Min(1),    // List
            ])
            .split(inner);

        // Query input line
        let query_line = Line::from(vec![
            Span::styled(
                "> ",
                self.theme
                    .style(SemanticRole::Tool)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(&self.query, self.theme.style(SemanticRole::CanvasFg)),
            if self.filtered.is_empty() {
                Span::styled(" (no matches)", self.theme.style(SemanticRole::CanvasMuted))
            } else {
                Span::raw("")
            },
        ]);
        Paragraph::new(query_line)
            .alignment(Alignment::Left)
            .render(layout[0], buf);

        // Command list
        if !self.filtered.is_empty() {
            let items: Vec<ListItem> = self
                .filtered
                .iter()
                .enumerate()
                .map(|(idx, action)| {
                    let is_selected = self.list_state.selected() == Some(idx);
                    self.render_command_item(idx, action, is_selected)
                })
                .collect();

            let list = List::new(items)
                .highlight_style(
                    Style::default()
                        .bg(self.theme.color(SemanticRole::CanvasBorder))
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("► ");

            let list_area = RRect {
                x: layout[1].x,
                y: layout[1].y,
                width: layout[1].width,
                height: layout[1].height,
            };

            let mut state = self.list_state.clone();
            ratatui::widgets::StatefulWidget::render(list, list_area, buf, &mut state);
        } else {
            // No matches
            let line = Line::from(vec![
                Span::styled(
                    "No commands match '",
                    self.theme.style(SemanticRole::CanvasMuted),
                ),
                Span::styled(&self.query, self.theme.style(SemanticRole::Warn)),
                Span::styled("'", self.theme.style(SemanticRole::CanvasMuted)),
            ]);
            Paragraph::new(line)
                .alignment(Alignment::Center)
                .render(layout[1], buf);
        }
    }

    /// Render a single command item.
    fn render_command_item(
        &self,
        _idx: usize,
        action: &CommandAction,
        is_selected: bool,
    ) -> ListItem<'_> {
        let cmd = action.command().to_string();
        let desc = action.description().to_string();

        let (cmd_style, desc_style) = if is_selected {
            (
                self.theme
                    .style(SemanticRole::CanvasFg)
                    .add_modifier(Modifier::BOLD),
                self.theme.style(SemanticRole::CanvasMuted),
            )
        } else {
            (
                self.theme.style(SemanticRole::Tool),
                self.theme.style(SemanticRole::CanvasMuted),
            )
        };

        let line = Line::from(vec![
            Span::styled(cmd, cmd_style),
            Span::raw("  "),
            Span::styled(desc, desc_style),
        ]);

        ListItem::new(line)
    }
}

/// Helper to create a centered rect.
fn centered_rect(width: u16, height: u16, r: RRect) -> RRect {
    let x = r.x.saturating_add((r.width.saturating_sub(width)) / 2);
    let y = r.y.saturating_add((r.height.saturating_sub(height)) / 2);
    RRect::new(x, y, width.min(r.width), height.min(r.height))
}

impl Default for CommandPalette {
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
    fn test_command_palette_new() {
        let palette = CommandPalette::new(test_theme());
        assert!(!palette.is_visible());
        assert_eq!(palette.query(), "");
        assert_eq!(palette.filtered.len(), 8); // 8 built-in commands (including /compact)
    }

    #[test]
    fn test_show_hide() {
        let mut palette = CommandPalette::new(test_theme());
        palette.show();
        assert!(palette.is_visible());
        assert_eq!(palette.list_state.selected(), Some(0));

        palette.hide();
        assert!(!palette.is_visible());
        assert_eq!(palette.query(), "");
    }

    #[test]
    fn test_toggle() {
        let mut palette = CommandPalette::new(test_theme());
        palette.toggle();
        assert!(palette.is_visible());
        palette.toggle();
        assert!(!palette.is_visible());
    }

    #[test]
    fn test_filter() {
        let mut palette = CommandPalette::new(test_theme());
        palette.show();

        // Type "/un" should match /undo
        palette.query.push_str("/un");
        palette.filter();
        assert_eq!(palette.filtered.len(), 1);
        assert_eq!(palette.filtered[0], CommandAction::Undo);

        // Type "/t" should match /todos
        palette.query.clear();
        palette.query.push_str("/t");
        palette.filter();
        assert_eq!(palette.filtered.len(), 1);
        assert_eq!(palette.filtered[0], CommandAction::Todos);

        // Type "help" should match /help
        palette.query.clear();
        palette.query.push_str("help");
        palette.filter();
        assert_eq!(palette.filtered.len(), 1);
        assert_eq!(palette.filtered[0], CommandAction::Help);
    }

    #[test]
    fn test_key_navigation() {
        let mut palette = CommandPalette::new(test_theme());
        palette.show();

        // Down arrow selects next
        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::NONE,
        );
        palette.handle_key(key);
        assert_eq!(palette.list_state.selected(), Some(1));

        // Up arrow selects previous
        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Up,
            crossterm::event::KeyModifiers::NONE,
        );
        palette.handle_key(key);
        assert_eq!(palette.list_state.selected(), Some(0));
    }

    #[test]
    fn test_command_action() {
        assert_eq!(CommandAction::Help.command(), "/help");
        assert_eq!(CommandAction::Undo.command(), "/undo");
        assert_eq!(CommandAction::Redo.command(), "/redo");
        assert_eq!(CommandAction::Approve.command(), "/approve");
        assert_eq!(CommandAction::Deny.command(), "/deny");
        assert_eq!(CommandAction::Todos.command(), "/todos");
        assert_eq!(CommandAction::Logs.command(), "/logs");
        assert_eq!(CommandAction::Compact.command(), "/compact");

        assert_eq!(CommandAction::Help.description(), "Show help");
        assert_eq!(CommandAction::Undo.description(), "Undo last action");
        assert_eq!(
            CommandAction::Compact.description(),
            "Compact context (summarize history)"
        );
    }

    #[test]
    fn test_on_execute_callback() {
        let mut palette = CommandPalette::new(test_theme());
        let executed = std::sync::Arc::new(std::sync::Mutex::new(None));
        let executed_clone = executed.clone();
        palette.on_execute(move |action| {
            *executed_clone.lock().unwrap() = Some(action);
        });
        palette.show();

        // Select first item and press Enter
        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        );
        palette.handle_key(key);

        assert_eq!(*executed.lock().unwrap(), Some(CommandAction::Help));
        assert!(!palette.is_visible());
    }
}
