//! Conversation pane component — vertical message list with streaming token render.
//!
//! Per DESIGN.md §5.1, §5.2:
//! - User messages left-aligned with `user` chip
//! - Assistant messages with `assistant` chip and streaming token render
//! - Tool results indented under assistant turn, collapsed by default
//! - Newest message at bottom; autoscroll follows stream unless user scrolls up
//! - Typewriter cursor blink; typing indicator (three pulsing dots) while model thinking

use crate::events::{Event, Message, MessageRole, ToolCall, ToolCallStatus, ToolResult};
use crate::ui::theme::{SemanticRole, Theme};
use ratatui::{
    buffer::Buffer,
    layout::Rect as RRect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Scrollbar, ScrollbarOrientation},
};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Maximum number of messages to keep in memory (prevent unbounded growth).
const MAX_MESSAGES: usize = 1000;

/// Maximum lines per message before truncation indicator.
const MAX_MESSAGE_LINES: usize = 200;

/// Typing indicator animation frames.
const TYPING_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Braille spinner frames for streaming.
const SPINNER_FRAMES: &[&str] = &["⠁", "⠂", "⠄", "⠂"];

/// A rendered message in the conversation.
#[derive(Debug, Clone)]
pub struct RenderedMessage {
    pub message: Message,
    pub tool_calls: Vec<RenderedToolCall>,
    pub tool_results: Vec<RenderedToolResult>,
    pub is_streaming: bool,
    pub stream_cursor_pos: usize,
    pub last_update: Instant,
}

/// A tool call being rendered.
#[derive(Debug, Clone)]
pub struct RenderedToolCall {
    pub call: ToolCall,
    pub status: ToolCallStatus,
    pub expanded: bool,
}

/// A tool result being rendered.
#[derive(Debug, Clone)]
pub struct RenderedToolResult {
    pub result: ToolResult,
    pub expanded: bool,
}

/// Conversation pane state.
#[derive(Debug)]
pub struct ConversationPane {
    messages: VecDeque<RenderedMessage>,
    list_state: ListState,
    auto_scroll: bool,
    typing_indicator: bool,
    typing_frame: usize,
    spinner_frame: usize,
    last_typing_tick: Instant,
    theme: Arc<Theme>,
    reduced_motion: bool,
    max_width: u16,
}

