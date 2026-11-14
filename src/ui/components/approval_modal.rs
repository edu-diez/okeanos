//! Approval/confirm modal — centered, keyboard-first per DESIGN.md §5.4.
//!
//! Title "Approve tool call?", tool name + args preview.
//! Three actions: Allow / Deny / Allow always for this session.
//! Keyboard-first: Enter allows, Esc denies, arrow keys move focus.
//! High-contrast focus ring.

use crate::events::{ApprovalDecision, ApprovalRequest};
use crate::ui::theme::{SemanticRole, Theme};
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Rect as RRect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget},
};
use std::sync::Arc;

/// Approval modal state.
#[derive(Debug)]
pub struct ApprovalModal {
    request: Option<ApprovalRequest>,
    theme: Arc<Theme>,
    reduced_motion: bool,
    focused_button: usize, // 0=Allow, 1=Deny, 2=Allow Always
    visible: bool,
}

impl ApprovalModal {
    pub fn new(theme: Arc<Theme>) -> Self {
        let reduced_motion = theme.reduced_motion.enabled;
        Self {
            request: None,
            theme,
            reduced_motion,
            focused_button: 0,
            visible: false,
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

    /// Show the approval modal with a request.
    pub fn show(&mut self, request: ApprovalRequest) {
        self.request = Some(request);
        self.visible = true;
        self.focused_button = 0; // Default to Allow
    }

    /// Hide the approval modal.
    pub fn hide(&mut self) {
        self.visible = false;
        self.request = None;
    }

    /// Check if modal is visible.
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Get the current request.
    pub fn request(&self) -> Option<&ApprovalRequest> {
        self.request.as_ref()
    }

    /// Handle keyboard input.
    /// Returns Some(decision) if a decision was made, None otherwise.
    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Option<ApprovalDecision> {
        if !self.visible {
            return None;
        }

        use crossterm::event::{KeyCode, KeyModifiers};

        match key.code {
            KeyCode::Enter => {
                let decision = match self.focused_button {
                    0 => ApprovalDecision::Allow,
                    1 => ApprovalDecision::Deny,
                    2 => ApprovalDecision::AllowAlways,
                    _ => ApprovalDecision::Deny,
                };
                self.hide();
                Some(decision)
            }
            KeyCode::Esc => {
                self.hide();
                Some(ApprovalDecision::Deny)
            }
            KeyCode::Tab | KeyCode::Right => {
                self.focused_button = (self.focused_button + 1) % 3;
                None
            }
            KeyCode::BackTab | KeyCode::Left => {
                self.focused_button = (self.focused_button + 2) % 3;
                None
            }
            KeyCode::Char('a') | KeyCode::Char('A')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                // Ctrl+A = Allow Always
                self.focused_button = 2;
                None
            }
            KeyCode::Char('d') | KeyCode::Char('D')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                // Ctrl+D = Deny
                self.focused_button = 1;
                None
            }
            _ => None,
        }
    }

