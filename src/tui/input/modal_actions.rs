use chrono::TimeZone;

use super::super::app::{
    ActiveHoursField, Day1SeedField, ExportModalField, FeedbackModalField, ModalState,
    PairingPromptModalState, SoftRebaselineField, TuiApp, UnenrollButton,
};
use super::super::event::TuiCommand;
use super::super::hit_map::UnenrollCheckbox;
use super::step_index;
use crate::calibration::{FeedbackVerdict, parse_optional_sleep_hr_input, resolve_onset_hhmm_ms};
use crate::config::{ClockTime, ConfigPatch, SchedulePatch};
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
            self.reset_enrollment_scan_tracking();
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
            ModalState::Day1SeedModal { .. } => self.submit_day1_seed_modal(),
            ModalState::PurgeRawModal { .. } => self.confirm_purge_raw_modal(),
            ModalState::HardWipeModal { .. } => self.submit_hard_wipe_modal(),
            ModalState::ActiveHoursModal { .. } => self.submit_active_hours_modal(),
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

    fn apply_validated_modal_result(
        &mut self,
        res: Result<TuiCommand, String>,
    ) -> Option<TuiCommand> {
        match res {
            Ok(cmd) => {
                self.modal = ModalState::None;
                Some(cmd)
            }
            Err(err) => {
                match self.modal {
                    ModalState::FeedbackModal {
                        ref mut error_message,
                        ..
                    }
                    | ModalState::SoftRebaselineModal {
                        ref mut error_message,
                        ..
                    }
                    | ModalState::Day1SeedModal {
                        ref mut error_message,
                        ..
                    } => *error_message = Some(err),
                    _ => {}
                }
                None
            }
        }
    }

    pub(super) fn submit_feedback_modal(&mut self) -> Option<TuiCommand> {
        let res = self.modal.validate_feedback_modal()?;
        self.apply_validated_modal_result(res)
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
        if !require_confirmed || confirmed {
            self.session_deletion_in_progress = true;
            Some(TuiCommand::SessionDelete { id: sid })
        } else {
            None
        }
    }

    pub(super) fn focus_day1_seed_field(&mut self, field: Day1SeedField) {
        if let ModalState::Day1SeedModal {
            ref mut active_field,
            ..
        } = self.modal
        {
            *active_field = field;
        }
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

    pub(super) fn toggle_active_hours_enabled(&mut self) {
        if let ModalState::ActiveHoursModal {
            ref mut enabled,
            ref mut active_field,
            ref mut error_message,
            ..
        } = self.modal
        {
            *enabled = !*enabled;
            *active_field = ActiveHoursField::Enabled;
            *error_message = None;
        }
    }

    pub(super) fn focus_active_hours_field(&mut self, field: ActiveHoursField) {
        if let ModalState::ActiveHoursModal {
            ref mut active_field,
            ..
        } = self.modal
        {
            *active_field = field;
        }
    }

    pub(super) fn step_active_hours_field(&mut self, field: ActiveHoursField, delta_minutes: i32) {
        let Some((buf, last_valid, active_field, error_message)) =
            active_hours_time_target(&mut self.modal, field)
        else {
            return;
        };
        let base = buf.parse::<ClockTime>().unwrap_or(*last_valid);
        let total_min = i32::from(base.hour()) * 60 + i32::from(base.minute());
        let next_min = (total_min + delta_minutes).rem_euclid(24 * 60);
        let next = ClockTime::new((next_min / 60) as u8, (next_min % 60) as u8)
            .expect("rem_euclid(1440) guarantees valid ClockTime");
        *last_valid = next;
        *buf = next.to_string();
        *active_field = field;
        *error_message = None;
    }

    pub(super) fn edit_active_hours_time(&mut self, ch: Option<char>) {
        let ModalState::ActiveHoursModal { active_field, .. } = self.modal else {
            return;
        };
        let Some((buf, last_valid, _, error_message)) =
            active_hours_time_target(&mut self.modal, active_field)
        else {
            return;
        };
        match ch {
            None => {
                buf.pop();
            }
            Some(c) if (c.is_ascii_digit() || c == ':') && buf.len() < 5 => {
                buf.push(c);
            }
            Some(_) => return,
        }
        if let Ok(parsed) = buf.parse::<ClockTime>() {
            *last_valid = parsed;
        }
        *error_message = None;
    }

    pub(super) fn submit_active_hours_modal(&mut self) -> Option<TuiCommand> {
        let ModalState::ActiveHoursModal {
            enabled,
            ref start_input,
            ref end_input,
            ref mut error_message,
            ..
        } = self.modal
        else {
            return None;
        };
        let Ok(start) = start_input.parse::<ClockTime>() else {
            *error_message = Some("Start must be valid 24h HH:MM (00:00-23:59)".into());
            return None;
        };
        let Ok(end) = end_input.parse::<ClockTime>() else {
            *error_message = Some("End must be valid 24h HH:MM (00:00-23:59)".into());
            return None;
        };
        if enabled && start == end {
            *error_message = Some("Start and End must differ when Active Hours are enabled".into());
            return None;
        }
        self.modal = ModalState::None;
        Some(TuiCommand::ConfigUpdate(ConfigPatch {
            schedule: Some(SchedulePatch {
                active_hours_enabled: Some(enabled),
                start: Some(start),
                end: Some(end),
            }),
            ..Default::default()
        }))
    }

    pub(super) fn submit_soft_rebaseline_modal(&mut self) -> Option<TuiCommand> {
        let res = self.modal.validate_soft_rebaseline_modal()?;
        self.apply_validated_modal_result(res)
    }

    pub(super) fn submit_day1_seed_modal(&mut self) -> Option<TuiCommand> {
        let res = self.modal.validate_day1_seed_modal()?;
        self.apply_validated_modal_result(res)
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

fn active_hours_time_target(
    modal: &mut ModalState,
    field: ActiveHoursField,
) -> Option<(
    &mut String,
    &mut ClockTime,
    &mut ActiveHoursField,
    &mut Option<String>,
)> {
    let ModalState::ActiveHoursModal {
        start_input,
        end_input,
        last_valid_start,
        last_valid_end,
        active_field,
        error_message,
        ..
    } = modal
    else {
        return None;
    };
    let (buf, last_valid) = match field {
        ActiveHoursField::Start => (start_input, last_valid_start),
        ActiveHoursField::End => (end_input, last_valid_end),
        _ => return None,
    };
    Some((buf, last_valid, active_field, error_message))
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

fn parse_onset_clock_time(raw: &str) -> Result<ClockTime, String> {
    let trimmed = raw.trim();
    if let Ok(ct) = trimmed.parse::<ClockTime>() {
        return Ok(ct);
    }
    let Some((hour_str, min_str)) = trimmed.split_once(':') else {
        return Err("Sleep-onset time must be in HH:MM format".into());
    };
    if !(1..=2).contains(&hour_str.len())
        || min_str.len() != 2
        || !hour_str.bytes().all(|b| b.is_ascii_digit())
        || !min_str.bytes().all(|b| b.is_ascii_digit())
    {
        return Err("Sleep-onset time must be in HH:MM format".into());
    }
    let (Ok(h), Ok(m)) = (hour_str.parse::<u8>(), min_str.parse::<u8>()) else {
        return Err("Sleep-onset time must be valid numbers (HH:MM)".into());
    };
    ClockTime::new(h, m)
        .map_err(|_| "Sleep-onset time must be a valid 24h time (00:00–23:59)".into())
}

fn format_local_hhmm(ts_ms: i64) -> Option<String> {
    chrono::Local
        .timestamp_millis_opt(ts_ms)
        .earliest()
        .map(|dt| dt.format("%H:%M").to_string())
}

impl ModalState {
    pub(super) fn validate_soft_rebaseline_modal(&self) -> Option<Result<TuiCommand, String>> {
        let ModalState::SoftRebaselineModal {
            seed_sleep_hr_input,
            reset_floors,
            ..
        } = self
        else {
            return None;
        };
        Some(
            parse_optional_sleep_hr_input(seed_sleep_hr_input, "Seed HR").map(|seed| {
                TuiCommand::CalibrationSetBoundary {
                    effective_ts_ms: None,
                    seed_sleep_hr: seed,
                    reset_floors: *reset_floors,
                }
            }),
        )
    }

    pub(super) fn validate_day1_seed_modal(&self) -> Option<Result<TuiCommand, String>> {
        let ModalState::Day1SeedModal {
            seed_sleep_hr_input,
            ..
        } = self
        else {
            return None;
        };
        Some(
            parse_optional_sleep_hr_input(seed_sleep_hr_input, "Seed HR")
                .map(TuiCommand::CalibrationSetSeed),
        )
    }

    pub(super) fn validate_feedback_modal(&self) -> Option<Result<TuiCommand, String>> {
        let ModalState::FeedbackModal {
            session_id,
            session_start_ts_ms,
            session_end_ts_ms,
            epochs_pruned,
            verdict_idx,
            onset_hhmm_input,
            avg_sleep_hr_input,
            ..
        } = self
        else {
            return None;
        };

        let verdict = match verdict_idx {
            1 => Some(FeedbackVerdict::FalsePositive),
            2 => Some(FeedbackVerdict::MissedSleep),
            3 => Some(FeedbackVerdict::AnomalousExclude),
            _ => None,
        };

        let onset_hhmm = if !epochs_pruned && !onset_hhmm_input.trim().is_empty() {
            let clock_time = match parse_onset_clock_time(onset_hhmm_input) {
                Ok(ct) => ct,
                Err(err) => return Some(Err(err)),
            };
            if resolve_onset_hhmm_ms(*session_start_ts_ms, *session_end_ts_ms, clock_time).is_err()
            {
                let Some(start_hhmm) = format_local_hhmm(*session_start_ts_ms) else {
                    return Some(Err("Invalid session start timestamp".to_string()));
                };
                let end_hhmm = format_local_hhmm(*session_end_ts_ms).unwrap_or_default();
                return Some(Err(format!(
                    "Sleep-onset time must fall within session window ({start_hhmm}–{end_hhmm})"
                )));
            }
            Some(clock_time)
        } else {
            None
        };

        let avg_sleep_hr =
            match parse_optional_sleep_hr_input(avg_sleep_hr_input, "Average sleeping HR") {
                Ok(hr) => hr,
                Err(err) => return Some(Err(err)),
            };

        let clear = verdict.is_none() && onset_hhmm.is_none() && avg_sleep_hr.is_none();

        Some(Ok(TuiCommand::SessionSetFeedback {
            session_id: session_id.clone(),
            verdict,
            onset_hhmm,
            avg_sleep_hr,
            clear,
        }))
    }
}
