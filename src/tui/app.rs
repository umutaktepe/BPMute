use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Widget};

use super::event::{IpcEvent, TUI_TICK_MS, TuiCommand, TuiEvent};
use super::sparkline::HrSparklineData;
use super::theme::Theme;
use super::views::centered_rect;
use super::views::dashboard::render_dashboard;
use super::views::footer::render_footer;
use super::views::header::{render_global_banner, render_header};
use super::views::help::render_help;
use super::views::history::render_history;
use super::views::settings::render_settings;
use super::views::wearables::render_wearables;
use crate::config::PowerAction;
use crate::ipc::protocol::{
    DaemonAlert, DaemonStatus, DiscoveredWearable, EnrolledWearableStatus, MonitoringState,
    PairingPrompt, alert_ids,
};
use crate::sleep_engine::{CalibrationMaturity, Epoch, StreamStatus};

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
}

pub struct TuiApp {
    pub theme: Theme,
    pub no_mouse: bool,
    pub active_tab: Tab,
    pub modal: ModalState,
    pub should_quit: bool,
    pub daemon_connected: bool,
    pub last_viewport_width: u16,

    // Live daemon status
    pub daemon_status: Option<DaemonStatus>,
    pub monitoring_state: MonitoringState,
    pub armed: bool,
    pub dry_run: bool,
    pub idle_backend: Option<String>,
    pub playing_players_count: Option<usize>,

    // Biometrics & sparkline
    pub sparkline_data: HrSparklineData,
    pub latest_bpm: Option<f64>,
    pub contact_ok: bool,
    pub latest_p_sleep: Option<f64>,
    pub latest_epoch: Option<Epoch>,
    pub calibration_maturity: CalibrationMaturity,

    // Alerts & Grace Period
    pub alerts: Vec<DaemonAlert>,
    pub grace_state: Option<GraceState>,

    // Diagnostics
    pub is_diagnostics_open: bool,

    // Wearables
    pub enrolled_wearables: Vec<EnrolledWearableStatus>,
    pub selected_wearable_idx: usize,
    pub discovered_wearables: Vec<DiscoveredWearable>,

    // systemd --user service detection
    pub systemd_service_installed: bool,
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
            last_viewport_width: 80,
            daemon_status: None,
            monitoring_state: MonitoringState::Idle,
            armed: true,
            dry_run: false,
            idle_backend: None,
            playing_players_count: None,
            sparkline_data: HrSparklineData::new(),
            latest_bpm: None,
            contact_ok: true,
            latest_p_sleep: None,
            latest_epoch: None,
            calibration_maturity: CalibrationMaturity::ColdStart,
            alerts: Vec::new(),
            grace_state: None,
            is_diagnostics_open: false,
            enrolled_wearables: Vec::new(),
            selected_wearable_idx: 0,
            discovered_wearables: Vec::new(),
            systemd_service_installed,
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

