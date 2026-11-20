use chrono::TimeZone;

use super::super::app::{
    ExportModalField, FeedbackModalField, ModalState, PairingPromptModalState, SoftRebaselineField,
    TuiApp, UnenrollButton,
};
use super::super::event::TuiCommand;
use super::super::hit_map::UnenrollCheckbox;
use super::step_index;
use crate::calibration::FeedbackVerdict;
use crate::ipc::protocol::{EnrollAuthResponseParams, PauseMode, PauseParams};
use crate::replay::TraceFormat;

const PAUSE_OPTION_COUNT: usize = 5;

impl TuiApp {
    pub(super) fn click_pause_option(&mut self, idx: usize) -> Option<TuiCommand> {
        let ModalState::PausePopover {
            ref mut selected_idx,
        } = self.modal
        else {
            return None;
        };
        if idx >= PAUSE_OPTION_COUNT {
            return None;
        }
        if *selected_idx == idx {
            self.confirm_pause_selection(idx)
        } else {
            *selected_idx = idx;
            None
        }
    }

    pub(super) fn scroll_pause_popover(&mut self, delta: isize) {
        if let ModalState::PausePopover {
            ref mut selected_idx,
        } = self.modal
        {
            let max_idx = (PAUSE_OPTION_COUNT - 1) as isize;
            let next = (*selected_idx as isize + delta).clamp(0, max_idx);
            *selected_idx = next as usize;
        }
    }

