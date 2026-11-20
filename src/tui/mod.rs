//! Interactive Terminal User Interface (TUI) for BPMute.
//!
//! Subsystem structure:
//! - [`app`]: In-memory TUI state container (`TuiApp`), session caches, and offline DB/TOML loaders.
//! - [`client`]: Daemon IPC bridge task running asynchronous socket requests and background polling.
//! - [`terminal`]: Alternate-screen terminal setup, raw mode lifecycle, event loop runner, and `$EDITOR` handoff.
//! - [`input`]: Keyboard and mouse routing split across top-level tabs, modal interactions, and mouse clicks.
//! - [`views`]: Ratatui frame layout and view renderers (Dashboard, Wearables, History & Epoch Chart, Settings, Footer).
//! - [`sparkline`]: Real-time ASCII/Unicode sparkline HR widget.
//! - [`theme`]: 16-color ANSI and NO_COLOR-compliant theme definitions.

pub mod app;
pub mod client;
mod client_commands;
pub mod event;
pub(crate) mod hit_map;
pub mod input;
pub mod sparkline;
pub mod terminal;
pub mod theme;
pub mod views;

pub use app::{
    GraceState, ModalState, OpFeedback, PairingPromptModalState, Tab, TuiApp, UnenrollButton,
};
pub use client::TuiIpcBridge;
pub use event::{IpcEvent, TUI_TICK_MS, TuiCommand, TuiEvent};
pub use terminal::{detect_systemd_service_installed, run_tui};
pub use theme::Theme;
