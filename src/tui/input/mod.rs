use chrono::TimeZone;
use crossterm::event::{KeyCode, KeyEvent};

use super::app::{
    ExportModalField, FeedbackModalField, ModalState, SoftRebaselineField, Tab, TuiApp,
    UnenrollButton,
};
use super::event::TuiCommand;
use crate::calibration::FeedbackVerdict;

mod modal;
mod modal_actions;
mod mouse;

pub(super) fn step_index(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let max_idx = len.saturating_sub(1) as isize;
    (current as isize + delta).clamp(0, max_idx) as usize
}

impl TuiApp {
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<TuiCommand> {
        // 1. If text input is focused, suspend all global hotkeys and handle typing
        if self.is_input_focused() {
            return self.handle_text_input_key(key);
        }

        // 2. Handle active modals
        if self.modal != ModalState::None {
            return self.handle_modal_key(key);
        }

        // 3. Global hotkeys when no modal is open
        match key.code {
            KeyCode::Char('q') => {
                self.should_quit = true;
                None
            }
            KeyCode::Char('?') => {
                self.open_help_modal();
                None
            }
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
            KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right => {
                self.active_tab = self.active_tab.next();
                None
            }
            KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Left => {
                self.active_tab = self.active_tab.prev();
                None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if self.active_tab == Tab::Wearables
                    && self.selected_wearable_idx + 1 < self.enrolled_wearables.len()
                {
                    self.selected_wearable_idx += 1;
                } else if self.active_tab == Tab::History
                    && self.selected_session_idx + 1 < self.history_sessions.len()
                {
                    self.selected_session_idx += 1;
                    if let Some(s) = self.history_sessions.get(self.selected_session_idx) {
                        if !self.daemon_connected {
                            let sid = s.id.clone();
                            self.load_session_detail_offline(&sid);
                            return None;
                        }
                        return Some(TuiCommand::SessionGet { id: s.id.clone() });
                    }
                } else if self.active_tab == Tab::Settings && self.selected_setting_idx + 1 < 6 {
                    self.selected_setting_idx += 1;
                }
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.active_tab == Tab::Wearables {
                    self.selected_wearable_idx = self.selected_wearable_idx.saturating_sub(1);
                } else if self.active_tab == Tab::History {
                    self.selected_session_idx = self.selected_session_idx.saturating_sub(1);
                    if let Some(s) = self.history_sessions.get(self.selected_session_idx) {
                        if !self.daemon_connected {
                            let sid = s.id.clone();
                            self.load_session_detail_offline(&sid);
                            return None;
                        }
                        return Some(TuiCommand::SessionGet { id: s.id.clone() });
                    }
                } else if self.active_tab == Tab::Settings {
                    self.selected_setting_idx = self.selected_setting_idx.saturating_sub(1);
                }
                None
            }
            // Global Grace Period controls (disabled while daemon offline)
            KeyCode::Char('c') if self.daemon_connected && self.grace_state.is_some() => {
                Some(TuiCommand::CancelGrace)
            }
            KeyCode::Char('s') if self.daemon_connected && self.grace_state.is_some() => {
                Some(TuiCommand::SnoozeGrace(None))
            }
            // Tab-specific controls
            _ => self.handle_tab_key(key),
        }
    }

    fn handle_tab_key(&mut self, key: KeyEvent) -> Option<TuiCommand> {
        match self.active_tab {
            Tab::Dashboard => match key.code {
                KeyCode::Char('d') => self.toggle_diagnostics(),
                KeyCode::Char('p') if self.daemon_connected => self.open_pause_popover(),
                KeyCode::Char('e')
                    if self.daemon_connected && self.enrolled_wearables.is_empty() =>
                {
                    self.enroll_from_dashboard()
                }
                KeyCode::Char('a')
                    if self.daemon_connected && self.has_persistent_shift_alert() =>
                {
                    Some(TuiCommand::AcceptShift)
                }
                KeyCode::Char('i')
                    if self.daemon_connected && self.has_persistent_shift_alert() =>
                {
                    Some(TuiCommand::DismissShift)
                }
                KeyCode::Char('x') if self.daemon_connected => self.dismiss_active_alert(),
                _ => None,
            },
            Tab::Wearables if self.daemon_connected => match key.code {
                KeyCode::Char('e') => Some(self.open_enrollment_modal()),
                KeyCode::Char('n') if self.can_act_on_selected_wearable() => {
                    self.open_rename_selected_wearable_modal()
                }
                KeyCode::Char('c') if self.can_act_on_selected_wearable() => {
                    self.reconnect_selected_wearable()
                }
                KeyCode::Char('u') if self.can_act_on_selected_wearable() => {
                    self.open_unenroll_selected_wearable_modal()
                }
                _ => None,
            },
            Tab::History if self.can_act_on_selected_session() => match key.code {
                KeyCode::Char('f') => self.open_history_feedback_modal(),
                KeyCode::Char('x') => self.open_history_export_modal(),
                KeyCode::Char('D') => self.open_history_delete_modal(),
                _ => None,
            },
            Tab::Settings => match key.code {
                KeyCode::Char('E') => Some(TuiCommand::EditorHandoff),
                KeyCode::Char(' ') | KeyCode::Enter if self.daemon_connected => {
                    self.toggle_selected_setting()
                }
                KeyCode::Char('B') if self.daemon_connected => self.open_soft_rebaseline_modal(),
                KeyCode::Char('P') if self.daemon_connected => self.open_purge_raw_modal(),
                KeyCode::Char('W') if self.daemon_connected => self.open_hard_wipe_modal(),
                KeyCode::Char('U') if self.daemon_connected => {
                    Some(TuiCommand::CalibrationUndoBoundary)
                }
                _ => None,
            },
            _ => None,
        }
    }

