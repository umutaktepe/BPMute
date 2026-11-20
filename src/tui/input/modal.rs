use chrono::TimeZone;
use crossterm::event::{KeyCode, KeyEvent};

use super::super::app::{
    ExportModalField, FeedbackModalField, ModalState, PairingPromptModalState, SoftRebaselineField,
    TuiApp, UnenrollButton,
};
use super::super::event::TuiCommand;
use crate::calibration::FeedbackVerdict;
use crate::ipc::protocol::{EnrollAuthResponseParams, PauseMode, PauseParams};
use crate::replay::TraceFormat;

impl TuiApp {
    pub(super) fn handle_modal_key(&mut self, key: KeyEvent) -> Option<TuiCommand> {
        match self.modal {
            ModalState::Help => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q')
                ) {
                    self.modal = ModalState::None;
                }
                None
            }
            ModalState::PausePopover {
                ref mut selected_idx,
            } => match key.code {
                KeyCode::Esc => {
                    self.modal = ModalState::None;
                    None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected_idx = selected_idx.saturating_sub(1);
                    None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if *selected_idx < 4 {
                        *selected_idx += 1;
                    }
                    None
                }
                KeyCode::Char(c @ '1'..='5') => {
                    let sel = (c as usize) - ('1' as usize);
                    *selected_idx = sel;
                    self.confirm_pause_selection(sel)
                }
                KeyCode::Enter => {
                    let sel = *selected_idx;
                    self.confirm_pause_selection(sel)
                }
                _ => None,
            },
            ModalState::EnrollmentModal { .. } => self.handle_enrollment_modal_key(key),
            ModalState::UnenrollModal {
                ref mut forget_bluez,
                ref mut delete_sessions,
                ref mut selected_button,
                ref wearable,
            } => match key.code {
                KeyCode::Esc => {
                    self.modal = ModalState::None;
                    None
                }
                KeyCode::Char('1') => {
                    *forget_bluez = !*forget_bluez;
                    None
                }
                KeyCode::Char('2') => {
                    *delete_sessions = !*delete_sessions;
                    None
                }
                KeyCode::Char(' ') => {
                    match *selected_button {
                        UnenrollButton::Confirm => *forget_bluez = !*forget_bluez,
                        UnenrollButton::Cancel => *delete_sessions = !*delete_sessions,
                    }
                    None
                }
                KeyCode::Tab
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Char('h')
                | KeyCode::Char('l') => {
                    *selected_button = match *selected_button {
                        UnenrollButton::Confirm => UnenrollButton::Cancel,
                        UnenrollButton::Cancel => UnenrollButton::Confirm,
                    };
                    None
                }
                KeyCode::Enter => match *selected_button {
                    UnenrollButton::Confirm => {
                        let id = wearable.wearable.id.clone();
                        let fb = *forget_bluez;
                        let ds = *delete_sessions;
                        self.modal = ModalState::None;
                        Some(TuiCommand::WearableUnenroll {
                            id,
                            forget_bluez: fb,
                            delete_sessions: ds,
                        })
                    }
                    UnenrollButton::Cancel => {
                        self.modal = ModalState::None;
                        None
                    }
                },
                _ => None,
            },
            ModalState::FeedbackModal {
                ref session_id,
                session_start_ts_ms,
                session_end_ts_ms,
                epochs_pruned,
                ref mut verdict_idx,
                ref onset_hhmm_input,
                ref avg_sleep_hr_input,
                ref mut active_field,
                ref mut error_message,
            } => match key.code {
                KeyCode::Esc => {
                    self.modal = ModalState::None;
                    None
                }
                KeyCode::Tab => {
                    *active_field = match *active_field {
                        FeedbackModalField::Verdict => {
                            if epochs_pruned {
                                FeedbackModalField::SleepHr
                            } else {
                                FeedbackModalField::Onset
                            }
                        }
                        FeedbackModalField::Onset => FeedbackModalField::SleepHr,
                        FeedbackModalField::SleepHr => FeedbackModalField::Submit,
                        FeedbackModalField::Submit => FeedbackModalField::Cancel,
                        FeedbackModalField::Cancel => FeedbackModalField::Verdict,
                    };
                    None
                }
                KeyCode::BackTab => {
                    *active_field = match *active_field {
                        FeedbackModalField::Verdict => FeedbackModalField::Cancel,
                        FeedbackModalField::Onset => FeedbackModalField::Verdict,
                        FeedbackModalField::SleepHr => {
                            if epochs_pruned {
                                FeedbackModalField::Verdict
                            } else {
                                FeedbackModalField::Onset
                            }
                        }
                        FeedbackModalField::Submit => FeedbackModalField::SleepHr,
                        FeedbackModalField::Cancel => FeedbackModalField::Submit,
                    };
                    None
                }
                KeyCode::Left | KeyCode::Char('h')
                    if *active_field == FeedbackModalField::Verdict =>
                {
                    *verdict_idx = if *verdict_idx == 0 {
                        3
                    } else {
                        *verdict_idx - 1
                    };
                    None
                }
                KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ')
                    if *active_field == FeedbackModalField::Verdict =>
                {
                    *verdict_idx = (*verdict_idx + 1) % 4;
                    None
                }
                KeyCode::Enter => {
                    if *active_field == FeedbackModalField::Cancel {
                        self.modal = ModalState::None;
                        return None;
                    }
                    match validate_and_submit_feedback(
                        session_id,
                        session_start_ts_ms,
                        session_end_ts_ms,
                        epochs_pruned,
                        *verdict_idx,
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
                _ => None,
            },
            ModalState::ExportTraceModal {
                ref session_id,
                ref mut format_idx,
                ref mut destination_path,
                ref mut anonymize,
                ref mut active_field,
                ..
            } => match key.code {
                KeyCode::Esc => {
                    self.modal = ModalState::None;
                    None
                }
                KeyCode::Tab => {
                    *active_field = match *active_field {
                        ExportModalField::Format => ExportModalField::Path,
                        ExportModalField::Path => ExportModalField::Anonymize,
                        ExportModalField::Anonymize => ExportModalField::Submit,
                        ExportModalField::Submit => ExportModalField::Cancel,
                        ExportModalField::Cancel => ExportModalField::Format,
                    };
                    None
                }
                KeyCode::BackTab => {
                    *active_field = match *active_field {
                        ExportModalField::Format => ExportModalField::Cancel,
                        ExportModalField::Path => ExportModalField::Format,
                        ExportModalField::Anonymize => ExportModalField::Path,
                        ExportModalField::Submit => ExportModalField::Anonymize,
                        ExportModalField::Cancel => ExportModalField::Submit,
                    };
                    None
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                    if *active_field == ExportModalField::Format =>
                {
                    *format_idx = (*format_idx + 1) % 2;
                    let ext = if *format_idx == 0 { "csv" } else { "jsonl" };
                    if let Some(dot_idx) = destination_path.rfind('.') {
                        destination_path.truncate(dot_idx + 1);
                        destination_path.push_str(ext);
                    }
                    None
                }
                KeyCode::Char(' ') if *active_field == ExportModalField::Anonymize => {
                    *anonymize = !*anonymize;
                    None
                }
                KeyCode::Enter => {
                    if *active_field == ExportModalField::Cancel {
                        self.modal = ModalState::None;
                        return None;
                    }
                    let sid = session_id.clone();
                    let path = destination_path.clone();
                    let fmt = if *format_idx == 0 {
                        TraceFormat::Csv
                    } else {
                        TraceFormat::Jsonl
                    };
                    let no_anon = !*anonymize;
                    self.modal = ModalState::None;
                    Some(TuiCommand::SessionExportTrace {
                        id: sid,
                        path,
                        format: fmt,
                        no_anonymize: no_anon,
                    })
                }
                _ => None,
            },
            ModalState::DeleteSessionModal {
                ref session_id,
                ref mut confirmed,
            } => match key.code {
                KeyCode::Esc => {
                    self.modal = ModalState::None;
                    None
                }
                KeyCode::Tab
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Char('h')
                | KeyCode::Char('l') => {
                    *confirmed = !*confirmed;
                    None
                }
                KeyCode::Enter => {
                    let sid = session_id.clone();
                    let is_conf = *confirmed;
                    self.modal = ModalState::None;
                    if is_conf {
                        Some(TuiCommand::SessionDelete { id: sid })
                    } else {
                        None
                    }
                }
                _ => None,
            },
            ModalState::SoftRebaselineModal {
                ref seed_sleep_hr_input,
                ref mut reset_floors,
                ref mut active_field,
                ref mut error_message,
            } => match key.code {
                KeyCode::Esc => {
                    self.modal = ModalState::None;
                    None
                }
                KeyCode::Tab => {
                    *active_field = match *active_field {
                        SoftRebaselineField::SeedSleepHr => SoftRebaselineField::ResetFloors,
                        SoftRebaselineField::ResetFloors => SoftRebaselineField::Submit,
                        SoftRebaselineField::Submit => SoftRebaselineField::Cancel,
                        SoftRebaselineField::Cancel => SoftRebaselineField::SeedSleepHr,
                    };
                    None
                }
                KeyCode::BackTab => {
                    *active_field = match *active_field {
                        SoftRebaselineField::SeedSleepHr => SoftRebaselineField::Cancel,
                        SoftRebaselineField::ResetFloors => SoftRebaselineField::SeedSleepHr,
                        SoftRebaselineField::Submit => SoftRebaselineField::ResetFloors,
                        SoftRebaselineField::Cancel => SoftRebaselineField::Submit,
                    };
                    None
                }
                KeyCode::Char(' ') if *active_field == SoftRebaselineField::ResetFloors => {
                    *reset_floors = !*reset_floors;
                    None
                }
                KeyCode::Enter => {
                    if *active_field == SoftRebaselineField::ResetFloors {
                        *reset_floors = !*reset_floors;
                        return None;
                    }
                    if *active_field == SoftRebaselineField::Cancel {
                        self.modal = ModalState::None;
                        return None;
                    }
                    let seed = if seed_sleep_hr_input.trim().is_empty() {
                        None
                    } else {
                        match seed_sleep_hr_input.trim().parse::<f64>() {
                            Ok(v) if (30.0..=120.0).contains(&v) => Some(v),
                            _ => {
                                *error_message =
                                    Some("Seed HR must be between 30 and 120 BPM".into());
                                return None;
                            }
                        }
                    };
                    let reset = *reset_floors;
                    self.modal = ModalState::None;
                    Some(TuiCommand::CalibrationSetBoundary {
                        effective_ts_ms: None,
                        seed_sleep_hr: seed,
                        reset_floors: reset,
                    })
                }
                _ => None,
            },
            ModalState::PurgeRawModal { .. } => match key.code {
                KeyCode::Esc => {
                    self.modal = ModalState::None;
                    None
                }
                KeyCode::Enter => {
                    self.modal = ModalState::None;
                    Some(TuiCommand::DataPurgeRaw)
                }
                _ => None,
            },
            ModalState::HardWipeModal { .. } => None,
            ModalState::RenameModal { .. } | ModalState::None => None,
        }
    }

    fn handle_enrollment_modal_key(&mut self, key: KeyEvent) -> Option<TuiCommand> {
        let ModalState::EnrollmentModal {
            ref mut scanning_remaining_ms,
            ref mut selected_discovered_idx,
            ref alias_input,
            ref mut alias_focused,
            ref mut enrolling,
            ref mut error_message,
            ref mut pairing_prompt,
        } = self.modal
        else {
            return None;
        };

        if let Some(PairingPromptModalState::Confirmation { .. }) = pairing_prompt {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    *pairing_prompt = None;
                    Some(TuiCommand::EnrollAuthReply(EnrollAuthResponseParams {
                        confirmed: Some(true),
                        passkey: None,
                        pincode: None,
                    }))
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    *pairing_prompt = None;
                    Some(TuiCommand::EnrollAuthReply(EnrollAuthResponseParams {
                        confirmed: Some(false),
                        passkey: None,
                        pincode: None,
                    }))
                }
                KeyCode::Esc => {
                    self.modal = ModalState::None;
                    Some(TuiCommand::EnrollCancel)
                }
                _ => None,
            };
        }

