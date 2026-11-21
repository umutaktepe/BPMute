use crate::config::PowerAction;
use crate::ipc::protocol::{DaemonStatus, MonitoringState, PairingPrompt};
use crate::tui::app::{GraceState, ModalState, OpFeedback, PairingPromptModalState, TuiApp};
use crate::tui::event::{IpcEvent, TuiCommand};

impl TuiApp {
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
        self.op_feedback = None;
        self.session_deletion_in_progress = false;
        self.discovered_wearables.clear();
        if self.modal != ModalState::Help {
            self.close_modal();
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
                        phase: tick.phase,
                    });
                }
            }
            IpcEvent::EpochUpdate(epoch_ev) => {
                self.latest_p_sleep = Some(epoch_ev.epoch.p_sleep);
                if epoch_ev.epoch.baseline_mu_hr.is_finite() && epoch_ev.epoch.baseline_mu_hr > 0.0
                {
                    self.sparkline_data.quiet_wake_baseline = Some(epoch_ev.epoch.baseline_mu_hr);
                }
                let session_known = self
                    .history_sessions
                    .iter()
                    .any(|s| s.id == epoch_ev.session_id);
                self.latest_epoch = Some(epoch_ev.epoch.clone());
                if !session_known {
                    if self.daemon_connected {
                        return Some(TuiCommand::RefreshSessions);
                    }
                    return None;
                }

                let detail_matches = self
                    .selected_session_detail
                    .as_ref()
                    .is_some_and(|d| d.session.id == epoch_ev.session_id);

                let is_duplicate = if detail_matches {
                    self.selected_session_detail
                        .as_ref()
                        .is_some_and(|d| d.epochs.iter().any(|e| e.ts_ms == epoch_ev.epoch.ts_ms))
                } else {
                    self.history_sessions
                        .iter()
                        .find(|s| s.id == epoch_ev.session_id)
                        .is_some_and(|s| s.epochs_count > 0 && epoch_ev.epoch.ts_ms <= s.end_ts_ms)
                };

                if let Some(entry) = self
                    .history_sessions
                    .iter_mut()
                    .find(|s| s.id == epoch_ev.session_id)
                {
                    if !is_duplicate {
                        entry.epochs_count = entry.epochs_count.saturating_add(1);
                    }
                    entry.end_ts_ms = entry.end_ts_ms.max(epoch_ev.epoch.ts_ms);
                    if epoch_ev.epoch.baseline_mu_hr.is_finite()
                        && epoch_ev.epoch.baseline_mu_hr > 0.0
                    {
                        entry.warmup_mu_hr = Some(epoch_ev.epoch.baseline_mu_hr);
                    }
                    if epoch_ev.epoch.hrv_valid || epoch_ev.epoch.features.ln_rmssd_60s.is_some() {
                        entry.had_rr = true;
                    }
                }

                if detail_matches && let Some(ref mut detail) = self.selected_session_detail {
                    if !is_duplicate {
                        detail.epochs.push(epoch_ev.epoch.clone());
                        if detail.epochs.len() > 1
                            && detail.epochs[detail.epochs.len() - 1].ts_ms
                                < detail.epochs[detail.epochs.len() - 2].ts_ms
                        {
                            detail.epochs.sort_by_key(|e| e.ts_ms);
                        }
                        detail.session.epochs_count = detail.epochs.len() as u32;
                    }
                    detail.session.end_ts_ms = detail.session.end_ts_ms.max(epoch_ev.epoch.ts_ms);
                    if epoch_ev.epoch.baseline_mu_hr.is_finite()
                        && epoch_ev.epoch.baseline_mu_hr > 0.0
                    {
                        detail.session.warmup_mu_hr = Some(epoch_ev.epoch.baseline_mu_hr);
                    }
                    if epoch_ev.epoch.hrv_valid || epoch_ev.epoch.features.ln_rmssd_60s.is_some() {
                        detail.session.had_rr = true;
                    }
                }
            }
            IpcEvent::SessionStarted(_) => {
                if self.daemon_connected {
                    return Some(TuiCommand::RefreshSessions);
                }
            }
            IpcEvent::SessionClosed(closed_detail) => {
                if let Some(entry) = self
                    .history_sessions
                    .iter_mut()
                    .find(|s| s.id == closed_detail.session.id)
                {
                    entry.status = closed_detail.status;
                    entry.end_ts_ms = closed_detail.session.end_ts_ms;
                    entry.end_reason = Some(closed_detail.session.end_reason);
                    entry.warmup_mu_hr = closed_detail.session.warmup_mu_hr;
                    entry.sleep_mu_hr = closed_detail.session.sleep_mu_hr;
                    entry.onset_confirmed_ts_ms = closed_detail.session.onset_confirmed_ts_ms;
                    entry.sleeping_ts_ms = closed_detail.session.sleeping_ts_ms;
                    entry.auto_anomalous = closed_detail.session.auto_anomalous;
                    entry.fp_count = u8::from(
                        closed_detail.session.grace_cancel_count > 0
                            || closed_detail.session.rapid_wake_fp_count > 0
                            || closed_detail.session.rapid_resume_within_5m,
                    );
                    entry.epochs_count = closed_detail.session.epochs_count;
                    entry.raw_samples_count = closed_detail.session.raw_samples_count;
                    entry.feedback = closed_detail.feedback.clone();
                }
                if self
                    .selected_session_detail
                    .as_ref()
                    .is_some_and(|d| d.session.id == closed_detail.session.id)
                {
                    self.selected_session_detail = Some(*closed_detail);
                }
                if self.daemon_connected {
                    return Some(TuiCommand::RefreshSessions);
                }
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
                let prev_status = self.active_wearable().map(|w| w.status);
                self.enrolled_wearables = wearables;
                if self.selected_wearable_idx >= self.enrolled_wearables.len() {
                    self.selected_wearable_idx = self.enrolled_wearables.len().saturating_sub(1);
                }
                let next_status = self.active_wearable().map(|w| w.status);
                if matches!(
                    next_status,
                    None | Some(crate::sleep_engine::StreamStatus::Disconnected)
                        | Some(crate::sleep_engine::StreamStatus::SignalLost)
                ) && prev_status != next_status
                {
                    self.latest_bpm = None;
                    self.contact_ok = false;
                }
            }
            IpcEvent::EnrollmentDiscovered(discovered) => {
                if !self.enrollment_update_seen {
                    self.discovered_wearables = discovered;
                    if let ModalState::EnrollmentModal {
                        ref mut selected_discovered_idx,
                        ..
                    } = self.modal
                        && *selected_discovered_idx >= self.discovered_wearables.len()
                    {
                        *selected_discovered_idx =
                            self.discovered_wearables.len().saturating_sub(1);
                    }
                }
            }
            IpcEvent::EnrollmentUpdate(update) => {
                self.enrollment_update_seen = true;
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
            IpcEvent::SessionsUpdated {
                sessions,
                boundaries,
            } => {
                let is_tui_deletion = std::mem::take(&mut self.session_deletion_in_progress);
                let prev_selected_id = self
                    .history_sessions
                    .get(self.selected_session_idx)
                    .map(|s| s.id.clone());
                self.history_sessions = sessions;
                self.history_boundaries = boundaries;
                if self.history_sessions.is_empty() {
                    self.selected_session_idx = 0;
                    self.selected_session_detail = None;
                } else if let Some(prev_id) = prev_selected_id
                    && let Some(new_idx) =
                        self.history_sessions.iter().position(|s| s.id == prev_id)
                {
                    self.selected_session_idx = new_idx;
                    if self.selected_session_detail.as_ref().map(|d| &d.session.id)
                        != Some(&prev_id)
                    {
                        self.selected_session_detail = None;
                        if self.daemon_connected {
                            return Some(TuiCommand::SessionGet { id: prev_id });
                        }
                    }
                } else if is_tui_deletion {
                    self.selected_session_idx = self
                        .selected_session_idx
                        .min(self.history_sessions.len().saturating_sub(1));
                    let target_id = self.history_sessions[self.selected_session_idx].id.clone();
                    self.selected_session_detail = None;
                    if self.daemon_connected {
                        return Some(TuiCommand::SessionGet { id: target_id });
                    }
                } else {
                    self.selected_session_idx = 0;
                    let target_id = self.history_sessions[0].id.clone();
                    self.selected_session_detail = None;
                    if self.daemon_connected {
                        return Some(TuiCommand::SessionGet { id: target_id });
                    }
                }
            }
            IpcEvent::SessionDetailLoaded(mut detail) => {
                if let Some(existing) = self.selected_session_detail.take()
                    && existing.session.id == detail.session.id
                {
                    for ep in existing.epochs {
                        if !detail.epochs.iter().any(|e| e.ts_ms == ep.ts_ms) {
                            detail.epochs.push(ep);
                        }
                    }
                    detail.epochs.sort_by_key(|e| e.ts_ms);
                    detail.session.epochs_count =
                        detail.session.epochs_count.max(detail.epochs.len() as u32);
                    if let Some(last_ep) = detail.epochs.last() {
                        detail.session.end_ts_ms = detail.session.end_ts_ms.max(last_ep.ts_ms);
                    }
                    if let Some(warmup_mu_hr) = existing.session.warmup_mu_hr
                        && detail.session.warmup_mu_hr.is_none()
                    {
                        detail.session.warmup_mu_hr = Some(warmup_mu_hr);
                    }
                }
                if let Some(entry) = self
                    .history_sessions
                    .iter_mut()
                    .find(|s| s.id == detail.session.id)
                {
                    entry.epochs_count = entry.epochs_count.max(detail.session.epochs_count);
                    entry.end_ts_ms = entry.end_ts_ms.max(detail.session.end_ts_ms);
                    if entry.warmup_mu_hr.is_none() {
                        entry.warmup_mu_hr = detail.session.warmup_mu_hr;
                    }
                }
                self.selected_session_detail = Some(*detail);
            }
            IpcEvent::ConfigLoaded(cfg) => {
                self.active_config = Some(cfg);
                self.config_error = None;
            }
            IpcEvent::ConfigError(err) => {
                self.config_error = Some(err);
            }
            IpcEvent::StorageStatsLoaded(stats) => {
                self.calibration_maturity = stats.calibration_maturity;
                self.storage_stats = Some(stats);
            }
            IpcEvent::OpCompleted {
                op,
                success,
                message,
            } => {
                if op == "monitor.cancel_grace" || op == "monitor.snooze_grace" {
                    self.grace_state = None;
                }
                if !success && op == "session.delete" {
                    self.session_deletion_in_progress = false;
                }
                if op == "wearable.enroll_select" {
                    if success {
                        if matches!(self.modal, ModalState::EnrollmentModal { .. }) {
                            self.close_modal();
                        }
                        self.op_feedback = message.map(|msg| OpFeedback {
                            is_error: false,
                            message: msg,
                        });
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
                    self.discovered_wearables.clear();
                    *scanning_remaining_ms = 0;
                    *error_message = message;
                } else if !success {
                    self.op_feedback = Some(OpFeedback {
                        is_error: true,
                        message: message.unwrap_or_else(|| format!("{op} failed")),
                    });
                } else if op == "session.export_trace" {
                    self.op_feedback = Some(OpFeedback {
                        is_error: false,
                        message: "Trace exported".to_string(),
                    });
                } else {
                    self.op_feedback = None;
                }
            }
        }
        None
    }
}
