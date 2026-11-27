//! Responsive layout grid per tui-design.md §4.
//!
//! Grid unit: 1 terminal character cell. Regions stack vertically by default;
//! columns appear when width allows.

use crate::ui::theme::Theme;
pub use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Layout mode based on terminal size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    /// 80×24 minimum: single column, right-rail content folds into conversation bottom
    Minimum,
    /// < 100 cols: narrow, right rail collapses
    Narrow,
    /// < 40 rows: short, status bar compresses
    Short,
    /// 120×40 comfortable: 3 regions (conversation ~60%, right rail ~40%, status bar)
    Comfortable,
}

/// Region identifiers for layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Header,
    Conversation,
    RightRail,
    StatusBar,
    Modal,
    CommandPalette,
}

/// Calculated layout rectangles for each region.
#[derive(Debug, Clone)]
pub struct LayoutRects {
    pub header: Rect,
    pub conversation: Rect,
    pub right_rail: Option<Rect>,
    pub status_bar: Rect,
    pub modal: Rect,
    pub command_palette: Rect,
    pub mode: LayoutMode,
}

impl LayoutRects {
    /// Calculate layout for the given terminal size and theme.
    pub fn new(area: Rect, theme: &Theme) -> Self {
        let mode = Self::determine_mode(area, theme);
        let (header, conversation, right_rail, status_bar) =
            Self::calculate_main_regions(area, theme, mode);

        // Modal is centered overlay
        let modal = Self::centered_rect(80, 50, area);
        // Command palette at center-bottom
        let command_palette = Self::command_palette_rect(area);

        Self {
            header,
            conversation,
            right_rail,
            status_bar,
            modal,
            command_palette,
            mode,
        }
    }

    /// Determine layout mode from terminal size.
    fn determine_mode(area: Rect, theme: &Theme) -> LayoutMode {
        let width = area.width;
        let height = area.height;

        // Check for minimum breakpoint first (80x24)
        if width <= theme.breakpoints.width_min && height <= theme.breakpoints.height_min {
            LayoutMode::Minimum
        } else if width < 100 {
            LayoutMode::Narrow
        } else if height < 40 {
            LayoutMode::Short
        } else if theme.is_comfortable(width, height) {
            LayoutMode::Comfortable
        } else {
            LayoutMode::Minimum
        }
    }

    /// Calculate main layout regions based on mode.
    fn calculate_main_regions(
        area: Rect,
        _theme: &Theme,
        mode: LayoutMode,
    ) -> (Rect, Rect, Option<Rect>, Rect) {
        let status_bar_height = match mode {
            LayoutMode::Short => 1,
            _ => 2,
        };

        match mode {
            LayoutMode::Comfortable => {
                // 3 regions: header (1), conversation (~60%), right rail (~40%), status bar (2)
                let main_layout = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1),                 // Header
                        Constraint::Min(10), // Conversation + Right Rail (split horizontally)
                        Constraint::Length(status_bar_height), // Status bar
                    ])
                    .split(area);

                let header = main_layout[0];
                let status_bar = main_layout[2];

                // Split middle region horizontally: conversation (60%) | right rail (40%)
                let middle_layout = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
                    .split(main_layout[1]);

                let conversation = middle_layout[0];
                let right_rail = Some(middle_layout[1]);

                (header, conversation, right_rail, status_bar)
            }
            LayoutMode::Minimum | LayoutMode::Narrow | LayoutMode::Short => {
                // Single column: header (1), conversation (rest - status), status bar (1-2)
                let constraints = vec![
                    Constraint::Length(1),                 // Header
                    Constraint::Min(5),                    // Conversation
                    Constraint::Length(status_bar_height), // Status bar
                ];

                let layout = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints(constraints)
                    .split(area);

                (layout[0], layout[1], None, layout[2])
            }
        }
    }

    /// Create a centered rectangle (percentage of parent).
    fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
        let popup_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage((100 - percent_y) / 2),
                Constraint::Percentage(percent_y),
                Constraint::Percentage((100 - percent_y) / 2),
            ])
            .split(r);

        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage((100 - percent_x) / 2),
                Constraint::Percentage(percent_x),
                Constraint::Percentage((100 - percent_x) / 2),
            ])
            .split(popup_layout[1])[1]
    }

    /// Command palette rect: centered horizontally, bottom-third vertically.
    fn command_palette_rect(r: Rect) -> Rect {
        let height = (r.height as f32 * 0.4).ceil() as u16;
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(height)])
            .split(r);

        let _width = (r.width as f32 * 0.8).ceil() as u16;
        let horizontal = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage((100 - 80) / 2),
                Constraint::Percentage(80),
                Constraint::Percentage((100 - 80) / 2),
            ])
            .split(vertical[1]);

        horizontal[1]
    }

    /// Get a region rect by identifier.
    pub fn region(&self, region: Region) -> Rect {
        match region {
            Region::Header => self.header,
            Region::Conversation => self.conversation,
            Region::RightRail => self.right_rail.unwrap_or(self.conversation),
            Region::StatusBar => self.status_bar,
            Region::Modal => self.modal,
            Region::CommandPalette => self.command_palette,
        }
    }

    /// Check if right rail is visible.
    pub fn has_right_rail(&self) -> bool {
        self.right_rail.is_some()
    }

    /// Get the conversation area (includes folded right-rail content in narrow mode).
    pub fn conversation_area(&self) -> Rect {
        self.conversation
    }

    /// Get the right rail area, or the bottom portion of conversation if folded.
    pub fn right_rail_area(&self) -> Rect {
        self.right_rail.unwrap_or_else(|| {
            // Fold into bottom of conversation: last 40% of conversation height
            let fold_height = (self.conversation.height as f32 * 0.4) as u16;
            Rect {
                x: self.conversation.x,
                y: self.conversation.y + self.conversation.height.saturating_sub(fold_height),
                width: self.conversation.width,
                height: fold_height.min(self.conversation.height),
            }
        })
    }
}

