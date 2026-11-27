//! Configuration modal component — stepper form for in-TUI provider setup.
//!
//! Per tui-design.md §5.11: centered overlay (~70% width, max 100 cols). Stepper flow:
//! Step 1: Provider kind → Step 2: Base URL → Step 3: API key → Step 4: Model →
//! Step 5: Confirm & write. Writes `okeanos.toml` at git root/CWD on save.

use crate::events::ConfigModalAction;
use crate::ui::theme::{SemanticRole, Theme};
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Rect as RRect},
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget},
};
use std::path::PathBuf;
use std::sync::Arc;

/// Provider kind options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    OpenRouter,
    OpenAICompatible,
    Ollama,
}

impl ProviderKind {
    pub fn all() -> [ProviderKind; 3] {
        [
            ProviderKind::OpenRouter,
            ProviderKind::OpenAICompatible,
            ProviderKind::Ollama,
        ]
    }

    pub fn label(&self) -> &'static str {
        match self {
            ProviderKind::OpenRouter => "OpenRouter (OpenAI-compatible)",
            ProviderKind::OpenAICompatible => "Custom OpenAI-compatible",
            ProviderKind::Ollama => "Ollama (local)",
        }
    }

    pub fn default_base_url(&self) -> &'static str {
        match self {
            ProviderKind::OpenRouter => "https://openrouter.ai/api/v1",
            ProviderKind::OpenAICompatible => "https://api.openai.com/v1",
            ProviderKind::Ollama => "http://localhost:11434/v1",
        }
    }

    pub fn requires_api_key(&self) -> bool {
        !matches!(self, ProviderKind::Ollama)
    }
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Configuration form data collected across the stepper.
#[derive(Debug, Clone, Default)]
pub struct ConfigFormData {
    pub provider_kind: Option<ProviderKind>,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

/// Current step in the stepper (1-indexed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigStep {
    Provider = 1,
    BaseUrl = 2,
    ApiKey = 3,
    Model = 4,
    Confirm = 5,
}

impl ConfigStep {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn title(self) -> &'static str {
        match self {
            ConfigStep::Provider => "Step 1/5 — Provider",
            ConfigStep::BaseUrl => "Step 2/5 — Base URL",
            ConfigStep::ApiKey => "Step 3/5 — API Key",
            ConfigStep::Model => "Step 4/5 — Model",
            ConfigStep::Confirm => "Step 5/5 — Confirm & Save",
        }
    }
}

/// Modal result returned to the caller after the modal closes.
#[derive(Debug, Clone)]
pub enum ConfigModalResult {
    /// User completed the form and config was saved.
    Saved { path: PathBuf, data: ConfigFormData },
    /// User cancelled/aborted the configuration.
    Cancelled,
}

/// Configuration modal state.
#[derive(Debug)]
pub struct ConfigModal {
    theme: Arc<Theme>,
    reduced_motion: bool,
    visible: bool,
    step: ConfigStep,
    form: ConfigFormData,
    provider_selected: usize, // index into ProviderKind::all()
    api_key_masked: bool,
    result_tx: Option<tokio::sync::mpsc::Sender<ConfigModalResult>>,
}

