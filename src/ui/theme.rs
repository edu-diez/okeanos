//! TUI Theme module — design tokens from assets/themes/
//!
//! Loads dark.toml / light.toml at runtime. Changing one file re-themes the app;
//! no visual rework. Per DESIGN.md §8.
//!
//! Themes are also embedded in the binary for standalone distribution.

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

// Embedded theme TOML strings for standalone binary operation
const EMBEDDED_DARK_TOML: &str = include_str!("../../assets/themes/dark.toml");
const EMBEDDED_LIGHT_TOML: &str = include_str!("../../assets/themes/light.toml");

/// Theme variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemeVariant {
    #[default]
    Dark,
    Light,
}

impl std::str::FromStr for ThemeVariant {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "dark" => Ok(ThemeVariant::Dark),
            "light" => Ok(ThemeVariant::Light),
            _ => Err(format!("invalid theme variant: {}", s)),
        }
    }
}

/// Canvas colors.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CanvasColors {
    pub bg: String,
    pub fg: String,
    pub muted: String,
    pub border: String,
}

/// Semantic colors (paired with glyphs for accessibility).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SemanticColors {
    pub ok: String,
    pub warn: String,
    pub error: String,
    pub pending: String,
    pub tool: String,
    pub system: String,
    pub user: String,
    pub assistant: String,
}

/// Typography scales.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Typography {
    pub heading_scale: f32,
    pub label_scale: f32,
    pub body_scale: f32,
    pub mono_data_scale: f32,
    pub muted_scale: f32,
}

/// Spacing in character cells.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Spacing {
    pub unit: u16,
    pub gap_small: u16,
    pub gap_medium: u16,
    pub pad: u16,
    pub modal_pad: u16,
}

/// Frame styles.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Frames {
    pub single: String,
    pub double: String,
    pub rounded: String,
    pub ascii: String,
}

/// Animation durations in milliseconds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Animation {
    pub modal: u64,
    pub typing_cycle: u64,
    pub spinner_frame: u64,
    pub badge: u64,
    pub progress: u64,
}

/// Layer z-order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Layers {
    pub background: u16,
    pub pane: u16,
    pub right_rail: u16,
    pub status_bar: u16,
    pub modal: u16,
    pub command_palette: u16,
}

/// Breakpoints in terminal cells.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Breakpoints {
    pub width_min: u16,
    pub width_comfortable: u16,
    pub height_min: u16,
    pub height_comfortable: u16,
}

/// Reduced motion settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReducedMotion {
    pub enabled: bool,
}

/// Complete theme loaded from TOML.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Theme {
    #[serde(default)]
    pub variant: ThemeVariant,
    pub canvas: CanvasColors,
    pub semantic: SemanticColors,
    pub typography: Typography,
    pub spacing: Spacing,
    pub frames: Frames,
    pub animation: Animation,
    pub layers: Layers,
    pub breakpoints: Breakpoints,
    pub reduced_motion: ReducedMotion,
}

impl Theme {
    /// Load theme from the default assets directory, with embedded fallback for standalone binaries.
    pub fn load(variant: ThemeVariant) -> anyhow::Result<Arc<Self>> {
        // Try loading from filesystem first (allows live theme editing in development)
        if let Some(theme_dir) = Self::theme_dir() {
            let file_name = match variant {
                ThemeVariant::Dark => "dark.toml",
                ThemeVariant::Light => "light.toml",
            };
            let path = theme_dir.join(file_name);
            if path.exists() {
                return Self::load_from_path(&path);
            }
        }

        // Fallback to embedded themes (for standalone binaries without assets/themes/)
        Self::load_embedded(variant)
    }

    /// Load theme from embedded TOML strings.
    fn load_embedded(variant: ThemeVariant) -> anyhow::Result<Arc<Self>> {
        let (content, variant) = match variant {
            ThemeVariant::Dark => (EMBEDDED_DARK_TOML, ThemeVariant::Dark),
            ThemeVariant::Light => (EMBEDDED_LIGHT_TOML, ThemeVariant::Light),
        };
        let mut theme: Theme = toml::from_str(content).context("failed to parse embedded theme")?;
        theme.variant = variant;
        Ok(Arc::new(theme))
    }