    pub(super) fn toggle_diagnostics(&mut self) -> Option<TuiCommand> {
        self.is_diagnostics_open = !self.is_diagnostics_open;
        if self.is_diagnostics_open && self.daemon_connected {
            Some(TuiCommand::RefreshStatus)
        } else {
            None
        }
    }

    pub(super) fn open_pause_popover(&mut self) -> Option<TuiCommand> {
        if !self.daemon_connected {
            return None;
        }
        self.modal = ModalState::PausePopover { selected_idx: 0 };
        None
    }

    pub(super) fn dismiss_active_alert(&mut self) -> Option<TuiCommand> {
        if !self.daemon_connected {
            return None;
        }
        let alert = self.active_alert()?;
        if !alert.dismissible {
            return None;
        }
        let id = alert.id.clone();
        self.alerts.retain(|a| a.id != id);
        Some(TuiCommand::DismissAlert { id })
    }

    pub(super) fn enroll_from_dashboard(&mut self) -> Option<TuiCommand> {
        if !self.daemon_connected || !self.enrolled_wearables.is_empty() {
            return None;
        }
        self.active_tab = Tab::Wearables;
        Some(self.open_enrollment_modal())
    }

    pub(super) fn open_rename_selected_wearable_modal(&mut self) -> Option<TuiCommand> {
        if !self.can_act_on_selected_wearable() {
            return None;
        }
        let w = &self.enrolled_wearables[self.selected_wearable_idx];
        let cur = w.wearable.alias.clone().unwrap_or_default();
        self.modal = ModalState::RenameModal {
            wearable_id: w.wearable.id.clone(),
            current_alias: cur.clone(),
            input: cur,
        };
        None
    }

    pub(super) fn reconnect_selected_wearable(&self) -> Option<TuiCommand> {
        if !self.can_act_on_selected_wearable() {
            return None;
        }
        let id = self.enrolled_wearables[self.selected_wearable_idx]
            .wearable
            .id
            .clone();
        Some(TuiCommand::WearableConnect { id })
    }

    pub(super) fn open_unenroll_selected_wearable_modal(&mut self) -> Option<TuiCommand> {
        if !self.can_act_on_selected_wearable() {
            return None;
        }
        let w = self.enrolled_wearables[self.selected_wearable_idx].clone();
        self.modal = ModalState::UnenrollModal {
            wearable: w,
            forget_bluez: false,
            delete_sessions: false,
            selected_button: UnenrollButton::Confirm,
        };
        None
    }

    pub(super) fn open_history_feedback_modal(&mut self) -> Option<TuiCommand> {
        if !self.can_act_on_selected_session() {
            return None;
        }
        let s = self.history_sessions.get(self.selected_session_idx)?;
        let epochs_pruned = s.epochs_count == 0;
        let (v_idx, onset_str, hr_str) = if let Some(fb) = &s.feedback {
            let vi = match fb.verdict {
                None => 0,
                Some(FeedbackVerdict::FalsePositive) => 1,
                Some(FeedbackVerdict::MissedSleep) => 2,
                Some(FeedbackVerdict::AnomalousExclude) => 3,
            };
            let o = fb
                .manual_onset_ts_ms
                .and_then(|ts| {
                    chrono::Local
                        .timestamp_millis_opt(ts)
                        .single()
                        .map(|dt| dt.format("%H:%M").to_string())
                })
                .unwrap_or_default();
            let h = fb
                .manual_avg_sleep_hr
                .map(|v| format!("{v:.1}"))
                .unwrap_or_default();
            (vi, o, h)
        } else {
            (0, String::new(), String::new())
        };

        self.modal = ModalState::FeedbackModal {
            session_id: s.id.clone(),
            session_start_ts_ms: s.start_ts_ms,
            session_end_ts_ms: s.end_ts_ms,
            epochs_pruned,
            verdict_idx: v_idx,
            onset_hhmm_input: onset_str,
            avg_sleep_hr_input: hr_str,
            active_field: FeedbackModalField::Verdict,
            error_message: None,
        };
        None
    }