    /// Render the modal.
    pub fn render(&self, area: RRect, buf: &mut Buffer) {
        if !self.visible {
            return;
        }

        let request = match &self.request {
            Some(r) => r,
            None => return,
        };

        // Clear the background area
        Clear.render(area, buf);

        // Modal dimensions
        let modal_width = (area.width as f32 * 0.8).clamp(50.0, 80.0) as u16;
        let modal_height = 12u16; // Fixed height for modal

        let modal_area = centered_rect(modal_width, modal_height, area);

        // Modal background
        let block = Block::default()
            .title(" Approve Tool Call? ")
            .borders(Borders::ALL)
            .border_style(
                self.theme
                    .style(SemanticRole::Error)
                    .add_modifier(Modifier::BOLD),
            )
            .style(
                self.theme
                    .style_bg(SemanticRole::CanvasFg, SemanticRole::CanvasBg),
            );

        let inner = RRect {
            x: modal_area.x + 1,
            y: modal_area.y + 1,
            width: modal_area.width.saturating_sub(2),
            height: modal_area.height.saturating_sub(2),
        };

        block.render(modal_area, buf);

        // Content layout
        let content_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // Tool name
                Constraint::Length(3), // Args preview
                Constraint::Min(1),    // Spacer
                Constraint::Length(1), // Buttons
            ])
            .split(inner);

        // Tool name
        let tool_name = Line::from(vec![
            Span::styled("Tool: ", self.theme.style(SemanticRole::CanvasMuted)),
            Span::styled(&request.tool, self.chip_style(SemanticRole::Tool)),
        ]);
        Paragraph::new(tool_name)
            .alignment(Alignment::Left)
            .render(content_layout[0], buf);

        // Args preview (truncated)
        let args_str = serde_json::to_string(&request.args).unwrap_or_default();
        let preview = if args_str.len() > 200 {
            format!("{}...", &args_str[..200])
        } else {
            args_str
        };

        let args_lines = vec![
            Line::from(vec![Span::styled(
                "Args: ",
                self.theme.style(SemanticRole::CanvasMuted),
            )]),
            Line::from(vec![Span::styled(
                preview,
                self.theme.style(SemanticRole::System),
            )]),
        ];
        Paragraph::new(args_lines)
            .alignment(Alignment::Left)
            .render(content_layout[1], buf);

        // Buttons
        let buttons = self.render_buttons();
        Paragraph::new(buttons)
            .alignment(Alignment::Center)
            .render(content_layout[3], buf);
    }

    /// Render the three action buttons.
    fn render_buttons(&self) -> Line<'_> {
        let buttons = [
            (" Allow ", ApprovalDecision::Allow),
            (" Deny ", ApprovalDecision::Deny),
            (" Allow Always ", ApprovalDecision::AllowAlways),
        ];

        let mut spans = Vec::new();

        for (i, (label, _)) in buttons.iter().enumerate() {
            let is_focused = i == self.focused_button;

            let style = if is_focused {
                self.theme
                    .style_bg(SemanticRole::CanvasBg, SemanticRole::Tool)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else if i == 1 {
                // Deny button gets error styling
                self.theme
                    .style_bg(SemanticRole::CanvasBg, SemanticRole::Error)
                    .add_modifier(Modifier::BOLD)
            } else if i == 2 {
                // Allow Always gets warn styling
                self.theme
                    .style_bg(SemanticRole::CanvasBg, SemanticRole::Warn)
                    .add_modifier(Modifier::BOLD)
            } else {
                // Allow gets ok styling
                self.theme
                    .style_bg(SemanticRole::CanvasBg, SemanticRole::Ok)
                    .add_modifier(Modifier::BOLD)
            };

            spans.push(Span::styled(label.to_string(), style));
            spans.push(Span::raw("  "));
        }

        // Remove trailing spaces
        if let Some(last) = spans.last_mut()
            && last.content.as_ref().trim().is_empty()
        {
            spans.pop();
        }

        Line::from(spans)
    }

    /// Get chip style for a semantic role.
    fn chip_style(&self, role: SemanticRole) -> Style {
        let (fg, bg) = match role {
            SemanticRole::Tool => (SemanticRole::CanvasBg, SemanticRole::Tool),
            SemanticRole::Ok => (SemanticRole::CanvasBg, SemanticRole::Ok),
            SemanticRole::Error => (SemanticRole::CanvasBg, SemanticRole::Error),
            SemanticRole::Warn => (SemanticRole::CanvasBg, SemanticRole::Warn),
            SemanticRole::Pending => (SemanticRole::CanvasBg, SemanticRole::Pending),
            _ => (SemanticRole::CanvasFg, SemanticRole::CanvasBg),
        };
        Style::default()
            .fg(self.theme.color(fg))
            .bg(self.theme.color(bg))
            .add_modifier(Modifier::BOLD)
    }
}

