//! Welcome screen component — shown on startup when no config and no prompt.
//!
//! Per DESIGN.md §5.10: centered panel with app name/version, provider/model status,
//! key bindings summary, "Press any key to begin" or "Run `okeanos init` to configure".
//! Dismisses on first keystroke or when composer receives focus.

use crate::ui::theme::{SemanticRole, Theme};
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Rect as RRect},
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget},
};
use std::sync::Arc;

/// Welcome screen state.
#[derive(Debug)]
pub struct WelcomeScreen {
    theme: Arc<Theme>,
    reduced_motion: bool,
    visible: bool,
    provider_configured: bool,
    model_name: Option<String>,
    version: String,
}

impl WelcomeScreen {
    pub fn new(theme: Arc<Theme>, version: String) -> Self {
        let reduced_motion = theme.reduced_motion.enabled;
        Self {
            theme,
            reduced_motion,
            visible: true,
            provider_configured: false,
            model_name: None,
            version,
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

    /// Update provider/model configuration status.
    pub fn set_config_status(&mut self, provider_configured: bool, model_name: Option<String>) {
        self.provider_configured = provider_configured;
        self.model_name = model_name;
    }

    /// Show the welcome screen.
    pub fn show(&mut self) {
        self.visible = true;
    }

    /// Hide the welcome screen.
    pub fn hide(&mut self) {
        self.visible = false;
    }

    /// Check if visible.
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Handle keyboard input. Returns true if the key was handled (dismissing the screen).
    pub fn handle_key(&mut self, _key: crossterm::event::KeyEvent) -> bool {
        if !self.visible {
            return false;
        }

        // Any key dismisses the welcome screen
        self.hide();
        true
    }

    /// Render the welcome screen.
    pub fn render(&self, area: RRect, buf: &mut Buffer) {
        if !self.visible {
            return;
        }

        Clear.render(area, buf);

        // Centered panel dimensions
        let panel_width = (area.width as f32 * 0.7).clamp(60.0, 90.0) as u16;
        let panel_height = 18u16;

        let panel_area = centered_rect(panel_width, panel_height, area);

        // Background block
        let block = Block::default()
            .title(" Welcome ")
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
            x: panel_area.x + 1,
            y: panel_area.y + 1,
            width: panel_area.width.saturating_sub(2),
            height: panel_area.height.saturating_sub(2),
        };

        block.render(panel_area, buf);

        // Build content
        let mut lines = Vec::new();

        // App name and version
        lines.push(Line::from(vec![
            Span::styled(
                "okeanos ",
                self.theme
                    .style(SemanticRole::Tool)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                self.version.clone(),
                self.theme.style(SemanticRole::CanvasMuted),
            ),
        ]));

        lines.push(Line::from(""));

        // Provider/Model status
        let provider_status = if self.provider_configured {
            let model = self
                .model_name
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            Line::from(vec![
                Span::styled("Provider: ", self.theme.style(SemanticRole::CanvasMuted)),
                Span::styled("Configured ✓", self.theme.style(SemanticRole::Ok)),
                Span::raw("  "),
                Span::styled("Model: ", self.theme.style(SemanticRole::CanvasMuted)),
                Span::styled(model, self.theme.style(SemanticRole::Tool)),
            ])
        } else {
            Line::from(vec![
                Span::styled("Provider: ", self.theme.style(SemanticRole::CanvasMuted)),
                Span::styled("Not configured", self.theme.style(SemanticRole::Warn)),
            ])
        };
        lines.push(provider_status);
        lines.push(Line::from(""));

        // Key bindings
        lines.push(Line::from(vec![Span::styled(
            "Key Bindings:",
            self.theme
                .style(SemanticRole::Tool)
                .add_modifier(Modifier::BOLD),
        )]));

        let bindings = [
            (":", "Open command palette"),
            ("Ctrl+C", "Cancel in-flight turn / Exit"),
            ("Ctrl+D", "Exit"),
            ("Enter", "Send prompt (in composer)"),
            ("Shift+Enter", "New line (in composer)"),
            ("↑/↓", "Scroll conversation"),
            ("PgUp/PgDn", "Page scroll"),
            ("Home/End", "Jump to top/bottom"),
            ("Tab", "Cycle focus"),
            ("f", "Cycle log filter"),
            ("Space", "Toggle todo"),
        ];

        for (key, desc) in bindings {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:<12}", key),
                    self.theme.style(SemanticRole::Tool),
                ),
                Span::styled(desc, self.theme.style(SemanticRole::CanvasMuted)),
            ]));
        }

        lines.push(Line::from(""));

        // Call to action
        if self.provider_configured {
            lines.push(Line::from(vec![Span::styled(
                "Press any key to begin…",
                self.theme
                    .style(SemanticRole::Ok)
                    .add_modifier(Modifier::BOLD),
            )]));
        } else {
            lines.push(Line::from(vec![
                Span::styled("Run ", self.theme.style(SemanticRole::CanvasMuted)),
                Span::styled(
                    "`okeanos init`",
                    self.theme
                        .style(SemanticRole::Tool)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" to configure", self.theme.style(SemanticRole::CanvasMuted)),
            ]));
        }

        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Left)
            .style(self.theme.style(SemanticRole::CanvasBg));

        paragraph.render(inner, buf);
    }
}

/// Helper to create a centered rect.
fn centered_rect(width: u16, height: u16, r: RRect) -> RRect {
    let x = r.x.saturating_add((r.width.saturating_sub(width)) / 2);
    let y = r.y.saturating_add((r.height.saturating_sub(height)) / 2);
    RRect::new(x, y, width.min(r.width), height.min(r.height))
}

impl Default for WelcomeScreen {
    fn default() -> Self {
        Self::new(Theme::default_dark(), env!("CARGO_PKG_VERSION").to_string())
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
    fn test_welcome_screen_new() {
        let screen = WelcomeScreen::new(test_theme(), "0.1.0".to_string());
        assert!(screen.is_visible());
    }

    #[test]
    fn test_show_hide() {
        let mut screen = WelcomeScreen::new(test_theme(), "0.1.0".to_string());
        screen.hide();
        assert!(!screen.is_visible());
        screen.show();
        assert!(screen.is_visible());
    }

    #[test]
    fn test_config_status() {
        let mut screen = WelcomeScreen::new(test_theme(), "0.1.0".to_string());
        screen.set_config_status(true, Some("gpt-4o-mini".to_string()));
        assert!(screen.provider_configured);
        assert_eq!(screen.model_name, Some("gpt-4o-mini".to_string()));
    }

    #[test]
    fn test_handle_key_dismisses() {
        let mut screen = WelcomeScreen::new(test_theme(), "0.1.0".to_string());
        assert!(screen.is_visible());
        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::NONE,
        );
        screen.handle_key(key);
        assert!(!screen.is_visible());
    }
}