    pub(super) fn open_history_export_modal(&mut self) -> Option<TuiCommand> {
        if !self.can_act_on_selected_session() {
            return None;
        }
        let s = self.history_sessions.get(self.selected_session_idx)?;
        let default_path = if let Some(ref paths) = self.offline_paths {
            paths
                .data_dir
                .join("exports")
                .join(format!("session-{}.csv", s.id))
                .to_string_lossy()
                .to_string()
        } else {
            format!("session-{}.csv", s.id)
        };
        self.modal = ModalState::ExportTraceModal {
            session_id: s.id.clone(),
            format_idx: 0,
            destination_path: default_path,
            anonymize: true,
            active_field: ExportModalField::Submit,
            error_message: None,
        };
        None
    }

    pub(super) fn open_history_delete_modal(&mut self) -> Option<TuiCommand> {
        if !self.can_act_on_selected_session() {
            return None;
        }
        let s = self.history_sessions.get(self.selected_session_idx)?;
        self.modal = ModalState::DeleteSessionModal {
            session_id: s.id.clone(),
            confirmed: true,
        };
        None
    }

    pub(super) fn open_soft_rebaseline_modal(&mut self) -> Option<TuiCommand> {
        if !self.daemon_connected {
            return None;
        }
        self.modal = ModalState::SoftRebaselineModal {
            seed_sleep_hr_input: String::new(),
            reset_floors: false,
            active_field: SoftRebaselineField::SeedSleepHr,
            error_message: None,
        };
        None
    }

    pub(super) fn open_purge_raw_modal(&mut self) -> Option<TuiCommand> {
        if !self.daemon_connected {
            return None;
        }
        self.modal = ModalState::PurgeRawModal { confirmed: true };
        None
    }

    pub(super) fn open_hard_wipe_modal(&mut self) -> Option<TuiCommand> {
        if !self.daemon_connected {
            return None;
        }
        self.modal = ModalState::HardWipeModal {
            typed_input: String::new(),
            error_message: None,
        };
        None
    }

    fn toggle_selected_setting(&mut self) -> Option<TuiCommand> {
        let cfg = self.active_config.as_ref()?;
        match self.selected_setting_idx {
            0 => {
                let next_sens = match cfg.detection.sensitivity {
                    crate::config::Sensitivity::Conservative => {
                        crate::config::Sensitivity::Balanced
                    }
                    crate::config::Sensitivity::Balanced => crate::config::Sensitivity::Responsive,
                    crate::config::Sensitivity::Responsive => {
                        crate::config::Sensitivity::Conservative
                    }
                };
                Some(TuiCommand::ConfigUpdate(crate::config::ConfigPatch {
                    detection: Some(crate::config::DetectionPatch {
                        sensitivity: Some(next_sens),
                        ..Default::default()
                    }),
                    ..Default::default()
                }))
            }
            1 => Some(TuiCommand::ConfigUpdate(crate::config::ConfigPatch {
                actions: Some(crate::config::ActionsPatch {
                    pause_media: Some(!cfg.actions.pause_media),
                    ..Default::default()
                }),
                ..Default::default()
            })),
            2 => Some(TuiCommand::ConfigUpdate(crate::config::ConfigPatch {
                actions: Some(crate::config::ActionsPatch {
                    lock_screen: Some(!cfg.actions.lock_screen),
                    ..Default::default()
                }),
                ..Default::default()
            })),
            3 => {
                let next_action = match cfg.actions.power_action {
                    crate::config::PowerAction::Suspend => crate::config::PowerAction::Poweroff,
                    crate::config::PowerAction::Poweroff => crate::config::PowerAction::None,
                    crate::config::PowerAction::None => crate::config::PowerAction::Suspend,
                };
                Some(TuiCommand::ConfigUpdate(crate::config::ConfigPatch {
                    actions: Some(crate::config::ActionsPatch {
                        power_action: Some(next_action),
                        ..Default::default()
                    }),
                    ..Default::default()
                }))
            }
            4 => {
                let cur = cfg.actions.grace_period_sec.unwrap_or(0);
                let next = match cur {
                    0..=15 => 30,
                    16..=30 => 60,
                    31..=60 => 90,
                    _ => 15,
                };
                Some(TuiCommand::ConfigUpdate(crate::config::ConfigPatch {
                    actions: Some(crate::config::ActionsPatch {
                        grace_period_sec: crate::config::PatchField::Set(next),
                        ..Default::default()
                    }),
                    ..Default::default()
                }))
            }
            5 => Some(TuiCommand::ConfigUpdate(crate::config::ConfigPatch {
                actions: Some(crate::config::ActionsPatch {
                    dry_run: Some(!cfg.actions.dry_run),
                    ..Default::default()
                }),
                ..Default::default()
            })),
            _ => None,
        }
    }
}
