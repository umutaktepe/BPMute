use crossterm::event::{KeyCode, KeyEvent};

use super::super::app::{
    ExportModalField, FeedbackModalField, ModalState, PairingPromptModalState, SoftRebaselineField,
    TuiApp, UnenrollButton,
};
use super::super::event::TuiCommand;

impl TuiApp {
    pub(super) fn handle_modal_key(&mut self, key: KeyEvent) -> Option<TuiCommand> {
        match self.modal {
            ModalState::Help => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => self.close_modal(),
                    KeyCode::Down | KeyCode::Char('j') => self.scroll_help(1),
                    KeyCode::Up | KeyCode::Char('k') => self.scroll_help(-1),
                    KeyCode::PageDown => self.scroll_help(5),
                    KeyCode::PageUp => self.scroll_help(-5),
                    _ => {}
                }
                None
            }
            ModalState::PausePopover { selected_idx } => match key.code {
                KeyCode::Esc => {
                    self.modal = ModalState::None;
                    None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.scroll_pause_popover(-1);
                    None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.scroll_pause_popover(1);
                    None
                }
                KeyCode::Char(c @ '1'..='5') => {
                    let sel = (c as usize) - ('1' as usize);
                    self.confirm_pause_selection(sel)
                }
                KeyCode::Enter => self.confirm_pause_selection(selected_idx),
                _ => None,
            },
            ModalState::EnrollmentModal { .. } => self.handle_enrollment_modal_key(key),
            ModalState::UnenrollModal {
                ref mut forget_bluez,
                ref mut delete_sessions,
                ref mut selected_button,
                ..
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
                    UnenrollButton::Confirm => self.confirm_unenroll_modal(),
                    UnenrollButton::Cancel => {
                        self.modal = ModalState::None;
                        None
                    }
                },
                _ => None,
            },
            ModalState::FeedbackModal { active_field, .. } => match key.code {
                KeyCode::Esc => {
                    self.modal = ModalState::None;
                    None
                }
                KeyCode::Tab => {
                    if let ModalState::FeedbackModal {
                        epochs_pruned,
                        ref mut active_field,
                        ..
                    } = self.modal
                    {
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
                    }
                    None
                }
                KeyCode::BackTab => {
                    if let ModalState::FeedbackModal {
                        epochs_pruned,
                        ref mut active_field,
                        ..
                    } = self.modal
                    {
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
                    }
                    None
                }
                KeyCode::Left | KeyCode::Char('h')
                    if active_field == FeedbackModalField::Verdict =>
                {
                    if let ModalState::FeedbackModal {
                        ref mut verdict_idx,
                        ..
                    } = self.modal
                    {
                        *verdict_idx = if *verdict_idx == 0 {
                            3
                        } else {
                            *verdict_idx - 1
                        };
                    }
                    None
                }
                KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ')
                    if active_field == FeedbackModalField::Verdict =>
                {
                    if let ModalState::FeedbackModal {
                        ref mut verdict_idx,
                        ..
                    } = self.modal
                    {
                        *verdict_idx = (*verdict_idx + 1) % 4;
                    }
                    None
                }
                KeyCode::Enter => {
                    if active_field == FeedbackModalField::Cancel {
                        self.modal = ModalState::None;
                        return None;
                    }
                    self.submit_feedback_modal()
                }
                _ => None,
            },
            ModalState::ExportTraceModal { active_field, .. } => match key.code {
                KeyCode::Esc => {
                    self.modal = ModalState::None;
                    None
                }
                KeyCode::Tab => {
                    if let ModalState::ExportTraceModal {
                        ref mut active_field,
                        ..
                    } = self.modal
                    {
                        *active_field = match *active_field {
                            ExportModalField::Format => ExportModalField::Path,
                            ExportModalField::Path => ExportModalField::Anonymize,
                            ExportModalField::Anonymize => ExportModalField::Submit,
                            ExportModalField::Submit => ExportModalField::Cancel,
                            ExportModalField::Cancel => ExportModalField::Format,
                        };
                    }
                    None
                }
                KeyCode::BackTab => {
                    if let ModalState::ExportTraceModal {
                        ref mut active_field,
                        ..
                    } = self.modal
                    {
                        *active_field = match *active_field {
                            ExportModalField::Format => ExportModalField::Cancel,
                            ExportModalField::Path => ExportModalField::Format,
                            ExportModalField::Anonymize => ExportModalField::Path,
                            ExportModalField::Submit => ExportModalField::Anonymize,
                            ExportModalField::Cancel => ExportModalField::Submit,
                        };
                    }
                    None
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                    if active_field == ExportModalField::Format =>
                {
                    if let ModalState::ExportTraceModal {
                        ref mut format_idx,
                        ref mut destination_path,
                        ..
                    } = self.modal
                    {
                        *format_idx = (*format_idx + 1) % 2;
                        let ext = if *format_idx == 0 { "csv" } else { "jsonl" };
                        if let Some(dot_idx) = destination_path.rfind('.') {
                            destination_path.truncate(dot_idx + 1);
                            destination_path.push_str(ext);
                        }
                    }
                    None
                }
                KeyCode::Char(' ') if active_field == ExportModalField::Anonymize => {
                    if let ModalState::ExportTraceModal {
                        ref mut anonymize, ..
                    } = self.modal
                    {
                        *anonymize = !*anonymize;
                    }
                    None
                }
                KeyCode::Enter => {
                    if active_field == ExportModalField::Cancel {
                        self.modal = ModalState::None;
                        return None;
                    }
                    self.submit_export_trace_modal()
                }
                _ => None,
            },
            ModalState::DeleteSessionModal {
                ref mut confirmed, ..
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
                KeyCode::Enter => self.confirm_delete_session_modal(),
                _ => None,
            },
            ModalState::SoftRebaselineModal { active_field, .. } => match key.code {
                KeyCode::Esc => {
                    self.modal = ModalState::None;
                    None
                }
                KeyCode::Tab => {
                    if let ModalState::SoftRebaselineModal {
                        ref mut active_field,
                        ..
                    } = self.modal
                    {
                        *active_field = match *active_field {
                            SoftRebaselineField::SeedSleepHr => SoftRebaselineField::ResetFloors,
                            SoftRebaselineField::ResetFloors => SoftRebaselineField::Submit,
                            SoftRebaselineField::Submit => SoftRebaselineField::Cancel,
                            SoftRebaselineField::Cancel => SoftRebaselineField::SeedSleepHr,
                        };
                    }
                    None
                }
                KeyCode::BackTab => {
                    if let ModalState::SoftRebaselineModal {
                        ref mut active_field,
                        ..
                    } = self.modal
                    {
                        *active_field = match *active_field {
                            SoftRebaselineField::SeedSleepHr => SoftRebaselineField::Cancel,
                            SoftRebaselineField::ResetFloors => SoftRebaselineField::SeedSleepHr,
                            SoftRebaselineField::Submit => SoftRebaselineField::ResetFloors,
                            SoftRebaselineField::Cancel => SoftRebaselineField::Submit,
                        };
                    }
                    None
                }
                KeyCode::Char(' ') if active_field == SoftRebaselineField::ResetFloors => {
                    if let ModalState::SoftRebaselineModal {
                        ref mut reset_floors,
                        ..
                    } = self.modal
                    {
                        *reset_floors = !*reset_floors;
                    }
                    None
                }
                KeyCode::Enter => {
                    if active_field == SoftRebaselineField::ResetFloors {
                        if let ModalState::SoftRebaselineModal {
                            ref mut reset_floors,
                            ..
                        } = self.modal
                        {
                            *reset_floors = !*reset_floors;
                        }
                        return None;
                    }
                    if active_field == SoftRebaselineField::Cancel {
                        self.modal = ModalState::None;
                        return None;
                    }
                    self.submit_soft_rebaseline_modal()
                }
                _ => None,
            },
            ModalState::PurgeRawModal { .. } => match key.code {
                KeyCode::Esc => {
                    self.modal = ModalState::None;
                    None
                }
                KeyCode::Enter => self.confirm_purge_raw_modal(),
                _ => None,
            },
            ModalState::HardWipeModal { .. } => None,
            ModalState::RenameModal { .. } | ModalState::None => None,
        }
    }

    fn handle_enrollment_modal_key(&mut self, key: KeyEvent) -> Option<TuiCommand> {
        let ModalState::EnrollmentModal {
            ref mut selected_discovered_idx,
            ref mut alias_focused,
            ref mut enrolling,
            ref mut pairing_prompt,
            ..
        } = self.modal
        else {
            return None;
        };

        if let Some(PairingPromptModalState::Confirmation { .. }) = pairing_prompt {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.reply_pairing_confirmation(true),
                KeyCode::Char('n') | KeyCode::Char('N') => self.reply_pairing_confirmation(false),
                KeyCode::Esc => {
                    self.close_modal();
                    Some(TuiCommand::EnrollCancel)
                }
                _ => None,
            };
        }

        if *enrolling {
            if key.code == KeyCode::Esc {
                self.close_modal();
                return Some(TuiCommand::EnrollCancel);
            }
            return None;
        }

        match key.code {
            KeyCode::Esc => {
                self.close_modal();
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
            KeyCode::Char('r') => self.rescan_enrollment_modal(),
            KeyCode::Enter => self.confirm_enrollment_selection(),
            _ => None,
        }
    }

    pub(super) fn handle_text_input_key(&mut self, key: KeyEvent) -> Option<TuiCommand> {
        match self.modal {
            ModalState::RenameModal { ref mut input, .. } => match key.code {
                KeyCode::Esc => {
                    self.modal = ModalState::None;
                    None
                }
                KeyCode::Enter => self.submit_rename_modal(),
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
                                    self.close_modal();
                                    Some(TuiCommand::EnrollCancel)
                                }
                                KeyCode::Backspace => {
                                    input.pop();
                                    None
                                }
                                KeyCode::Enter => self.submit_pairing_prompt_input(),
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
                            self.close_modal();
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
                epochs_pruned,
                ref mut onset_hhmm_input,
                ref mut avg_sleep_hr_input,
                ref mut active_field,
                ref mut error_message,
                ..
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
                KeyCode::Enter => self.submit_feedback_modal(),
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
                ref mut destination_path,
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
                KeyCode::Enter => self.submit_export_trace_modal(),
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
                ref mut active_field,
                ref mut error_message,
                ..
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
                KeyCode::Enter => self.submit_soft_rebaseline_modal(),
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
                KeyCode::Enter => self.submit_hard_wipe_modal(),
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
}
