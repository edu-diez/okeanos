//! Status bar component — always visible status per DESIGN.md §5.6.
//!
//! Left-to-right: model name, tokens in/out, cost, elapsed time, retry count, mode dot.
//! Two rows at comfortable height, one row at minimum height.

use crate::events::Status;
use crate::ui::theme::{SemanticRole, Theme};
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Rect as RRect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use std::sync::Arc;
use std::time::Instant;

/// Status bar component.
#[derive(Debug)]
pub struct StatusBar {
    status: Option<Status>,
    theme: Arc<Theme>,
    reduced_motion: bool,
    last_render: Instant,
    mode: String, // "interactive" | "headless" | "rpc"
    permission: Option<crate::harness::ModeHandle>,
    width: u16,
}

impl StatusBar {
    pub fn new(theme: Arc<Theme>) -> Self {
        let reduced_motion = theme.reduced_motion.enabled;
        Self {
            status: None,
            theme,
            reduced_motion,
            last_render: Instant::now(),
            mode: "interactive".to_string(),
            permission: None,
            width: 80,
        }
    }

    /// Show the live permission mode (Shift+Tab cycles it).
    pub fn set_permission_mode(&mut self, handle: crate::harness::ModeHandle) {
        self.permission = Some(handle);
    }

    fn permission_span(&self) -> Span<'static> {
        match &self.permission {
            Some(h) => {
                let mode = h.get();
                let role = match mode {
                    crate::harness::PermissionMode::Yolo => SemanticRole::Error,
                    crate::harness::PermissionMode::Default => SemanticRole::CanvasMuted,
                    _ => SemanticRole::Tool,
                };
                Span::styled(format!("[{mode}]"), self.theme.style(role))
            }
            None => Span::raw(""),
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

    /// Update the status from an event.
    pub fn update(&mut self, status: Status) {
        self.status = Some(status);
    }

    /// Set the mode indicator.
    pub fn set_mode(&mut self, mode: &str) {
        self.mode = mode.to_string();
    }

    /// Set the width for layout calculations.
    pub fn set_width(&mut self, width: u16) {
        self.width = width;
    }

    /// Check if we should use compact (single-row) layout.
    fn is_compact(&self) -> bool {
        self.width < 100
    }

    /// Render the status bar.
    pub fn render(&mut self, area: RRect, buf: &mut Buffer) {
        self.last_render = Instant::now();

        let status = match &self.status {
            Some(s) => s,
            None => return,
        };

        if self.is_compact() {
            self.render_compact(area, buf, status);
        } else {
            self.render_full(area, buf, status);
        }
    }

    /// Compact single-row layout (minimum/narrow).
    fn render_compact(&self, area: RRect, buf: &mut Buffer, status: &Status) {
        let line = self.build_compact_line(status);
        let paragraph = Paragraph::new(line)
            .style(self.theme.style(SemanticRole::CanvasBg))
            .alignment(Alignment::Left);
        paragraph.render(area, buf);
    }

    /// Full two-row layout (comfortable).
    fn render_full(&self, area: RRect, buf: &mut Buffer, status: &Status) {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(area);

        let line1 = self.build_full_line1(status);
        let line2 = self.build_full_line2(status);

        Paragraph::new(line1)
            .style(self.theme.style(SemanticRole::CanvasBg))
            .alignment(Alignment::Left)
            .render(rows[0], buf);

        Paragraph::new(line2)
            .style(self.theme.style(SemanticRole::CanvasBg))
            .alignment(Alignment::Left)
            .render(rows[1], buf);
    }

    /// Build compact single line.
    fn build_compact_line(&self, status: &Status) -> Line<'_> {
        let elapsed = self.format_elapsed(status.elapsed_ms);
        let cost = format!("${:.4}", status.cost_usd);
        let tokens = format!("{}/{}", status.tokens_in, status.tokens_out);

        Line::from(vec![
            Span::styled(
                status.model.clone(),
                self.theme
                    .style(SemanticRole::Tool)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" | "),
            Span::styled("tokens: ", self.theme.style(SemanticRole::CanvasMuted)),
            Span::styled(tokens.clone(), self.theme.style(SemanticRole::CanvasFg)),
            Span::raw(" | "),
            Span::styled("cost: ", self.theme.style(SemanticRole::CanvasMuted)),
            Span::styled(cost.clone(), self.cost_style(status.cost_usd)),
            Span::raw(" | "),
            Span::styled("elapsed: ", self.theme.style(SemanticRole::CanvasMuted)),
            Span::styled(elapsed.clone(), self.theme.style(SemanticRole::CanvasFg)),
            Span::raw(" | "),
            Span::styled("retries: ", self.theme.style(SemanticRole::CanvasMuted)),
            Span::styled(status.retries.to_string(), self.retry_style(status.retries)),
            Span::raw(" "),
            self.mode_dot(),
            Span::raw(" "),
            self.permission_span(),
        ])
    }

