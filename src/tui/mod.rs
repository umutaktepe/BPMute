pub mod app;
pub mod client;
pub mod event;
pub mod sparkline;
pub mod terminal;
pub mod theme;
pub mod views;

pub use app::{GraceState, ModalState, PairingPromptModalState, Tab, TuiApp};
pub use client::TuiIpcBridge;
pub use event::{IpcEvent, TuiCommand, TuiEvent};
pub use terminal::run_tui;
pub use theme::Theme;
