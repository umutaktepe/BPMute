use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

use super::super::app::{CorePreference, TuiApp};
use super::super::event::TuiCommand;
use super::super::hit_map::{ClickTarget, ScrollTarget};
use super::step_index;

impl TuiApp {
    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<TuiCommand> {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.op_feedback = None;
                match self.hit_map.click_at(mouse.column, mouse.row)? {
                    ClickTarget::SelectTab(tab) => self.switch_tab(tab),
                    ClickTarget::CancelGrace | ClickTarget::SnoozeGrace
                        if !self.daemon_connected || self.grace_state.is_none() =>
                    {
                        None
                    }
                    ClickTarget::CancelGrace => Some(TuiCommand::CancelGrace),
                    ClickTarget::SnoozeGrace => Some(TuiCommand::SnoozeGrace(None)),
                    ClickTarget::ToggleDiagnostics => self.toggle_diagnostics(),
                    ClickTarget::OpenPausePopover => self.open_pause_popover(),
                    ClickTarget::AcceptShift => (self.daemon_connected
                        && self.has_persistent_shift_alert())
                    .then_some(TuiCommand::AcceptShift),
                    ClickTarget::IgnoreShift => (self.daemon_connected
                        && self.has_persistent_shift_alert())
                    .then_some(TuiCommand::DismissShift),
                    ClickTarget::DismissAlert => self.dismiss_active_alert(),
                    ClickTarget::DashboardEnroll => self.enroll_from_dashboard(),
                    ClickTarget::WearablesEnroll => {
                        self.daemon_connected.then(|| self.open_enrollment_modal())
                    }
                    ClickTarget::WearablesRename => self.open_rename_selected_wearable_modal(),
                    ClickTarget::WearablesReconnect => self.reconnect_selected_wearable(),
                    ClickTarget::WearablesUnenroll => self.open_unenroll_selected_wearable_modal(),
                    ClickTarget::SelectWearable(idx) => {
                        if idx < self.enrolled_wearables.len() {
                            self.selected_wearable_idx = idx;
                        }
                        None
                    }
                    ClickTarget::SelectSession(idx) => self.select_history_session(idx),
                    ClickTarget::HistoryFeedback => self.open_history_feedback_modal(),
                    ClickTarget::HistoryExport => self.open_history_export_modal(),
                    ClickTarget::HistoryDelete => self.open_history_delete_modal(),
                    ClickTarget::SettingsToggleSelected => self.toggle_selected_setting(),
                    ClickTarget::SettingsSelectOrToggle(idx) => {
                        if idx >= CorePreference::COUNT {
                            None
                        } else if self.selected_setting_idx == idx {
                            self.toggle_selected_setting()
                        } else {
                            self.selected_setting_idx = idx;
                            None
                        }
                    }
                    ClickTarget::SettingsEditorHandoff => Some(TuiCommand::EditorHandoff),
                    ClickTarget::SettingsOpenSeed => self.open_day1_seed_modal(),
                    ClickTarget::SettingsOpenRebase => self.open_soft_rebaseline_modal(),
                    ClickTarget::SettingsOpenPurge => self.open_purge_raw_modal(),
                    ClickTarget::SettingsOpenWipe => self.open_hard_wipe_modal(),
                    ClickTarget::SettingsUndoBoundary => (self.daemon_connected
                        && self.has_active_calibration_boundary())
                    .then_some(TuiCommand::CalibrationUndoBoundary),
                    ClickTarget::OpenHelp => {
                        self.open_help_modal();
                        None
                    }
                    ClickTarget::Quit => {
                        self.should_quit = true;
                        None
                    }
                    ClickTarget::CloseModal => self.cancel_active_modal(),
                    ClickTarget::ModalConfirm => self.confirm_active_modal(),
                    ClickTarget::ModalRescan => self.rescan_enrollment_modal(),
                    ClickTarget::EnrollmentFocusList => {
                        self.set_enrollment_alias_focus(false);
                        None
                    }
                    ClickTarget::EnrollmentSelectRow(idx) => self.click_enrollment_row(idx),
                    ClickTarget::FocusModalInput => {
                        self.focus_modal_input();
                        None
                    }
                    ClickTarget::PairingConfirm(confirmed) => {
                        self.reply_pairing_confirmation(confirmed)
                    }
                    ClickTarget::UnenrollToggle(checkbox) => {
                        self.toggle_unenroll_option(checkbox);
                        None
                    }
                    ClickTarget::UnenrollConfirm => self.confirm_unenroll_modal(),
                    ClickTarget::FeedbackVerdictStep(forward) => {
                        self.step_feedback_verdict(forward);
                        None
                    }
                    ClickTarget::FeedbackFocusField(field) => {
                        self.focus_feedback_field(field);
                        None
                    }
                    ClickTarget::ExportCycleFormat => {
                        self.cycle_export_format();
                        None
                    }
                    ClickTarget::ExportFocusField(field) => {
                        self.focus_export_field(field);
                        None
                    }
                    ClickTarget::ExportToggleAnonymize => {
                        self.toggle_export_anonymize();
                        None
                    }
                    ClickTarget::DeleteSessionConfirm => self.delete_session_modal(false),
                    ClickTarget::Day1SeedFocusField(field) => {
                        self.focus_day1_seed_field(field);
                        None
                    }
                    ClickTarget::SoftRebaselineFocusField(field) => {
                        self.focus_soft_rebaseline_field(field);
                        None
                    }
                    ClickTarget::SoftRebaselineToggleResetFloors => {
                        self.toggle_soft_rebaseline_reset_floors();
                        None
                    }
                    ClickTarget::ActiveHoursToggleEnabled => {
                        self.toggle_active_hours_enabled();
                        None
                    }
                    ClickTarget::ActiveHoursFocusField(field) => {
                        self.focus_active_hours_field(field);
                        None
                    }
                    ClickTarget::PauseOption(idx) => self.click_pause_option(idx),
                    ClickTarget::Consume => None,
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let target = self.hit_map.scroll_at(mouse.column, mouse.row)?;
                let delta: isize = if matches!(mouse.kind, MouseEventKind::ScrollDown) {
                    1
                } else {
                    -1
                };
                match target {
                    ScrollTarget::WearablesTable => {
                        self.scroll_wearables_selection(delta);
                        None
                    }
                    ScrollTarget::HistorySessions => self.scroll_history_selection(delta),
                    ScrollTarget::SettingsPreferences => {
                        self.selected_setting_idx =
                            step_index(self.selected_setting_idx, CorePreference::COUNT, delta);
                        None
                    }
                    ScrollTarget::ActiveHours(field) => {
                        let step = if delta < 0 { 15 } else { -15 };
                        self.step_active_hours_field(field, step);
                        None
                    }
                    ScrollTarget::EnrollmentList => {
                        self.scroll_enrollment_list(delta);
                        None
                    }
                    ScrollTarget::HelpModal => {
                        self.scroll_help(delta);
                        None
                    }
                    ScrollTarget::PausePopover => {
                        self.scroll_pause_popover(delta);
                        None
                    }
                }
            }
            _ => None,
        }
    }

    pub(super) fn scroll_help(&mut self, delta: isize) {
        let next =
            (self.help_scroll_offset as isize + delta).clamp(0, self.help_max_scroll as isize);
        self.help_scroll_offset = next as u16;
    }

    fn scroll_wearables_selection(&mut self, delta: isize) {
        self.selected_wearable_idx = step_index(
            self.selected_wearable_idx,
            self.enrolled_wearables.len(),
            delta,
        );
    }

    fn scroll_history_selection(&mut self, delta: isize) -> Option<TuiCommand> {
        let next = step_index(
            self.selected_session_idx,
            self.history_sessions.len(),
            delta,
        );
        if next == self.selected_session_idx {
            return None;
        }
        self.select_history_session(next)
    }
}
