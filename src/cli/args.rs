use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::config::LogFormat;

/// BPMute — BLE Heart-Rate Sleep-Onset Daemon & CLI.
#[derive(Debug, Parser)]
#[command(
    name = "bpmute",
    version,
    about = "BPMute sleep-onset monitor daemon and CLI"
)]
pub struct Cli {
    /// Override configuration file path (`$XDG_CONFIG_HOME/bpmute/config.toml`).
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Override persistent data directory (`$XDG_DATA_HOME/bpmute`).
    #[arg(long, global = true, value_name = "PATH")]
    pub data_dir: Option<PathBuf>,

    /// Override daemon Unix domain socket path (`$XDG_RUNTIME_DIR/bpmute/daemon.sock`).
    #[arg(long, global = true, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// Override tracing filter level or EnvFilter expression (`info`, `debug`, `bpmute=trace`).
    #[arg(long, global = true, value_name = "FILTER")]
    pub log_level: Option<String>,

    /// Override log output format.
    #[arg(long, global = true, value_enum, value_name = "FORMAT")]
    pub log_format: Option<LogFormat>,

    /// Unlock raw BPM/RR telemetry and full MAC addresses in logs (emits startup WARN banner).
    #[arg(long, global = true)]
    pub log_biometrics: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the background BPMute daemon.
    Daemon {
        /// Exercise notifications/state transitions without executing Sleep Action Pipeline side effects.
        #[arg(long)]
        dry_run: bool,
    },
    /// Query live daemon status.
    Status {
        /// Emit machine-readable JSON matching the `daemon.status` IPC schema.
        #[arg(long)]
        json: bool,
    },
    /// Temporarily pause Sleep Action Pipeline triggers (`until-morning`, duration like `30m`, or indefinite).
    Pause {
        /// Duration (e.g. `30m`, `2h`, `1h30m`) or `until-morning`; omit to pause indefinitely.
        target: Option<String>,
    },
    /// Resume Sleep Action Pipeline triggers after a pause.
    Resume,
    /// Manage declarative TOML configuration (`config.toml`).
    Config {
        #[command(subcommand)]
        action: ConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Initialize `config.toml` with documented defaults (`0700`/`0600`).
    Init {
        /// Overwrite an existing `config.toml`.
        #[arg(long)]
        force: bool,
    },
    /// Validate `config.toml`, `BPMUTE_*` environment overrides, and `BPMUTE_LOG`.
    Validate,
    /// Print the resolved path to `config.toml`.
    Path,
    /// Print the effective configuration resolved from defaults, `config.toml`, env, and CLI flags.
    Show {
        /// Emit effective configuration as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Open `config.toml` in `$VISUAL` / `$EDITOR` and validate on exit.
    Edit,
    /// Ask the running daemon to reload `config.toml` live without dropping IPC connections.
    Reload,
}