    pub fn open_enrollment_modal(&mut self) -> TuiCommand {
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
            TuiEvent::Resize(width, _) => {
                self.last_viewport_width = width;
                None
            }
        }
    }

    pub fn tick(&mut self) -> Option<TuiCommand> {
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

    fn apply_daemon_status(&mut self, status: DaemonStatus) {
        self.daemon_connected = true;
        self.monitoring_state = status.monitoring_state;
        self.armed = status.armed;
        self.dry_run = status.dry_run;
        self.idle_backend = status.idle_backend.clone();
        self.playing_players_count = status.playing_players_count;
        self.alerts = status.alerts.clone();
        if status.monitoring_state != MonitoringState::Confirmed
            && self
                .grace_state
                .as_ref()
                .is_none_or(|g| g.session_id != "test-trigger-session")
        {
            self.grace_state = None;
        }
        self.daemon_status = Some(status);
    }

    fn handle_disconnected(&mut self) {
        self.daemon_connected = false;
        self.daemon_status = None;
        self.monitoring_state = MonitoringState::Idle;
        self.grace_state = None;
        self.latest_bpm = None;
        self.latest_p_sleep = None;
        self.latest_epoch = None;
        self.contact_ok = false;
        self.playing_players_count = None;
        self.alerts.clear();
        self.discovered_wearables.clear();
        if self.modal != ModalState::Help {
            self.modal = ModalState::None;
        }
    }

    pub fn handle_ipc(&mut self, ipc: IpcEvent) -> Option<TuiCommand> {
        match ipc {
            IpcEvent::Connected(status) => {
                self.apply_daemon_status(status);
            }
            IpcEvent::Disconnected => {
                self.handle_disconnected();
            }
            IpcEvent::Snapshot {
                status,
                hr_ring_buffer,
                alerts,
            } => {
                self.apply_daemon_status(status);
                self.alerts = alerts;
                self.sparkline_data.set_samples(hr_ring_buffer);
                self.latest_bpm = self.sparkline_data.latest_bpm();
            }
            IpcEvent::HrSample(sample) => {
                self.latest_bpm = Some(sample.bpm);
                self.contact_ok =
                    sample.contact_status != crate::sleep_engine::ContactStatus::NoContact;
                self.sparkline_data.push_sample(sample);
            }
            IpcEvent::GraceTick(tick) => {
                if tick.remaining_sec == 0 {
                    self.grace_state = None;
                } else {
                    let planned = if !tick.planned_steps.is_empty() {
                        tick.planned_steps
                    } else {
                        match tick.power_action {
                            PowerAction::Suspend => vec!["Pause Media".into(), "Suspend".into()],
                            PowerAction::Poweroff => {
                                vec!["Pause Media".into(), "Power Off".into()]
                            }
                            PowerAction::None => vec!["Pause Media".into()],
                        }
                    };
                    self.grace_state = Some(GraceState {
                        session_id: tick.session_id,
                        remaining_sec: tick.remaining_sec,
                        total_sec: tick.total_sec,
                        planned_actions: planned,
                        dry_run: tick.dry_run,
                    });
                }
            }
            IpcEvent::EpochUpdate(epoch_ev) => {
                self.latest_p_sleep = Some(epoch_ev.epoch.p_sleep);
                if epoch_ev.epoch.baseline_mu_hr.is_finite() && epoch_ev.epoch.baseline_mu_hr > 0.0
                {
                    self.sparkline_data.quiet_wake_baseline = Some(epoch_ev.epoch.baseline_mu_hr);
                }
                self.latest_epoch = Some(epoch_ev.epoch);
            }
            IpcEvent::StateChanged(status) => {
                self.apply_daemon_status(status);
            }
            IpcEvent::AlertChanged(alerts) => {
                self.alerts = alerts;
            }
            IpcEvent::CalibrationUpdated(cal) => {
                self.calibration_maturity = cal.maturity;
                if let Some(mu_wake) = cal.mu_wake {
                    self.sparkline_data.quiet_wake_baseline = Some(mu_wake);
                }
                self.sparkline_data.sleeping_hr = cal.mu_sleep;
            }
            IpcEvent::WearablesUpdated(wearables) => {
                self.enrolled_wearables = wearables;
                if self.selected_wearable_idx >= self.enrolled_wearables.len() {
                    self.selected_wearable_idx = self.enrolled_wearables.len().saturating_sub(1);
                }
            }
            IpcEvent::EnrollmentDiscovered(discovered) => {
                self.discovered_wearables = discovered;
                if let ModalState::EnrollmentModal {
                    ref mut selected_discovered_idx,
                    ..
                } = self.modal
                    && *selected_discovered_idx >= self.discovered_wearables.len()
                {
                    *selected_discovered_idx = self.discovered_wearables.len().saturating_sub(1);
                }
            }
            IpcEvent::EnrollmentUpdate(update) => {
                self.discovered_wearables = update.wearables;
                if let ModalState::EnrollmentModal {
                    ref mut pairing_prompt,
                    ref mut scanning_remaining_ms,
                    ref mut selected_discovered_idx,
                    ..
                } = self.modal
                {
                    if *selected_discovered_idx >= self.discovered_wearables.len() {
                        *selected_discovered_idx =
                            self.discovered_wearables.len().saturating_sub(1);
                    }
                    if let Some(rem) = update.remaining_sec {
                        *scanning_remaining_ms = rem.saturating_mul(1000);
                    }
                    if let Some(prompt) = update.pairing_prompt {
                        let prompt_modal = match prompt {
                            PairingPrompt::Confirmation { passkey, wearable } => {
                                PairingPromptModalState::Confirmation { passkey, wearable }
                            }
                            PairingPrompt::Passkey { wearable } => {
                                PairingPromptModalState::Passkey {
                                    wearable,
                                    input: String::new(),
                                }
                            }
                            PairingPrompt::PinCode { wearable } => {
                                PairingPromptModalState::PinCode {
                                    wearable,
                                    input: String::new(),
                                }
                            }
                        };
                        *pairing_prompt = Some(prompt_modal);
                    }
                }
            }
            IpcEvent::OpCompleted {
                op,
                success,
                message,
            } => {
                if success && (op == "monitor.cancel_grace" || op == "monitor.snooze_grace") {
                    self.grace_state = None;
                }
                if op == "wearable.enroll_select" {
                    if success {
                        if matches!(self.modal, ModalState::EnrollmentModal { .. }) {
                            self.modal = ModalState::None;
                        }
                    } else if let ModalState::EnrollmentModal {
                        ref mut enrolling,
                        ref mut pairing_prompt,
                        ref mut error_message,
                        ..
                    } = self.modal
                    {
                        *enrolling = false;
                        *pairing_prompt = None;
                        *error_message =
                            Some(message.unwrap_or_else(|| "Enrollment failed".to_string()));
                    }
                } else if !success
                    && op == "wearable.enroll_start"
                    && let ModalState::EnrollmentModal {
                        ref mut scanning_remaining_ms,
                        ref mut error_message,
                        ..
                    } = self.modal
                {
                    *scanning_remaining_ms = 0;
                    *error_message = message;
                }
            }
        }
        None
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width < 10 || area.height < 5 {
            return;
        }

        // Resize notice below 60x18
        if area.width < 60 || area.height < 18 {
            render_resize_notice(area, buf, self.theme);
            return;
        }

        let has_banner = self.grace_state.is_some() || !self.daemon_connected;
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

        render_header(self, header_area, buf);
        if has_banner {
            render_global_banner(self, banner_area, buf);
        }

        match self.active_tab {
            Tab::Dashboard => render_dashboard(self, content_area, buf),
            Tab::History => render_history(self, content_area, buf),
            Tab::Wearables => render_wearables(self, content_area, buf),
            Tab::Settings => render_settings(self, content_area, buf),
        }

        render_footer(self, footer_area, buf);

        if self.modal == ModalState::Help {
            render_help(self, area, buf);
        }
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
