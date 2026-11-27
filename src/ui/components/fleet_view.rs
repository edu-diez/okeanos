//! FleetView panel — shows active sub-agents with status, tokens, depth, and controls.
//!
//! Per tui-design.md §5: right-rail panel for multi-agent delegation monitoring.

use crate::events::{Event, SubAgentComplete, SubAgentError, SubAgentProgress};
use crate::ui::theme::{SemanticRole, Theme};
use ratatui::{
    buffer::Buffer,
    layout::Rect as RRect,
    prelude::Widget,
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

/// Status of a sub-agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubAgentStatus {
    Starting,
    Running,
    Completed,
    Error,
    Cancelled,
}

/// Rendered state of a sub-agent for the FleetView.
#[derive(Debug, Clone)]
pub struct RenderedSubAgent {
    pub subagent_id: String,
    pub profile_name: String,
    pub status: SubAgentStatus,
    pub iteration: usize,
    pub message: String,
    pub tokens_used: u64,
    pub depth: u32,
    pub started_at: Instant,
    pub last_update: Instant,
    pub selected: bool,
}

/// FleetView panel for monitoring sub-agents.
#[derive(Debug)]
pub struct FleetView {
    agents: HashMap<String, RenderedSubAgent>,
    list_state: ListState,
    theme: Arc<Theme>,
    reduced_motion: bool,
    spinner_frame: usize,
    last_tick: Instant,
}

impl FleetView {
    /// Create a new FleetView panel.
    pub fn new(theme: Arc<Theme>) -> Self {
        let reduced_motion = theme.reduced_motion.enabled;
        Self {
            agents: HashMap::new(),
            list_state: ListState::default(),
            theme,
            reduced_motion,
            spinner_frame: 0,
            last_tick: Instant::now(),
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
            Event::SubAgentProgress(progress) => {
                self.handle_progress(progress);
            }
            Event::SubAgentComplete(complete) => {
                self.handle_complete(complete);
            }
            Event::SubAgentError(error) => {
                self.handle_error(error);
            }
            _ => {}
        }
    }

    /// Handle a progress event.
    fn handle_progress(&mut self, progress: &SubAgentProgress) {
        let now = Instant::now();
        let entry = self
            .agents
            .entry(progress.subagent_id.clone())
            .or_insert_with(|| RenderedSubAgent {
                subagent_id: progress.subagent_id.clone(),
                profile_name: "unknown".to_string(),
                status: SubAgentStatus::Starting,
                iteration: 0,
                message: String::new(),
                tokens_used: 0,
                depth: 0,
                started_at: now,
                last_update: now,
                selected: false,
            });

        entry.iteration = progress.iteration;
        entry.message = progress.message.clone();
        entry.status = SubAgentStatus::Running;
        entry.last_update = now;
    }

    /// Handle a completion event.
    fn handle_complete(&mut self, complete: &SubAgentComplete) {
        if let Some(entry) = self.agents.get_mut(&complete.subagent_id) {
            entry.status = SubAgentStatus::Completed;
            entry.message = complete.summary.clone();
            entry.tokens_used = complete.tokens_used;
            entry.last_update = Instant::now();
        }
    }

    /// Handle an error event.
    fn handle_error(&mut self, error: &SubAgentError) {
        if let Some(entry) = self.agents.get_mut(&error.subagent_id) {
            entry.status = SubAgentStatus::Error;
            entry.message = format!("{}: {}", error.code, error.message);
            entry.last_update = Instant::now();
        }
    }

    /// Add a sub-agent manually (when spawned).
    pub fn add_subagent(&mut self, subagent_id: String, profile_name: String, depth: u32) {
        let now = Instant::now();
        self.agents.insert(
            subagent_id.clone(),
            RenderedSubAgent {
                subagent_id: subagent_id.clone(),
                profile_name,
                status: SubAgentStatus::Starting,
                iteration: 0,
                message: "Starting...".to_string(),
                tokens_used: 0,
                depth,
                started_at: now,
                last_update: now,
                selected: false,
            },
        );

        // Select the first agent if none selected
        if self.list_state.selected().is_none() && !self.agents.is_empty() {
            self.list_state.select(Some(0));
        }
    }

    /// Remove a completed sub-agent.
    pub fn remove_subagent(&mut self, subagent_id: &str) {
        self.agents.remove(subagent_id);
        // Adjust selection if needed
        if let Some(selected) = self.list_state.selected()
            && selected >= self.agents.len()
        {
            if self.agents.is_empty() {
                self.list_state.select(None);
            } else {
                self.list_state.select(Some(self.agents.len() - 1));
            }
        }
    }

