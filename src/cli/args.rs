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

    /// Disable mouse capture in the interactive TUI.
    #[arg(long)]
    pub no_mouse: bool,

    /// Force ASCII rendering in the interactive TUI instead of Unicode Braille.
    #[arg(long)]
    pub ascii: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
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
    /// Manage biometric data retention, purge raw samples, wipe history, and import/export ledger bundles.
    Data {
        #[command(subcommand)]
        action: DataCommand,
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
    /// Provision and manage the `systemd --user` background service.
    Service {
        #[command(subcommand)]
        action: ServiceCommand,
    },
    /// Upgrade BPMute via the companion `bpmute-update` binary.
    Update,
    /// Generate shell completion scripts for bash, zsh, or fish.
    Completions {
        /// Target shell (`bash`, `zsh`, or `fish`).
        #[arg(value_enum)]
        shell: CompletionShell,
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
    /// Record or clear feedback (`--verdict`, `--onset`, `--avg-sleep-hr`, `--clear-*`, or `--clear`) on a closed session.
    Feedback {
        /// Session UUIDv7 or unambiguous ID prefix.
        id: String,
        /// Feedback verdict (`false_positive`, `missed_sleep`, or `anomalous_exclude`).
        #[arg(long, value_enum, conflicts_with_all = ["clear", "clear_verdict"])]
        verdict: Option<FeedbackVerdict>,
        /// Clear stored feedback verdict for this session.
        #[arg(long, conflicts_with_all = ["clear", "verdict"])]
        clear_verdict: bool,
        /// Wearable-app reported sleep-onset local time (`HH:MM`).
        #[arg(long, value_name = "HH:MM", conflicts_with_all = ["clear", "clear_onset"])]
        onset: Option<ClockTime>,
        /// Clear stored manual sleep-onset override for this session.
        #[arg(long, conflicts_with_all = ["clear", "onset"])]
        clear_onset: bool,
        /// Wearable-app reported overnight average sleeping HR (`30..=140` bpm).
        #[arg(long, value_name = "BPM", conflicts_with_all = ["clear", "clear_avg_sleep_hr"])]
        avg_sleep_hr: Option<f64>,
        /// Clear stored manual average sleeping HR override for this session.
        #[arg(long, conflicts_with_all = ["clear", "avg_sleep_hr"])]
        clear_avg_sleep_hr: bool,
        /// Clear all stored feedback overrides for this session.
        #[arg(long)]
        clear: bool,
    },
    /// Delete a closed Monitoring Session and immediately recompute calibration.
    Delete {
        /// Session UUIDv7 or unambiguous ID prefix.
        id: String,
        /// Skip interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Export a session trace to canonical CSV or JSONL format.
    Export {
        /// Session UUIDv7 or unambiguous ID prefix.
        id: String,
        /// Destination file path for trace.
        path: PathBuf,
        /// Trace format (`csv` or `jsonl`).
        #[arg(long, value_enum, default_value = "csv")]
        format: TraceFormatArg,
        /// Preserve absolute timestamps and hardware identifiers (do not anonymize).
        #[arg(long)]
        no_anonymize: bool,
    },
    /// Immediately finalize an active Monitoring Session while disconnected or in Signal Lost.
    End {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
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
    /// Rebaseline personal physiology to Cold Start (recalculating from this boundary onward).
    Rebaseline {
        /// Optional initial Day-1 sleeping HR seed (30..=140 bpm).
        #[arg(long, value_name = "BPM")]
        seed: Option<f64>,
        /// Also reset per-wearable hardware signal floors back to Cold Start.
        #[arg(long)]
        reset_floors: bool,
        /// Skip interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Undo the most recent Calibration Boundary, restoring continuous historical calibration bit-for-bit.
    UndoBoundary {
        /// Skip interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum DataCommand {
    /// Show storage statistics, retention policies, maturity, and active boundary.
    Stats {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Zero-overwrite and purge all raw biometric samples across closed sessions without modifying epochs or calibration.
    PurgeRaw {
        /// Skip interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Wipe biometric session history, epochs, and calibration (aborts active session; runs VACUUM with secure_delete).
    Wipe {
        /// Also wipe enrolled wearables and custom configuration.
        #[arg(long)]
        all: bool,
        /// Confirmation keyword; must be 'WIPE' when invoked non-interactively.
        #[arg(long, value_name = "CONFIRM")]
        confirm: Option<String>,
    },
    /// Export portable schema-versioned JSON ledger bundle.
    Export {
        /// Destination file path (.json).
        path: PathBuf,
        /// Include 15-second summary epochs.
        #[arg(long)]
        include_epochs: bool,
        /// Include enrolled wearables.
        #[arg(long)]
        include_wearables: bool,
        /// Include raw biometric samples.
        #[arg(long)]
        include_raw: bool,
    },
    /// Import portable schema-versioned JSON ledger bundle.
    Import {
        /// Source bundle file path (.json).
        path: PathBuf,
        /// Import mode (`merge` or `replace`).
        #[arg(long, value_enum, default_value = "merge")]
        mode: ImportModeArg,
        /// In merge mode, force overwriting newer local feedback with incoming bundle records.
        #[arg(long)]
        force: bool,
        /// Skip interactive confirmation (required for non-interactive `replace` mode).
        #[arg(long)]
        yes: bool,
    },
    /// Create a WAL-safe online backup of the SQLite ledger database (0600 permissions).
    Backup {
        /// Destination backup SQLite file path.
        path: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum TraceFormatArg {
    Csv,
    Jsonl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum ImportModeArg {
    Merge,
    Replace,
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

#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// Generate and enable `~/.config/systemd/user/bpmute.service`.
    Install {
        /// systemd target unit mode (`auto`, `graphical-session`, or `default`).
        #[arg(long, value_enum, default_value = "auto", value_name = "TARGET")]
        target: ServiceTargetArg,
    },
    /// Disable and remove `~/.config/systemd/user/bpmute.service`.
    Uninstall {
        /// Also remove `$XDG_CONFIG_HOME/bpmute/config.toml`.
        #[arg(long)]
        purge_config: bool,
    },
    /// Start `bpmute.service` via `systemctl --user start`.
    Start,
    /// Stop `bpmute.service` via `systemctl --user stop`.
    Stop,
    /// Restart `bpmute.service` via `systemctl --user restart`.
    Restart,
    /// Reload daemon configuration via `systemctl --user reload` (`SIGHUP`).
    Reload,
    /// Show `bpmute.service` status via `systemctl --user status`.
    Status,
    /// Show `bpmute.service` logs via `journalctl --user -u bpmute.service`.
    Logs {
        /// Stream new log entries continuously (`-f`).
        #[arg(short = 'f', long)]
        follow: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum ServiceTargetArg {
    Auto,
    #[value(alias = "graphical_session")]
    GraphicalSession,
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
}
