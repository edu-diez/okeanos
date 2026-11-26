//! TUI module — terminal rendering per DESIGN.md.
//!
//! Uses ratatui + crossterm; themes from assets/themes/.
//! Consumes the same event stream that headless/RPC mode emits (DESIGN.md §7).

pub mod components;
pub mod layout;
pub mod theme;

use crate::events::{Event, EventReceiver, EventSender, event_channel};
use crate::ui::components::welcome_screen::WelcomeAction;
use crate::ui::components::{
    ApprovalModal, CommandAction, CommandPalette, ConfigModal, ConfigModalResult, ConversationPane,
    FleetView, InputComposer, LogTailPanel, StatusBar, TodoPanel, ToolPanel, WelcomeScreen,
};
use crate::ui::layout::LayoutRects;

pub use crate::ui::theme::{Theme, ThemeManager, ThemeVariant};
use anyhow::Result;
use atty;
use crossterm::event::{self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Undo/redo action from command palette.
#[derive(Debug, Clone, Copy)]
pub enum UndoRedoAction {
    Undo,
    Redo,
}

/// Compaction action from command palette.
#[derive(Debug, Clone, Copy)]
pub enum CompactionAction {
    /// Trigger manual compaction now.
    Compact,
}

/// TUI application state for single-turn interactive mode.
pub struct TerminalApp {
    pub theme_manager: ThemeManager,
    pub layout: LayoutRects,
    pub conversation: ConversationPane,
    pub tool_panel: ToolPanel,
    pub todo_panel: TodoPanel,
    pub fleet_view: FleetView,
    pub status_bar: StatusBar,
    /// Shared permission mode, cycled with Shift+Tab.
    pub permission_mode: Option<crate::harness::ModeHandle>,
    pub log_tail: LogTailPanel,
    pub approval_modal: ApprovalModal,
    pub command_palette: CommandPalette,
    pub input_composer: InputComposer,
    pub welcome_screen: WelcomeScreen,
    pub config_modal: ConfigModal,
    pub config_modal_tx: mpsc::Sender<ConfigModalResult>,
    pub config_modal_rx: Option<mpsc::Receiver<ConfigModalResult>>,
    pub event_rx: EventReceiver,
    pub event_tx: EventSender,
    pub approval_tx: mpsc::Sender<crate::events::Approval>,
    pub approval_rx: Option<mpsc::Receiver<crate::events::Approval>>,
    pub composer_tx: mpsc::Sender<String>,
    pub composer_rx: Option<mpsc::Receiver<String>>,
    /// Channel for command palette actions
    pub palette_tx: mpsc::Sender<CommandAction>,
    pub palette_rx: mpsc::Receiver<CommandAction>,
    /// Channel for undo/redo actions
    pub undo_redo_tx: mpsc::Sender<UndoRedoAction>,
    pub undo_redo_rx: mpsc::Receiver<UndoRedoAction>,
    /// Channel for compaction actions
    pub compaction_tx: mpsc::Sender<CompactionAction>,
    pub compaction_rx: mpsc::Receiver<CompactionAction>,
    /// Session manager for log rotation (shared with harness)
    pub session_manager:
        Option<std::sync::Arc<tokio::sync::Mutex<crate::session::log::SessionManager>>>,
    pub terminal: Terminal<CrosstermBackend<io::Stdout>>,
    pub last_tick: Instant,
    pub running: bool,
    pub suspended: bool,
    pub width: u16,
    pub height: u16,
    /// Whether we're waiting for a turn to complete
    pub turn_in_progress: bool,
    /// Pending turn result from the harness
    pub pending_decision: Option<crate::harness::react::LoopDecision>,
    /// Whether a provider is configured (from config load or config save)
    pub provider_configured: bool,
    /// Shutdown sender for the current turn (to interrupt model generation)
    pub turn_shutdown_tx: Option<mpsc::Sender<()>>,
}

impl TerminalApp {
    /// Create a new TerminalApp with the given theme variant, optional session manager, and markdown render mode.
    pub fn new(
        theme_variant: ThemeVariant,
        session_manager: Option<
            std::sync::Arc<tokio::sync::Mutex<crate::session::log::SessionManager>>,
        >,
        #[cfg(feature = "markdown")] render_mode: crate::ui::components::markdown::RenderMode,
    ) -> Result<Self> {
        let theme_manager = ThemeManager::new(theme_variant)?;
        let theme = theme_manager.current();

        // Initialize crossterm backend
        let backend = CrosstermBackend::new(io::stdout());
        let terminal = Terminal::new(backend)
            .map_err(|e| anyhow::anyhow!("failed to create terminal backend: {}", e))?;

        // Enable raw mode and alternate screen
        // Check if stdout is a TTY first for better error message
        if !atty::is(atty::Stream::Stdout) {
            return Err(anyhow::anyhow!(
                "stdout is not a TTY (is it redirected?). Interactive TUI requires a real terminal."
            ));
        }
        crossterm::terminal::enable_raw_mode()
            .map_err(|e| anyhow::anyhow!("failed to enable raw mode: {}", e))?;
        crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen)
            .map_err(|e| anyhow::anyhow!("failed to enter alternate screen: {}", e))?;
        crossterm::execute!(io::stdout(), crossterm::event::EnableMouseCapture)
            .map_err(|e| anyhow::anyhow!("failed to enable mouse capture: {}", e))?;

        // Get initial terminal size
        let size = terminal
            .size()
            .map_err(|e| anyhow::anyhow!("failed to get terminal size: {}", e))?;
        let width = size.width;
        let height = size.height;

        // Create event channel (shared with harness)
        let (event_tx, event_rx) = event_channel();
        let (approval_tx, approval_rx) = mpsc::channel(16);
        let (composer_tx, composer_rx) = mpsc::channel(16);
        let (palette_tx, palette_rx) = mpsc::channel(16);
        let (undo_redo_tx, undo_redo_rx) = mpsc::channel(16);
        let (compaction_tx, compaction_rx) = mpsc::channel(16);

        // Create layout
        let area = ratatui::layout::Rect::new(0, 0, width, height);
        let layout = LayoutRects::new(area, &theme);

        // Create components
        #[cfg(feature = "markdown")]
        let mut conversation = ConversationPane::new(theme.clone());
        #[cfg(not(feature = "markdown"))]
        let conversation = ConversationPane::new(theme.clone());
        #[cfg(feature = "markdown")]
        {
            conversation.set_render_mode(render_mode);
        }
        let tool_panel = ToolPanel::new(theme.clone());
        let todo_panel = TodoPanel::new(theme.clone());
        let fleet_view = FleetView::new(theme.clone());
        let status_bar = StatusBar::new(theme.clone());
        let log_tail = LogTailPanel::new(theme.clone());
        let approval_modal = ApprovalModal::new(theme.clone());
        let command_palette = CommandPalette::new(theme.clone());
        let input_composer = InputComposer::new(theme.clone());
        let welcome_screen =
            WelcomeScreen::new(theme.clone(), env!("CARGO_PKG_VERSION").to_string());
        let config_modal = ConfigModal::new(theme.clone());
        let (config_modal_tx, config_modal_rx) = mpsc::channel(16);

        // Set up command palette callback
        let mut command_palette = command_palette;
        let palette_tx_clone = palette_tx.clone();
        command_palette.on_execute(move |action| {
            let _ = palette_tx_clone.try_send(action);
        });

        // Set up input composer callback
        let mut input_composer = input_composer;
        input_composer.on_submit(composer_tx.clone());

        Ok(Self {
            theme_manager,
            layout,
            conversation,
            tool_panel,
            todo_panel,
            fleet_view,
            status_bar,
            permission_mode: None,
            log_tail,
            approval_modal,
            command_palette,
            input_composer,
            welcome_screen,
            config_modal,
            config_modal_tx,
            config_modal_rx: Some(config_modal_rx),
            event_rx,
            event_tx,
            approval_tx,
            approval_rx: Some(approval_rx),
            composer_tx,
            composer_rx: Some(composer_rx),
            palette_tx,
            palette_rx,
            undo_redo_tx,
            undo_redo_rx,
            compaction_tx,
            compaction_rx,
            session_manager,
            terminal,
            last_tick: Instant::now(),
            running: true,
            suspended: false,
            width,
            height,
            turn_in_progress: false,
            pending_decision: None,
            provider_configured: false,
            turn_shutdown_tx: None,
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

    /// Get the composer receiver for receiving user prompts.
    pub fn take_composer_receiver(&mut self) -> Option<mpsc::Receiver<String>> {
        self.composer_rx.take()
    }

    /// Check if a turn is in progress.
    pub fn is_turn_in_progress(&self) -> bool {
        self.turn_in_progress
    }

    /// Set the turn in progress state.
    pub fn set_turn_in_progress(&mut self, in_progress: bool) {
        self.turn_in_progress = in_progress;
        self.input_composer.set_disabled(in_progress);
        if in_progress {
            self.status_bar.set_mode("running");
        } else {
            self.status_bar.set_mode("interactive");
        }
    }

    /// Set the pending turn decision.
    pub fn set_pending_decision(&mut self, decision: crate::harness::react::LoopDecision) {
        self.pending_decision = Some(decision);
    }

    /// Take the pending turn decision.
    pub fn take_pending_decision(&mut self) -> Option<crate::harness::react::LoopDecision> {
        self.pending_decision.take()
    }

    /// Get a reference to the pending turn decision.
    pub fn pending_decision(&self) -> Option<&crate::harness::react::LoopDecision> {
        self.pending_decision.as_ref()
    }

    /// Share the policy's permission mode so Shift+Tab can cycle it.
    pub fn set_permission_mode(&mut self, handle: crate::harness::ModeHandle) {
        self.status_bar.set_permission_mode(handle.clone());
        self.permission_mode = Some(handle);
    }

    /// Set provider/model configuration status for welcome screen.
    pub fn set_config_status(&mut self, provider_configured: bool, model_name: Option<String>) {
        self.provider_configured = provider_configured;
        self.welcome_screen
            .set_config_status(provider_configured, model_name);
        // Update composer placeholder based on config state
        if provider_configured {
            self.input_composer
                .set_placeholder("Type your prompt…".to_string());
        } else {
            self.input_composer
                .set_placeholder("Describe what you want to do…".to_string());
        }
    }

    /// Handle a config save event: mark provider configured and update UI.
    pub fn on_config_saved(&mut self, _saved: &crate::events::ConfigSaved) {
        self.provider_configured = true;
        self.welcome_screen.set_config_status(true, None);
        self.input_composer
            .set_placeholder("Type your prompt…".to_string());
    }

    /// Check if welcome screen is visible.
    pub fn welcome_screen_visible(&self) -> bool {
        self.welcome_screen.is_visible()
    }

    /// Check if config modal is visible.
    pub fn config_modal_visible(&self) -> bool {
        self.config_modal.is_visible()
    }

    /// Open the configuration modal (stepper form for provider setup).
    pub fn open_config_modal(&mut self) {
        self.config_modal
            .set_result_tx(self.config_modal_tx.clone());
        self.config_modal.show();
        // Emit config_modal open event
        let _ = self.event_tx.send(crate::events::Event::config_modal(
            crate::events::ConfigModalAction::Open,
            Some(1),
            None,
        ));
    }

    /// Check for and handle config modal results (saved/cancelled).
    pub fn check_config_modal_result(&mut self) {
        if let Some(ref mut rx) = self.config_modal_rx
            && let Ok(result) = rx.try_recv()
        {
            match result {
                ConfigModalResult::Saved { path, data } => {
                    let provider_kind = data
                        .provider_kind
                        .as_ref()
                        .map(|p| p.label().to_string())
                        .unwrap_or_default();
                    let saved = crate::events::ConfigSaved {
                        path: path.to_string_lossy().to_string(),
                        provider_kind: provider_kind.clone(),
                        model: data.model.clone(),
                        base_url: data.base_url.clone(),
                    };
                    let _ = self.event_tx.send(crate::events::Event::config_saved(
                        saved.path.clone(),
                        saved.provider_kind.clone(),
                        saved.model.clone(),
                        saved.base_url.clone(),
                    ));
                    self.on_config_saved(&saved);
                }
                ConfigModalResult::Cancelled => {
                    // Modal cancelled, no state change needed
                }
            }
        }
    }

    /// Get a reference to the welcome screen for rendering.
    pub fn welcome_screen(&self) -> &WelcomeScreen {
        &self.welcome_screen
    }

    /// Get the current theme.
    pub fn current_theme(&self) -> Arc<Theme> {
        self.theme_manager.current()
    }

    /// Handle events from the ReAct loop (public version for external loop).
    pub async fn handle_events_from_loop<M: crate::model::Model>(
        &mut self,
        _loop_: &mut crate::harness::react::ReactLoop<M>,
    ) {
        // Process events from the loop's event channel
        while let Ok(event) = self.event_rx.try_recv() {
            // Update components
            self.conversation.handle_event(&event);
            self.tool_panel.handle_event(&event);
            self.todo_panel.handle_event(&event);
            self.fleet_view.handle_event(&event);
            self.log_tail.handle_event(&event);
            self.input_composer.handle_event(&event);

            // Update status bar
            if let Event::Status(status) = &event {
                self.status_bar.update(status.clone());
            }

            // Show approval modal
            if let Event::ApprovalRequest(req) = &event {
                self.approval_modal.show(req.clone());
            }

            // Handle typing indicator for streaming
            if let Event::Token(_) = &event {
                self.conversation.set_typing_indicator(false);
            }
        }

        // Also check for command palette actions
        if let Ok(action) = self.palette_rx.try_recv() {
            self.handle_command_action(action).await;
        }
    }

    /// Called when a turn completes.
    pub async fn on_turn_complete(&mut self) {
        if let Some(decision) = self.take_pending_decision() {
            self.set_turn_in_progress(false);

            match decision {
                crate::harness::react::LoopDecision::Continue => {
                    // Turn completed, ready for next input
                }
                crate::harness::react::LoopDecision::Complete(result) => {
                    // Task completed - could show a completion message
                    self.emit_log(
                        crate::events::LogLevel::Info,
                        format!("Task complete: {}", result),
                    )
                    .await;
                }
                crate::harness::react::LoopDecision::Stop(reason) => {
                    // Loop stopped
                    self.emit_log(
                        crate::events::LogLevel::Warn,
                        format!("Loop stopped: {}", reason),
                    )
                    .await;
                }
            }
        }
    }

    /// Emit a log event.
    pub async fn emit_log(&self, level: crate::events::LogLevel, message: String) {
        let event = crate::events::Event::Log(crate::events::Log { level, message });
        let _ = self.event_tx.send(event);
    }

    /// Tick animations.
    pub fn tick(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_tick) >= Duration::from_millis(50) {
            self.conversation.tick();
            self.tool_panel.tick();
            self.input_composer.tick();
            self.last_tick = now;
        }
    }

    /// Cleanup terminal state.
    pub fn cleanup(&mut self) -> Result<()> {
        crossterm::execute!(io::stdout(), crossterm::event::DisableMouseCapture)?;
        crossterm::execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen)?;
        crossterm::terminal::disable_raw_mode()?;
        self.terminal.show_cursor()?;
        Ok(())
    }

    /// Run the TUI event loop (single-turn architecture).
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

            // Check for composer input (user submitted a prompt)
            if let Some(ref mut rx) = self.composer_rx
                && let Ok(prompt) = rx.try_recv()
            {
                self.handle_user_prompt(prompt).await;
            }

            // Check for command palette actions
            if let Ok(action) = self.palette_rx.try_recv() {
                self.handle_command_action(action).await;
            }

            // Handle events from the harness
            self.handle_events().await;

            // Check for completed turn
            if self.turn_in_progress && self.pending_decision.is_some() {
                self.on_turn_complete().await;
            }

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
                    &mut self.fleet_view,
                    &mut self.status_bar,
                    &mut self.log_tail,
                    &self.approval_modal,
                    &self.command_palette,
                    &mut self.input_composer,
                    &self.welcome_screen,
                    &self.config_modal,
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

    /// Handle a user prompt from the input composer.
    async fn handle_user_prompt(&mut self, prompt: String) {
        // Hide welcome screen on first user input
        if self.welcome_screen.is_visible() {
            self.welcome_screen.hide();
        }

        // Emit user message event
        let user_msg = crate::events::Message {
            role: crate::events::MessageRole::User,
            content: prompt.clone(),
            tool_calls: None,
            tool_call_id: None,
            ts: current_timestamp(),
            thinking: None,
            extras: Default::default(),
        };
        let _ = self.event_tx.send(crate::events::Event::Message(user_msg));

        // Mark turn as in progress
        self.set_turn_in_progress(true);
    }

    /// Handle a key event.
    pub async fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        // If config modal is visible, route keys to it first
        if self.config_modal.is_visible() {
            self.config_modal.handle_key(key);
            return Ok(true);
        }

        // Handle welcome screen (Esc dismiss, Enter configure provider; other keys ignored)
        if self.welcome_screen.is_visible() {
            if let Some(action) = self.welcome_screen.handle_key(key) {
                match action {
                    WelcomeAction::Dismiss => {
                        // Welcome dismissed, composer stays focused
                    }
                    WelcomeAction::ConfigureProvider => {
                        self.open_config_modal();
                    }
                }
            }
            return Ok(true);
        }

        // Global shortcuts
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                if self.turn_in_progress {
                    // Ctrl+C during turn: cancel the turn
                    self.input_composer.set_error("Cancelled".to_string());
                    // Send interrupt signal if we have a shutdown sender
                    if let Some(tx) = self.turn_shutdown_tx.take() {
                        let _ = tx.send(()).await;
                    }
                } else {
                    // Ctrl+C at idle: exit
                    self.running = false;
                }
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
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                // Ctrl+D: exit
                if !self.turn_in_progress && self.input_composer.get_input().is_empty() {
                    self.running = false;
                }
                return Ok(true);
            }
            (KeyCode::BackTab, _)
                if !self.approval_modal.is_visible() && !self.command_palette.is_visible() =>
            {
                // Shift+Tab: cycle the permission mode (default → accept-edits → plan → yolo)
                if let Some(handle) = &self.permission_mode {
                    let mode = handle.cycle();
                    self.input_composer
                        .set_error(format!("Permission mode: {mode}"));
                }
                return Ok(true);
            }
            (KeyCode::Esc, KeyModifiers::NONE) if self.turn_in_progress => {
                // Esc during turn: interrupt model generation
                self.input_composer.set_error("Turn cancelled".to_string());
                if let Some(tx) = self.turn_shutdown_tx.take() {
                    let _ = tx.send(()).await;
                }
                return Ok(true);
            }
            (KeyCode::Esc, KeyModifiers::NONE) => {
                // Esc at idle: no-op (welcome screen handled above)
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

        // Let input composer handle keys if not in a turn
        if !self.turn_in_progress && self.input_composer.handle_key(key) {
            return Ok(true);
        }

        // Handle other keys based on focus (conversation scroll, etc.)
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
                if let Some(_msg_idx) = self.conversation.selected() {
                    // Could expand tool calls here
                }
            }
            _ => {}
        }

        Ok(true)
    }

    /// Handle a command palette action.
    async fn handle_command_action(&mut self, action: CommandAction) {
        match action {
            CommandAction::Help => {
                self.emit_log(
                    crate::events::LogLevel::Info,
                    "Help: Commands: /help, /undo, /redo, /todos, /logs, /approve, /deny"
                        .to_string(),
                )
                .await;
            }
            CommandAction::Undo => {
                let _ = self.undo_redo_tx.try_send(UndoRedoAction::Undo);
                self.emit_log(crate::events::LogLevel::Info, "Undo requested".to_string())
                    .await;
            }
            CommandAction::Redo => {
                let _ = self.undo_redo_tx.try_send(UndoRedoAction::Redo);
                self.emit_log(crate::events::LogLevel::Info, "Redo requested".to_string())
                    .await;
            }
            CommandAction::Approve => {
                // Approve pending approval (if any)
                if let Some(req) = self.approval_modal.request() {
                    let _ = self
                        .approval_tx
                        .send(crate::events::Approval {
                            id: req.id.clone(),
                            decision: crate::events::ApprovalDecision::Allow,
                        })
                        .await;
                }
            }
            CommandAction::Deny => {
                // Deny pending approval (if any)
                if let Some(req) = self.approval_modal.request() {
                    let _ = self
                        .approval_tx
                        .send(crate::events::Approval {
                            id: req.id.clone(),
                            decision: crate::events::ApprovalDecision::Deny,
                        })
                        .await;
                }
            }
            CommandAction::Todos => {
                self.todo_panel.toggle_expanded();
            }
            CommandAction::Logs => {
                self.log_tail.cycle_filter();
            }
            CommandAction::Compact => {
                let _ = self.compaction_tx.try_send(CompactionAction::Compact);
                self.emit_log(
                    crate::events::LogLevel::Info,
                    "Manual compaction requested".to_string(),
                )
                .await;
            }
            CommandAction::Rotate => {
                if let Some(session_manager) = &self.session_manager {
                    let session_manager = session_manager.clone();
                    tokio::spawn(async move {
                        let mut sm = session_manager.lock().await;
                        if let Err(e) = sm.rotate() {
                            eprintln!("Failed to rotate session: {}", e);
                        } else {
                            eprintln!("Session log rotated to new segment");
                        }
                    });
                }
            }
            CommandAction::Custom(_) => {}
        }
    }

    /// Handle terminal resize.
    pub fn handle_resize(&mut self, width: u16, height: u16) -> Result<()> {
        self.width = width;
        self.height = height;

        let area = ratatui::layout::Rect::new(0, 0, width, height);
        let theme = self.theme_manager.current();
        self.layout = LayoutRects::new(area, &theme);

        // Update component widths
        self.conversation
            .set_max_width(self.layout.conversation.width);
        self.status_bar.set_width(width);
        self.input_composer.set_max_width(width);

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
            self.input_composer.set_theme(theme.clone());
            self.welcome_screen.set_theme(theme.clone());
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
            self.fleet_view.handle_event(&event);
            self.log_tail.handle_event(&event);
            self.input_composer.handle_event(&event);

            // Update status bar
            if let Event::Status(status) = &event {
                self.status_bar.update(status.clone());
                // Track provider_configured from status events
                self.provider_configured = status.provider_configured;
            }

            // Show approval modal
            if let Event::ApprovalRequest(req) = &event {
                self.approval_modal.show(req.clone());
            }

            // Handle config saved: update UI state
            if let Event::ConfigSaved(saved) = &event {
                self.on_config_saved(saved);
            }

            // Handle typing indicator for streaming
            if let Event::Token(_) = &event {
                self.conversation.set_typing_indicator(false);
            }
        }

        // Check for config modal results (saved/cancelled)
        self.check_config_modal_result();
    }

    /// Render the UI (public for external loop integration).
    #[allow(clippy::too_many_arguments)]
    pub fn render_ui(
        frame: &mut ratatui::Frame,
        layout: &LayoutRects,
        conversation: &mut ConversationPane,
        tool_panel: &mut ToolPanel,
        todo_panel: &mut TodoPanel,
        fleet_view: &mut FleetView,
        status_bar: &mut StatusBar,
        log_tail: &mut LogTailPanel,
        approval_modal: &ApprovalModal,
        command_palette: &CommandPalette,
        input_composer: &mut InputComposer,
        welcome_screen: &WelcomeScreen,
        config_modal: &ConfigModal,
        theme: &Theme,
    ) {
        let area = frame.area();

        // Always render main layout regions (input composer always visible)
        if layout.has_right_rail() {
            // Comfortable layout: header | conversation | right rail | input composer | status bar
            Self::render_comfortable(
                frame,
                layout,
                conversation,
                tool_panel,
                todo_panel,
                fleet_view,
                log_tail,
                status_bar,
                input_composer,
                theme,
            );
        } else {
            // Minimum/narrow/short layout: stacked
            Self::render_stacked(
                frame,
                layout,
                conversation,
                tool_panel,
                todo_panel,
                fleet_view,
                log_tail,
                status_bar,
                input_composer,
                theme,
            );
        }

        // Render modal overlays on top
        if welcome_screen.is_visible() {
            welcome_screen.render(area, frame.buffer_mut());
        }

        if config_modal.is_visible() {
            config_modal.render(area, frame.buffer_mut());
        }

        // Render approval modal overlay
        if approval_modal.is_visible() {
            approval_modal.render(area, frame.buffer_mut());
        }

        // Render command palette overlay
        if command_palette.is_visible() {
            command_palette.render(area, frame.buffer_mut());
        }
    }

    /// Render comfortable layout (3+ regions) with input composer.
    #[allow(clippy::too_many_arguments)]
    fn render_comfortable(
        frame: &mut ratatui::Frame,
        layout: &LayoutRects,
        conversation: &mut ConversationPane,
        tool_panel: &mut ToolPanel,
        todo_panel: &mut TodoPanel,
        fleet_view: &mut FleetView,
        log_tail: &mut LogTailPanel,
        status_bar: &mut StatusBar,
        input_composer: &mut InputComposer,
        theme: &Theme,
    ) {
        // Header
        Self::render_header(frame, layout.header, theme);

        // Conversation (left)
        conversation.render(layout.conversation, frame.buffer_mut());

        // Right rail (tool panel + todo + fleet view + log tail)
        if let Some(right_rail) = layout.right_rail {
            Self::render_right_rail(
                frame, right_rail, tool_panel, todo_panel, fleet_view, log_tail, theme,
            );
        }

        // Input composer (above status bar)
        let composer_height = input_composer.required_height();
        let composer_area = ratatui::layout::Rect {
            x: layout.status_bar.x,
            y: layout.status_bar.y.saturating_sub(composer_height),
            width: layout.status_bar.width,
            height: composer_height,
        };
        input_composer.render(composer_area, frame.buffer_mut());

        // Status bar (bottom)
        status_bar.render(layout.status_bar, frame.buffer_mut());
    }

    /// Render stacked layout (minimum/narrow/short) with input composer.
    #[allow(clippy::too_many_arguments)]
    fn render_stacked(
        frame: &mut ratatui::Frame,
        layout: &LayoutRects,
        conversation: &mut ConversationPane,
        tool_panel: &mut ToolPanel,
        todo_panel: &mut TodoPanel,
        fleet_view: &mut FleetView,
        log_tail: &mut LogTailPanel,
        status_bar: &mut StatusBar,
        input_composer: &mut InputComposer,
        theme: &Theme,
    ) {
        // Header
        Self::render_header(frame, layout.header, theme);

        // Calculate composer area (above status bar)
        let composer_height = input_composer.required_height();
        let composer_area = ratatui::layout::Rect {
            x: layout.status_bar.x,
            y: layout.status_bar.y.saturating_sub(composer_height),
            width: layout.status_bar.width,
            height: composer_height,
        };

        // Conversation area (header to composer)
        let conversation_area = ratatui::layout::Rect {
            x: layout.conversation.x,
            y: layout.conversation.y,
            width: layout.conversation.width,
            height: layout.conversation.height.saturating_sub(composer_height),
        };
        conversation.render(conversation_area, frame.buffer_mut());

        // Folded right rail content at bottom of conversation
        if layout.mode != crate::ui::layout::LayoutMode::Minimum {
            let rail_area = layout.right_rail_area();
            if rail_area.height > 2 {
                let rail_height = rail_area
                    .height
                    .min(conversation_area.height.saturating_sub(5));
                let rail_area_adjusted = ratatui::layout::Rect {
                    x: rail_area.x,
                    y: conversation_area.y + conversation_area.height.saturating_sub(rail_height),
                    width: rail_area.width,
                    height: rail_height,
                };
                Self::render_right_rail(
                    frame,
                    rail_area_adjusted,
                    tool_panel,
                    todo_panel,
                    fleet_view,
                    log_tail,
                    theme,
                );
            }
        }

        // Input composer
        input_composer.render(composer_area, frame.buffer_mut());

        // Status bar (bottom)
        status_bar.render(layout.status_bar, frame.buffer_mut());
    }

    /// Render header region.
    fn render_header(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, theme: &Theme) {
        let line = ratatui::text::Line::from(vec![
            ratatui::text::Span::styled(
                " okeanos ∼ ",
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

    /// Render right rail.
    fn render_right_rail(
        frame: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
        tool_panel: &mut ToolPanel,
        todo_panel: &mut TodoPanel,
        fleet_view: &mut FleetView,
        log_tail: &mut LogTailPanel,
        _theme: &Theme,
    ) {
        let sections = 4;
        let section_height = area.height / sections;

        let layout = ratatui::layout::Layout::default()
            .direction(ratatui::layout::Direction::Vertical)
            .constraints([
                ratatui::layout::Constraint::Length(section_height.max(5)),
                ratatui::layout::Constraint::Length(section_height.max(5)),
                ratatui::layout::Constraint::Length(section_height.max(5)),
                ratatui::layout::Constraint::Min(3),
            ])
            .split(area);

        tool_panel.render(layout[0], frame.buffer_mut());
        todo_panel.render(layout[1], frame.buffer_mut());
        fleet_view.render(layout[2], frame.buffer_mut());
        log_tail.render(layout[3], frame.buffer_mut());
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
}

impl Drop for TerminalApp {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

/// Get current Unix timestamp in milliseconds.
pub fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
