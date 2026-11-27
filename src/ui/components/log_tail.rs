//! Log tail panel component — newest JSONL line at bottom per tui-design.md §5.8.
//!
//! Autoscroll on new lines; level color (system/ok/error); optional filter chips.

use crate::events::{Event, Log, LogLevel};
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

/// Maximum log lines to keep.
const MAX_LOG_LINES: usize = 500;

/// Log tail panel state.
#[derive(Debug)]
pub struct LogTailPanel {
    logs: VecDeque<LogEntry>,
    list_state: ListState,
    auto_scroll: bool,
    theme: Arc<Theme>,
    reduced_motion: bool,
    filter: LogFilter,
}

#[derive(Debug, Clone)]
struct LogEntry {
    log: Log,
    timestamp: std::time::Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFilter {
    #[default]
    All,
    ErrorsOnly,
    WarningsAndErrors,
    SystemOnly,
}

impl LogTailPanel {
    pub fn new(theme: Arc<Theme>) -> Self {
        let reduced_motion = theme.reduced_motion.enabled;
        Self {
            logs: VecDeque::with_capacity(MAX_LOG_LINES),
            list_state: ListState::default(),
            auto_scroll: true,
            theme,
            reduced_motion,
            filter: LogFilter::All,
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
        if let Event::Log(log) = event {
            self.add_log(log.clone());
        }
    }

    /// Add a log entry.
    fn add_log(&mut self, log: Log) {
        let entry = LogEntry {
            log,
            timestamp: std::time::Instant::now(),
        };

        self.logs.push_back(entry);
        if self.logs.len() > MAX_LOG_LINES {
            self.logs.pop_front();
        }

        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    /// Set the log filter.
    pub fn set_filter(&mut self, filter: LogFilter) {
        self.filter = filter;
    }

    /// Cycle to next filter.
    pub fn cycle_filter(&mut self) {
        self.filter = match self.filter {
            LogFilter::All => LogFilter::ErrorsOnly,
            LogFilter::ErrorsOnly => LogFilter::WarningsAndErrors,
            LogFilter::WarningsAndErrors => LogFilter::SystemOnly,
            LogFilter::SystemOnly => LogFilter::All,
        };
    }

    /// Get current filter.
    pub fn filter(&self) -> LogFilter {
        self.filter
    }

    /// Scroll up.
    pub fn scroll_up(&mut self) {
        self.auto_scroll = false;
        let i = self
            .list_state
            .selected()
            .unwrap_or(self.filtered_count().saturating_sub(1));
        if i > 0 {
            self.list_state.select(Some(i - 1));
        }
    }

    /// Scroll down.
    pub fn scroll_down(&mut self) {
        let i = self.list_state.selected().unwrap_or(0);
        let max = self.filtered_count().saturating_sub(1);
        if i < max {
            self.list_state.select(Some(i + 1));
        } else {
            self.scroll_to_bottom();
        }
    }

    /// Scroll to bottom.
    pub fn scroll_to_bottom(&mut self) {
        self.auto_scroll = true;
        let count = self.filtered_count();
        if count > 0 {
            self.list_state.select(Some(count - 1));
        }
    }

    /// Get filtered log count.
    fn filtered_count(&self) -> usize {
        self.logs
            .iter()
            .filter(|e| self.matches_filter(&e.log))
            .count()
    }

    /// Check if log entry matches current filter.
    fn matches_filter(&self, log: &Log) -> bool {
        match self.filter {
            LogFilter::All => true,
            LogFilter::ErrorsOnly => log.level == LogLevel::Error,
            LogFilter::WarningsAndErrors => matches!(log.level, LogLevel::Warn | LogLevel::Error),
            LogFilter::SystemOnly => log.level == LogLevel::Info || log.level == LogLevel::Debug,
        }
    }

    /// Get filtered logs.
    fn filtered_logs(&self) -> Vec<&LogEntry> {
        self.logs
            .iter()
            .filter(|e| self.matches_filter(&e.log))
            .collect()
    }

    /// Render the log tail panel.
    pub fn render(&mut self, area: RRect, buf: &mut Buffer) {
        let block = Block::default()
            .title(self.title())
            .borders(Borders::ALL)
            .border_style(self.theme.style(SemanticRole::CanvasBorder))
            .style(self.theme.style(SemanticRole::CanvasBg));

        let items: Vec<ListItem> = self
            .filtered_logs()
            .iter()
            .map(|entry| self.render_log_entry(entry))
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

        // Scrollbar
        if self.filtered_count() > inner.height as usize {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("▲"))
                .end_symbol(Some("▼"))
                .track_symbol(Some("│"))
                .thumb_symbol("█");
            let mut scrollbar_state = ratatui::widgets::ScrollbarState::new(self.filtered_count())
                .position(self.list_state.selected().unwrap_or(0));
            ratatui::widgets::StatefulWidget::render(scrollbar, area, buf, &mut scrollbar_state);
        }
    }

    /// Get panel title with filter indicator.
    fn title(&self) -> String {
        let filter_str = match self.filter {
            LogFilter::All => "All",
            LogFilter::ErrorsOnly => "Errors",
            LogFilter::WarningsAndErrors => "Warn+Err",
            LogFilter::SystemOnly => "System",
        };
        format!(" Logs [{}] ", filter_str)
    }

    /// Render a single log entry.
    fn render_log_entry(&self, entry: &LogEntry) -> ListItem<'_> {
        let log = &entry.log;

        let (level_glyph, level_role) = match log.level {
            LogLevel::Trace => ("▸", SemanticRole::CanvasMuted),
            LogLevel::Debug => ("▸", SemanticRole::CanvasMuted),
            LogLevel::Info => ("ℹ", SemanticRole::System),
            LogLevel::Warn => ("⚠", SemanticRole::Warn),
            LogLevel::Error => ("✗", SemanticRole::Error),
        };

        // Format timestamp
        let elapsed = entry.timestamp.elapsed();
        let ts = if elapsed.as_secs() < 60 {
            format!("{:.1}s", elapsed.as_secs_f32())
        } else {
            format!("{}m", elapsed.as_secs() / 60)
        };

        let line = Line::from(vec![
            Span::styled(
                level_glyph,
                self.theme.style(level_role).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(ts, self.theme.style(SemanticRole::CanvasMuted)),
            Span::raw(" "),
            Span::styled(log.message.clone(), self.log_level_style(log.level.clone())),
        ]);

        ListItem::new(line)
    }

    /// Get style for log level.
    fn log_level_style(&self, level: LogLevel) -> Style {
        match level {
            LogLevel::Trace | LogLevel::Debug => self.theme.style(SemanticRole::CanvasMuted),
            LogLevel::Info => self.theme.style(SemanticRole::System),
            LogLevel::Warn => self.theme.style(SemanticRole::Warn),
            LogLevel::Error => self
                .theme
                .style(SemanticRole::Error)
                .add_modifier(Modifier::BOLD),
        }
    }

    /// Get log count.
    pub fn len(&self) -> usize {
        self.logs.len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.logs.is_empty()
    }
}

impl Default for LogTailPanel {
    fn default() -> Self {
        Self::new(Theme::default_dark())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Log, LogLevel};
    use std::sync::Arc;

