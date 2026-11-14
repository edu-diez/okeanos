//! Todo panel component — ordered list with checkboxes per DESIGN.md §5.5.
//!
//! Each row shows checkbox glyph ([ ] / [x]), short title, status chip.
//! Add via `/todo`; completing toggles the row.
//! Completed items fade to `muted`.

use crate::events::{Event, Todo};
use crate::ui::theme::{SemanticRole, Theme};
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Rect as RRect},
    style::{Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Widget},
};
use std::sync::Arc;

/// Todo panel state.
#[derive(Debug)]
pub struct TodoPanel {
    todos: Vec<Todo>,
    list_state: ListState,
    theme: Arc<Theme>,
    reduced_motion: bool,
    expanded: bool,
}

impl TodoPanel {
    pub fn new(theme: Arc<Theme>) -> Self {
        let reduced_motion = theme.reduced_motion.enabled;
        Self {
            todos: Vec::new(),
            list_state: ListState::default(),
            theme,
            reduced_motion,
            expanded: true,
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

    /// Handle an event from the event stream.
    pub fn handle_event(&mut self, event: &Event) {
        if let Event::Todo(todo) = event {
            self.update_todo(todo.clone())
        }
    }

    /// Update or add a todo.
    fn update_todo(&mut self, todo: Todo) {
        let was_empty = self.todos.is_empty();
        if let Some(existing) = self.todos.iter_mut().find(|t| t.id == todo.id) {
            *existing = todo;
        } else {
            self.todos.push(todo);
        }
        // Auto-select first item if list was empty
        if was_empty && !self.todos.is_empty() {
            self.list_state.select(Some(0));
        }
    }

    /// Toggle todo completion by index.
    pub fn toggle(&mut self, index: usize) {
        if let Some(todo) = self.todos.get_mut(index) {
            todo.done = !todo.done;
        }
    }

    /// Remove a todo by index.
    pub fn remove(&mut self, index: usize) {
        if index < self.todos.len() {
            self.todos.remove(index);
        }
    }

    /// Clear completed todos.
    pub fn clear_completed(&mut self) {
        self.todos.retain(|t| !t.done);
    }

    /// Set expanded/collapsed state.
    pub fn set_expanded(&mut self, expanded: bool) {
        self.expanded = expanded;
    }

    /// Toggle expanded state.
    pub fn toggle_expanded(&mut self) {
        self.expanded = !self.expanded;
    }

    /// Select next todo.
    pub fn select_next(&mut self) {
        let i = self.list_state.selected().unwrap_or(0);
        if i + 1 < self.todos.len() {
            self.list_state.select(Some(i + 1));
        }
    }

    /// Select previous todo.
    pub fn select_prev(&mut self) {
        let i = self.list_state.selected().unwrap_or(0);
        if i > 0 {
            self.list_state.select(Some(i - 1));
        }
    }

    /// Get selected todo index.
    pub fn selected(&self) -> Option<usize> {
        self.list_state.selected()
    }

    /// Render the todo panel.
    pub fn render(&mut self, area: RRect, buf: &mut Buffer) {
        if !self.expanded {
            self.render_collapsed(area, buf);
            return;
        }

        let block = Block::default()
            .title(" Todos ")
            .borders(Borders::ALL)
            .border_style(self.theme.style(SemanticRole::CanvasBorder))
            .style(self.theme.style(SemanticRole::CanvasBg));

        let items: Vec<ListItem> = self
            .todos
            .iter()
            .enumerate()
            .map(|(idx, todo)| self.render_todo(idx, todo))
            .collect();

        let list = List::new(items)
            .block(block)
            .highlight_style(
                Style::default()
                    .bg(self.theme.color(SemanticRole::CanvasBorder))
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("► ");

        let inner = RRect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        };

        let mut state = self.list_state.clone();
        ratatui::widgets::StatefulWidget::render(list, inner, buf, &mut state);
        self.list_state = state;
    }

    /// Render collapsed state (just a summary line).
    fn render_collapsed(&self, area: RRect, buf: &mut Buffer) {
        let total = self.todos.len();
        let done = self.todos.iter().filter(|t| t.done).count();
        let pending = total - done;

        let line = Line::from(vec![
            Span::styled(
                " Todos ",
                self.theme
                    .style(SemanticRole::Tool)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(": "),
            Span::styled(done.to_string(), self.theme.style(SemanticRole::Ok)),
            Span::raw("/"),
            Span::styled(total.to_string(), self.theme.style(SemanticRole::CanvasFg)),
            Span::raw(" done, "),
            Span::styled(pending.to_string(), self.theme.style(SemanticRole::Pending)),
            Span::raw(" pending"),
            Span::raw("  [Enter to expand]"),
        ]);

        Paragraph::new(line)
            .style(self.theme.style(SemanticRole::CanvasBg))
            .alignment(Alignment::Left)
            .render(area, buf);
    }

    /// Render a single todo item.
    fn render_todo(&self, _idx: usize, todo: &Todo) -> ListItem<'_> {
        let checkbox = if todo.done { "[x]" } else { "[ ]" };
        let checkbox_style = if todo.done {
            self.theme.style(SemanticRole::Ok)
        } else {
            self.theme.style(SemanticRole::Pending)
        };

        let title_style = if todo.done {
            self.theme
                .style(SemanticRole::CanvasMuted)
                .add_modifier(Modifier::CROSSED_OUT)
        } else {
            self.theme.style(SemanticRole::CanvasFg)
        };

        let status_chip = if todo.done {
            Span::styled(" done ", self.chip_style(SemanticRole::Ok))
        } else {
            Span::styled(" pending ", self.chip_style(SemanticRole::Pending))
        };

        let line = Line::from(vec![
            Span::styled(checkbox, checkbox_style.add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::styled(todo.title.clone(), title_style),
            Span::raw(" "),
            status_chip,
        ]);

        ListItem::new(line).style(self.theme.style(SemanticRole::CanvasBg))
    }

    /// Get chip style for a semantic role.
    fn chip_style(&self, role: SemanticRole) -> Style {
        let (fg, bg) = match role {
            SemanticRole::Ok => (SemanticRole::CanvasBg, SemanticRole::Ok),
            SemanticRole::Pending => (SemanticRole::CanvasBg, SemanticRole::Pending),
            _ => (SemanticRole::CanvasFg, SemanticRole::CanvasBg),
        };
        Style::default()
            .fg(self.theme.color(fg))
            .bg(self.theme.color(bg))
            .add_modifier(Modifier::BOLD)
    }

    /// Get todo count.
    pub fn len(&self) -> usize {
        self.todos.len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.todos.is_empty()
    }

    /// Get completed count.
    pub fn completed_count(&self) -> usize {
        self.todos.iter().filter(|t| t.done).count()
    }

    /// Get pending count.
    pub fn pending_count(&self) -> usize {
        self.todos.iter().filter(|t| !t.done).count()
    }
}

impl Default for TodoPanel {
    fn default() -> Self {
        Self::new(Theme::default_dark())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Todo;
    use std::sync::Arc;

    fn test_theme() -> Arc<Theme> {
        Theme::default_dark()
    }

    #[test]
    fn test_todo_panel_new() {
        let panel = TodoPanel::new(test_theme());
        assert!(panel.is_empty());
        assert_eq!(panel.len(), 0);
    }

    #[test]
    fn test_add_todo() {
        let mut panel = TodoPanel::new(test_theme());
        let todo = Todo {
            id: "1".to_string(),
            title: "Test task".to_string(),
            done: false,
        };
        panel.handle_event(&crate::events::Event::Todo(todo));
        assert_eq!(panel.len(), 1);
        assert_eq!(panel.pending_count(), 1);
    }

    #[test]
    fn test_toggle_todo() {
        let mut panel = TodoPanel::new(test_theme());
        let todo = Todo {
            id: "1".to_string(),
            title: "Test task".to_string(),
            done: false,
        };
        panel.handle_event(&crate::events::Event::Todo(todo));
        panel.toggle(0);
        assert!(panel.todos[0].done);
        assert_eq!(panel.completed_count(), 1);
    }

    #[test]
    fn test_update_existing_todo() {
        let mut panel = TodoPanel::new(test_theme());
        let todo1 = Todo {
            id: "1".to_string(),
            title: "Test task".to_string(),
            done: false,
        };
        panel.handle_event(&crate::events::Event::Todo(todo1));

        let todo2 = Todo {
            id: "1".to_string(),
            title: "Updated task".to_string(),
            done: true,
        };
        panel.handle_event(&crate::events::Event::Todo(todo2));

        assert_eq!(panel.len(), 1);
        assert_eq!(panel.todos[0].title, "Updated task");
        assert!(panel.todos[0].done);
    }

    #[test]
    fn test_remove_todo() {
        let mut panel = TodoPanel::new(test_theme());
        let todo = Todo {
            id: "1".to_string(),
            title: "Test task".to_string(),
            done: false,
        };
        panel.handle_event(&crate::events::Event::Todo(todo));
        panel.remove(0);
        assert!(panel.is_empty());
    }

    #[test]
    fn test_clear_completed() {
        let mut panel = TodoPanel::new(test_theme());
        let todo1 = Todo {
            id: "1".to_string(),
            title: "Task 1".to_string(),
            done: true,
        };
        let todo2 = Todo {
            id: "2".to_string(),
            title: "Task 2".to_string(),
            done: false,
        };
        panel.handle_event(&crate::events::Event::Todo(todo1));
        panel.handle_event(&crate::events::Event::Todo(todo2));
        panel.clear_completed();
        assert_eq!(panel.len(), 1);
        assert_eq!(panel.todos[0].id, "2");
    }

    #[test]
    fn test_expand_collapse() {
        let mut panel = TodoPanel::new(test_theme());
        assert!(panel.expanded);
        panel.toggle_expanded();
        assert!(!panel.expanded);
        panel.toggle_expanded();
        assert!(panel.expanded);
    }

    #[test]
    fn test_selection() {
        let mut panel = TodoPanel::new(test_theme());
        for i in 0..3 {
            panel.handle_event(&crate::events::Event::Todo(Todo {
                id: i.to_string(),
                title: format!("Task {}", i),
                done: false,
            }));
        }
        assert_eq!(panel.selected(), Some(0));
        panel.select_next();
        assert_eq!(panel.selected(), Some(1));
        panel.select_next();
        assert_eq!(panel.selected(), Some(2));
        panel.select_next(); // Should stay at 2
        assert_eq!(panel.selected(), Some(2));
        panel.select_prev();
        assert_eq!(panel.selected(), Some(1));
    }
}
