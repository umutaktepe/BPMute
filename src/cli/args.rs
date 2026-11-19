use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::calibration::FeedbackVerdict;
use crate::config::{ClockTime, LogFormat, Sensitivity};

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
    /// Cancel an active Grace Period countdown ("Cancel (I'm Awake)").
    Cancel,
    /// Snooze sleep triggers during an active Grace Period without calibration penalty.
    Snooze {
        /// Optional snooze duration (e.g. `30m`, `1h`); defaults to configured snooze duration.
        duration: Option<String>,
    },
    /// Exercise notifications and state transitions via a test trigger without executing Sleep Action Pipeline side effects.
    TestTrigger {
        /// Optional custom Grace Period duration in seconds (10–300 s).
        #[arg(long, value_name = "SEC")]
        grace_sec: Option<u32>,
    },
    /// Replay a recorded session (--session <id>) or canonical CSV/JSONL trace read-only through the sleep-onset engine.
    Replay {
        /// Path to canonical CSV or JSONL trace file.
        #[arg(
            value_name = "FILE",
            required_unless_present = "session",
            conflicts_with = "session"
        )]
        file: Option<PathBuf>,
        /// Replay a stored Monitoring Session UUID (or unambiguous prefix) from the local SQLite ledger.
        #[arg(long, value_name = "ID", conflicts_with = "file")]
        session: Option<String>,
        /// Override sensitivity preset for what-if comparison.
        #[arg(long, value_enum, value_name = "NAME")]
        preset: Option<Sensitivity>,
        /// Force Cold Start priors regardless of personal calibration history.
        #[arg(long)]
        cold_start: bool,
        /// Emit machine-readable JSON replay report (`epochs`, transitions, and sessions).
        #[arg(long)]
        json: bool,
    },
    /// Inspect Monitoring Sessions and edit calibration feedback verdicts or Wearable-app overrides.
    Sessions {
        #[command(subcommand)]
        action: SessionsCommand,
    },
    /// Inspect and manage adaptive per-user calibration state.
    Calibration {
        #[command(subcommand)]
        action: CalibrationCommand,
    },
    /// Manage declarative TOML configuration (`config.toml`).
    Config {
        #[command(subcommand)]
        action: ConfigCommand,
    },
    /// Manage enrolled BLE Heart Rate Service wearables.
    Wearables {
        #[command(subcommand)]
        action: WearablesCommand,
    },
    /// Interactively scan and enroll a standard BLE 0x180D Wearable.
    Enroll {
        /// Explicit MAC address to enroll (bypasses discovery list).
        #[arg(long, value_name = "MAC")]
        address: Option<String>,
        /// Optional alias to assign to the enrolled wearable.
        #[arg(long, value_name = "NAME")]
        alias: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum WearablesCommand {
    /// List enrolled Wearables.
    List {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Rename an enrolled Wearable's alias.
    Rename {
        /// Wearable UUIDv7 ID or unambiguous prefix.
        id: String,
        /// New alias name.
        alias: String,
    },
    /// Request connection to an enrolled Wearable.
    Connect {
        /// Wearable UUIDv7 ID or unambiguous prefix.
        id: String,
    },
    /// Unenroll a Wearable, removing it from BPMute's allowlist.
    Unenroll {
        /// Wearable UUIDv7 ID or unambiguous prefix.
        id: String,
        /// Also remove BlueZ system pairing/device record (Adapter1.RemoveDevice).
        #[arg(long)]
        forget_bluez: bool,
        /// Also delete historical Monitoring Sessions associated with this Wearable.
        #[arg(long)]
        delete_sessions: bool,
    },
    /// Interactively scan and enroll a standard BLE 0x180D Wearable.
    Enroll {
        /// Explicit MAC address to enroll (bypasses discovery list).
        #[arg(long, value_name = "MAC")]
        address: Option<String>,
        /// Optional alias to assign to the enrolled wearable.
        #[arg(long, value_name = "NAME")]
        alias: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum SessionsCommand {
    /// List recorded Monitoring Sessions newest-first.
    List {
        /// Maximum number of sessions to display.
        #[arg(long, default_value_t = 20, value_name = "N")]
        limit: u32,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show detailed metadata, feedback, and 15-second Epochs for a Monitoring Session.
    Show {
        /// Session UUIDv7 or unambiguous ID prefix.
        id: String,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Record or clear feedback (`--verdict`, `--onset`, `--avg-sleep-hr`, or `--clear`) on a closed session.
    Feedback {
        /// Session UUIDv7 or unambiguous ID prefix.
        id: String,
        /// Feedback verdict (`false_positive`, `missed_sleep`, or `anomalous_exclude`).
        #[arg(long, value_enum, conflicts_with = "clear")]
        verdict: Option<FeedbackVerdict>,
        /// Wearable-app reported sleep-onset local time (`HH:MM`).
        #[arg(long, value_name = "HH:MM", conflicts_with = "clear")]
        onset: Option<ClockTime>,
        /// Wearable-app reported overnight average sleeping HR (`30..=140` bpm).
        #[arg(long, value_name = "BPM", conflicts_with = "clear")]
        avg_sleep_hr: Option<f64>,
        /// Clear all stored feedback overrides for this session.
        #[arg(long)]
        clear: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum CalibrationCommand {
    /// Show Calibration Maturity, learned personal priors, and per-Wearable signal floors.
    Status {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Set or clear the optional Day-1 average sleeping-HR seed (`30..=140` bpm).
    Seed {
        /// Overnight average sleeping HR in BPM (`+2.5 bpm` onset nadir offset applied automatically).
        #[arg(
            value_name = "BPM",
            required_unless_present = "clear",
            conflicts_with = "clear"
        )]
        bpm: Option<f64>,
        /// Clear the Day-1 sleeping-HR seed.
        #[arg(long, conflicts_with = "bpm")]
        clear: bool,
    },
    /// Accept a pending 5-session persistent-shift prompt by inserting a reversible Calibration Boundary.
    AcceptShift,
    /// Dismiss a pending 5-session persistent-shift prompt (`Keep ignoring`).
    DismissShift,
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