/// Helper for splitting a region into sub-regions (e.g., right rail tabs).
pub fn split_right_rail(area: Rect, tab_count: usize) -> Vec<Rect> {
    if tab_count == 0 {
        return vec![];
    }
    let constraints: Vec<Constraint> = (0..tab_count)
        .map(|_| Constraint::Percentage(100 / tab_count as u16))
        .collect();
    Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area)
        .to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::Theme;

    #[test]
    fn test_layout_modes() {
        let theme = Theme::default_dark();

        // Comfortable: 120x40
        let area = Rect::new(0, 0, 120, 40);
        let layout = LayoutRects::new(area, &theme);
        assert_eq!(layout.mode, LayoutMode::Comfortable);
        assert!(layout.has_right_rail());

        // Minimum: 80x24
        let area = Rect::new(0, 0, 80, 24);
        let layout = LayoutRects::new(area, &theme);
        assert_eq!(layout.mode, LayoutMode::Minimum);
        assert!(!layout.has_right_rail());

        // Narrow: 90x40
        let area = Rect::new(0, 0, 90, 40);
        let layout = LayoutRects::new(area, &theme);
        assert_eq!(layout.mode, LayoutMode::Narrow);
        assert!(!layout.has_right_rail());

        // Short: 120x30
        let area = Rect::new(0, 0, 120, 30);
        let layout = LayoutRects::new(area, &theme);
        assert_eq!(layout.mode, LayoutMode::Short);
        assert!(!layout.has_right_rail());
    }

    #[test]
    fn test_comfortable_layout_dimensions() {
        let theme = Theme::default_dark();
        let area = Rect::new(0, 0, 120, 40);
        let layout = LayoutRects::new(area, &theme);

        assert_eq!(layout.header.height, 1);
        assert_eq!(layout.status_bar.height, 2);
        assert_eq!(layout.conversation.width, 72); // 60% of 120
        assert_eq!(layout.right_rail.unwrap().width, 48); // 40% of 120
    }

    #[test]
    fn test_minimum_layout_dimensions() {
        let theme = Theme::default_dark();
        let area = Rect::new(0, 0, 80, 24);
        let layout = LayoutRects::new(area, &theme);

        assert_eq!(layout.header.height, 1);
        assert_eq!(layout.status_bar.height, 2);
        assert_eq!(layout.conversation.height, 21); // 24 - 1 - 2
        assert!(!layout.has_right_rail());
    }

    #[test]
    fn test_modal_centered() {
        let theme = Theme::default_dark();
        let area = Rect::new(0, 0, 80, 24);
        let layout = LayoutRects::new(area, &theme);

        // Modal should be centered (80% width, 50% height)
        assert_eq!(layout.modal.width, 64); // 80% of 80
        assert_eq!(layout.modal.height, 12); // 50% of 24
        assert_eq!(layout.modal.x, 8); // (80-64)/2
        assert_eq!(layout.modal.y, 6); // (24-12)/2
    }

    #[test]
    fn test_command_palette_position() {
        let theme = Theme::default_dark();
        let area = Rect::new(0, 0, 80, 24);
        let layout = LayoutRects::new(area, &theme);

        // Command palette at center-bottom, 80% width, 40% height
        assert_eq!(layout.command_palette.width, 64);
        assert_eq!(layout.command_palette.height, 10); // 40% of 24
    }

    #[test]
    fn test_right_rail_area_folded() {
        let theme = Theme::default_dark();
        let area = Rect::new(0, 0, 80, 24);
        let layout = LayoutRects::new(area, &theme);

        let rail = layout.right_rail_area();
        // Should be bottom 40% of conversation
        assert_eq!(rail.width, 80);
        assert_eq!(rail.height, 8); // 40% of 21 ≈ 8
        assert_eq!(rail.y, layout.conversation.y + 13); // 21 - 8 = 13
    }
}
