//! UI Components — conversation, tool panel, approval modal, todo panel, status bar, command palette, log tail, input composer, welcome screen.

pub mod approval_modal;
pub mod command_palette;
pub mod conversation;
pub mod input_composer;
pub mod log_tail;
pub mod status_bar;
pub mod todo_panel;
pub mod tool_panel;
pub mod welcome_screen;

pub use approval_modal::ApprovalModal;
pub use command_palette::{CommandAction, CommandPalette};
pub use conversation::ConversationPane;
pub use input_composer::{ComposerStatus, InputComposer};
pub use log_tail::{LogFilter, LogTailPanel};
pub use status_bar::StatusBar;
pub use todo_panel::TodoPanel;
pub use tool_panel::ToolPanel;
pub use welcome_screen::WelcomeScreen;