/// Helper to create a centered rect.
fn centered_rect(width: u16, height: u16, r: RRect) -> RRect {
    let x = r.x.saturating_add((r.width.saturating_sub(width)) / 2);
    let y = r.y.saturating_add((r.height.saturating_sub(height)) / 2);
    RRect::new(x, y, width.min(r.width), height.min(r.height))
}

impl Default for ApprovalModal {
    fn default() -> Self {
        Self::new(Theme::default_dark())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::ApprovalRequest;
    use std::sync::Arc;

    fn test_theme() -> Arc<Theme> {
        Theme::default_dark()
    }

    #[test]
    fn test_approval_modal_new() {
        let modal = ApprovalModal::new(test_theme());
        assert!(!modal.is_visible());
        assert!(modal.request().is_none());
    }

    #[test]
    fn test_show_hide() {
        let mut modal = ApprovalModal::new(test_theme());
        let request = ApprovalRequest {
            id: "test-1".to_string(),
            tool: "write".to_string(),
            args: serde_json::json!({"path": "test.txt", "content": "hello"}),
        };
        modal.show(request.clone());
        assert!(modal.is_visible());
        assert_eq!(modal.request().unwrap().id, "test-1");
        assert_eq!(modal.focused_button, 0);

        modal.hide();
        assert!(!modal.is_visible());
        assert!(modal.request().is_none());
    }

    #[test]
    fn test_handle_key_enter_allow() {
        let mut modal = ApprovalModal::new(test_theme());
        let request = ApprovalRequest {
            id: "test-1".to_string(),
            tool: "write".to_string(),
            args: serde_json::json!({}),
        };
        modal.show(request);

        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        );
        let decision = modal.handle_key(key);
        assert_eq!(decision, Some(ApprovalDecision::Allow));
        assert!(!modal.is_visible());
    }

    #[test]
    fn test_handle_key_esc_deny() {
        let mut modal = ApprovalModal::new(test_theme());
        let request = ApprovalRequest {
            id: "test-1".to_string(),
            tool: "write".to_string(),
            args: serde_json::json!({}),
        };
        modal.show(request);

        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        );
        let decision = modal.handle_key(key);
        assert_eq!(decision, Some(ApprovalDecision::Deny));
        assert!(!modal.is_visible());
    }

    #[test]
    fn test_handle_key_tab_cycle() {
        let mut modal = ApprovalModal::new(test_theme());
        let request = ApprovalRequest {
            id: "test-1".to_string(),
            tool: "write".to_string(),
            args: serde_json::json!({}),
        };
        modal.show(request);

        // Tab cycles through buttons
        assert_eq!(modal.focused_button, 0);
        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Tab,
            crossterm::event::KeyModifiers::NONE,
        );
        modal.handle_key(key);
        assert_eq!(modal.focused_button, 1);
        modal.handle_key(key);
        assert_eq!(modal.focused_button, 2);
        modal.handle_key(key);
        assert_eq!(modal.focused_button, 0);
    }

    #[test]
    fn test_handle_key_shift_tab_cycle_back() {
        let mut modal = ApprovalModal::new(test_theme());
        let request = ApprovalRequest {
            id: "test-1".to_string(),
            tool: "write".to_string(),
            args: serde_json::json!({}),
        };
        modal.show(request);

        assert_eq!(modal.focused_button, 0);
        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::BackTab,
            crossterm::event::KeyModifiers::NONE,
        );
        modal.handle_key(key);
        assert_eq!(modal.focused_button, 2);
    }

    #[test]
    fn test_handle_key_allow_always() {
        let mut modal = ApprovalModal::new(test_theme());
        let request = ApprovalRequest {
            id: "test-1".to_string(),
            tool: "write".to_string(),
            args: serde_json::json!({}),
        };
        modal.show(request);

        // Tab to Allow Always (index 2) and press Enter
        modal.focused_button = 2;
        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        );
        let decision = modal.handle_key(key);
        assert_eq!(decision, Some(ApprovalDecision::AllowAlways));
    }
}
