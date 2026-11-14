//! Tool panel component — tool-call invocation panel per DESIGN.md §5.3.
//!
//! Shows name chip (tool color), arguments as scrollable mono-data with key/string coloring,
//! result block (expandable), status badge (pending → ok/error), retry counter.

use crate::events::{Event, ToolCall, ToolCallStatus, ToolResult};
use crate::ui::theme::{SemanticRole, Theme};
use ratatui::{
    buffer::Buffer,
    layout::Rect as RRect,
    style::{Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{
        Block, Borders, List, ListItem, ListState,
    },
};
use std::collections::HashMap;
use std::sync::Arc;

/// Tool panel state.
#[derive(Debug)]
pub struct ToolPanel {
    tool_calls: HashMap<String, RenderedToolCall>,
    tool_results: HashMap<String, RenderedToolResult>,
    list_state: ListState,
    theme: Arc<Theme>,
    reduced_motion: bool,
    spinner_frame: usize,
    last_tick: std::time::Instant,
}

#[derive(Debug, Clone)]
struct RenderedToolCall {
    call: ToolCall,
    expanded: bool,
}

#[derive(Debug, Clone)]
struct RenderedToolResult {
    result: ToolResult,
    expanded: bool,
}

impl ToolPanel {
    pub fn new(theme: Arc<Theme>) -> Self {
        let reduced_motion = theme.reduced_motion.enabled;
        Self {
            tool_calls: HashMap::new(),
            tool_results: HashMap::new(),
            list_state: ListState::default(),
            theme,
            reduced_motion,
            spinner_frame: 0,
            last_tick: std::time::Instant::now(),
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
        match event {
            Event::ToolCall(call) => {
                self.tool_calls.insert(
                    call.id.clone(),
                    RenderedToolCall {
                        call: call.clone(),
                        expanded: false,
                    },
                );
            }
            Event::ToolResult(result) => {
                self.tool_results.insert(
                    result.id.clone(),
                    RenderedToolResult {
                        result: result.clone(),
                        expanded: false,
                    },
                );
                // Update the call status to Ok/Error
                if let Some(call) = self.tool_calls.get_mut(&result.id) {
                    call.call.status = if result.ok {
                        ToolCallStatus::Ok
                    } else {
                        ToolCallStatus::Error
                    };
                }
            }
            _ => {}
        }
    }

    /// Update animation tick.
    pub fn tick(&mut self) {
        let now = std::time::Instant::now();
        if !self.reduced_motion
            && now.duration_since(self.last_tick) >= std::time::Duration::from_millis(100)
        {
            self.spinner_frame = (self.spinner_frame + 1) % 4;
            self.last_tick = now;
        }
    }

    /// Toggle expansion of a tool call/result.
    pub fn toggle_expand(&mut self, id: &str, is_call: bool) {
        if is_call {
            if let Some(call) = self.tool_calls.get_mut(id) {
                call.expanded = !call.expanded;
            }
        } else if let Some(result) = self.tool_results.get_mut(id) {
            result.expanded = !result.expanded;
        }
    }

    /// Select next item.
    pub fn select_next(&mut self) {
        let total = self.tool_calls.len() + self.tool_results.len();
        if total == 0 {
            return;
        }
        let i = self.list_state.selected().unwrap_or(0);
        if i + 1 < total {
            self.list_state.select(Some(i + 1));
        }
    }

    /// Select previous item.
    pub fn select_prev(&mut self) {
        let i = self.list_state.selected().unwrap_or(0);
        if i > 0 {
            self.list_state.select(Some(i - 1));
        }
    }

    /// Get selected item index.
    pub fn selected(&self) -> Option<usize> {
        self.list_state.selected()
    }

    /// Get the item at the given index (combined calls + results).
    fn get_item(&self, index: usize) -> Option<(String, bool, bool)> {
        // Calls first, then results
        let calls: Vec<_> = self.tool_calls.keys().cloned().collect();
        if index < calls.len() {
            return Some((
                calls[index].clone(),
                true,
                self.tool_calls[&calls[index]].expanded,
            ));
        }
        let results: Vec<_> = self.tool_results.keys().cloned().collect();
        let result_idx = index - calls.len();
        if result_idx < results.len() {
            return Some((
                results[result_idx].clone(),
                false,
                self.tool_results[&results[result_idx]].expanded,
            ));
        }
        None
    }

    /// Render the tool panel.
    pub fn render(&mut self, area: RRect, buf: &mut Buffer) {
        let block = Block::default()
            .title(" Tool Calls ")
            .borders(Borders::ALL)
            .border_style(self.theme.style(SemanticRole::CanvasBorder))
            .style(self.theme.style(SemanticRole::CanvasBg));

        let items: Vec<ListItem> = self.build_items();

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

    /// Build list items from tool calls and results.
    fn build_items(&self) -> Vec<ListItem<'_>> {
        let mut items = Vec::new();

        // Tool calls first
        for (id, call) in &self.tool_calls {
            items.push(self.render_tool_call(id, call));
        }

        // Then tool results
        for (id, result) in &self.tool_results {
            items.push(self.render_tool_result(id, result));
        }

        if items.is_empty() {
            items.push(ListItem::new(Line::from(vec![Span::styled(
                "No tool calls yet",
                self.theme.style(SemanticRole::CanvasMuted),
            )])));
        }

        items
    }

    /// Render a tool call item.
    fn render_tool_call(&self, id: &str, call: &RenderedToolCall) -> ListItem<'_> {
        let mut lines = Vec::new();
        let expanded = call.expanded;

        // Status glyph
        let (status_glyph, status_role) = match call.call.status {
            ToolCallStatus::Pending => {
                let spinner = ["⠁", "⠂", "⠄", "⠂"][self.spinner_frame];
                (spinner, SemanticRole::Pending)
            }
            ToolCallStatus::Ok => ("✓", SemanticRole::Ok),
            ToolCallStatus::Error => ("✗", SemanticRole::Error),
        };

        let expand_glyph = if expanded { "▼" } else { "▶" };

        // Header line
        lines.push(Line::from(vec![
            Span::styled(expand_glyph, self.theme.style(SemanticRole::CanvasMuted)),
            Span::raw(" "),
            Span::styled(status_glyph, self.theme.style(status_role)),
            Span::raw(" "),
            Span::styled(call.call.name.clone(), self.chip_style(SemanticRole::Tool)),
            Span::raw(" "),
            Span::styled(id.to_string(), self.theme.style(SemanticRole::CanvasMuted)),
        ]));

        if expanded {
            // Arguments
            let args_str = serde_json::to_string_pretty(&call.call.args).unwrap_or_default();
            for line in args_str.lines().take(30) {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        line.to_string(),
                        self.theme.style(SemanticRole::CanvasMuted),
                    ),
                ]));
            }
            if args_str.lines().count() > 30 {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        "... (truncated)",
                        self.theme.style(SemanticRole::CanvasMuted),
                    ),
                ]));
            }
        }

        ListItem::new(lines).style(self.theme.style(SemanticRole::CanvasBg))
    }

    /// Render a tool result item.
    fn render_tool_result(&self, _id: &str, result: &RenderedToolResult) -> ListItem<'_> {
        let mut lines = Vec::new();
        let expanded = result.expanded;

        let status_glyph = if result.result.ok { "✓" } else { "✗" };
        let status_role = if result.result.ok {
            SemanticRole::Ok
        } else {
            SemanticRole::Error
        };
        let expand_glyph = if expanded { "▼" } else { "▶" };

        // Header line
        lines.push(Line::from(vec![
            Span::styled(expand_glyph, self.theme.style(SemanticRole::CanvasMuted)),
            Span::raw(" "),
            Span::styled(status_glyph, self.theme.style(status_role)),
            Span::raw(" "),
            Span::styled(
                result.result.name.clone(),
                self.chip_style(SemanticRole::Tool),
            ),
            Span::raw(" "),
            Span::styled(
                format!("({}ms)", result.result.duration_ms),
                self.theme.style(SemanticRole::CanvasMuted),
            ),
            if result.result.truncated {
                Span::styled(" ⟳", self.theme.style(SemanticRole::Warn))
            } else {
                Span::raw("")
            },
            if result.result.timed_out {
                Span::styled(" ⏱", self.theme.style(SemanticRole::Warn))
            } else {
                Span::raw("")
            },
            if result.result.cancelled {
                Span::styled(" ⛔", self.theme.style(SemanticRole::Error))
            } else {
                Span::raw("")
            },
        ]));

        if expanded {
            // Output
            for line in result.result.output.lines().take(40) {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(line.to_string(), self.theme.style(SemanticRole::System)),
                ]));
            }
            if result.result.output.lines().count() > 40 {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        "... (truncated)",
                        self.theme.style(SemanticRole::CanvasMuted),
                    ),
                ]));
            }
        }

        ListItem::new(lines).style(self.theme.style(SemanticRole::CanvasBg))
    }

    /// Get chip style for a semantic role.
    fn chip_style(&self, role: SemanticRole) -> Style {
        let (fg, bg) = match role {
            SemanticRole::Tool => (SemanticRole::CanvasBg, SemanticRole::Tool),
            SemanticRole::Ok => (SemanticRole::CanvasBg, SemanticRole::Ok),
            SemanticRole::Error => (SemanticRole::CanvasBg, SemanticRole::Error),
            SemanticRole::Pending => (SemanticRole::CanvasBg, SemanticRole::Pending),
            _ => (SemanticRole::CanvasFg, SemanticRole::CanvasBg),
        };
        Style::default()
            .fg(self.theme.color(fg))
            .bg(self.theme.color(bg))
            .add_modifier(Modifier::BOLD)
    }

    /// Get total item count.
    pub fn len(&self) -> usize {
        self.tool_calls.len() + self.tool_results.len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.tool_calls.is_empty() && self.tool_results.is_empty()
    }
}