impl ConversationPane {
    pub fn new(theme: Arc<Theme>) -> Self {
        let reduced_motion = theme.reduced_motion.enabled;
        Self {
            messages: VecDeque::with_capacity(MAX_MESSAGES),
            list_state: ListState::default(),
            auto_scroll: true,
            typing_indicator: false,
            typing_frame: 0,
            spinner_frame: 0,
            last_typing_tick: Instant::now(),
            theme,
            reduced_motion,
            max_width: 80,
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

    /// Update max width for wrapping calculations.
    pub fn set_max_width(&mut self, width: u16) {
        self.max_width = width;
    }

    /// Handle an event from the event stream.
    pub fn handle_event(&mut self, event: &Event) {
        match event {
            Event::Message(msg) => self.add_message(msg.clone()),
            Event::Token(token) => self.append_token(&token.delta),
            Event::ToolCall(call) => self.add_or_update_tool_call(call.clone()),
            Event::ToolResult(result) => self.add_tool_result(result.clone()),
            Event::Log(log) if log.level == crate::events::LogLevel::Info => {
                // Could add system messages to conversation
            }
            _ => {}
        }
    }

    /// Add a new message to the conversation.
    fn add_message(&mut self, message: Message) {
        let rendered = RenderedMessage {
            message,
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            is_streaming: false,
            stream_cursor_pos: 0,
            last_update: Instant::now(),
        };

        self.messages.push_back(rendered);
        if self.messages.len() > MAX_MESSAGES {
            self.messages.pop_front();
        }

        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    /// Append a streaming token delta to the last assistant message.
    fn append_token(&mut self, delta: &str) {
        if let Some(last) = self.messages.back_mut()
            && last.message.role == MessageRole::Assistant
        {
            last.message.content.push_str(delta);
            last.is_streaming = true;
            last.last_update = Instant::now();
            last.stream_cursor_pos = last.message.content.len();
        }
    }

    /// Add or update a tool call.
    fn add_or_update_tool_call(&mut self, call: ToolCall) {
        if let Some(last) = self.messages.back_mut()
            && last.message.role == MessageRole::Assistant
        {
            // Find existing tool call or add new
            if let Some(existing) = last.tool_calls.iter_mut().find(|tc| tc.call.id == call.id) {
                existing.call = call.clone();
                existing.status = call.status;
            } else {
                last.tool_calls.push(RenderedToolCall {
                    call,
                    status: ToolCallStatus::Pending,
                    expanded: false,
                });
            }
            last.last_update = Instant::now();
        }
    }

    /// Add a tool result.
    fn add_tool_result(&mut self, result: ToolResult) {
        if let Some(last) = self.messages.back_mut()
            && last.message.role == MessageRole::Assistant
        {
            last.tool_results.push(RenderedToolResult {
                result,
                expanded: false,
            });
            last.last_update = Instant::now();
        }
    }

    /// Set typing indicator (model is thinking between tokens).
    pub fn set_typing_indicator(&mut self, typing: bool) {
        self.typing_indicator = typing;
        if typing {
            self.last_typing_tick = Instant::now();
        }
    }

    /// Update animation frames (call periodically).
    pub fn tick(&mut self) {
        let now = Instant::now();

        // Typing indicator animation
        if self.typing_indicator
            && !self.reduced_motion
            && now.duration_since(self.last_typing_tick) >= Duration::from_millis(60)
        {
            self.typing_frame = (self.typing_frame + 1) % TYPING_FRAMES.len();
            self.last_typing_tick = now;
        }

        // Spinner animation for streaming
        if !self.reduced_motion {
            // Update spinner based on streaming messages
            let has_streaming = self.messages.iter().any(|m| m.is_streaming);
            if has_streaming {
                self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
            }
        }

        // Auto-hide typing indicator after timeout
        if self.typing_indicator
            && now.duration_since(self.last_typing_tick) > Duration::from_secs(2)
        {
            self.typing_indicator = false;
        }
    }

    /// Scroll up by one message.
    pub fn scroll_up(&mut self) {
        self.auto_scroll = false;
        let i = self
            .list_state
            .selected()
            .unwrap_or(self.messages.len().saturating_sub(1));
        if i > 0 {
            self.list_state.select(Some(i - 1));
        }
    }

    /// Scroll down by one message.
    pub fn scroll_down(&mut self) {
        let i = self.list_state.selected().unwrap_or(0);
        if i + 1 < self.messages.len() {
            self.list_state.select(Some(i + 1));
        } else {
            self.scroll_to_bottom();
        }
    }

    /// Scroll to bottom (latest message).
    pub fn scroll_to_bottom(&mut self) {
        self.auto_scroll = true;
        if !self.messages.is_empty() {
            self.list_state.select(Some(self.messages.len() - 1));
        }
    }

    /// Scroll page up.
    pub fn scroll_page_up(&mut self, page_size: usize) {
        self.auto_scroll = false;
        let i = self
            .list_state
            .selected()
            .unwrap_or(self.messages.len().saturating_sub(1));
        let new_i = i.saturating_sub(page_size);
        self.list_state.select(Some(new_i));
    }

    /// Scroll page down.
    pub fn scroll_page_down(&mut self, page_size: usize) {
        let i = self.list_state.selected().unwrap_or(0);
        let new_i = (i + page_size).min(self.messages.len().saturating_sub(1));
        self.list_state.select(Some(new_i));
    }

    /// Toggle expansion of a tool call/result at the given message index.
    pub fn toggle_expand(&mut self, msg_idx: usize, is_tool_call: bool, item_idx: usize) {
        if let Some(msg) = self.messages.get_mut(msg_idx) {
            if is_tool_call {
                if let Some(tc) = msg.tool_calls.get_mut(item_idx) {
                    tc.expanded = !tc.expanded;
                }
            } else if let Some(tr) = msg.tool_results.get_mut(item_idx) {
                tr.expanded = !tr.expanded;
            }
        }
    }

    /// Render the conversation pane.
    pub fn render(&mut self, area: RRect, buf: &mut Buffer) {
        let items: Vec<ListItem> = self
            .messages
            .iter()
            .enumerate()
            .map(|(idx, msg)| self.render_message(idx, msg))
            .collect();

        let block = Block::default()
            .title(" Conversation ")
            .borders(Borders::ALL)
            .border_style(self.theme.style(SemanticRole::CanvasBorder))
            .style(self.theme.style(SemanticRole::CanvasBg));

        let list = List::new(items)
            .block(block)
            .highlight_style(
                Style::default()
                    .bg(self.theme.color(SemanticRole::CanvasBorder))
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("► ");

        // Render list with scrollbar
        let list_area = RRect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        };

        // We need to render the list and track scroll position
        // For now, render the list widget directly
        let mut state = self.list_state.clone();
        ratatui::widgets::StatefulWidget::render(list, list_area, buf, &mut state);
        self.list_state = state;

        // Render scrollbar if needed
        if self.messages.len() as u16 > list_area.height {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("▲"))
                .end_symbol(Some("▼"))
                .track_symbol(Some("│"))
                .thumb_symbol("█");
            let mut scrollbar_state = ratatui::widgets::ScrollbarState::new(self.messages.len())
                .position(self.list_state.selected().unwrap_or(0));
            ratatui::widgets::StatefulWidget::render(scrollbar, area, buf, &mut scrollbar_state);
        }
    }

    /// Render a single message as a ListItem.
    fn render_message(&self, idx: usize, msg: &RenderedMessage) -> ListItem<'_> {
        let mut lines = Vec::new();

        // Role chip + content
        match msg.message.role {
            MessageRole::User => {
                lines.push(Line::from(vec![
                    Span::styled(" user ", self.chip_style(SemanticRole::User)),
                    Span::raw(" "),
                    Span::styled(
                        msg.message.content.clone(),
                        self.theme.style(SemanticRole::User),
                    ),
                ]));
            }
            MessageRole::Assistant => {
                let mut spans = vec![
                    Span::styled(" assistant ", self.chip_style(SemanticRole::Assistant)),
                    Span::raw(" "),
                ];

                if msg.is_streaming {
                    // Streaming: show content + cursor
                    let content = msg.message.content.clone();
                    let cursor_pos = msg.stream_cursor_pos.min(content.len());
                    let before = content[..cursor_pos].to_string();
                    let after = if cursor_pos < content.len() {
                        content[cursor_pos..].to_string()
                    } else {
                        String::new()
                    };

                    spans.push(Span::styled(
                        before,
                        self.theme.style(SemanticRole::Assistant),
                    ));

                    // Blinking cursor
                    if !self.reduced_motion
                        && (Instant::now().elapsed().as_millis() / 500).is_multiple_of(2)
                    {
                        spans.push(Span::styled(
                            "█",
                            self.theme
                                .style(SemanticRole::Assistant)
                                .add_modifier(Modifier::SLOW_BLINK),
                        ));
                    } else if self.reduced_motion {
                        spans.push(Span::styled("|", self.theme.style(SemanticRole::Assistant)));
                    }

                    if !after.is_empty() {
                        spans.push(Span::styled(
                            after,
                            self.theme.style(SemanticRole::Assistant),
                        ));
                    }

                    // Typing indicator
                    if self.typing_indicator && !self.reduced_motion {
                        spans.push(Span::raw(" "));
                        spans.push(Span::styled(
                            TYPING_FRAMES[self.typing_frame],
                            self.theme.style(SemanticRole::Pending),
                        ));
                    } else if self.typing_indicator && self.reduced_motion {
                        spans.push(Span::raw(" …"));
                    }
                } else {
                    spans.push(Span::styled(
                        msg.message.content.clone(),
                        self.theme.style(SemanticRole::Assistant),
                    ));
                }

                lines.push(Line::from(spans));
            }
            MessageRole::Tool => {
                lines.push(Line::from(vec![
                    Span::styled(" tool ", self.chip_style(SemanticRole::Tool)),
                    Span::raw(" "),
                    Span::styled(
                        msg.message.content.clone(),
                        self.theme.style(SemanticRole::System),
                    ),
                ]));
            }
            MessageRole::System => {
                lines.push(Line::from(vec![
                    Span::styled(" system ", self.chip_style(SemanticRole::System)),
                    Span::raw(" "),
                    Span::styled(
                        msg.message.content.clone(),
                        self.theme.style(SemanticRole::System),
                    ),
                ]));
            }
        }

        // Tool calls
        for (tc_idx, tc) in msg.tool_calls.iter().enumerate() {
            lines.extend(self.render_tool_call(idx, tc_idx, tc));
        }

        // Tool results
        for (tr_idx, tr) in msg.tool_results.iter().enumerate() {
            lines.extend(self.render_tool_result(idx, tr_idx, tr));
        }

        // Truncate if too many lines
        if lines.len() > MAX_MESSAGE_LINES {
            lines.truncate(MAX_MESSAGE_LINES);
            lines.push(Line::from(vec![Span::styled(
                "... (truncated) ",
                self.theme.style(SemanticRole::CanvasMuted),
            )]));
        }

        ListItem::new(lines).style(self.theme.style(SemanticRole::CanvasBg))
    }

    /// Render a tool call.
    fn render_tool_call(
        &self,
        _msg_idx: usize,
        _tc_idx: usize,
        tc: &RenderedToolCall,
    ) -> Vec<Line<'_>> {
        let mut lines = Vec::new();
        let indent = "  │ ";
        let expanded = tc.expanded;

        // Status badge
        let (status_glyph, status_role) = match tc.status {
            ToolCallStatus::Pending => ("⟳", SemanticRole::Pending),
            ToolCallStatus::Ok => ("✓", SemanticRole::Ok),
            ToolCallStatus::Error => ("✗", SemanticRole::Error),
        };

        let expand_glyph = if expanded { "▼" } else { "▶" };

        lines.push(Line::from(vec![
            Span::raw(indent),
            Span::styled(expand_glyph, self.theme.style(SemanticRole::CanvasMuted)),
            Span::raw(" "),
            Span::styled(status_glyph, self.theme.style(status_role)),
            Span::raw(" "),
            Span::styled(tc.call.name.clone(), self.chip_style(SemanticRole::Tool)),
            Span::raw(" "),
            Span::styled(
                format!("({})", tc.call.id),
                self.theme.style(SemanticRole::CanvasMuted),
            ),
        ]));

        if expanded {
            // Show arguments as JSON
            let args_str = serde_json::to_string_pretty(&tc.call.args).unwrap_or_default();
            for line in args_str.lines().take(20) {
                lines.push(Line::from(vec![
                    Span::raw(indent),
                    Span::raw("  "),
                    Span::styled(
                        line.to_string(),
                        self.theme.style(SemanticRole::CanvasMuted),
                    ),
                ]));
            }
            if args_str.lines().count() > 20 {
                lines.push(Line::from(vec![
                    Span::raw(indent),
                    Span::raw("  "),
                    Span::styled(
                        "... (truncated)",
                        self.theme.style(SemanticRole::CanvasMuted),
                    ),
                ]));
            }
        }

        lines
    }