impl ConfigModal {
    pub fn new(theme: Arc<Theme>) -> Self {
        let reduced_motion = theme.reduced_motion.enabled;
        Self {
            theme,
            reduced_motion,
            visible: false,
            step: ConfigStep::Provider,
            form: ConfigFormData::default(),
            provider_selected: 0,
            api_key_masked: true,
            result_tx: None,
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

    /// Set the result channel sender so the modal can report completion.
    pub fn set_result_tx(&mut self, tx: tokio::sync::mpsc::Sender<ConfigModalResult>) {
        self.result_tx = Some(tx);
    }

    /// Show the modal.
    pub fn show(&mut self) {
        self.visible = true;
        self.step = ConfigStep::Provider;
        self.form = ConfigFormData::default();
        self.provider_selected = 0;
        self.api_key_masked = true;
        // Pre-select the default provider
        self.form.provider_kind = Some(ProviderKind::all()[self.provider_selected]);
        self.form.base_url = ProviderKind::all()[self.provider_selected]
            .default_base_url()
            .to_string();
    }

    /// Hide the modal.
    pub fn hide(&mut self) {
        self.visible = false;
    }

    /// Check if visible.
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Get current step.
    pub fn step(&self) -> ConfigStep {
        self.step
    }

    /// Handle keyboard input. Returns true if the key was handled.
    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        if !self.visible {
            return false;
        }

        use crossterm::event::{KeyCode, KeyModifiers};

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+C: abort entirely
                self.abort();
                return true;
            }
            KeyCode::Esc => {
                // Esc: back/cancel (on step 1, cancel entirely)
                if self.step == ConfigStep::Provider {
                    self.abort();
                } else {
                    self.prev_step();
                }
                return true;
            }
            KeyCode::Enter => {
                // Enter: next step / confirm
                if self.step == ConfigStep::Confirm {
                    self.save_and_complete();
                } else {
                    self.next_step();
                }
                return true;
            }
            KeyCode::Tab => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.prev_field();
                } else {
                    self.next_field();
                }
                return true;
            }
            _ => {}
        }

        // Field-specific input handling
        match self.step {
            ConfigStep::Provider => self.handle_provider_key(key),
            ConfigStep::BaseUrl => Self::handle_text_input(key, &mut self.form.base_url),
            ConfigStep::ApiKey => Self::handle_text_input(key, &mut self.form.api_key),
            ConfigStep::Model => Self::handle_text_input(key, &mut self.form.model),
            ConfigStep::Confirm => false, // no text input on confirm
        }
    }

    fn handle_provider_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        use crossterm::event::KeyCode;
        let providers = ProviderKind::all();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.provider_selected = self.provider_selected.saturating_sub(1);
                true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.provider_selected = (self.provider_selected + 1).min(providers.len() - 1);
                true
            }
            KeyCode::Char('1') => {
                self.provider_selected = 0;
                true
            }
            KeyCode::Char('2') => {
                if providers.len() > 1 {
                    self.provider_selected = 1;
                }
                true
            }
            KeyCode::Char('3') => {
                if providers.len() > 2 {
                    self.provider_selected = 2;
                }
                true
            }
            _ => false,
        }
    }

    /// Handle text input for a single field (static to avoid borrow conflicts with &mut self).
    fn handle_text_input(key: crossterm::event::KeyEvent, text: &mut String) -> bool {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Backspace => {
                text.pop();
                true
            }
            KeyCode::Char(c) => {
                text.push(c);
                true
            }
            _ => false,
        }
    }

    fn next_step(&mut self) {
        if !self.validate_current_step() {
            return;
        }
        self.step = match self.step {
            ConfigStep::Provider => {
                self.form.provider_kind = Some(ProviderKind::all()[self.provider_selected]);
                // Prefill base URL based on provider
                let kind = self.form.provider_kind.unwrap();
                self.form.base_url = kind.default_base_url().to_string();
                ConfigStep::BaseUrl
            }
            ConfigStep::BaseUrl => ConfigStep::ApiKey,
            ConfigStep::ApiKey => {
                if self
                    .form
                    .provider_kind
                    .is_none_or(|k| !k.requires_api_key())
                {
                    ConfigStep::Confirm
                } else {
                    ConfigStep::Model
                }
            }
            ConfigStep::Model => ConfigStep::Confirm,
            ConfigStep::Confirm => ConfigStep::Confirm,
        };
        self.emit_step_event();
    }

    fn prev_step(&mut self) {
        self.step = match self.step {
            ConfigStep::Provider => ConfigStep::Provider,
            ConfigStep::BaseUrl => ConfigStep::Provider,
            ConfigStep::ApiKey => ConfigStep::BaseUrl,
            ConfigStep::Model => ConfigStep::ApiKey,
            ConfigStep::Confirm => ConfigStep::Model,
        };
        self.emit_step_event();
    }

    fn next_field(&mut self) {
        // Within a step, move focus; between steps, Enter advances
        // For now, Tab on a step just stays (single field per step)
    }

    fn prev_field(&mut self) {
        // Same as next_field — single field per step
    }

    fn validate_current_step(&self) -> bool {
        match self.step {
            ConfigStep::Provider => self.form.provider_kind.is_some(),
            ConfigStep::BaseUrl => !self.form.base_url.trim().is_empty(),
            ConfigStep::ApiKey => {
                let kind = self.form.provider_kind.unwrap_or(ProviderKind::OpenRouter);
                if !kind.requires_api_key() {
                    return true;
                }
                !self.form.api_key.trim().is_empty()
            }
            ConfigStep::Model => !self.form.model.trim().is_empty(),
            ConfigStep::Confirm => true,
        }
    }

    fn abort(&mut self) {
        let _ = self
            .result_tx
            .as_ref()
            .and_then(|tx| tx.try_send(ConfigModalResult::Cancelled).ok());
        self.visible = false;
        self.emit_close_event(false);
    }

    fn save_and_complete(&mut self) {
        if !self.validate_current_step() {
            return;
        }
        // Determine provider kind
        let kind = self.form.provider_kind.unwrap_or(ProviderKind::OpenRouter);

        // Find config path (git root or CWD)
        let path = find_config_path();

        // Write okeanos.toml
        let content = self.render_toml();
        if let Err(e) = std::fs::write(&path, &content) {
            // In a real implementation, we'd show an error. For now, just abort.
            eprintln!("Failed to write config: {e}");
            self.abort();
            return;
        }

        let result = ConfigModalResult::Saved {
            path: path.clone(),
            data: self.form.clone(),
        };
        let _ = self
            .result_tx
            .as_ref()
            .and_then(|tx| tx.try_send(result.clone()).ok());

        self.visible = false;
        self.emit_close_event(true);

        // Emit config_saved event
        let _ = crate::events::Event::config_saved(
            path.to_string_lossy().to_string(),
            kind.label().to_string(),
            self.form.model.clone(),
            self.form.base_url.clone(),
        );
    }

    fn emit_step_event(&self) {
        let _ = crate::events::Event::config_modal(
            ConfigModalAction::Step,
            Some(self.step.as_u8()),
            Some(self.form_data_json()),
        );
    }

    fn emit_close_event(&self, completed: bool) {
        let _ = crate::events::Event::config_modal(
            ConfigModalAction::Close,
            None,
            Some(serde_json::json!({ "completed": completed })),
        );
    }

    fn form_data_json(&self) -> serde_json::Value {
        serde_json::json!({
            "provider_kind": self.form.provider_kind.as_ref().map(|p| p.label()),
            "base_url": self.form.base_url,
            "api_key": if self.form.api_key.is_empty() { None } else { Some("***") },
            "model": self.form.model,
        })
    }

    fn render_toml(&self) -> String {
        let api_key_line = if self.form.api_key.is_empty() {
            String::new()
        } else {
            format!(r#"api_key = "{}""#, self.form.api_key)
        };

        format!(
            r#"# okeanos configuration
# Created by in-TUI provider configuration (M12.1)

[model]
name = "{}"
tier = "frontier"

[provider]
kind = "openai-compatible"
base_url = "{}"
{}

[context]
max_iterations = 50
max_elapsed_sec = 300

[permissions]
allow_list = ["todo", "read", "grep"]

[ui]
theme = "dark"
"#,
            self.form.model, self.form.base_url, api_key_line
        )
    }

    /// Render the modal.
    pub fn render(&self, area: RRect, buf: &mut Buffer) {
        if !self.visible {
            return;
        }

        Clear.render(area, buf);

        let panel_width = (area.width as f32 * 0.7).clamp(50.0, 100.0) as u16;
        let panel_height = 20u16;

        let panel_area = centered_rect(panel_width, panel_height, area);

        let block = Block::default()
            .title(format!(" {} ", self.step.title()))
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

        let mut lines: Vec<Line> = Vec::new();

        // Hint line
        lines.push(Line::from(vec![
            Span::styled(
                "Tab: next field · ",
                self.theme.style(SemanticRole::CanvasMuted),
            ),
            Span::styled("Esc: back · ", self.theme.style(SemanticRole::CanvasMuted)),
            Span::styled(
                "Enter: next / save",
                self.theme.style(SemanticRole::CanvasMuted),
            ),
            Span::styled(
                "   Ctrl+C: abort",
                self.theme.style(SemanticRole::CanvasMuted),
            ),
        ]));
        lines.push(Line::from(""));

        match self.step {
            ConfigStep::Provider => self.render_provider_step(&mut lines),
            ConfigStep::BaseUrl => self.render_base_url_step(&mut lines),
            ConfigStep::ApiKey => self.render_api_key_step(&mut lines),
            ConfigStep::Model => self.render_model_step(&mut lines),
            ConfigStep::Confirm => self.render_confirm_step(&mut lines),
        }

        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Left)
            .style(self.theme.style(SemanticRole::CanvasBg));

        paragraph.render(inner, buf);
    }

    fn render_provider_step(&self, lines: &mut Vec<Line>) {
        lines.push(Line::from(vec![Span::styled(
            "Select provider kind:",
            self.theme
                .style(SemanticRole::CanvasFg)
                .add_modifier(Modifier::BOLD),
        )]));
        lines.push(Line::from(""));

        let providers = ProviderKind::all();
        for (idx, provider) in providers.iter().enumerate() {
            let selected = idx == self.provider_selected;
            let marker = if selected { "▸ " } else { "  " };
            let style = if selected {
                self.theme
                    .style(SemanticRole::Tool)
                    .add_modifier(Modifier::BOLD)
            } else {
                self.theme.style(SemanticRole::CanvasFg)
            };
            lines.push(Line::from(vec![
                Span::styled(marker, style),
                Span::styled(provider.label(), style),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "Press Enter to continue, or use ↑/↓ (or 1/2/3) to select.",
            self.theme.style(SemanticRole::CanvasMuted),
        )]));
    }

    fn render_base_url_step(&self, lines: &mut Vec<Line>) {
        lines.push(Line::from(vec![Span::styled(
            "Base URL:",
            self.theme
                .style(SemanticRole::CanvasFg)
                .add_modifier(Modifier::BOLD),
        )]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            format!("> {}", self.form.base_url),
            self.theme.style(SemanticRole::CanvasFg),
        )]));
        if self.form.base_url.is_empty() {
            lines.push(Line::from(vec![Span::styled(
                "  (default: https://openrouter.ai/api/v1)",
                self.theme.style(SemanticRole::CanvasMuted),
            )]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "Type the API base URL for your provider.",
            self.theme.style(SemanticRole::CanvasMuted),
        )]));
    }

    fn render_api_key_step(&self, lines: &mut Vec<Line>) {
        let kind = self.form.provider_kind.unwrap_or(ProviderKind::OpenRouter);
        if !kind.requires_api_key() {
            lines.push(Line::from(vec![Span::styled(
                "API Key: (not required for Ollama)",
                self.theme
                    .style(SemanticRole::CanvasFg)
                    .add_modifier(Modifier::BOLD),
            )]));
            lines.push(Line::from(""));
            lines.push(Line::from(vec![Span::styled(
                "> skipped",
                self.theme.style(SemanticRole::CanvasMuted),
            )]));
        } else {
            lines.push(Line::from(vec![Span::styled(
                "API Key:",
                self.theme
                    .style(SemanticRole::CanvasFg)
                    .add_modifier(Modifier::BOLD),
            )]));
            lines.push(Line::from(""));
            let display = if self.api_key_masked {
                "*".repeat(self.form.api_key.len())
            } else {
                self.form.api_key.clone()
            };
            lines.push(Line::from(vec![Span::styled(
                format!("> {display}"),
                self.theme.style(SemanticRole::CanvasFg),
            )]));
            lines.push(Line::from(vec![Span::styled(
                "  (toggle masking not yet implemented; use {env:VAR} or {file:path} for secrets)",
                self.theme.style(SemanticRole::CanvasMuted),
            )]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "Enter your provider API key. For secrets, use {env:VAR} or {file:path}.",
            self.theme.style(SemanticRole::CanvasMuted),
        )]));
    }

    fn render_model_step(&self, lines: &mut Vec<Line>) {
        lines.push(Line::from(vec![Span::styled(
            "Model:",
            self.theme
                .style(SemanticRole::CanvasFg)
                .add_modifier(Modifier::BOLD),
        )]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            format!("> {}", self.form.model),
            self.theme.style(SemanticRole::CanvasFg),
        )]));
        if self.form.model.is_empty() {
            lines.push(Line::from(vec![Span::styled(
                "  (e.g., gpt-4o, gpt-4o-mini, claude-3-5-sonnet)",
                self.theme.style(SemanticRole::CanvasMuted),
            )]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "Enter the model name to use. OpenRouter supports model discovery.",
            self.theme.style(SemanticRole::CanvasMuted),
        )]));
    }

    fn render_confirm_step(&self, lines: &mut Vec<Line>) {
        let kind = self.form.provider_kind.unwrap_or(ProviderKind::OpenRouter);
        lines.push(Line::from(vec![Span::styled(
            "Confirm & Save:",
            self.theme
                .style(SemanticRole::CanvasFg)
                .add_modifier(Modifier::BOLD),
        )]));
        lines.push(Line::from(""));

        let fields = vec![
            ("Provider", kind.label().to_string()),
            ("Base URL", self.form.base_url.clone()),
            (
                "API Key",
                if self.form.api_key.is_empty() {
                    "(not set)".to_string()
                } else {
                    "***set***".to_string()
                },
            ),
            ("Model", self.form.model.clone()),
        ];

        for (label, value) in fields {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:<12}", label),
                    self.theme.style(SemanticRole::CanvasMuted),
                ),
                Span::styled(value, self.theme.style(SemanticRole::CanvasFg)),
            ]));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "Config will be written to okeanos.toml at git root/CWD.",
            self.theme.style(SemanticRole::CanvasMuted),
        )]));
        lines.push(Line::from(vec![Span::styled(
            "Press Enter to save and continue, or Esc to go back.",
            self.theme.style(SemanticRole::CanvasMuted),
        )]));
    }

    /// Take the pending result, if any.
    pub fn take_result(&mut self) -> Option<ConfigModalResult> {
        None
    }
}