    /// Load theme from a specific path.
    pub fn load_from_path(path: &Path) -> anyhow::Result<Arc<Self>> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read theme file: {}", path.display()))?;
        let mut theme: Theme = toml::from_str(&content)
            .with_context(|| format!("failed to parse theme file: {}", path.display()))?;
        theme.variant = match path.file_stem().and_then(|s| s.to_str()) {
            Some("light") => ThemeVariant::Light,
            _ => ThemeVariant::Dark,
        };
        Ok(Arc::new(theme))
    }

    /// Get the default theme directory (assets/themes/).
    /// Returns None if not found (caller should fall back to embedded themes).
    fn theme_dir() -> Option<std::path::PathBuf> {
        // First try relative to the current executable
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
            let themes = dir.join("assets").join("themes");
            if themes.exists() {
                return Some(themes);
            }
        }

        // Fallback to CARGO_MANIFEST_DIR for development
        if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
            let themes = std::path::PathBuf::from(manifest_dir)
                .join("assets")
                .join("themes");
            if themes.exists() {
                return Some(themes);
            }
        }

        // Fallback to CWD
        if let Ok(cwd) = std::env::current_dir() {
            let themes = cwd.join("assets").join("themes");
            if themes.exists() {
                return Some(themes);
            }
        }

        None
    }

    /// Get the default dark theme.
    pub fn default_dark() -> Arc<Self> {
        Self::load(ThemeVariant::Dark).expect("built-in dark theme should load")
    }

    /// Get the default light theme.
    pub fn default_light() -> Arc<Self> {
        Self::load(ThemeVariant::Light).expect("built-in light theme should load")
    }

    /// Check if running at minimum breakpoint (80x24).
    pub fn is_minimum(&self, width: u16, height: u16) -> bool {
        width <= self.breakpoints.width_min || height <= self.breakpoints.height_min
    }

    /// Check if running at comfortable breakpoint (120x40).
    pub fn is_comfortable(&self, width: u16, height: u16) -> bool {
        width >= self.breakpoints.width_comfortable && height >= self.breakpoints.height_comfortable
    }

    /// Check if narrow (< 100 cols).
    pub fn is_narrow(&self, width: u16) -> bool {
        width < 100
    }

    /// Check if short (< 40 rows).
    pub fn is_short(&self, height: u16) -> bool {
        height < 40
    }

    /// Get a semantic color as ratatui Color.
    pub fn color(&self, role: SemanticRole) -> ratatui::style::Color {
        let hex = match role {
            SemanticRole::Ok => &self.semantic.ok,
            SemanticRole::Warn => &self.semantic.warn,
            SemanticRole::Error => &self.semantic.error,
            SemanticRole::Pending => &self.semantic.pending,
            SemanticRole::Tool => &self.semantic.tool,
            SemanticRole::System => &self.semantic.system,
            SemanticRole::User => &self.semantic.user,
            SemanticRole::Assistant => &self.semantic.assistant,
            SemanticRole::CanvasBg => &self.canvas.bg,
            SemanticRole::CanvasFg => &self.canvas.fg,
            SemanticRole::CanvasMuted => &self.canvas.muted,
            SemanticRole::CanvasBorder => &self.canvas.border,
        };
        Self::hex_to_color(hex)
    }

    /// Convert hex string to ratatui Color.
    fn hex_to_color(hex: &str) -> ratatui::style::Color {
        let hex = hex.trim_start_matches('#');
        if let Ok(rgb) = u32::from_str_radix(hex, 16) {
            let r = ((rgb >> 16) & 0xFF) as u8;
            let g = ((rgb >> 8) & 0xFF) as u8;
            let b = (rgb & 0xFF) as u8;
            ratatui::style::Color::Rgb(r, g, b)
        } else {
            ratatui::style::Color::Reset
        }
    }

    /// Get a style for a semantic role.
    pub fn style(&self, role: SemanticRole) -> ratatui::style::Style {
        ratatui::style::Style::default().fg(self.color(role))
    }

    /// Get a style for a semantic role with background.
    pub fn style_bg(&self, fg_role: SemanticRole, bg_role: SemanticRole) -> ratatui::style::Style {
        ratatui::style::Style::default()
            .fg(self.color(fg_role))
            .bg(self.color(bg_role))
    }
}

/// Semantic color roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticRole {
    Ok,
    Warn,
    Error,
    Pending,
    Tool,
    System,
    User,
    Assistant,
    CanvasBg,
    CanvasFg,
    CanvasMuted,
    CanvasBorder,
}

/// Theme manager for runtime theme switching.
#[derive(Debug, Clone)]
pub struct ThemeManager {
    current: Arc<Theme>,
}

impl ThemeManager {
    pub fn new(variant: ThemeVariant) -> anyhow::Result<Self> {
        let theme = Theme::load(variant)?;
        Ok(Self { current: theme })
    }

    pub fn current(&self) -> Arc<Theme> {
        self.current.clone()
    }

    pub fn set_variant(&mut self, variant: ThemeVariant) -> anyhow::Result<()> {
        self.current = Theme::load(variant)?;
        Ok(())
    }

    pub fn toggle(&mut self) -> anyhow::Result<ThemeVariant> {
        let new_variant = match &*self.current {
            t if t.variant == ThemeVariant::Dark => ThemeVariant::Light,
            _ => ThemeVariant::Dark,
        };
        self.set_variant(new_variant)?;
        Ok(new_variant)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_theme_variant_from_str() {
        assert_eq!("dark".parse::<ThemeVariant>(), Ok(ThemeVariant::Dark));
        assert_eq!("light".parse::<ThemeVariant>(), Ok(ThemeVariant::Light));
        assert!("invalid".parse::<ThemeVariant>().is_err());
    }

    #[test]
    fn test_load_default_themes() {
        let dark = Theme::default_dark();
        let light = Theme::default_light();
        assert_eq!(dark.variant, ThemeVariant::Dark);
        assert_eq!(light.variant, ThemeVariant::Light);
        assert_eq!(dark.canvas.bg, "#0b0e11");
        assert_eq!(light.canvas.bg, "#f8fafc");
    }

    #[test]
    fn test_breakpoints() {
        let theme = Theme::default_dark();
        assert!(theme.is_minimum(80, 24));
        assert!(!theme.is_minimum(120, 40));
        assert!(theme.is_comfortable(120, 40));
        assert!(!theme.is_comfortable(80, 24));
        assert!(theme.is_narrow(90));
        assert!(!theme.is_narrow(100));
        assert!(theme.is_short(30));
        assert!(!theme.is_short(40));
    }

    #[test]
    fn test_semantic_colors() {
        let theme = Theme::default_dark();
        assert_eq!(theme.semantic.ok, "#4ade80");
        assert_eq!(theme.semantic.error, "#f87171");
        assert_eq!(theme.semantic.tool, "#818cf8");
    }
}