impl Default for ToolPanel {
    fn default() -> Self {
        Self::new(Theme::default_dark())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ToolCall, ToolCallStatus, ToolResult};
    use std::sync::Arc;

    fn test_theme() -> Arc<Theme> {
        Theme::default_dark()
    }

    #[test]
    fn test_tool_panel_new() {
        let panel = ToolPanel::new(test_theme());
        assert!(panel.is_empty());
        assert_eq!(panel.len(), 0);
    }

    #[test]
    fn test_tool_call_event() {
        let mut panel = ToolPanel::new(test_theme());
        let call = ToolCall {
            id: "call-1".to_string(),
            name: "read".to_string(),
            args: serde_json::json!({"path": "test.txt"}),
            status: ToolCallStatus::Pending,
        };
        panel.handle_event(&crate::events::Event::ToolCall(call));
        assert_eq!(panel.len(), 1);
        assert!(panel.tool_calls.contains_key("call-1"));
    }

    #[test]
    fn test_tool_result_event() {
        let mut panel = ToolPanel::new(test_theme());
        let call = ToolCall {
            id: "call-1".to_string(),
            name: "read".to_string(),
            args: serde_json::json!({"path": "test.txt"}),
            status: ToolCallStatus::Pending,
        };
        panel.handle_event(&crate::events::Event::ToolCall(call));

        let result = ToolResult {
            id: "call-1".to_string(),
            name: "read".to_string(),
            args: serde_json::json!({"path": "test.txt"}),
            output: "file content".to_string(),
            ok: true,
            duration_ms: 100,
            truncated: false,
            exit_code: None,
            timed_out: false,
            cancelled: false,
        };
        panel.handle_event(&crate::events::Event::ToolResult(result));

        assert_eq!(panel.len(), 2); // Call and Result stored separately
        assert!(panel.tool_results.contains_key("call-1"));
        let call = panel.tool_calls.get("call-1").unwrap();
        assert_eq!(call.call.status, ToolCallStatus::Ok);
    }