/// Find the config file path: git root if available, otherwise CWD.
fn find_config_path() -> PathBuf {
    if let Ok(cwd) = std::env::current_dir() {
        let mut dir = cwd.clone();
        loop {
            if dir.join(".git").exists() {
                return dir.join("okeanos.toml");
            }
            if !dir.pop() {
                return cwd.join("okeanos.toml");
            }
        }
    }
    PathBuf::from("okeanos.toml")
}

impl Default for ConfigModal {
    fn default() -> Self {
        Self::new(crate::ui::theme::Theme::default_dark())
    }
}

/// Helper to create a centered rect.
fn centered_rect(width: u16, height: u16, r: RRect) -> RRect {
    let x = r.x.saturating_add((r.width.saturating_sub(width)) / 2);
    let y = r.y.saturating_add((r.height.saturating_sub(height)) / 2);
    RRect::new(x, y, width.min(r.width), height.min(r.height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn test_theme() -> Arc<Theme> {
        Theme::default_dark()
    }

    #[test]
    fn test_config_modal_new() {
        let modal = ConfigModal::new(test_theme());
        assert!(!modal.is_visible());
        assert_eq!(modal.step(), ConfigStep::Provider);
    }

    #[test]
    fn test_provider_kind_default_base_url() {
        assert_eq!(
            ProviderKind::OpenRouter.default_base_url(),
            "https://openrouter.ai/api/v1"
        );
        assert_eq!(
            ProviderKind::Ollama.default_base_url(),
            "http://localhost:11434/v1"
        );
        assert!(!ProviderKind::Ollama.requires_api_key());
        assert!(ProviderKind::OpenRouter.requires_api_key());
    }

    #[test]
    fn test_show_hide() {
        let mut modal = ConfigModal::new(test_theme());
        modal.show();
        assert!(modal.is_visible());
        modal.hide();
        assert!(!modal.is_visible());
    }

    #[test]
    fn test_next_step_provider_to_base_url() {
        let mut modal = ConfigModal::new(test_theme());
        modal.show();
        // Provider is pre-selected at index 0
        assert!(modal.validate_current_step());
        modal.next_step();
        assert_eq!(modal.step(), ConfigStep::BaseUrl);
    }

    #[test]
    fn test_next_step_skips_model_for_ollama() {
        let mut modal = ConfigModal::new(test_theme());
        modal.show();
        modal.provider_selected = 2; // Ollama
        assert!(modal.validate_current_step());
        modal.next_step(); // Provider -> BaseUrl
        assert_eq!(modal.step(), ConfigStep::BaseUrl);
        modal.next_step(); // BaseUrl -> ApiKey
        assert_eq!(modal.step(), ConfigStep::ApiKey);
        modal.next_step(); // ApiKey -> Confirm (skips Model for Ollama)
        assert_eq!(modal.step(), ConfigStep::Confirm);
    }

    #[test]
    fn test_prev_step() {
        let mut modal = ConfigModal::new(test_theme());
        modal.show();
        modal.next_step();
        assert_eq!(modal.step(), ConfigStep::BaseUrl);
        modal.prev_step();
        assert_eq!(modal.step(), ConfigStep::Provider);
    }
}
