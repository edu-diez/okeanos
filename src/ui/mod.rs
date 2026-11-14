//! TUI module — terminal rendering per DESIGN.md.
//!
//! Uses ratatui + crossterm; themes from assets/themes/.
//! Consumes the same event stream that headless/RPC mode emits (DESIGN.md §7).

pub mod components;
pub mod layout;
pub mod theme;

use crate::events::{Event, EventReceiver, EventSender, event_channel};
use crate::ui::components::{
    ApprovalModal, CommandPalette, ConversationPane, LogTailPanel, StatusBar, TodoPanel, ToolPanel,
};
use crate::ui::layout::LayoutRects;

pub use crate::ui::theme::{Theme, ThemeManager, ThemeVariant};
use anyhow::Result;
use crossterm::event::{self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// TUI application state.
pub struct TerminalApp {
    theme_manager: ThemeManager,
    layout: LayoutRects,
    conversation: ConversationPane,
    tool_panel: ToolPanel,
    todo_panel: TodoPanel,
    status_bar: StatusBar,
    log_tail: LogTailPanel,
    approval_modal: ApprovalModal,
    command_palette: CommandPalette,
    event_rx: EventReceiver,
    event_tx: EventSender,
    approval_tx: mpsc::Sender<crate::events::Approval>,
    approval_rx: Option<mpsc::Receiver<crate::events::Approval>>,
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    last_tick: Instant,
    running: bool,
    suspended: bool,
    width: u16,
    height: u16,
}

impl TerminalApp {
    /// Create a new TerminalApp with the given theme variant.
    pub fn new(theme_variant: ThemeVariant) -> Result<Self> {
        let theme_manager = ThemeManager::new(theme_variant)?;
        let theme = theme_manager.current();

        // Initialize crossterm backend
        let backend = CrosstermBackend::new(io::stdout());
        let terminal = Terminal::new(backend)?;

        // Enable raw mode and alternate screen
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
        crossterm::execute!(io::stdout(), crossterm::event::EnableMouseCapture)?;

        // Get initial terminal size
        let size = terminal.size()?;
        let width = size.width;
        let height = size.height;

        // Create event channel (shared with harness)
        let (event_tx, event_rx) = event_channel();
        let (approval_tx, approval_rx) = mpsc::channel(16);

        // Create layout
        let area = ratatui::layout::Rect::new(0, 0, width, height);
        let layout = LayoutRects::new(area, &theme);

        // Create components
        let conversation = ConversationPane::new(theme.clone());
        let tool_panel = ToolPanel::new(theme.clone());
        let todo_panel = TodoPanel::new(theme.clone());
        let status_bar = StatusBar::new(theme.clone());
        let log_tail = LogTailPanel::new(theme.clone());
        let approval_modal = ApprovalModal::new(theme.clone());
        let command_palette = CommandPalette::new(theme.clone());

        // Set up command palette callback
        let mut command_palette = command_palette;
        command_palette.on_execute(move |_action| {
            // Commands are handled in the main loop
        });

        Ok(Self {
            theme_manager,
            layout,
            conversation,
            tool_panel,
            todo_panel,
            status_bar,
            log_tail,
            approval_modal,
            command_palette,
            event_rx,
            event_tx,
            approval_tx,
            approval_rx: Some(approval_rx),
            terminal,
            last_tick: Instant::now(),
            running: true,
            suspended: false,
            width,
            height,
        })
    }

    /// Get the event sender for the harness to emit events.
    pub fn event_sender(&self) -> EventSender {
        self.event_tx.clone()
    }

    /// Get the approval sender for the harness to send approval decisions.
    pub fn approval_sender(&self) -> mpsc::Sender<crate::events::Approval> {
        self.approval_tx.clone()
    }

    /// Take the approval receiver for the harness to receive approval requests.
    pub fn take_approval_receiver(&mut self) -> Option<mpsc::Receiver<crate::events::Approval>> {
        self.approval_rx.take()
    }

    /// Run the TUI event loop.
    pub async fn run(&mut self) -> Result<()> {
        while self.running {
            // Handle crossterm events
            if event::poll(Duration::from_millis(16))? {
                if let CrosstermEvent::Key(key) = event::read()? {
                    if self.handle_key(key).await? {
                        continue;
                    }
                } else if let CrosstermEvent::Resize(w, h) = event::read()? {
                    self.handle_resize(w, h)?;
                }
            }

            // Handle events from the harness
            self.handle_events().await;

            // Tick animations
            self.tick();

            // Render
            let theme = self.theme_manager.current();
            self.terminal.draw(|frame| {
                Self::render_ui(
                    frame,
                    &self.layout,
                    &mut self.conversation,
                    &mut self.tool_panel,
                    &mut self.todo_panel,
                    &mut self.status_bar,
                    &mut self.log_tail,
                    &self.approval_modal,
                    &self.command_palette,
                    &theme,
                )
            })?;

            // Cap frame rate
            tokio::time::sleep(Duration::from_millis(16)).await;
        }

        // Cleanup
        self.cleanup()?;
        Ok(())
    }

    /// Handle a key event.
    async fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        // Global shortcuts
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                self.running = false;
                return Ok(true);
            }
            (KeyCode::Char('q'), KeyModifiers::CONTROL) => {
                self.running = false;
                return Ok(true);
            }
            (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
                self.toggle_theme();
                return Ok(true);
            }
            _ => {}
        }

        // If modal is visible, handle modal keys first
        if self.approval_modal.is_visible() {
            if let Some(decision) = self.approval_modal.handle_key(key) {
                let _ = self
                    .approval_tx
                    .send(crate::events::Approval {
                        id: self.approval_modal.request().unwrap().id.clone(),
                        decision,
                    })
                    .await;
            }
            return Ok(true);
        }

        // If command palette is visible, handle palette keys
        if self.command_palette.is_visible() {
            self.command_palette.handle_key(key);
            return Ok(true);
        }

        // Handle command palette trigger
        if key.code == KeyCode::Char(':') && key.modifiers == KeyModifiers::NONE {
            self.command_palette.show();
            return Ok(true);
        }

        // Handle other keys based on focus
        // For now, conversation pane gets scroll keys
        match key.code {
            KeyCode::Up => {
                self.conversation.scroll_up();
            }
            KeyCode::Down => {
                self.conversation.scroll_down();
            }
            KeyCode::PageUp => {
                self.conversation
                    .scroll_page_up(self.layout.conversation.height as usize);
            }
            KeyCode::PageDown => {
                self.conversation
                    .scroll_page_down(self.layout.conversation.height as usize);
            }
            KeyCode::Home => {
                self.conversation.scroll_to_bottom();
            }
            KeyCode::End => {
                self.conversation.scroll_to_top();
            }
            KeyCode::Tab => {
                // Cycle focus between panels (simplified)
            }
            KeyCode::Char('f') => {
                self.log_tail.cycle_filter();
            }
            KeyCode::Char(' ') => {
                // Toggle todo at selection
                if let Some(idx) = self.todo_panel.selected() {
                    self.todo_panel.toggle(idx);
                }
            }
            KeyCode::Enter => {
                // Expand/collapse tool call/result at selection
            }
            _ => {}
        }

        Ok(true)
    }

    /// Handle terminal resize.
    fn handle_resize(&mut self, width: u16, height: u16) -> Result<()> {
        self.width = width;
        self.height = height;

        let area = ratatui::layout::Rect::new(0, 0, width, height);
        let theme = self.theme_manager.current();
        self.layout = LayoutRects::new(area, &theme);

        // Update component widths
        self.conversation
            .set_max_width(self.layout.conversation.width);
        self.status_bar.set_width(width);

        // Resize terminal
        self.terminal.resize(area)?;
        Ok(())
    }

    /// Toggle theme.
    fn toggle_theme(&mut self) {
        if let Ok(_new_variant) = self.theme_manager.toggle() {
            let theme = self.theme_manager.current();
            self.conversation.set_theme(theme.clone());
            self.tool_panel.set_theme(theme.clone());
            self.todo_panel.set_theme(theme.clone());
            self.status_bar.set_theme(theme.clone());
            self.log_tail.set_theme(theme.clone());
            self.approval_modal.set_theme(theme.clone());
            self.command_palette.set_theme(theme.clone());
            self.layout = LayoutRects::new(
                ratatui::layout::Rect::new(0, 0, self.width, self.height),
                &theme,
            );
        }
    }

    /// Handle events from the event channel.
    async fn handle_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            // Update components
            self.conversation.handle_event(&event);
            self.tool_panel.handle_event(&event);
            self.todo_panel.handle_event(&event);
            self.log_tail.handle_event(&event);

            // Update status bar
            if let Event::Status(status) = &event {
                self.status_bar.update(status.clone());
            }

            // Show approval modal
            if let Event::ApprovalRequest(req) = &event {
                self.approval_modal.show(req.clone());
            }

            // Handle command execution from palette
            if let Event::Message(msg) = &event
                && msg.content.starts_with('/')
            {
                // Parse slash command
                let parts: Vec<&str> = msg.content[1..].split_whitespace().collect();
                if let Some(cmd) = parts.first() {
                    match *cmd {
                        "help" => { /* show help */ }
                        "undo" => { /* emit undo event */ }
                        "redo" => { /* emit redo event */ }
                        "approve" => { /* approve pending */ }
                        "deny" => { /* deny pending */ }
                        "todos" => {
                            self.todo_panel.toggle_expanded();
                        }
                        "logs" => { /* toggle logs */ }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Tick animations.
    fn tick(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_tick) >= Duration::from_millis(50) {
            self.conversation.tick();
            self.tool_panel.tick();
            self.last_tick = now;
        }
    }

    /// Render the UI.
    #[allow(clippy::too_many_arguments)]
    fn render_ui(
        frame: &mut ratatui::Frame,
        layout: &LayoutRects,
        conversation: &mut ConversationPane,
        tool_panel: &mut ToolPanel,
        todo_panel: &mut TodoPanel,
        status_bar: &mut StatusBar,
        log_tail: &mut LogTailPanel,
        approval_modal: &ApprovalModal,
        command_palette: &CommandPalette,
        theme: &Theme,
    ) {
        let area = frame.area();

        // Render main layout regions
        if layout.has_right_rail() {
            // Comfortable layout: conversation | right rail | status bar
            Self::render_comfortable_static(
                frame,
                layout,
                conversation,
                tool_panel,
                todo_panel,
                log_tail,
                status_bar,
                theme,
            );
        } else {
            // Minimum/narrow/short layout: stacked
            Self::render_stacked_static(
                frame,
                layout,
                conversation,
                tool_panel,
                todo_panel,
                log_tail,
                status_bar,
                theme,
            );
        }

        // Render modal overlay
        if approval_modal.is_visible() {
            approval_modal.render(area, frame.buffer_mut());
        }

        // Render command palette overlay
        if command_palette.is_visible() {
            command_palette.render(area, frame.buffer_mut());
        }
    }

    /// Render comfortable layout (3 regions) - static version.
    #[allow(clippy::too_many_arguments)]
    fn render_comfortable_static(
        frame: &mut ratatui::Frame,
        layout: &LayoutRects,
        conversation: &mut ConversationPane,
        tool_panel: &mut ToolPanel,
        todo_panel: &mut TodoPanel,
        log_tail: &mut LogTailPanel,
        status_bar: &mut StatusBar,
        theme: &Theme,
    ) {
        // Header
        Self::render_header_static(frame, layout.header, theme);

        // Conversation (left)
        conversation.render(layout.conversation, frame.buffer_mut());

        // Right rail (tool panel + todo + log tail)
        if let Some(right_rail) = layout.right_rail {
            Self::render_right_rail_static(
                frame, right_rail, tool_panel, todo_panel, log_tail, theme,
            );
        }

        // Status bar (bottom)
        status_bar.render(layout.status_bar, frame.buffer_mut());
    }

    /// Render stacked layout (minimum/narrow/short) - static version.
    #[allow(clippy::too_many_arguments)]
    fn render_stacked_static(
        frame: &mut ratatui::Frame,
        layout: &LayoutRects,
        conversation: &mut ConversationPane,
        tool_panel: &mut ToolPanel,
        todo_panel: &mut TodoPanel,
        log_tail: &mut LogTailPanel,
        status_bar: &mut StatusBar,
        theme: &Theme,
    ) {
        // Header
        Self::render_header_static(frame, layout.header, theme);

        // Conversation (main area)
        conversation.render(layout.conversation, frame.buffer_mut());

        // Folded right rail content at bottom of conversation
        if layout.mode != crate::ui::layout::LayoutMode::Minimum {
            let rail_area = layout.right_rail_area();
            if rail_area.height > 2 {
                Self::render_right_rail_static(
                    frame, rail_area, tool_panel, todo_panel, log_tail, theme,
                );
            }
        }

        // Status bar (bottom)
        status_bar.render(layout.status_bar, frame.buffer_mut());
    }

    /// Render header region - static version.
    fn render_header_static(
        frame: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
        theme: &Theme,
    ) {
        let line = ratatui::text::Line::from(vec![
            ratatui::text::Span::styled(
                " okeanos ",
                theme
                    .style(crate::ui::theme::SemanticRole::Tool)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            ),
            ratatui::text::Span::styled(
                "│",
                theme.style(crate::ui::theme::SemanticRole::CanvasBorder),
            ),
            ratatui::text::Span::styled(
                " interactive ",
                theme.style(crate::ui::theme::SemanticRole::System),
            ),
        ]);
        let paragraph = ratatui::widgets::Paragraph::new(line)
            .style(theme.style(crate::ui::theme::SemanticRole::CanvasBg));
        ratatui::widgets::Widget::render(paragraph, area, frame.buffer_mut());
    }

    /// Render right rail - static version.
    fn render_right_rail_static(
        frame: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
        tool_panel: &mut ToolPanel,
        todo_panel: &mut TodoPanel,
        log_tail: &mut LogTailPanel,
        _theme: &Theme,
    ) {
        let sections = 3;
        let section_height = area.height / sections;

        let layout = ratatui::layout::Layout::default()
            .direction(ratatui::layout::Direction::Vertical)
            .constraints([
                ratatui::layout::Constraint::Length(section_height.max(5)),
                ratatui::layout::Constraint::Length(section_height.max(5)),
                ratatui::layout::Constraint::Min(3),
            ])
            .split(area);

        tool_panel.render(layout[0], frame.buffer_mut());
        todo_panel.render(layout[1], frame.buffer_mut());
        log_tail.render(layout[2], frame.buffer_mut());
    }

    /// Suspend the TUI (for running external commands).
    pub fn suspend(&mut self) -> Result<()> {
        self.cleanup()?;
        self.suspended = true;
        Ok(())
    }

    /// Resume the TUI after suspension.
    pub fn resume(&mut self) -> Result<()> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
        crossterm::execute!(io::stdout(), crossterm::event::EnableMouseCapture)?;
        self.suspended = false;
        Ok(())
    }

    /// Cleanup terminal state.
    fn cleanup(&mut self) -> Result<()> {
        crossterm::execute!(io::stdout(), crossterm::event::DisableMouseCapture)?;
        crossterm::execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen)?;
        crossterm::terminal::disable_raw_mode()?;
        self.terminal.show_cursor()?;
        Ok(())
    }
}

impl Drop for TerminalApp {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use crate::ui::theme::ThemeVariant;

    #[test]
    fn test_theme_variant() {
        assert_eq!(ThemeVariant::default(), ThemeVariant::Dark);
        assert_eq!("light".parse::<ThemeVariant>(), Ok(ThemeVariant::Light));
        assert_eq!("dark".parse::<ThemeVariant>(), Ok(ThemeVariant::Dark));
    }
}