    /// Get the currently selected sub-agent ID.
    pub fn selected_subagent(&self) -> Option<String> {
        let idx = self.list_state.selected()?;
        self.agents.keys().nth(idx).cloned()
    }

    /// Select next sub-agent.
    pub fn select_next(&mut self) {
        if self.agents.is_empty() {
            return;
        }
        let next = self
            .list_state
            .selected()
            .map_or(0, |i| (i + 1) % self.agents.len());
        self.list_state.select(Some(next));
    }

    /// Select previous sub-agent.
    pub fn select_prev(&mut self) {
        if self.agents.is_empty() {
            return;
        }
        let prev = self
            .list_state
            .selected()
            .map_or(0, |i| if i == 0 { self.agents.len() - 1 } else { i - 1 });
        self.list_state.select(Some(prev));
    }

    /// Get the abort sender for the selected sub-agent (if any).
    /// This would need to be connected from the TerminalApp.
    pub fn get_selected_abort(&self) -> Option<tokio::sync::oneshot::Sender<()>> {
        // This is a placeholder - actual abort senders are stored in TerminalApp
        None
    }

    /// Tick animation.
    pub fn tick(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_tick).as_millis() >= 100 {
            self.spinner_frame = (self.spinner_frame + 1) % 4;
            self.last_tick = now;
        }
    }

    /// Render the FleetView panel.
    pub fn render(&mut self, area: RRect, buf: &mut Buffer) {
        let block = Block::default()
            .title(" FleetView ")
            .borders(Borders::ALL)
            .border_style(self.theme.style(SemanticRole::CanvasBorder))
            .style(self.theme.style(SemanticRole::CanvasBg));

        let inner = block.inner(area);
        ratatui::widgets::Widget::render(block, area, buf);

        if self.agents.is_empty() {
            let msg = Paragraph::new("No active sub-agents")
                .style(self.theme.style(SemanticRole::System))
                .alignment(ratatui::layout::Alignment::Center);
            msg.render(inner, buf);
            return;
        }

        // Build list items - extract needed values first to avoid borrow issues
        let spinner_frame = self.spinner_frame;
        let theme = self.theme.clone();
        let agents: Vec<_> = self.agents.values().cloned().collect();
        let items: Vec<ListItem> = agents
            .iter()
            .map(|agent| Self::render_agent_item_static(agent, spinner_frame, &theme))
            .collect();

        let list = List::new(items)
            .style(theme.style(SemanticRole::CanvasBg))
            .highlight_style(theme.style(SemanticRole::Tool).add_modifier(Modifier::BOLD))
            .highlight_symbol("► ");

        ratatui::widgets::StatefulWidget::render(list, inner, buf, &mut self.list_state);
    }

    /// Render a single agent item (static to avoid borrow issues).
    fn render_agent_item_static<'a>(
        agent: &'a RenderedSubAgent,
        spinner_frame: usize,
        theme: &Arc<Theme>,
    ) -> ListItem<'a> {
        let status_icon = match agent.status {
            SubAgentStatus::Starting => "⏳",
            SubAgentStatus::Running => {
                // Spinner animation
                const SPINNERS: [&str; 4] = ["⠋", "⠙", "⠹", "⠸"];
                SPINNERS[spinner_frame % 4]
            }
            SubAgentStatus::Completed => "✓",
            SubAgentStatus::Error => "✗",
            SubAgentStatus::Cancelled => "⊘",
        };

        let status_color = match agent.status {
            SubAgentStatus::Starting => theme.style(SemanticRole::Warn),
            SubAgentStatus::Running => theme.style(SemanticRole::Tool),
            SubAgentStatus::Completed => theme.style(SemanticRole::Ok),
            SubAgentStatus::Error => theme.style(SemanticRole::Error),
            SubAgentStatus::Cancelled => theme.style(SemanticRole::System),
        };

        let elapsed = agent.started_at.elapsed().as_secs();
        let elapsed_str = format!("{:02}:{:02}", elapsed / 60, elapsed % 60);

        let line = Line::from(vec![
            Span::styled(
                format!("{} ", status_icon),
                status_color.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                &agent.subagent_id,
                theme.style(SemanticRole::Tool).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                format!("({})", agent.profile_name),
                theme.style(SemanticRole::System),
            ),
            Span::raw(" "),
            Span::styled(
                format!("depth={}", agent.depth),
                theme.style(SemanticRole::CanvasMuted),
            ),
            Span::raw(" "),
            Span::styled(
                format!("iter={}", agent.iteration),
                theme.style(SemanticRole::CanvasMuted),
            ),
            Span::raw(" "),
            Span::styled(
                format!("{}tok", agent.tokens_used),
                theme.style(SemanticRole::Warn),
            ),
            Span::raw(" "),
            Span::styled(elapsed_str, theme.style(SemanticRole::CanvasMuted)),
            Span::raw(" "),
            Span::styled(&agent.message, theme.style(SemanticRole::CanvasFg)),
        ]);

        ListItem::new(line)
    }

    /// Get the number of active sub-agents.
    pub fn len(&self) -> usize {
        self.agents.len()
    }

    /// Check if there are no active sub-agents.
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::Theme;

    #[test]
    fn test_fleet_view_new() {
        let theme = Theme::default_dark();
        let fleet = FleetView::new(theme);
        assert!(fleet.is_empty());
        assert_eq!(fleet.len(), 0);
    }

    #[test]
    fn test_fleet_view_add_subagent() {
        let theme = Theme::default_dark();
        let mut fleet = FleetView::new(theme);
        fleet.add_subagent("subagent-1".to_string(), "coder".to_string(), 1);
        assert_eq!(fleet.len(), 1);
        assert!(fleet.list_state.selected().is_some());
    }

    #[test]
    fn test_fleet_view_select_next_prev() {
        let theme = Theme::default_dark();
        let mut fleet = FleetView::new(theme);
        fleet.add_subagent("subagent-1".to_string(), "coder".to_string(), 1);
        fleet.add_subagent("subagent-2".to_string(), "tester".to_string(), 1);

        assert_eq!(fleet.list_state.selected(), Some(0));
        fleet.select_next();
        assert_eq!(fleet.list_state.selected(), Some(1));
        fleet.select_next();
        assert_eq!(fleet.list_state.selected(), Some(0));
        fleet.select_prev();
        assert_eq!(fleet.list_state.selected(), Some(1));
    }

    #[test]
    fn test_fleet_view_handle_progress() {
        let theme = Theme::default_dark();
        let mut fleet = FleetView::new(theme);
        fleet.add_subagent("subagent-1".to_string(), "coder".to_string(), 1);

        let progress = SubAgentProgress {
            subagent_id: "subagent-1".to_string(),
            iteration: 5,
            message: "Working on task...".to_string(),
            ts: 1234567890,
        };
        fleet.handle_event(&Event::SubAgentProgress(progress));

        let agent = fleet.agents.get("subagent-1").unwrap();
        assert_eq!(agent.iteration, 5);
        assert_eq!(agent.message, "Working on task...");
        assert_eq!(agent.status, SubAgentStatus::Running);
    }

    #[test]
    fn test_fleet_view_handle_complete() {
        let theme = Theme::default_dark();
        let mut fleet = FleetView::new(theme);
        fleet.add_subagent("subagent-1".to_string(), "coder".to_string(), 1);

        let complete = SubAgentComplete {
            subagent_id: "subagent-1".to_string(),
            todo_id: "todo-1".to_string(),
            summary: "Task completed successfully".to_string(),
            tokens_used: 1500,
            result: crate::events::ToolResult {
                id: "result-1".to_string(),
                name: "subagent".to_string(),
                args: serde_json::json!({}),
                output: "done".to_string(),
                ok: true,
                duration_ms: 1000,
                truncated: false,
                exit_code: Some(0),
                timed_out: false,
                cancelled: false,
                images: Vec::new(),
            },
            ts: 1234567890,
        };
        fleet.handle_event(&Event::SubAgentComplete(complete));

        let agent = fleet.agents.get("subagent-1").unwrap();
        assert_eq!(agent.status, SubAgentStatus::Completed);
        assert_eq!(agent.tokens_used, 1500);
        assert_eq!(agent.message, "Task completed successfully");
    }

    #[test]
    fn test_fleet_view_handle_error() {
        let theme = Theme::default_dark();
        let mut fleet = FleetView::new(theme);
        fleet.add_subagent("subagent-1".to_string(), "coder".to_string(), 1);

        let error = SubAgentError {
            subagent_id: "subagent-1".to_string(),
            code: "execution_failed".to_string(),
            message: "Something went wrong".to_string(),
            ts: 1234567890,
        };
        fleet.handle_event(&Event::SubAgentError(error));

        let agent = fleet.agents.get("subagent-1").unwrap();
        assert_eq!(agent.status, SubAgentStatus::Error);
        assert!(agent.message.contains("execution_failed"));
    }
}
