use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};

use super::app::{ModalState, PairingPromptModalState, Tab, TuiApp, UnenrollButton};
use super::event::TuiCommand;
use super::views::header::{GraceBannerLayout, tab_hit_ranges};
use crate::ipc::protocol::{EnrollAuthResponseParams, PauseMode, PauseParams};

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
                self.modal = ModalState::Help;
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
                }
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.active_tab == Tab::Wearables {
                    self.selected_wearable_idx = self.selected_wearable_idx.saturating_sub(1);
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

    fn handle_modal_key(&mut self, key: KeyEvent) -> Option<TuiCommand> {
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

    fn handle_tab_key(&mut self, key: KeyEvent) -> Option<TuiCommand> {
        match self.active_tab {
            Tab::Dashboard => match key.code {
                KeyCode::Char('d') => {
                    self.is_diagnostics_open = !self.is_diagnostics_open;
                    if self.is_diagnostics_open && self.daemon_connected {
                        Some(TuiCommand::RefreshStatus)
                    } else {
                        None
                    }
                }
                KeyCode::Char('p') if self.daemon_connected => {
                    self.modal = ModalState::PausePopover { selected_idx: 0 };
                    None
                }
                KeyCode::Char('e')
                    if self.daemon_connected && self.enrolled_wearables.is_empty() =>
                {
                    self.active_tab = Tab::Wearables;
                    Some(self.open_enrollment_modal())
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
                KeyCode::Char('x') if self.daemon_connected => {
                    if let Some(alert) = self.active_alert()
                        && alert.dismissible
                    {
                        let id = alert.id.clone();
                        self.alerts.retain(|a| a.id != id);
                        Some(TuiCommand::DismissAlert { id })
                    } else {
                        None
                    }
                }
                _ => None,
            },
            Tab::Wearables if self.daemon_connected => match key.code {
                KeyCode::Char('e') => Some(self.open_enrollment_modal()),
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
                        selected_button: UnenrollButton::Confirm,
                    };
                    None
                }
                _ => None,
            },
            _ => None,
        }
    }

    fn handle_text_input_key(&mut self, key: KeyEvent) -> Option<TuiCommand> {
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
        if self.modal != ModalState::None {
            return None;
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if mouse.row == 0 {
                    for (tab, start, end) in tab_hit_ranges() {
                        if mouse.column >= start && mouse.column < end {
                            self.active_tab = tab;
                            break;
                        }
                    }
                    return None;
                }

                if mouse.row == 1
                    && self.daemon_connected
                    && let Some(ref grace) = self.grace_state
                {
                    let layout = GraceBannerLayout::compute(grace, self.last_viewport_width);
                    if mouse.column >= layout.cancel_cols.0 && mouse.column < layout.cancel_cols.1 {
                        return Some(TuiCommand::CancelGrace);
                    } else if mouse.column >= layout.snooze_cols.0
                        && mouse.column < layout.snooze_cols.1
                    {
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
}