    /// Build full layout line 1: model | tokens | cost
    fn build_full_line1(&self, status: &Status) -> Line<'_> {
        let cost = format!("${:.4}", status.cost_usd);
        let tokens = format!("{}/{}", status.tokens_in, status.tokens_out);

        Line::from(vec![
            Span::styled(
                status.model.clone(),
                self.theme
                    .style(SemanticRole::Tool)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("tokens: ", self.theme.style(SemanticRole::CanvasMuted)),
            Span::styled(tokens.clone(), self.theme.style(SemanticRole::CanvasFg)),
            Span::raw("  "),
            Span::styled("cost: ", self.theme.style(SemanticRole::CanvasMuted)),
            Span::styled(cost.clone(), self.cost_style(status.cost_usd)),
            Span::raw("  "),
            self.mode_dot(),
        ])
    }

    /// Build full layout line 2: elapsed | retries | mode
    fn build_full_line2(&self, status: &Status) -> Line<'_> {
        let elapsed = self.format_elapsed(status.elapsed_ms);

        Line::from(vec![
            Span::styled("elapsed: ", self.theme.style(SemanticRole::CanvasMuted)),
            Span::styled(elapsed.clone(), self.theme.style(SemanticRole::CanvasFg)),
            Span::raw("  "),
            Span::styled("retries: ", self.theme.style(SemanticRole::CanvasMuted)),
            Span::styled(status.retries.to_string(), self.retry_style(status.retries)),
            Span::raw("  "),
            Span::styled("mode: ", self.theme.style(SemanticRole::CanvasMuted)),
            Span::styled(self.mode.clone(), self.theme.style(SemanticRole::Tool)),
            Span::raw("  "),
            self.permission_span(),
        ])
    }

    /// Format elapsed milliseconds as human-readable string.
    fn format_elapsed(&self, ms: u64) -> String {
        let secs = ms / 1000;
        if secs < 60 {
            format!("{}s", secs)
        } else if secs < 3600 {
            format!("{}m{}s", secs / 60, secs % 60)
        } else {
            format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
        }
    }

    /// Cost color style based on amount.
    fn cost_style(&self, cost: f64) -> Style {
        if cost > 1.0 {
            self.theme.style(SemanticRole::Error)
        } else if cost > 0.10 {
            self.theme.style(SemanticRole::Warn)
        } else {
            self.theme.style(SemanticRole::Ok)
        }
    }

    /// Retry count color style.
    fn retry_style(&self, retries: u32) -> Style {
        if retries > 5 {
            self.theme.style(SemanticRole::Error)
        } else if retries > 0 {
            self.theme.style(SemanticRole::Warn)
        } else {
            self.theme.style(SemanticRole::Ok)
        }
    }

    /// Mode indicator dot.
    fn mode_dot(&self) -> Span<'_> {
        let (glyph, role) = match self.mode.as_str() {
            "interactive" => ("●", SemanticRole::Ok),
            "headless" => ("○", SemanticRole::System),
            "rpc" => ("◆", SemanticRole::Tool),
            _ => ("?", SemanticRole::CanvasMuted),
        };
        Span::styled(glyph, self.theme.style(role).add_modifier(Modifier::BOLD))
    }
}

impl Default for StatusBar {
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
    fn test_status_bar_new() {
        let bar = StatusBar::new(test_theme());
        assert_eq!(bar.mode, "interactive");
    }

    #[test]
    fn test_format_elapsed() {
        let bar = StatusBar::new(test_theme());
        assert_eq!(bar.format_elapsed(500), "0s");
        assert_eq!(bar.format_elapsed(1500), "1s");
        assert_eq!(bar.format_elapsed(65000), "1m5s");
        assert_eq!(bar.format_elapsed(3661000), "1h1m");
    }

    #[test]
    fn test_cost_style() {
        let bar = StatusBar::new(test_theme());
        // Just verify it returns a style (can't easily test colors without rendering)
        let _ = bar.cost_style(0.01);
        let _ = bar.cost_style(0.50);
        let _ = bar.cost_style(2.00);
    }

    #[test]
    fn test_retry_style() {
        let bar = StatusBar::new(test_theme());
        let _ = bar.retry_style(0);
        let _ = bar.retry_style(3);
        let _ = bar.retry_style(10);
    }

    #[test]
    fn test_mode_dot() {
        let bar = StatusBar::new(test_theme());
        let _ = bar.mode_dot();
    }

    #[test]
    fn test_set_mode() {
        let mut bar = StatusBar::new(test_theme());
        bar.set_mode("headless");
        assert_eq!(bar.mode, "headless");
        bar.set_mode("rpc");
        assert_eq!(bar.mode, "rpc");
    }
}
