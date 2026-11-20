//! Command-line interface parsing, duration helpers, and subcommand dispatch.
//!
//! Subsystem structure:
//! - [`args`]: `clap` definitions (`Cli`, `Command`, subcommand enums).
//! - [`duration`]: Human duration and pause-target parsing (`parse_pause_target`).
//! - [`commands`]: Subcommand handlers split between daemon IPC (`mod`, `monitor`,
//!   `sessions`, `data`, `wearables`), offline/host-boundary commands (`completions`,
//!   `service`, `update`), and OS/systemd execution helpers (`system_exec`).

pub mod args;
pub mod commands;
pub mod duration;

pub use args::{Cli, Command, CompletionShell, ConfigCommand, ServiceCommand, ServiceTargetArg};
pub use commands::execute_cli;
pub use duration::{DurationParseError, PauseTarget, parse_pause_target};
