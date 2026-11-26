use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Widget};

use super::event::{TUI_TICK_MS, TuiCommand, TuiEvent};
use super::hit_map::HitMap;
use super::sparkline::HrSparklineData;
use super::theme::Theme;
use super::views::centered_rect;
use super::views::dashboard::{render_dashboard, render_pause_popover};
use super::views::footer::render_footer;
use super::views::header::{render_global_banner, render_header};
use super::views::help::render_help;
use super::views::history::render_history;
use super::views::settings::render_settings;
use super::views::wearables::render_wearables;
use crate::calibration::CalibrationBoundaryRecord;
use crate::config::{ClockTime, Config};
use crate::ipc::protocol::{
    DaemonAlert, DaemonStatus, DiscoveredWearable, EnrolledWearableStatus, MonitoringState,
    alert_ids,
};
use crate::sleep_engine::{CalibrationMaturity, Epoch, StreamStatus};
use crate::storage::{SessionFullDetail, SessionListEntry, StorageStats};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Dashboard = 0,
    History = 1,
    Wearables = 2,
    Settings = 3,
}

impl Tab {
    pub fn next(self) -> Self {
        match self {
            Self::Dashboard => Self::History,
            Self::History => Self::Wearables,
            Self::Wearables => Self::Settings,
            Self::Settings => Self::Dashboard,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Dashboard => Self::Settings,
            Self::History => Self::Dashboard,
            Self::Wearables => Self::History,
            Self::Settings => Self::Wearables,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GraceState {
    pub session_id: String,
    pub remaining_sec: u32,
    pub total_sec: u32,
    pub planned_actions: Vec<String>,
    pub dry_run: bool,
    pub phase: crate::actions::GracePhase,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpFeedback {
    pub is_error: bool,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingPromptModalState {
    Confirmation { passkey: u32, wearable: String },
    Passkey { wearable: String, input: String },
    PinCode { wearable: String, input: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnenrollButton {
    Confirm,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackModalField {
    Verdict,
    Onset,
    SleepHr,
    Submit,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportModalField {
    Format,
    Path,
    Anonymize,
    Submit,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoftRebaselineField {
    SeedSleepHr,
    ResetFloors,
    Submit,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Day1SeedField {
    SeedSleepHr,
    Submit,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorePreference {
    Sensitivity,
    PauseMedia,
    LockScreen,
    PowerAction,
    GracePeriod,
    DryRun,
    ActiveHours,
}

impl CorePreference {
    pub const ALL: [Self; 7] = [
        Self::Sensitivity,
        Self::PauseMedia,
        Self::LockScreen,
        Self::PowerAction,
        Self::GracePeriod,
        Self::DryRun,
        Self::ActiveHours,
    ];

    pub const COUNT: usize = Self::ALL.len();

    pub fn label(self) -> &'static str {
        match self {
            Self::Sensitivity => "Sensitivity",
            Self::PauseMedia => "Pause Media",
            Self::LockScreen => "Lock Screen",
            Self::PowerAction => "Power Action",
            Self::GracePeriod => "Grace Period",
            Self::DryRun => "Dry Run",
            Self::ActiveHours => "Active Hours",
        }
    }

    pub fn from_index(idx: usize) -> Option<Self> {
        Self::ALL.get(idx).copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveHoursField {
    Enabled,
    Start,
    End,
    Save,
    Cancel,
}

impl ActiveHoursField {
    pub(crate) fn next(self) -> Self {
        match self {
            Self::Enabled => Self::Start,
            Self::Start => Self::End,
            Self::End => Self::Save,
            Self::Save => Self::Cancel,
            Self::Cancel => Self::Enabled,
        }
    }

    pub(crate) fn prev(self) -> Self {
        match self {
            Self::Enabled => Self::Cancel,
            Self::Start => Self::Enabled,
            Self::End => Self::Start,
            Self::Save => Self::End,
            Self::Cancel => Self::Save,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModalState {
    None,
    Help,
    PausePopover {
        selected_idx: usize,
    },
    EnrollmentModal {
        scanning_remaining_ms: u32,
        selected_discovered_idx: usize,
        alias_input: String,
        alias_focused: bool,
        enrolling: bool,
        error_message: Option<String>,
        pairing_prompt: Option<PairingPromptModalState>,
    },
    RenameModal {
        wearable_id: String,
        current_alias: String,
        input: String,
    },
    UnenrollModal {
        wearable: EnrolledWearableStatus,
        forget_bluez: bool,
        delete_sessions: bool,
        selected_button: UnenrollButton,
    },
    FeedbackModal {
        session_id: String,
        session_start_ts_ms: i64,
        session_end_ts_ms: i64,
        epochs_pruned: bool,
        verdict_idx: usize,
        onset_hhmm_input: String,
        avg_sleep_hr_input: String,
        active_field: FeedbackModalField,
        error_message: Option<String>,
    },
    ExportTraceModal {
        session_id: String,
        format_idx: usize,
        destination_path: String,
        anonymize: bool,
        active_field: ExportModalField,
        error_message: Option<String>,
    },
    DeleteSessionModal {
        session_id: String,
        confirmed: bool,
    },
    SoftRebaselineModal {
        seed_sleep_hr_input: String,
        reset_floors: bool,
        active_field: SoftRebaselineField,
        error_message: Option<String>,
    },
    Day1SeedModal {
        seed_sleep_hr_input: String,
        active_field: Day1SeedField,
        error_message: Option<String>,
    },
    PurgeRawModal {
        confirmed: bool,
    },
    HardWipeModal {
        typed_input: String,
        error_message: Option<String>,
    },
    ActiveHoursModal {
        enabled: bool,
        start_input: String,
        end_input: String,
        last_valid_start: ClockTime,
        last_valid_end: ClockTime,
        active_field: ActiveHoursField,
        error_message: Option<String>,
    },
}

/// Interactive column cursor state for the History Epoch chart.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HistoryChartInspectState {
    pub(crate) col: Option<u16>,
    pub(crate) chart_width: u16,
}

impl HistoryChartInspectState {
    pub(crate) fn is_active(self) -> bool {
        self.col.is_some() && self.chart_width > 0
    }

    pub(crate) fn clear(&mut self) {
        self.col = None;
    }

    pub(crate) fn set_col(&mut self, col: u16) {
        if self.chart_width > 0 {
            self.col = Some(col.min(self.chart_width - 1));
        }
    }

    pub(crate) fn step(&mut self, delta: isize) {
        if let Some(cur) = self.col
            && self.chart_width > 0
        {
            let next = (cur as isize + delta).clamp(0, (self.chart_width - 1) as isize) as u16;
            self.col = Some(next);
        }
    }

    pub(crate) fn sync_rendered_width(&mut self, chart_width: u16) {
        self.chart_width = chart_width;
        if chart_width == 0 {
            self.col = None;
        } else if let Some(col) = self.col {
            self.col = Some(col.min(chart_width - 1));
        }
    }
}

pub struct TuiApp {
    pub theme: Theme,
    pub no_mouse: bool,
    pub active_tab: Tab,
    pub modal: ModalState,
    pub should_quit: bool,
    pub daemon_connected: bool,

    // Live daemon status
    pub daemon_status: Option<DaemonStatus>,
    pub monitoring_state: MonitoringState,
    pub armed: bool,
    pub dry_run: bool,
    pub idle_backend: Option<String>,
    pub playing_players_count: Option<usize>,
    pub session_close_deadline_ms: Option<i64>,
    pub daemon_clock_offset_ms: Option<i64>,
    pub daemon_now_ms: Option<i64>,

    // Biometrics & sparkline
    pub sparkline_data: HrSparklineData,
    pub latest_bpm: Option<f64>,
    pub contact_ok: bool,
    pub latest_p_sleep: Option<f64>,
    pub latest_epoch: Option<Epoch>,
    pub calibration_maturity: CalibrationMaturity,
    pub day1_seed_sleep_hr: Option<f64>,

    // Alerts & Grace Period
    pub alerts: Vec<DaemonAlert>,
    pub grace_state: Option<GraceState>,
    pub op_feedback: Option<OpFeedback>,
    pub session_deletion_in_progress: bool,

    // Diagnostics
    pub is_diagnostics_open: bool,

    // Wearables
    pub enrolled_wearables: Vec<EnrolledWearableStatus>,
    pub selected_wearable_idx: usize,
    pub(crate) wearables_scroll_offset: usize,
    pub discovered_wearables: Vec<DiscoveredWearable>,
    pub(crate) enrollment_scroll_offset: usize,
    pub(crate) enrollment_update_seen: bool,

    // History
    pub history_sessions: Vec<SessionListEntry>,
    pub history_boundaries: Vec<CalibrationBoundaryRecord>,
    pub selected_session_idx: usize,
    pub selected_session_detail: Option<SessionFullDetail>,
    pub(crate) history_inspect: HistoryChartInspectState,

    // Settings
    pub active_config: Option<Config>,
    pub selected_setting_idx: usize,
    pub config_error: Option<String>,
    pub storage_stats: Option<StorageStats>,

    // Offline data browsing paths
    pub offline_paths: Option<crate::paths::ResolvedPaths>,

    // systemd --user service detection
    pub systemd_service_installed: bool,

    // Render-emitted mouse hit-testing map (ADR-0010)
    pub(crate) hit_map: HitMap,

    // Help modal vertical scroll state
    pub help_scroll_offset: u16,
    pub(crate) help_max_scroll: u16,
}

impl TuiApp {
    pub fn new(no_mouse: bool, ascii: bool, no_color: bool) -> Self {
        Self::new_with_theme(no_mouse, Theme::new(no_color, ascii), false)
    }

    pub fn new_with_theme(no_mouse: bool, theme: Theme, systemd_service_installed: bool) -> Self {
        Self {
            theme,
            no_mouse,
            active_tab: Tab::Dashboard,
            modal: ModalState::None,
            should_quit: false,
            daemon_connected: false,
            daemon_status: None,
            monitoring_state: MonitoringState::Idle,
            armed: true,
            dry_run: false,
            idle_backend: None,
            playing_players_count: None,
            session_close_deadline_ms: None,
            daemon_clock_offset_ms: None,
            daemon_now_ms: None,
            sparkline_data: HrSparklineData::new(),
            latest_bpm: None,
            contact_ok: true,
            latest_p_sleep: None,
            latest_epoch: None,
            calibration_maturity: CalibrationMaturity::ColdStart,
            day1_seed_sleep_hr: None,
            alerts: Vec::new(),
            grace_state: None,
            op_feedback: None,
            session_deletion_in_progress: false,
            is_diagnostics_open: false,
            enrolled_wearables: Vec::new(),
            selected_wearable_idx: 0,
            wearables_scroll_offset: 0,
            discovered_wearables: Vec::new(),
            enrollment_scroll_offset: 0,
            enrollment_update_seen: false,
            history_sessions: Vec::new(),
            history_boundaries: Vec::new(),
            selected_session_idx: 0,
            selected_session_detail: None,
            history_inspect: HistoryChartInspectState::default(),
            active_config: None,
            selected_setting_idx: 0,
            config_error: None,
            storage_stats: None,
            offline_paths: None,
            systemd_service_installed,
            hit_map: HitMap::default(),
            help_scroll_offset: 0,
            help_max_scroll: 0,
        }
    }

    pub fn set_offline_paths(&mut self, paths: crate::paths::ResolvedPaths) {
        self.offline_paths = Some(paths);
    }

    pub fn load_offline_data(&mut self, env: &dyn crate::paths::EnvSource) {
        let Some(ref paths) = self.offline_paths else {
            return;
        };
        // Load config
        match crate::config::LoadedConfig::load(
            &paths.config_file,
            env,
            &crate::config::CliOverrides::default(),
        ) {
            Ok(loaded) => {
                self.active_config = Some(loaded.config);
                self.config_error = None;
            }
            Err(crate::config::ConfigError::Io(err))
                if err.kind() == std::io::ErrorKind::NotFound =>
            {
                self.active_config = Some(crate::config::Config::default());
                self.config_error = None;
            }
            Err(e) => {
                self.config_error = Some(e.to_string());
                if self.active_config.is_none() {
                    self.active_config = Some(crate::config::Config::default());
                }
            }
        }

        // Load storage
        let db_path = crate::storage::db_path_in_data_dir(&paths.data_dir);
        if let Ok(Some(store)) = crate::storage::LedgerStore::open_read_only(&db_path) {
            if let Ok(page) = store.list_sessions(50, 0) {
                self.history_sessions = page.sessions;
                if self.selected_session_idx >= self.history_sessions.len() {
                    self.selected_session_idx = 0;
                }
                if let Some(s) = self.history_sessions.get(self.selected_session_idx)
                    && let Ok(detail) = store.get_session(&s.id)
                {
                    self.selected_session_detail = Some(detail);
                }
            }
            if let Ok(boundaries) = store.list_calibration_boundaries() {
                self.history_boundaries = boundaries;
            }
            if let Ok(stats) = store.get_storage_stats() {
                self.calibration_maturity = stats.calibration_maturity;
                self.storage_stats = Some(stats);
            }
        }
    }

    pub fn load_session_detail_offline(&mut self, session_id: &str) {
        let Some(ref paths) = self.offline_paths else {
            return;
        };
        let db_path = crate::storage::db_path_in_data_dir(&paths.data_dir);
        if let Ok(Some(store)) = crate::storage::LedgerStore::open_read_only(&db_path)
            && let Ok(detail) = store.get_session(session_id)
        {
            self.selected_session_detail = Some(detail);
        }
    }

    pub fn is_input_focused(&self) -> bool {
        match self.modal {
            ModalState::RenameModal { .. } => true,
            ModalState::EnrollmentModal {
                enrolling: false,
                alias_focused: true,
                pairing_prompt: None,
                ..
            } => true,
            ModalState::EnrollmentModal {
                ref pairing_prompt, ..
            } => matches!(
                pairing_prompt,
                Some(PairingPromptModalState::Passkey { .. })
                    | Some(PairingPromptModalState::PinCode { .. })
            ),
            ModalState::FeedbackModal {
                active_field: FeedbackModalField::Onset | FeedbackModalField::SleepHr,
                ..
            } => true,
            ModalState::ExportTraceModal {
                active_field: ExportModalField::Path,
                ..
            } => true,
            ModalState::SoftRebaselineModal {
                active_field: SoftRebaselineField::SeedSleepHr,
                ..
            } => true,
            ModalState::Day1SeedModal {
                active_field: Day1SeedField::SeedSleepHr,
                ..
            } => true,
            ModalState::ActiveHoursModal {
                active_field: ActiveHoursField::Start | ActiveHoursField::End,
                ..
            } => true,
            ModalState::HardWipeModal { .. } => true,
            _ => false,
        }
    }

    pub fn active_wearable(&self) -> Option<&EnrolledWearableStatus> {
        self.enrolled_wearables
            .iter()
            .find(|w| w.status == StreamStatus::Streaming || w.status == StreamStatus::Acquiring)
            .or_else(|| self.enrolled_wearables.first())
    }

    pub fn active_alert(&self) -> Option<&DaemonAlert> {
        self.alerts.first()
    }

    pub fn has_persistent_shift_alert(&self) -> bool {
        self.alerts
            .iter()
            .any(|a| a.id == alert_ids::PERSISTENT_SHIFT)
    }

    pub(crate) fn can_act_on_selected_wearable(&self) -> bool {
        self.daemon_connected && self.selected_wearable_idx < self.enrolled_wearables.len()
    }

    pub(crate) fn can_act_on_selected_session(&self) -> bool {
        self.daemon_connected
            && self
                .history_sessions
                .get(self.selected_session_idx)
                .is_some_and(|s| s.status != crate::storage::SessionStatus::Active)
    }

    pub(crate) fn is_history_chart_inspecting(&self) -> bool {
        self.active_tab == Tab::History
            && self.modal == ModalState::None
            && self.history_inspect.is_active()
    }

    pub(crate) fn has_active_calibration_boundary(&self) -> bool {
        self.storage_stats
            .as_ref()
            .and_then(|s| s.active_calibration_boundary.as_ref())
            .is_some()
    }

    pub(crate) fn open_help_modal(&mut self) {
        self.modal = ModalState::Help;
        self.help_scroll_offset = 0;
        self.help_max_scroll = 0;
    }

    pub(crate) fn reset_enrollment_scan_tracking(&mut self) {
        self.enrollment_update_seen = false;
    }

    pub(crate) fn close_modal(&mut self) {
        self.modal = ModalState::None;
        self.help_scroll_offset = 0;
        self.help_max_scroll = 0;
        self.enrollment_scroll_offset = 0;
        self.reset_enrollment_scan_tracking();
    }

    pub fn open_enrollment_modal(&mut self) -> TuiCommand {
        self.enrollment_scroll_offset = 0;
        self.reset_enrollment_scan_tracking();
        self.modal = ModalState::EnrollmentModal {
            scanning_remaining_ms: 30_000,
            selected_discovered_idx: 0,
            alias_input: String::new(),
            alias_focused: false,
            enrolling: false,
            error_message: None,
            pairing_prompt: None,
        };
        TuiCommand::EnrollStart(Some(30))
    }

    pub fn handle_event(&mut self, event: TuiEvent) -> Option<TuiCommand> {
        match event {
            TuiEvent::Key(key) => self.handle_key(key),
            TuiEvent::Mouse(mouse) => {
                if !self.no_mouse {
                    self.handle_mouse(mouse)
                } else {
                    None
                }
            }
            TuiEvent::Tick => self.tick(),
            TuiEvent::Ipc(ipc) => self.handle_ipc(*ipc),
            TuiEvent::Resize(_, _) => None,
        }
    }

    pub fn tick(&mut self) -> Option<TuiCommand> {
        if self.daemon_connected {
            self.sparkline_data.advance_tick(i64::from(TUI_TICK_MS));
            if let Some(ref mut now_ms) = self.daemon_now_ms {
                *now_ms = now_ms.saturating_add(i64::from(TUI_TICK_MS));
            }
        }
        if let ModalState::EnrollmentModal {
            ref mut scanning_remaining_ms,
            enrolling: false,
            ..
        } = self.modal
        {
            *scanning_remaining_ms = scanning_remaining_ms.saturating_sub(TUI_TICK_MS);
        }
        None
    }

    /// Resolves the current estimated timestamp (in milliseconds) of the daemon clock,
    /// anchored to `server_now_ms` and smoothly advanced on UI ticks.
    pub fn current_daemon_now_ms(&self) -> i64 {
        if let Some(offset) = self.daemon_clock_offset_ms {
            let local_now = chrono::Utc::now().timestamp_millis();
            let wall_now = local_now.saturating_add(offset);
            if let Some(tick_now) = self.daemon_now_ms {
                tick_now.max(wall_now)
            } else {
                wall_now
            }
        } else if let Some(tick_now) = self.daemon_now_ms {
            tick_now
        } else {
            chrono::Utc::now().timestamp_millis()
        }
    }

    /// Returns remaining close countdown seconds while `session_close_deadline_ms` is active.
    pub fn session_close_remaining_sec(&self) -> Option<u64> {
        let deadline_ms = self.session_close_deadline_ms?;
        let now_ms = self.current_daemon_now_ms();
        let remaining_ms = deadline_ms.saturating_sub(now_ms).max(0) as u64;
        Some(remaining_ms.saturating_sub(1).div_ceil(1000))
    }

    /// Invokes immediate manual closure of an active session during a gap/Signal Lost window.
    pub fn end_active_session(&mut self) -> Option<TuiCommand> {
        if !self.daemon_connected || self.session_close_deadline_ms.is_none() {
            return None;
        }

        Some(TuiCommand::SessionEnd)
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer) {
        self.hit_map.clear();

        if area.width < 10 || area.height < 5 {
            return;
        }

        // Resize notice below 60x18
        if area.width < 60 || area.height < 18 {
            render_resize_notice(area, buf, self.theme);
            return;
        }

        let has_banner =
            self.grace_state.is_some() || !self.daemon_connected || self.op_feedback.is_some();
        let banner_height = u16::from(has_banner);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),             // Header tabs
                Constraint::Length(banner_height), // Global Grace / Offline banner
                Constraint::Min(12),               // Active Tab content
                Constraint::Length(1),             // Keybinding footer
            ])
            .split(area);

        let header_area = chunks[0];
        let banner_area = chunks[1];
        let content_area = chunks[2];
        let footer_area = chunks[3];

        let mut hit_map = HitMap::default();
        hit_map.set_modal_active(self.modal != ModalState::None);

        render_header(self, header_area, buf, &mut hit_map);
        if has_banner {
            render_global_banner(self, banner_area, buf, &mut hit_map);
        }

        match self.active_tab {
            Tab::Dashboard => render_dashboard(self, content_area, buf, &mut hit_map),
            Tab::History => {
                let rendered = render_history(self, content_area, buf, &mut hit_map);
                self.history_inspect
                    .sync_rendered_width(rendered.chart_plot_width);
            }
            Tab::Wearables => {
                let scroll = render_wearables(self, content_area, buf, &mut hit_map);
                self.wearables_scroll_offset = scroll.table;
                self.enrollment_scroll_offset = scroll.enrollment;
            }
            Tab::Settings => render_settings(self, content_area, buf, &mut hit_map),
        }

        if self.modal == ModalState::Help {
            let max_scroll = render_help(self, area, buf, &mut hit_map);
            self.help_max_scroll = max_scroll;
            self.help_scroll_offset = self.help_scroll_offset.min(max_scroll);
        } else if let ModalState::PausePopover { selected_idx } = self.modal {
            render_pause_popover(self, selected_idx, area, content_area, buf, &mut hit_map);
        }

        render_footer(self, footer_area, buf, &mut hit_map);

        self.hit_map = hit_map;
    }
}

fn render_resize_notice(area: Rect, buf: &mut Buffer, theme: Theme) {
    let box_area = centered_rect(area, 46, 6);
    Clear.render(box_area, buf);

    let block = theme
        .card_block(" Terminal too small ")
        .border_style(theme.warning());

    let inner = block.inner(box_area);
    block.render(box_area, buf);

    let sep = if theme.ascii { "x" } else { "×" };
    let lines = vec![
        Line::from(vec![
            Span::styled(" Current: ", theme.muted()),
            Span::styled(
                format!("{}{sep}{}", area.width, area.height),
                theme.danger(),
            ),
        ]),
        Line::from(vec![
            Span::styled(" Minimum: ", theme.muted()),
            Span::styled(format!("60{sep}18 (recommended: 80{sep}24)"), theme.base()),
        ]),
    ];

    Paragraph::new(lines).render(inner, buf);
}

/// Formats remaining seconds as compact countdown string in `Xm Ys` format (e.g. `2m 0s`, `1m 59s`, `0m 29s`).
pub fn format_close_countdown(sec: u64) -> String {
    format!("{}m {}s", sec / 60, sec % 60)
}
