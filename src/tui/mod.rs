pub mod app;
pub mod client;
pub mod event;
pub mod input;
pub mod sparkline;
pub mod terminal;
pub mod theme;
pub mod views;

pub use app::{GraceState, ModalState, PairingPromptModalState, Tab, TuiApp, UnenrollButton};
pub use client::TuiIpcBridge;
pub use event::{IpcEvent, TUI_TICK_MS, TuiCommand, TuiEvent};
pub use terminal::{detect_systemd_service_installed, run_tui};
pub use theme::Theme;
