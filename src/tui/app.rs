use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};

use super::event::{IpcEvent, TuiCommand, TuiEvent};
use super::sparkline::HrSparklineData;
use super::theme::Theme;
use super::views::dashboard::render_dashboard;
use super::views::footer::render_footer;
use super::views::header::{render_global_banner, render_header};
use super::views::help::render_help;
use super::views::history::render_history;
use super::views::settings::render_settings;
use super::views::wearables::render_wearables;
use crate::ipc::protocol::{
    DaemonAlert, DaemonStatus, DiscoveredWearable, EnrollAuthResponseParams,
    EnrolledWearableStatus, MonitoringState, PairingPrompt, PauseMode, PauseParams,
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
    Confirmation { passkey: u32, device: String },
    Passkey { device: String, input: String },
    PinCode { device: String, input: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModalState {
    None,
    Help,
    PausePopover {
        selected_idx: usize,
    },
    EnrollmentModal {
        scanning_timer_sec: u32,
        selected_device_idx: usize,
        alias_input: String,
        alias_focused: bool,
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
        selected_button: usize, // 0 = Confirm, 1 = Cancel
    },
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
    pub playing_players_count: usize,

    // biometrics & sparkline
    pub sparkline_data: HrSparklineData,
    pub latest_bpm: Option<f64>,
    pub contact_ok: bool,
    pub latest_p_sleep: Option<f64>,
    pub latest_epoch: Option<Epoch>,
    pub calibration_maturity: CalibrationMaturity,

    // alerts & grace
    pub alerts: Vec<DaemonAlert>,
    pub grace_state: Option<GraceState>,

    // diagnostics
    pub is_diagnostics_open: bool,

    // wearables
    pub enrolled_wearables: Vec<EnrolledWearableStatus>,
    pub selected_wearable_idx: usize,
    pub discovered_wearables: Vec<DiscoveredWearable>,

    // systemd service setup check
    pub systemd_service_installed: bool,
}

impl TuiApp {
    pub fn new(no_mouse: bool, ascii: bool, no_color: bool) -> Self {
        Self {
            theme: Theme::new(no_color, ascii),
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
            playing_players_count: 0,
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
            systemd_service_installed: false,
        }
    }

    pub fn is_input_focused(&self) -> bool {
        match self.modal {
            ModalState::RenameModal { .. } => true,
            ModalState::EnrollmentModal {
                alias_focused: true,
                ref pairing_prompt,
                ..
            } => !matches!(
                pairing_prompt,
                Some(PairingPromptModalState::Confirmation { .. })
            ),
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
        self.alerts.iter().any(|a| a.id == "persistent_shift")
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
        // Decrement scanning timer if enrollment modal active
        if let ModalState::EnrollmentModal {
            ref mut scanning_timer_sec,
            ..
        } = self.modal
        {
            *scanning_timer_sec = scanning_timer_sec.saturating_sub(1);
        }
        None
    }

    pub fn handle_ipc(&mut self, ipc: IpcEvent) -> Option<TuiCommand> {
        match ipc {
            IpcEvent::Connected(status) => {
                self.daemon_connected = true;
                self.monitoring_state = status.monitoring_state;
                self.armed = status.armed;
                self.dry_run = status.dry_run;
                self.idle_backend = status.idle_backend.clone();
                self.alerts = status.alerts.clone();
                self.daemon_status = Some(status);
            }
            IpcEvent::Disconnected => {
                self.daemon_connected = false;
                self.monitoring_state = MonitoringState::Idle;
            }
            IpcEvent::Snapshot {
                status,
                hr_ring_buffer,
                alerts,
            } => {
                self.daemon_connected = true;
                self.monitoring_state = status.monitoring_state;
                self.armed = status.armed;
                self.dry_run = status.dry_run;
                self.idle_backend = status.idle_backend.clone();
                self.alerts = alerts;
                self.daemon_status = Some(status);
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
                let mut planned = Vec::new();
                match tick.power_action {
                    crate::config::PowerAction::Suspend => {
                        planned.push("Pause Media".into());
                        planned.push("Suspend".into());
                    }
                    crate::config::PowerAction::Poweroff => {
                        planned.push("Pause Media".into());
                        planned.push("Power Off".into());
                    }
                    _ => {
                        planned.push("Pause Media".into());
                    }
                }
                self.grace_state = Some(GraceState {
                    session_id: tick.session_id,
                    remaining_sec: tick.remaining_sec,
                    total_sec: tick.total_sec,
                    planned_actions: planned,
                    dry_run: tick.dry_run,
                });
            }
            IpcEvent::EpochUpdate(epoch_ev) => {
                self.latest_epoch = Some(epoch_ev.epoch.clone());
                self.latest_p_sleep = Some(epoch_ev.epoch.p_sleep);
            }
            IpcEvent::StateChanged(status) => {
                self.monitoring_state = status.monitoring_state;
                self.armed = status.armed;
                self.dry_run = status.dry_run;
                self.alerts = status.alerts.clone();
                self.idle_backend = status.idle_backend.clone();
                if status.monitoring_state != MonitoringState::Confirmed {
                    self.grace_state = None;
                }
                self.daemon_status = Some(status);
            }
            IpcEvent::AlertChanged(alerts) => {
                self.alerts = alerts;
            }
            IpcEvent::CalibrationUpdated(cal) => {
                self.calibration_maturity = cal.maturity;
                self.sparkline_data.quiet_wake_baseline = cal.mu_wake;
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
            }
            IpcEvent::EnrollmentUpdate(update) => {
                self.discovered_wearables = update.devices;
                if let Some(prompt) = update.pairing_prompt {
                    let prompt_modal = match prompt {
                        PairingPrompt::Confirmation { passkey, device } => {
                            PairingPromptModalState::Confirmation { passkey, device }
                        }
                        PairingPrompt::Passkey { device } => PairingPromptModalState::Passkey {
                            device,
                            input: String::new(),
                        },
                        PairingPrompt::PinCode { device } => PairingPromptModalState::PinCode {
                            device,
                            input: String::new(),
                        },
                    };
                    if let ModalState::EnrollmentModal {
                        ref mut pairing_prompt,
                        ..
                    } = self.modal
                    {
                        *pairing_prompt = Some(prompt_modal);
                    }
                }
            }
            IpcEvent::PairingPrompt(prompt) => {
                let prompt_modal = match prompt {
                    PairingPrompt::Confirmation { passkey, device } => {
                        PairingPromptModalState::Confirmation { passkey, device }
                    }
                    PairingPrompt::Passkey { device } => PairingPromptModalState::Passkey {
                        device,
                        input: String::new(),
                    },
                    PairingPrompt::PinCode { device } => PairingPromptModalState::PinCode {
                        device,
                        input: String::new(),
                    },
                };
                if let ModalState::EnrollmentModal {
                    ref mut pairing_prompt,
                    ..
                } = self.modal
                {
                    *pairing_prompt = Some(prompt_modal);
                }
            }
            IpcEvent::OpCompleted { op, success, .. } => {
                if success && (op == "monitor.cancel_grace" || op == "monitor.snooze_grace") {
                    self.grace_state = None;
                }
            }
        }
        None
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<TuiCommand> {
        // 1. If text input is focused, suspend all global hotkeys and handle typing!
        if self.is_input_focused() {
            return self.handle_text_input_key(key);
        }

        // 2. Handle active modals
        match self.modal {
            ModalState::Help => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q')
                ) {
                    self.modal = ModalState::None;
                }
                return None;
            }
            ModalState::PausePopover {
                ref mut selected_idx,
            } => match key.code {
                KeyCode::Esc => {
                    self.modal = ModalState::None;
                    return None;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected_idx = selected_idx.saturating_sub(1);
                    return None;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if *selected_idx < 4 {
                        *selected_idx += 1;
                    }
                    return None;
                }
                KeyCode::Char('1') => {
                    *selected_idx = 0;
                    return self.confirm_pause_selection(0);
                }
                KeyCode::Char('2') => {
                    *selected_idx = 1;
                    return self.confirm_pause_selection(1);
                }
                KeyCode::Char('3') => {
                    *selected_idx = 2;
                    return self.confirm_pause_selection(2);
                }
                KeyCode::Char('4') => {
                    *selected_idx = 3;
                    return self.confirm_pause_selection(3);
                }
                KeyCode::Char('5') => {
                    *selected_idx = 4;
                    return self.confirm_pause_selection(4);
                }
                KeyCode::Enter => {
                    let sel = *selected_idx;
                    return self.confirm_pause_selection(sel);
                }
                _ => return None,
            },
            ModalState::EnrollmentModal {
                ref mut selected_device_idx,
                ref mut alias_focused,
                ref pairing_prompt,
                ..
            } => {
                if let Some(prompt) = pairing_prompt {
                    if let PairingPromptModalState::Confirmation { .. } = prompt {
                        if key.code == KeyCode::Char('y') || key.code == KeyCode::Char('Y') {
                            self.modal = ModalState::None;
                            return Some(TuiCommand::EnrollAuthReply(EnrollAuthResponseParams {
                                confirmed: Some(true),
                                passkey: None,
                                pincode: None,
                            }));
                        } else if key.code == KeyCode::Char('n')
                            || key.code == KeyCode::Char('N')
                            || key.code == KeyCode::Esc
                        {
                            self.modal = ModalState::None;
                            return Some(TuiCommand::EnrollAuthReply(EnrollAuthResponseParams {
                                confirmed: Some(false),
                                passkey: None,
                                pincode: None,
                            }));
                        }
                    }
                    return None;
                }

                match key.code {
                    KeyCode::Esc => {
                        self.modal = ModalState::None;
                        return Some(TuiCommand::EnrollCancel);
                    }
                    KeyCode::Tab => {
                        *alias_focused = !*alias_focused;
                        return None;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        *selected_device_idx = selected_device_idx.saturating_sub(1);
                        return None;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if !self.discovered_wearables.is_empty()
                            && *selected_device_idx + 1 < self.discovered_wearables.len()
                        {
                            *selected_device_idx += 1;
                        }
                        return None;
                    }
                    KeyCode::Enter => {
                        if let Some(dev) = self.discovered_wearables.get(*selected_device_idx) {
                            let addr = dev.address.clone();
                            let alias = if let ModalState::EnrollmentModal {
                                ref alias_input, ..
                            } = self.modal
                            {
                                if alias_input.trim().is_empty() {
                                    None
                                } else {
                                    Some(alias_input.trim().to_string())
                                }
                            } else {
                                None
                            };
                            self.modal = ModalState::None;
                            return Some(TuiCommand::EnrollSelect {
                                address: addr,
                                alias,
                            });
                        }
                        return None;
                    }
                    _ => return None,
                }
            }
            ModalState::UnenrollModal {
                ref mut forget_bluez,
                ref mut delete_sessions,
                ref mut selected_button,
                ref wearable,
            } => match key.code {
                KeyCode::Esc => {
                    self.modal = ModalState::None;
                    return None;
                }
                KeyCode::Char('1') => {
                    *forget_bluez = !*forget_bluez;
                    return None;
                }
                KeyCode::Char('2') => {
                    *delete_sessions = !*delete_sessions;
                    return None;
                }
                KeyCode::Char(' ') => {
                    if *selected_button == 0 {
                        *forget_bluez = !*forget_bluez;
                    } else {
                        *delete_sessions = !*delete_sessions;
                    }
                    return None;
                }
                KeyCode::Tab
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Char('h')
                | KeyCode::Char('l') => {
                    *selected_button = if *selected_button == 0 { 1 } else { 0 };
                    return None;
                }
                KeyCode::Enter => {
                    if *selected_button == 0 {
                        let id = wearable.wearable.id.clone();
                        let fb = *forget_bluez;
                        let ds = *delete_sessions;
                        self.modal = ModalState::None;
                        return Some(TuiCommand::WearableUnenroll {
                            id,
                            forget_bluez: fb,
                            delete_sessions: ds,
                        });
                    } else {
                        self.modal = ModalState::None;
                        return None;
                    }
                }
                _ => return None,
            },
            ModalState::RenameModal { .. } => {
                // Handled in handle_text_input_key
                return None;
            }
            ModalState::None => {}
        }

        // 3. Global hotkeys when no modal is open
        match key.code {
            KeyCode::Char('q') => {
                self.should_quit = true;
                None
            }
            KeyCode::Char('?') => {
                self.modal = ModalState::Help;
                None
            }
            // Tab switching
            KeyCode::Char('1') => {
                self.active_tab = Tab::Dashboard;
                None
            }
            KeyCode::Char('2') => {
                self.active_tab = Tab::History;
                None
            }
            KeyCode::Char('3') => {
                self.active_tab = Tab::Wearables;
                None
            }
            KeyCode::Char('4') => {
                self.active_tab = Tab::Settings;
                None
            }
            KeyCode::Tab => {
                self.active_tab = self.active_tab.next();
                None
            }
            KeyCode::BackTab => {
                self.active_tab = self.active_tab.prev();
                None
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.active_tab = self.active_tab.prev();
                None
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.active_tab = self.active_tab.next();
                None
            }
            // List selection
            KeyCode::Char('j') | KeyCode::Down => {
                if self.active_tab == Tab::Wearables
                    && self.selected_wearable_idx + 1 < self.enrolled_wearables.len()
                {
                    self.selected_wearable_idx += 1;
                }
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.active_tab == Tab::Wearables {
                    self.selected_wearable_idx = self.selected_wearable_idx.saturating_sub(1);
                }
                None
            }
            // Global Grace period controls
            KeyCode::Char('c') if self.grace_state.is_some() => Some(TuiCommand::CancelGrace),
            KeyCode::Char('s') if self.grace_state.is_some() => Some(TuiCommand::SnoozeGrace(None)),

            // Tab-specific controls
            _ => match self.active_tab {
                Tab::Dashboard => match key.code {
                    KeyCode::Char('d') => {
                        self.is_diagnostics_open = !self.is_diagnostics_open;
                        None
                    }
                    KeyCode::Char('p') if self.daemon_connected => {
                        self.modal = ModalState::PausePopover { selected_idx: 0 };
                        None
                    }
                    KeyCode::Char('e') if self.enrolled_wearables.is_empty() => {
                        self.active_tab = Tab::Wearables;
                        self.modal = ModalState::EnrollmentModal {
                            scanning_timer_sec: 30,
                            selected_device_idx: 0,
                            alias_input: String::new(),
                            alias_focused: false,
                            pairing_prompt: None,
                        };
                        Some(TuiCommand::EnrollStart(Some(30)))
                    }
                    KeyCode::Char('a') if self.has_persistent_shift_alert() => {
                        Some(TuiCommand::AcceptShift)
                    }
                    KeyCode::Char('i') if self.has_persistent_shift_alert() => {
                        Some(TuiCommand::DismissShift)
                    }
                    KeyCode::Char('x') => {
                        if let Some(alert) = self.active_alert() {
                            let id = alert.id.clone();
                            self.alerts.retain(|a| a.id != id);
                            Some(TuiCommand::DismissAlert { id })
                        } else {
                            None
                        }
                    }
                    _ => None,
                },
                Tab::Wearables => match key.code {
                    KeyCode::Char('e') if self.daemon_connected => {
                        self.modal = ModalState::EnrollmentModal {
                            scanning_timer_sec: 30,
                            selected_device_idx: 0,
                            alias_input: String::new(),
                            alias_focused: false,
                            pairing_prompt: None,
                        };
                        Some(TuiCommand::EnrollStart(Some(30)))
                    }
                    KeyCode::Char('n') if !self.enrolled_wearables.is_empty() => {
                        let w = &self.enrolled_wearables[self.selected_wearable_idx];
                        let cur = w.wearable.alias.clone().unwrap_or_default();
                        self.modal = ModalState::RenameModal {
                            wearable_id: w.wearable.id.clone(),
                            current_alias: cur.clone(),
                            input: cur,
                        };
                        None
                    }
                    KeyCode::Char('c') if !self.enrolled_wearables.is_empty() => {
                        let id = self.enrolled_wearables[self.selected_wearable_idx]
                            .wearable
                            .id
                            .clone();
                        Some(TuiCommand::WearableConnect { id })
                    }
                    KeyCode::Char('u') if !self.enrolled_wearables.is_empty() => {
                        let w = self.enrolled_wearables[self.selected_wearable_idx].clone();
                        self.modal = ModalState::UnenrollModal {
                            wearable: w,
                            forget_bluez: false,
                            delete_sessions: false,
                            selected_button: 0,
                        };
                        None
                    }
                    _ => None,
                },
                _ => None,
            },
        }
    }

    fn handle_text_input_key(&mut self, key: KeyEvent) -> Option<TuiCommand> {
        match self.modal {
            ModalState::RenameModal {
                ref wearable_id,
                ref input,
                ..
            } => match key.code {
                KeyCode::Esc => {
                    self.modal = ModalState::None;
                    None
                }
                KeyCode::Enter => {
                    let w_id = wearable_id.clone();
                    let new_alias = if input.trim().is_empty() {
                        None
                    } else {
                        Some(input.trim().to_string())
                    };
                    self.modal = ModalState::None;
                    Some(TuiCommand::WearableRename {
                        id: w_id,
                        alias: new_alias,
                    })
                }
                KeyCode::Backspace => {
                    if let ModalState::RenameModal { ref mut input, .. } = self.modal {
                        input.pop();
                    }
                    None
                }
                KeyCode::Char(c) => {
                    if let ModalState::RenameModal { ref mut input, .. } = self.modal {
                        input.push(c);
                    }
                    None
                }
                _ => None,
            },
            ModalState::EnrollmentModal {
                ref mut alias_input,
                ref mut alias_focused,
                ref mut pairing_prompt,
                ..
            } => {
                if let Some(prompt) = pairing_prompt {
                    match prompt {
                        PairingPromptModalState::Passkey { input, .. }
                        | PairingPromptModalState::PinCode { input, .. } => match key.code {
                            KeyCode::Esc => {
                                self.modal = ModalState::None;
                                return Some(TuiCommand::EnrollCancel);
                            }
                            KeyCode::Backspace => {
                                input.pop();
                                return None;
                            }
                            KeyCode::Enter => {
                                let ans = input.clone();
                                let cmd = match prompt {
                                    PairingPromptModalState::Passkey { .. } => {
                                        let passkey_num = ans.parse::<u32>().ok();
                                        TuiCommand::EnrollAuthReply(EnrollAuthResponseParams {
                                            confirmed: Some(true),
                                            passkey: passkey_num,
                                            pincode: None,
                                        })
                                    }
                                    PairingPromptModalState::PinCode { .. } => {
                                        TuiCommand::EnrollAuthReply(EnrollAuthResponseParams {
                                            confirmed: Some(true),
                                            passkey: None,
                                            pincode: Some(ans),
                                        })
                                    }
                                    _ => return None,
                                };
                                self.modal = ModalState::None;
                                return Some(cmd);
                            }
                            KeyCode::Char(c) => {
                                input.push(c);
                                return None;
                            }
                            _ => return None,
                        },
                        _ => return None,
                    }
                }

                if *alias_focused {
                    match key.code {
                        KeyCode::Esc => {
                            self.modal = ModalState::None;
                            Some(TuiCommand::EnrollCancel)
                        }
                        KeyCode::Tab => {
                            *alias_focused = false;
                            None
                        }
                        KeyCode::Backspace => {
                            alias_input.pop();
                            None
                        }
                        KeyCode::Char(c) => {
                            alias_input.push(c);
                            None
                        }
                        _ => None,
                    }
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn confirm_pause_selection(&mut self, sel: usize) -> Option<TuiCommand> {
        self.modal = ModalState::None;
        match sel {
            0 => Some(TuiCommand::Resume),
            1 => Some(TuiCommand::Pause(PauseParams {
                mode: Some(PauseMode::Indefinite),
                duration_sec: None,
            })),
            2 => Some(TuiCommand::Pause(PauseParams {
                mode: Some(PauseMode::Duration),
                duration_sec: Some(3600),
            })),
            3 => Some(TuiCommand::Pause(PauseParams {
                mode: Some(PauseMode::Duration),
                duration_sec: Some(7200),
            })),
            4 => Some(TuiCommand::Pause(PauseParams {
                mode: Some(PauseMode::UntilMorning),
                duration_sec: None,
            })),
            _ => None,
        }
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<TuiCommand> {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // Tab click on row 0
                if mouse.row == 0 {
                    if mouse.column < 15 {
                        self.active_tab = Tab::Dashboard;
                    } else if mouse.column < 30 {
                        self.active_tab = Tab::History;
                    } else if mouse.column < 47 {
                        self.active_tab = Tab::Wearables;
                    } else if mouse.column < 62 {
                        self.active_tab = Tab::Settings;
                    }
                    return None;
                }

                // Global Grace banner click (row 1)
                if mouse.row == 1 && self.grace_state.is_some() {
                    // Click Cancel or Snooze in banner
                    if mouse.column >= 45 && mouse.column <= 68 {
                        return Some(TuiCommand::CancelGrace);
                    } else if mouse.column > 68 && mouse.column <= 82 {
                        return Some(TuiCommand::SnoozeGrace(None));
                    }
                }

                None
            }
            MouseEventKind::ScrollDown => {
                if self.active_tab == Tab::Wearables
                    && self.selected_wearable_idx + 1 < self.enrolled_wearables.len()
                {
                    self.selected_wearable_idx += 1;
                }
                None
            }
            MouseEventKind::ScrollUp => {
                if self.active_tab == Tab::Wearables {
                    self.selected_wearable_idx = self.selected_wearable_idx.saturating_sub(1);
                }
                None
            }
            _ => None,
        }
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
        let banner_height = if has_banner { 1u16 } else { 0u16 };

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
    let box_w = 46u16.min(area.width);
    let box_h = 6u16.min(area.height);
    let box_x = area.x + (area.width.saturating_sub(box_w)) / 2;
    let box_y = area.y + (area.height.saturating_sub(box_h)) / 2;
    let box_area = Rect::new(box_x, box_y, box_w, box_h);

    Clear.render(box_area, buf);

    let block = Block::default()
        .title(" Terminal too small ")
        .title_style(theme.card_title())
        .borders(Borders::ALL)
        .border_style(theme.warning());

    let inner = block.inner(box_area);
    block.render(box_area, buf);

    let lines = vec![
        Line::from(vec![
            Span::styled(" Current: ", theme.muted()),
            Span::styled(format!("{}×{}", area.width, area.height), theme.danger()),
        ]),
        Line::from(vec![
            Span::styled(" Minimum: ", theme.muted()),
            Span::styled("60×18 (recommended: 80×24)", theme.base()),
        ]),
    ];

    Paragraph::new(lines).render(inner, buf);
}