    pub(super) fn confirm_pause_selection(&mut self, sel: usize) -> Option<TuiCommand> {
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

    pub(super) fn cancel_active_modal(&mut self) -> Option<TuiCommand> {
        let is_enroll = matches!(self.modal, ModalState::EnrollmentModal { .. });
        self.close_modal();
        is_enroll.then_some(TuiCommand::EnrollCancel)
    }

    pub(super) fn rescan_enrollment_modal(&mut self) -> Option<TuiCommand> {
        if let ModalState::EnrollmentModal {
            ref mut scanning_remaining_ms,
            ref mut alias_focused,
            ref mut error_message,
            enrolling: false,
            pairing_prompt: None,
            ..
        } = self.modal
        {
            *alias_focused = false;
            *scanning_remaining_ms = 30_000;
            *error_message = None;
            Some(TuiCommand::EnrollStart(Some(30)))
        } else {
            None
        }
    }

    pub(super) fn focus_modal_input(&mut self) {
        match self.modal {
            ModalState::EnrollmentModal { .. } => self.set_enrollment_alias_focus(true),
            // RenameModal and HardWipeModal keep their text input focused while open.
            ModalState::RenameModal { .. } | ModalState::HardWipeModal { .. } => {}
            _ => {}
        }
    }

    pub(super) fn set_enrollment_alias_focus(&mut self, focused: bool) {
        if let ModalState::EnrollmentModal {
            ref mut alias_focused,
            enrolling: false,
            pairing_prompt: None,
            ..
        } = self.modal
        {
            *alias_focused = focused;
        }
    }

    pub(super) fn click_enrollment_row(&mut self, idx: usize) -> Option<TuiCommand> {
        let ModalState::EnrollmentModal {
            ref mut selected_discovered_idx,
            ref mut alias_focused,
            enrolling: false,
            pairing_prompt: None,
            ..
        } = self.modal
        else {
            return None;
        };
        if idx >= self.discovered_wearables.len() {
            return None;
        }
        let was_focused = *alias_focused;
        *alias_focused = false;
        if !was_focused && *selected_discovered_idx == idx {
            self.confirm_enrollment_selection()
        } else {
            *selected_discovered_idx = idx;
            None
        }
    }

    pub(super) fn scroll_enrollment_list(&mut self, delta: isize) {
        let ModalState::EnrollmentModal {
            ref mut selected_discovered_idx,
            enrolling: false,
            pairing_prompt: None,
            ..
        } = self.modal
        else {
            return;
        };
        *selected_discovered_idx = step_index(
            *selected_discovered_idx,
            self.discovered_wearables.len(),
            delta,
        );
    }

    pub(super) fn reply_pairing_confirmation(&mut self, confirmed: bool) -> Option<TuiCommand> {
        let ModalState::EnrollmentModal {
            ref mut pairing_prompt,
            ..
        } = self.modal
        else {
            return None;
        };
        if matches!(
            pairing_prompt,
            Some(PairingPromptModalState::Confirmation { .. })
        ) {
            *pairing_prompt = None;
            Some(TuiCommand::EnrollAuthReply(EnrollAuthResponseParams {
                confirmed: Some(confirmed),
                passkey: None,
                pincode: None,
            }))
        } else {
            None
        }
    }

    pub(super) fn submit_pairing_prompt_input(&mut self) -> Option<TuiCommand> {
        let ModalState::EnrollmentModal {
            ref mut pairing_prompt,
            ..
        } = self.modal
        else {
            return None;
        };
        let cmd = match pairing_prompt {
            Some(PairingPromptModalState::Passkey { input, .. }) => {
                TuiCommand::EnrollAuthReply(EnrollAuthResponseParams {
                    confirmed: Some(true),
                    passkey: input.parse::<u32>().ok(),
                    pincode: None,
                })
            }
            Some(PairingPromptModalState::PinCode { input, .. }) => {
                TuiCommand::EnrollAuthReply(EnrollAuthResponseParams {
                    confirmed: Some(true),
                    passkey: None,
                    pincode: Some(input.clone()),
                })
            }
            _ => return None,
        };
        *pairing_prompt = None;
        Some(cmd)
    }

    pub(super) fn confirm_active_modal(&mut self) -> Option<TuiCommand> {
        match self.modal {
            ModalState::PausePopover { selected_idx } => self.confirm_pause_selection(selected_idx),
            ModalState::EnrollmentModal {
                pairing_prompt:
                    Some(
                        PairingPromptModalState::Passkey { .. }
                        | PairingPromptModalState::PinCode { .. },
                    ),
                ..
            } => self.submit_pairing_prompt_input(),
            ModalState::EnrollmentModal {
                enrolling: false,
                pairing_prompt: None,
                ..
            } => self.confirm_enrollment_selection(),
            ModalState::RenameModal { .. } => self.submit_rename_modal(),
            ModalState::UnenrollModal {
                selected_button: UnenrollButton::Confirm,
                ..
            } => self.confirm_unenroll_modal(),
            ModalState::UnenrollModal {
                selected_button: UnenrollButton::Cancel,
                ..
            } => {
                self.modal = ModalState::None;
                None
            }
            ModalState::FeedbackModal { .. } => self.submit_feedback_modal(),
            ModalState::ExportTraceModal { .. } => self.submit_export_trace_modal(),
            ModalState::DeleteSessionModal { .. } => self.delete_session_modal(true),
            ModalState::SoftRebaselineModal { .. } => self.submit_soft_rebaseline_modal(),
            ModalState::PurgeRawModal { .. } => self.confirm_purge_raw_modal(),
            ModalState::HardWipeModal { .. } => self.submit_hard_wipe_modal(),
            _ => None,
        }
    }

    pub(super) fn confirm_enrollment_selection(&mut self) -> Option<TuiCommand> {
        let ModalState::EnrollmentModal {
            selected_discovered_idx,
            ref alias_input,
            ref mut alias_focused,
            ref mut enrolling,
            ref mut error_message,
            ..
        } = self.modal
        else {
            return None;
        };
        *alias_focused = false;
        let dev = self.discovered_wearables.get(selected_discovered_idx)?;
        let addr = dev.address.clone();
        let alias = if alias_input.trim().is_empty() {
            None
        } else {
            Some(alias_input.trim().to_string())
        };
        *enrolling = true;
        *error_message = None;
        Some(TuiCommand::EnrollSelect {
            address: addr,
            alias,
        })
    }

    pub(super) fn submit_rename_modal(&mut self) -> Option<TuiCommand> {
        let ModalState::RenameModal {
            ref wearable_id,
            ref input,
            ..
        } = self.modal
        else {
            return None;
        };
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

    pub(super) fn toggle_unenroll_option(&mut self, checkbox: UnenrollCheckbox) {
        if let ModalState::UnenrollModal {
            ref mut forget_bluez,
            ref mut delete_sessions,
            ..
        } = self.modal
        {
            match checkbox {
                UnenrollCheckbox::ForgetBluez => *forget_bluez = !*forget_bluez,
                UnenrollCheckbox::DeleteSessions => *delete_sessions = !*delete_sessions,
            }
        }
    }

    pub(super) fn confirm_unenroll_modal(&mut self) -> Option<TuiCommand> {
        let ModalState::UnenrollModal {
            ref wearable,
            forget_bluez,
            delete_sessions,
            ..
        } = self.modal
        else {
            return None;
        };
        let id = wearable.wearable.id.clone();
        self.modal = ModalState::None;
        Some(TuiCommand::WearableUnenroll {
            id,
            forget_bluez,
            delete_sessions,
        })
    }

    pub(super) fn step_feedback_verdict(&mut self, forward: bool) {
        if let ModalState::FeedbackModal {
            ref mut verdict_idx,
            ref mut active_field,
            ..
        } = self.modal
        {
            *active_field = FeedbackModalField::Verdict;
            *verdict_idx = if forward {
                (*verdict_idx + 1) % 4
            } else if *verdict_idx == 0 {
                3
            } else {
                *verdict_idx - 1
            };
        }
    }

    pub(super) fn focus_feedback_field(&mut self, field: FeedbackModalField) {
        if let ModalState::FeedbackModal {
            epochs_pruned,
            ref mut active_field,
            ..
        } = self.modal
        {
            if field == FeedbackModalField::Onset && epochs_pruned {
                return;
            }
            *active_field = field;
        }
    }

    pub(super) fn submit_feedback_modal(&mut self) -> Option<TuiCommand> {
        let ModalState::FeedbackModal {
            ref session_id,
            session_start_ts_ms,
            session_end_ts_ms,
            epochs_pruned,
            verdict_idx,
            ref onset_hhmm_input,
            ref avg_sleep_hr_input,
            ref mut error_message,
            ..
        } = self.modal
        else {
            return None;
        };
        match validate_and_submit_feedback(
            session_id,
            session_start_ts_ms,
            session_end_ts_ms,
            epochs_pruned,
            verdict_idx,
            onset_hhmm_input,
            avg_sleep_hr_input,
        ) {
            Ok(cmd) => {
                self.modal = ModalState::None;
                Some(cmd)
            }
            Err(err) => {
                *error_message = Some(err);
                None
            }
        }
    }

    pub(super) fn cycle_export_format(&mut self) {
        if let ModalState::ExportTraceModal {
            ref mut format_idx,
            ref mut destination_path,
            ref mut active_field,
            ..
        } = self.modal
        {
            *active_field = ExportModalField::Format;
            *format_idx = (*format_idx + 1) % 2;
            let ext = if *format_idx == 0 { "csv" } else { "jsonl" };
            update_export_path_extension(destination_path, ext);
        }
    }

    pub(super) fn focus_export_field(&mut self, field: ExportModalField) {
        if let ModalState::ExportTraceModal {
            ref mut active_field,
            ..
        } = self.modal
        {
            *active_field = field;
        }
    }

    pub(super) fn toggle_export_anonymize(&mut self) {
        if let ModalState::ExportTraceModal {
            ref mut anonymize,
            ref mut active_field,
            ..
        } = self.modal
        {
            *anonymize = !*anonymize;
            *active_field = ExportModalField::Anonymize;
        }
    }

    pub(super) fn submit_export_trace_modal(&mut self) -> Option<TuiCommand> {
        let ModalState::ExportTraceModal {
            ref session_id,
            format_idx,
            ref destination_path,
            anonymize,
            ..
        } = self.modal
        else {
            return None;
        };
        let sid = session_id.clone();
        let path = destination_path.clone();
        let fmt = if format_idx == 0 {
            TraceFormat::Csv
        } else {
            TraceFormat::Jsonl
        };
        self.modal = ModalState::None;
        Some(TuiCommand::SessionExportTrace {
            id: sid,
            path,
            format: fmt,
            no_anonymize: !anonymize,
        })
    }

    pub(super) fn delete_session_modal(&mut self, require_confirmed: bool) -> Option<TuiCommand> {
        let ModalState::DeleteSessionModal {
            ref session_id,
            confirmed,
        } = self.modal
        else {
            return None;
        };
        let sid = session_id.clone();
        self.modal = ModalState::None;
        (!require_confirmed || confirmed).then_some(TuiCommand::SessionDelete { id: sid })
    }

    pub(super) fn focus_soft_rebaseline_field(&mut self, field: SoftRebaselineField) {
        if let ModalState::SoftRebaselineModal {
            ref mut active_field,
            ..
        } = self.modal
        {
            *active_field = field;
        }
    }

    pub(super) fn toggle_soft_rebaseline_reset_floors(&mut self) {
        if let ModalState::SoftRebaselineModal {
            ref mut reset_floors,
            ref mut active_field,
            ..
        } = self.modal
        {
            *reset_floors = !*reset_floors;
            *active_field = SoftRebaselineField::ResetFloors;
        }
    }

    pub(super) fn submit_soft_rebaseline_modal(&mut self) -> Option<TuiCommand> {
        let ModalState::SoftRebaselineModal {
            ref seed_sleep_hr_input,
            reset_floors,
            ref mut error_message,
            ..
        } = self.modal
        else {
            return None;
        };
        let seed = if seed_sleep_hr_input.trim().is_empty() {
            None
        } else {
            match seed_sleep_hr_input.trim().parse::<f64>() {
                Ok(v) if (30.0..=120.0).contains(&v) => Some(v),
                _ => {
                    *error_message = Some("Seed HR must be between 30 and 120 BPM".into());
                    return None;
                }
            }
        };
        self.modal = ModalState::None;
        Some(TuiCommand::CalibrationSetBoundary {
            effective_ts_ms: None,
            seed_sleep_hr: seed,
            reset_floors,
        })
    }

    pub(super) fn confirm_purge_raw_modal(&mut self) -> Option<TuiCommand> {
        self.modal = ModalState::None;
        Some(TuiCommand::DataPurgeRaw)
    }

    pub(super) fn submit_hard_wipe_modal(&mut self) -> Option<TuiCommand> {
        let ModalState::HardWipeModal {
            ref typed_input,
            ref mut error_message,
        } = self.modal
        else {
            return None;
        };
        if typed_input.trim() == "WIPE" {
            self.modal = ModalState::None;
            Some(TuiCommand::DataHardWipe {
                confirm: "WIPE".into(),
                wipe_all: false,
            })
        } else {
            *error_message = Some("Confirmation mismatch: please type 'WIPE' to proceed".into());
            None
        }
    }
}

fn update_export_path_extension(path: &mut String, ext: &str) {
    if path.trim().is_empty() || path.ends_with('/') {
        return;
    }
    let file_start = path.rfind('/').map_or(0, |i| i + 1);
    if let Some(rel_dot) = path[file_start..].rfind('.')
        && rel_dot > 0
    {
        path.truncate(file_start + rel_dot + 1);
        path.push_str(ext);
    } else {
        path.push('.');
        path.push_str(ext);
    }
}

fn validate_and_submit_feedback(
    session_id: &str,
    session_start_ts_ms: i64,
    session_end_ts_ms: i64,
    epochs_pruned: bool,
    verdict_idx: usize,
    onset_input: &str,
    hr_input: &str,
) -> Result<TuiCommand, String> {
    let verdict = match verdict_idx {
        1 => Some(FeedbackVerdict::FalsePositive),
        2 => Some(FeedbackVerdict::MissedSleep),
        3 => Some(FeedbackVerdict::AnomalousExclude),
        _ => None,
    };

    let onset_hhmm = if !epochs_pruned && !onset_input.trim().is_empty() {
        let trimmed = onset_input.trim();
        let parts: Vec<&str> = trimmed.split(':').collect();
        if parts.len() != 2 {
            return Err("Sleep-onset time must be in HH:MM format".into());
        }
        let (Ok(h), Ok(m)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) else {
            return Err("Sleep-onset time must be valid numbers (HH:MM)".into());
        };
        if h > 23 || m > 59 {
            return Err("Sleep-onset time must be a valid 24h time (00:00–23:59)".into());
        }

        let Some(start_dt) = chrono::Local
            .timestamp_millis_opt(session_start_ts_ms)
            .single()
        else {
            return Err("Invalid session start timestamp".into());
        };
        let Some(candidate_today) = start_dt.date_naive().and_hms_opt(h, m, 0) else {
            return Err("Invalid time".into());
        };
        let ts_today = chrono::Local
            .from_local_datetime(&candidate_today)
            .single()
            .map(|dt| dt.timestamp_millis());
        let candidate_tomorrow = start_dt
            .date_naive()
            .succ_opt()
            .and_then(|d| d.and_hms_opt(h, m, 0));
        let ts_tomorrow = candidate_tomorrow
            .and_then(|dt| chrono::Local.from_local_datetime(&dt).single())
            .map(|dt| dt.timestamp_millis());

        let valid_ts = match (ts_today, ts_tomorrow) {
            (Some(t1), _) if t1 >= session_start_ts_ms && t1 <= session_end_ts_ms => Some(t1),
            (_, Some(t2)) if t2 >= session_start_ts_ms && t2 <= session_end_ts_ms => Some(t2),
            _ => None,
        };

        if valid_ts.is_none() {
            let start_hhmm = start_dt.format("%H:%M").to_string();
            let end_hhmm = chrono::Local
                .timestamp_millis_opt(session_end_ts_ms)
                .single()
                .map(|dt| dt.format("%H:%M").to_string())
                .unwrap_or_default();
            return Err(format!(
                "Sleep-onset time must fall within session window ({start_hhmm}–{end_hhmm})"
            ));
        }
        Some(trimmed.to_string())
    } else {
        None
    };

    let avg_sleep_hr = if !hr_input.trim().is_empty() {
        let Ok(hr) = hr_input.trim().parse::<f64>() else {
            return Err("Average sleeping HR must be a valid number".into());
        };
        if !(30.0..=120.0).contains(&hr) {
            return Err("Average sleeping HR must be between 30 and 120 BPM".into());
        }
        Some(hr)
    } else {
        None
    };

    let clear = verdict.is_none() && onset_hhmm.is_none() && avg_sleep_hr.is_none();

    Ok(TuiCommand::SessionSetFeedback {
        session_id: session_id.to_string(),
        verdict,
        onset_hhmm,
        avg_sleep_hr,
        clear,
    })
}