    fn test_theme() -> Arc<Theme> {
        Theme::default_dark()
    }

    #[test]
    fn test_log_tail_new() {
        let panel = LogTailPanel::new(test_theme());
        assert!(panel.is_empty());
        assert_eq!(panel.len(), 0);
    }

    #[test]
    fn test_add_log() {
        let mut panel = LogTailPanel::new(test_theme());
        let log = Log {
            level: LogLevel::Info,
            message: "Test message".to_string(),
        };
        panel.handle_event(&crate::events::Event::Log(log));
        assert_eq!(panel.len(), 1);
    }

    #[test]
    fn test_log_levels() {
        let mut panel = LogTailPanel::new(test_theme());

        panel.handle_event(&crate::events::Event::Log(Log {
            level: LogLevel::Info,
            message: "Info".to_string(),
        }));
        panel.handle_event(&crate::events::Event::Log(Log {
            level: LogLevel::Warn,
            message: "Warn".to_string(),
        }));
        panel.handle_event(&crate::events::Event::Log(Log {
            level: LogLevel::Error,
            message: "Error".to_string(),
        }));
        panel.handle_event(&crate::events::Event::Log(Log {
            level: LogLevel::Debug,
            message: "Debug".to_string(),
        }));

        assert_eq!(panel.len(), 4);

        // Test filters
        panel.set_filter(LogFilter::All);
        assert_eq!(panel.filtered_count(), 4);

        panel.set_filter(LogFilter::ErrorsOnly);
        assert_eq!(panel.filtered_count(), 1);

        panel.set_filter(LogFilter::WarningsAndErrors);
        assert_eq!(panel.filtered_count(), 2);

        panel.set_filter(LogFilter::SystemOnly);
        assert_eq!(panel.filtered_count(), 2); // Info + Debug
    }

    #[test]
    fn test_cycle_filter() {
        let mut panel = LogTailPanel::new(test_theme());
        assert_eq!(panel.filter(), LogFilter::All);
        panel.cycle_filter();
        assert_eq!(panel.filter(), LogFilter::ErrorsOnly);
        panel.cycle_filter();
        assert_eq!(panel.filter(), LogFilter::WarningsAndErrors);
        panel.cycle_filter();
        assert_eq!(panel.filter(), LogFilter::SystemOnly);
        panel.cycle_filter();
        assert_eq!(panel.filter(), LogFilter::All);
    }

    #[test]
    fn test_auto_scroll() {
        let mut panel = LogTailPanel::new(test_theme());
        for i in 0..10 {
            panel.handle_event(&crate::events::Event::Log(Log {
                level: LogLevel::Info,
                message: format!("Msg {}", i),
            }));
        }
        panel.scroll_to_bottom();
        assert!(panel.auto_scroll);
        assert_eq!(panel.list_state.selected(), Some(9));

        panel.scroll_up();
        assert!(!panel.auto_scroll);
    }

    #[test]
    fn test_max_log_lines() {
        let mut panel = LogTailPanel::new(test_theme());
        for i in 0..600 {
            panel.handle_event(&crate::events::Event::Log(Log {
                level: LogLevel::Info,
                message: format!("Msg {}", i),
            }));
        }
        // Should be capped at MAX_LOG_LINES
        assert_eq!(panel.len(), MAX_LOG_LINES);
    }
}