        if *enrolling {
            if key.code == KeyCode::Esc {
                self.modal = ModalState::None;
                return Some(TuiCommand::EnrollCancel);
            }
            return None;
        }

        match key.code {
            KeyCode::Esc => {
                self.modal = ModalState::None;
                Some(TuiCommand::EnrollCancel)
            }
            KeyCode::Tab => {
                *alias_focused = !*alias_focused;
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                *selected_discovered_idx = selected_discovered_idx.saturating_sub(1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if *selected_discovered_idx + 1 < self.discovered_wearables.len() {
                    *selected_discovered_idx += 1;
                }
                None
            }
            KeyCode::Char('r') => {
                *scanning_remaining_ms = 30_000;
                *error_message = None;
                Some(TuiCommand::EnrollStart(Some(30)))
            }
            KeyCode::Enter => {
                if let Some(dev) = self.discovered_wearables.get(*selected_discovered_idx) {
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
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    pub(super) fn handle_text_input_key(&mut self, key: KeyEvent) -> Option<TuiCommand> {
        match self.modal {
            ModalState::RenameModal {
                ref wearable_id,
                ref mut input,
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
                    input.pop();
                    None
                }
                KeyCode::Char(c) => {
                    input.push(c);
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
                        | PairingPromptModalState::PinCode { input, .. } => {
                            return match key.code {
                                KeyCode::Esc => {
                                    self.modal = ModalState::None;
                                    Some(TuiCommand::EnrollCancel)
                                }
                                KeyCode::Backspace => {
                                    input.pop();
                                    None
                                }
                                KeyCode::Enter => {
                                    let ans = input.clone();
                                    let cmd = match prompt {
                                        PairingPromptModalState::Passkey { .. } => {
                                            TuiCommand::EnrollAuthReply(EnrollAuthResponseParams {
                                                confirmed: Some(true),
                                                passkey: ans.parse::<u32>().ok(),
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
                                    *pairing_prompt = None;
                                    Some(cmd)
                                }
                                KeyCode::Char(c) => {
                                    input.push(c);
                                    None
                                }
                                _ => None,
                            };
                        }
                        _ => return None,
                    }
                }

                if *alias_focused {
                    match key.code {
                        KeyCode::Esc => {
                            self.modal = ModalState::None;
                            Some(TuiCommand::EnrollCancel)
                        }
                        KeyCode::Tab | KeyCode::Enter => {
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
            ModalState::FeedbackModal {
                ref session_id,
                session_start_ts_ms,
                session_end_ts_ms,
                epochs_pruned,
                verdict_idx,
                ref mut onset_hhmm_input,
                ref mut avg_sleep_hr_input,
                ref mut active_field,
                ref mut error_message,
            } => match key.code {
                KeyCode::Esc => {
                    self.modal = ModalState::None;
                    None
                }
                KeyCode::Tab => {
                    *active_field = match *active_field {
                        FeedbackModalField::Onset => FeedbackModalField::SleepHr,
                        FeedbackModalField::SleepHr => FeedbackModalField::Submit,
                        _ => FeedbackModalField::Submit,
                    };
                    None
                }
                KeyCode::BackTab => {
                    *active_field = match *active_field {
                        FeedbackModalField::SleepHr => {
                            if epochs_pruned {
                                FeedbackModalField::Verdict
                            } else {
                                FeedbackModalField::Onset
                            }
                        }
                        FeedbackModalField::Onset => FeedbackModalField::Verdict,
                        _ => FeedbackModalField::Verdict,
                    };
                    None
                }
                KeyCode::Enter => match validate_and_submit_feedback(
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
                },
                KeyCode::Backspace => {
                    if *active_field == FeedbackModalField::Onset {
                        onset_hhmm_input.pop();
                    } else if *active_field == FeedbackModalField::SleepHr {
                        avg_sleep_hr_input.pop();
                    }
                    *error_message = None;
                    None
                }
                KeyCode::Char(c) => {
                    if *active_field == FeedbackModalField::Onset {
                        if (c.is_ascii_digit() || c == ':') && onset_hhmm_input.len() < 5 {
                            onset_hhmm_input.push(c);
                        }
                    } else if *active_field == FeedbackModalField::SleepHr
                        && (c.is_ascii_digit() || c == '.')
                        && avg_sleep_hr_input.len() < 5
                    {
                        avg_sleep_hr_input.push(c);
                    }
                    *error_message = None;
                    None
                }
                _ => None,
            },
            ModalState::ExportTraceModal {
                ref session_id,
                format_idx,
                ref mut destination_path,
                anonymize,
                ref mut active_field,
                ..
            } => match key.code {
                KeyCode::Esc => {
                    self.modal = ModalState::None;
                    None
                }
                KeyCode::Tab => {
                    *active_field = ExportModalField::Anonymize;
                    None
                }
                KeyCode::Enter => {
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
                KeyCode::Backspace => {
                    destination_path.pop();
                    None
                }
                KeyCode::Char(c) => {
                    destination_path.push(c);
                    None
                }
                _ => None,
            },
            ModalState::SoftRebaselineModal {
                ref mut seed_sleep_hr_input,
                ref mut reset_floors,
                ref mut active_field,
                ref mut error_message,
            } => match key.code {
                KeyCode::Esc => {
                    self.modal = ModalState::None;
                    None
                }
                KeyCode::Tab => {
                    *active_field = SoftRebaselineField::ResetFloors;
                    None
                }
                KeyCode::BackTab => {
                    *active_field = SoftRebaselineField::Cancel;
                    None
                }
                KeyCode::Enter => {
                    let seed = if seed_sleep_hr_input.trim().is_empty() {
                        None
                    } else {
                        match seed_sleep_hr_input.trim().parse::<f64>() {
                            Ok(v) if (30.0..=120.0).contains(&v) => Some(v),
                            _ => {
                                *error_message =
                                    Some("Seed HR must be between 30 and 120 BPM".into());
                                return None;
                            }
                        }
                    };
                    let reset = *reset_floors;
                    self.modal = ModalState::None;
                    Some(TuiCommand::CalibrationSetBoundary {
                        effective_ts_ms: None,
                        seed_sleep_hr: seed,
                        reset_floors: reset,
                    })
                }
                KeyCode::Backspace => {
                    seed_sleep_hr_input.pop();
                    *error_message = None;
                    None
                }
                KeyCode::Char(c)
                    if (c.is_ascii_digit() || c == '.') && seed_sleep_hr_input.len() < 6 =>
                {
                    seed_sleep_hr_input.push(c);
                    *error_message = None;
                    None
                }
                _ => None,
            },
            ModalState::HardWipeModal {
                ref mut typed_input,
                ref mut error_message,
            } => match key.code {
                KeyCode::Esc => {
                    self.modal = ModalState::None;
                    None
                }
                KeyCode::Enter => {
                    if typed_input.trim() == "WIPE" {
                        self.modal = ModalState::None;
                        Some(TuiCommand::DataHardWipe {
                            confirm: "WIPE".into(),
                            wipe_all: false,
                        })
                    } else {
                        *error_message =
                            Some("Confirmation mismatch: please type 'WIPE' to proceed".into());
                        None
                    }
                }
                KeyCode::Backspace => {
                    typed_input.pop();
                    *error_message = None;
                    None
                }
                KeyCode::Char(c) => {
                    typed_input.push(c);
                    *error_message = None;
                    None
                }
                _ => None,
            },
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
