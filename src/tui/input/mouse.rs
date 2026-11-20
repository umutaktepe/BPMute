use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

use super::super::app::TuiApp;
use super::super::event::TuiCommand;
use super::super::hit_map::{ClickTarget, ScrollTarget};

impl TuiApp {
    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<TuiCommand> {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                match self.hit_map.click_at(mouse.column, mouse.row)? {
                    ClickTarget::SelectTab(tab) => {
                        self.active_tab = tab;
                        None
                    }
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
                    ClickTarget::WearablesEnroll => {
                        self.daemon_connected.then(|| self.open_enrollment_modal())
                    }
                    ClickTarget::WearablesRename => self.open_rename_selected_wearable_modal(),
                    ClickTarget::WearablesReconnect => self.reconnect_selected_wearable(),
                    ClickTarget::WearablesUnenroll => self.open_unenroll_selected_wearable_modal(),
                    ClickTarget::HistoryFeedback => self.open_history_feedback_modal(),
                    ClickTarget::HistoryExport => self.open_history_export_modal(),
                    ClickTarget::HistoryDelete => self.open_history_delete_modal(),
                    ClickTarget::SettingsToggleSelected => {
                        if self.daemon_connected {
                            self.toggle_selected_setting()
                        } else {
                            None
                        }
                    }
                    ClickTarget::SettingsEditorHandoff => Some(TuiCommand::EditorHandoff),
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
                    }
                    ScrollTarget::HelpModal => {
                        self.scroll_help(delta);
                    }
                }
                None
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
        let len = self.enrolled_wearables.len();
        if len == 0 {
            return;
        }
        let max_idx = len.saturating_sub(1) as isize;
        let next = (self.selected_wearable_idx as isize + delta).clamp(0, max_idx);
        self.selected_wearable_idx = next as usize;
    }
}