    /// Render a tool result.
    fn render_tool_result(
        &self,
        _msg_idx: usize,
        _tr_idx: usize,
        tr: &RenderedToolResult,
    ) -> Vec<Line<'_>> {
        let mut lines = Vec::new();
        let indent = "  │ ";
        let expanded = tr.expanded;

        let status_glyph = if tr.result.ok { "✓" } else { "✗" };
        let status_role = if tr.result.ok {
            SemanticRole::Ok
        } else {
            SemanticRole::Error
        };
        let expand_glyph = if expanded { "▼" } else { "▶" };

        lines.push(Line::from(vec![
            Span::raw(indent),
            Span::styled(expand_glyph, self.theme.style(SemanticRole::CanvasMuted)),
            Span::raw(" "),
            Span::styled(status_glyph, self.theme.style(status_role)),
            Span::raw(" "),
            Span::styled(tr.result.name.clone(), self.chip_style(SemanticRole::Tool)),
            Span::raw(" "),
            Span::styled(
                format!("({}ms)", tr.result.duration_ms),
                self.theme.style(SemanticRole::CanvasMuted),
            ),
            if tr.result.truncated {
                Span::styled(" ⟳", self.theme.style(SemanticRole::Warn))
            } else {
                Span::raw("")
            },
        ]));