    #[test]
    fn test_toggle_expand() {
        let mut panel = ToolPanel::new(test_theme());
        let call = ToolCall {
            id: "call-1".to_string(),
            name: "read".to_string(),
            args: serde_json::json!({"path": "test.txt"}),
            status: ToolCallStatus::Pending,
        };
        panel.handle_event(&crate::events::Event::ToolCall(call));
        assert!(!panel.tool_calls["call-1"].expanded);
        panel.toggle_expand("call-1", true);
        assert!(panel.tool_calls["call-1"].expanded);
    }

    #[test]
    fn test_tool_result_error() {
        let mut panel = ToolPanel::new(test_theme());
        let call = ToolCall {
            id: "call-1".to_string(),
            name: "run".to_string(),
            args: serde_json::json!({"cmd": "false"}),
            status: ToolCallStatus::Pending,
        };
        panel.handle_event(&crate::events::Event::ToolCall(call));

        let result = ToolResult {
            id: "call-1".to_string(),
            name: "run".to_string(),
            args: serde_json::json!({"cmd": "false"}),
            output: "command failed".to_string(),
            ok: false,
            duration_ms: 100,
            truncated: false,
            exit_code: Some(1),
            timed_out: false,
            cancelled: false,
        };
        panel.handle_event(&crate::events::Event::ToolResult(result));

        assert!(panel.tool_results.contains_key("call-1"));
        let call = panel.tool_calls.get("call-1").unwrap();
        assert_eq!(call.call.status, ToolCallStatus::Error);
    }
}
