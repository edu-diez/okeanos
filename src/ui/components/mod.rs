//! UI Components — conversation, tool panel, approval modal, todo panel, status bar, command palette, log tail, input composer, welcome screen, markdown, fleet view.

pub mod approval_modal;
pub mod command_palette;
pub mod config_modal;
pub mod conversation;
pub mod fleet_view;
pub mod input_composer;
pub mod log_tail;
#[cfg(feature = "markdown")]
pub mod markdown;
pub mod status_bar;
pub mod todo_panel;
pub mod tool_panel;
pub mod welcome_screen;

pub use approval_modal::ApprovalModal;
pub use command_palette::{CommandAction, CommandPalette};
pub use config_modal::{ConfigModal, ConfigModalResult};
pub use conversation::ConversationPane;
pub use fleet_view::FleetView;
pub use input_composer::{ComposerStatus, InputComposer};
pub use log_tail::{LogFilter, LogTailPanel};
#[cfg(feature = "markdown")]
pub use markdown::{RenderMode, render_markdown};
pub use status_bar::StatusBar;
pub use todo_panel::TodoPanel;
pub use tool_panel::ToolPanel;
pub use welcome_screen::WelcomeScreen;