        if expanded {
            // Show output
            for line in tr.result.output.lines().take(30) {
                lines.push(Line::from(vec![
                    Span::raw(indent),
                    Span::raw("  "),
                    Span::styled(line.to_string(), self.theme.style(SemanticRole::System)),
                ]));
            }
            if tr.result.output.lines().count() > 30 {
                lines.push(Line::from(vec![
                    Span::raw(indent),
                    Span::raw("  "),
                    Span::styled(
                        "... (truncated)",
                        self.theme.style(SemanticRole::CanvasMuted),
                    ),
                ]));
            }
        }

        lines
    }

    /// Get chip style for a semantic role.
    fn chip_style(&self, role: SemanticRole) -> Style {
        let (fg, bg) = match role {
            SemanticRole::User => (SemanticRole::CanvasBg, SemanticRole::User),
            SemanticRole::Assistant => (SemanticRole::CanvasBg, SemanticRole::Assistant),
            SemanticRole::Tool => (SemanticRole::CanvasBg, SemanticRole::Tool),
            SemanticRole::System => (SemanticRole::CanvasBg, SemanticRole::System),
            _ => (SemanticRole::CanvasFg, SemanticRole::CanvasBg),
        };
        Style::default()
            .fg(self.theme.color(fg))
            .bg(self.theme.color(bg))
            .add_modifier(Modifier::BOLD)
    }

    /// Get the number of messages.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Get selected message index.
    pub fn selected(&self) -> Option<usize> {
        self.list_state.selected()
    }

    /// Scroll to the first message (top).
    pub fn scroll_to_top(&mut self) {
        self.auto_scroll = false;
        if !self.messages.is_empty() {
            self.list_state.select(Some(0));
        }
    }
}

impl Default for ConversationPane {
    fn default() -> Self {
        Self::new(Theme::default_dark())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Message, MessageRole, ToolCall, ToolCallStatus, ToolResult};
    use std::sync::Arc;

    fn test_theme() -> Arc<Theme> {
        Theme::default_dark()
    }

    #[test]
    fn test_conversation_pane_new() {
        let pane = ConversationPane::new(test_theme());
        assert!(pane.is_empty());
        assert_eq!(pane.len(), 0);
    }

    #[test]
    fn test_add_user_message() {
        let mut pane = ConversationPane::new(test_theme());
        let msg = Message {
            role: MessageRole::User,
            content: "Hello".to_string(),
            tool_calls: None,
            tool_call_id: None,
            ts: 1,
        };
        pane.handle_event(&crate::events::Event::Message(msg));
        assert_eq!(pane.len(), 1);
    }

    #[test]
    fn test_add_assistant_message() {
        let mut pane = ConversationPane::new(test_theme());
        let msg = Message {
            role: MessageRole::Assistant,
            content: "Hi there".to_string(),
            tool_calls: None,
            tool_call_id: None,
            ts: 1,
        };
        pane.handle_event(&crate::events::Event::Message(msg));
        assert_eq!(pane.len(), 1);
    }

    #[test]
    fn test_streaming_tokens() {
        let mut pane = ConversationPane::new(test_theme());
        let msg = Message {
            role: MessageRole::Assistant,
            content: "Hello".to_string(),
            tool_calls: None,
            tool_call_id: None,
            ts: 1,
        };
        pane.handle_event(&crate::events::Event::Message(msg));
        pane.handle_event(&crate::events::Event::Token(crate::events::Token {
            delta: " world".to_string(),
            ts: 2,
        }));
        assert!(
            pane.messages
                .back()
                .unwrap()
                .message
                .content
                .contains("world")
        );
        assert!(pane.messages.back().unwrap().is_streaming);
    }

    #[test]
    fn test_tool_call_and_result() {
        let mut pane = ConversationPane::new(test_theme());
        let msg = Message {
            role: MessageRole::Assistant,
            content: "Let me run a tool".to_string(),
            tool_calls: None,
            tool_call_id: None,
            ts: 1,
        };
        pane.handle_event(&crate::events::Event::Message(msg));

        let call = ToolCall {
            id: "call-1".to_string(),
            name: "read".to_string(),
            args: serde_json::json!({"path": "test.txt"}),
            status: ToolCallStatus::Pending,
        };
        pane.handle_event(&crate::events::Event::ToolCall(call));

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
        pane.handle_event(&crate::events::Event::ToolResult(result));

        let last = pane.messages.back().unwrap();
        assert_eq!(last.tool_calls.len(), 1);
        assert_eq!(last.tool_results.len(), 1);
    }

    #[test]
    fn test_scroll() {
        let mut pane = ConversationPane::new(test_theme());
        for i in 0..10 {
            pane.handle_event(&crate::events::Event::Message(Message {
                role: MessageRole::User,
                content: format!("Msg {}", i),
                tool_calls: None,
                tool_call_id: None,
                ts: i as u64,
            }));
        }
        pane.scroll_to_bottom();
        assert_eq!(pane.selected(), Some(9));
        pane.scroll_up();
        assert_eq!(pane.selected(), Some(8));
        pane.scroll_down();
        assert_eq!(pane.selected(), Some(9));
    }

    #[test]
    fn test_auto_scroll_disabled_on_scroll_up() {
        let mut pane = ConversationPane::new(test_theme());
        for i in 0..5 {
            pane.handle_event(&crate::events::Event::Message(Message {
                role: MessageRole::User,
                content: format!("Msg {}", i),
                tool_calls: None,
                tool_call_id: None,
                ts: i as u64,
            }));
        }
        pane.scroll_to_bottom();
        assert!(pane.auto_scroll);
        pane.scroll_up();
        assert!(!pane.auto_scroll);
    }
}
