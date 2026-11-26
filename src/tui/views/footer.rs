use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::tui::app::{ModalState, Tab, TuiApp};
use crate::tui::hit_map::{ClickTarget, HitMap};

struct FooterWriter<'a> {
    area: Rect,
    x: u16,
    spans: Vec<Span<'a>>,
    hit_map: &'a mut HitMap,
    modal_active: bool,
    key_style: Style,
    text_style: Style,
    muted_style: Style,
}

impl<'a> FooterWriter<'a> {
    fn new(app: &TuiApp, area: Rect, hit_map: &'a mut HitMap) -> Self {
        Self {
            area,
            x: area.x,
            spans: Vec::new(),
            hit_map,
            modal_active: app.modal != ModalState::None,
            key_style: app.theme.key_hint(),
            text_style: app.theme.base(),
            muted_style: app.theme.muted(),
        }
    }

    fn push_item_with_gap(
        &mut self,
        key: &'a str,
        label: &'a str,
        gap: &'a str,
        enabled: bool,
        target: Option<ClickTarget>,
    ) {
        let (key_style, text_style) = if enabled {
            (self.key_style, self.text_style)
        } else {
            (self.muted_style, self.muted_style)
        };
        let key_span = Span::styled(key, key_style);
        let label_span = Span::styled(label, text_style);
        let gap_span = Span::styled(gap, text_style);
        let item_w = (key_span.width() + label_span.width()) as u16;
        let gap_w = gap_span.width() as u16;

        if self.x < self.area.right()
            && let Some(click_target) = if enabled {
                target.or(self.modal_active.then_some(ClickTarget::Consume))
            } else {
                None
            }
        {
            let visible_w = item_w.min(self.area.right().saturating_sub(self.x));
            let rect = Rect::new(self.x, self.area.y, visible_w, 1);
            if self.modal_active {
                self.hit_map.push_modal_click(rect, click_target);
            } else {
                self.hit_map.push_click(rect, click_target);
            }
        }

        self.x = self.x.saturating_add(item_w).saturating_add(gap_w);
        self.spans.push(key_span);
        self.spans.push(label_span);
        if !gap.is_empty() {
            self.spans.push(gap_span);
        }
    }

    fn push_hint(&mut self, key: &'a str, label: &'a str) {
        self.push_item_with_gap(key, label, "  ", true, None);
    }

    fn push_action(&mut self, key: &'a str, label: &'a str, enabled: bool, target: ClickTarget) {
        self.push_item_with_gap(key, label, "  ", enabled, Some(target));
    }

    fn push_modal_confirm_cancel(&mut self, confirm_label: &'a str) {
        self.push_action("[Enter]", confirm_label, true, ClickTarget::ModalConfirm);
        self.push_action("[Esc]", " Cancel", true, ClickTarget::CloseModal);
    }

    fn finish(self, buf: &mut Buffer) {
        let line = Line::from(self.spans);
        buf.set_line(self.area.x, self.area.y, &line, self.area.width);
    }
}

pub(crate) fn render_footer(app: &TuiApp, area: Rect, buf: &mut Buffer, hit_map: &mut HitMap) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let mut writer = FooterWriter::new(app, area, hit_map);
    let connected = app.daemon_connected;

    match app.modal {
        ModalState::Help => {
            writer.push_action("[Esc/?]", " Close Help", true, ClickTarget::CloseModal);
            writer.push_action("[q]", " Quit", true, ClickTarget::Quit);
        }
        ModalState::PausePopover { .. } => {
            writer.push_hint("[1-5/j/k]", " Select");
            writer.push_modal_confirm_cancel(" Confirm");
        }
        ModalState::EnrollmentModal { .. } => {
            let nav_hint = if app.theme.ascii {
                "[j/k]"
            } else {
                "[j/k/↑/↓]"
            };
            writer.push_hint(nav_hint, " Select Wearable");
            writer.push_action("[Enter]", " Enroll", true, ClickTarget::ModalConfirm);
            writer.push_action("[r]", " Rescan", true, ClickTarget::ModalRescan);
            writer.push_action("[Esc]", " Cancel", true, ClickTarget::CloseModal);
        }
        ModalState::RenameModal { .. } => {
            writer.push_modal_confirm_cancel(" Save Alias");
        }
        ModalState::UnenrollModal { .. } => {
            let btn_hint = if app.theme.ascii {
                "[Tab/<-/->]"
            } else {
                "[Tab/←/→]"
            };
            writer.push_hint("[1/2/Space]", " Toggle");
            writer.push_hint(btn_hint, " Button");
            writer.push_modal_confirm_cancel(" Confirm");
        }
        ModalState::FeedbackModal { .. } => {
            let cycle_hint = if app.theme.ascii {
                "[<-/->/Space]"
            } else {
                "[←/→/Space]"
            };
            writer.push_hint("[Tab]", " Field");
            writer.push_hint(cycle_hint, " Option");
            writer.push_modal_confirm_cancel(" Submit");
        }
        ModalState::ExportTraceModal { .. } => {
            let cycle_hint = if app.theme.ascii {
                "[<-/->/Space]"
            } else {
                "[←/→/Space]"
            };
            writer.push_hint("[Tab]", " Field");
            writer.push_hint(cycle_hint, " Toggle");
            writer.push_modal_confirm_cancel(" Export");
        }
        ModalState::DeleteSessionModal { .. } => {
            let btn_hint = if app.theme.ascii {
                "[Tab/<-/->]"
            } else {
                "[Tab/←/→]"
            };
            writer.push_hint(btn_hint, " Button");
            writer.push_modal_confirm_cancel(" Confirm Delete");
        }
        ModalState::SoftRebaselineModal { .. } => {
            writer.push_hint("[Tab]", " Field");
            writer.push_hint("[Space]", " Toggle");
            writer.push_modal_confirm_cancel(" Submit");
        }
        ModalState::Day1SeedModal { .. } => {
            writer.push_hint("[Tab]", " Field");
            writer.push_modal_confirm_cancel(" Submit");
        }
        ModalState::PurgeRawModal { .. } => {
            writer.push_modal_confirm_cancel(" Confirm Purge");
        }
        ModalState::HardWipeModal { .. } => {
            writer.push_modal_confirm_cancel(" Confirm Wipe");
        }
        ModalState::ActiveHoursModal { .. } => {
            let (step_hint, step_label) = if app.theme.ascii {
                ("[<-/->]", " +/-15m")
            } else {
                ("[←/→]", " ±15m")
            };
            writer.push_hint("[Tab]", " Field");
            writer.push_hint("[Space]", " Toggle");
            writer.push_hint(step_hint, step_label);
            writer.push_modal_confirm_cancel(" Save");
        }
        ModalState::None => {
            writer.push_hint("[1-4]", " Tabs");

            match app.active_tab {
                Tab::Dashboard => {
                    writer.push_action("[d]", " Diagnostics", true, ClickTarget::ToggleDiagnostics);
                    writer.push_action("[p]", " Pause", connected, ClickTarget::OpenPausePopover);
                    if connected && app.session_close_deadline_ms.is_some() {
                        writer.push_action("[E]", " End Session", true, ClickTarget::EndSession);
                    }
                    if connected && app.grace_state.is_some() {
                        writer.push_action("[c]", " Cancel", true, ClickTarget::CancelGrace);
                        writer.push_action("[s]", " Snooze", true, ClickTarget::SnoozeGrace);
                    }
                    if connected && app.has_persistent_shift_alert() {
                        writer.push_action("[a]", " Accept Shift", true, ClickTarget::AcceptShift);
                        writer.push_action("[i]", " Ignore Shift", true, ClickTarget::IgnoreShift);
                    }
                }
                Tab::Wearables => {
                    let can_act = app.can_act_on_selected_wearable();
                    writer.push_action("[e]", " Enroll", connected, ClickTarget::WearablesEnroll);
                    writer.push_action("[n]", " Rename", can_act, ClickTarget::WearablesRename);
                    writer.push_action(
                        "[c]",
                        " Reconnect",
                        can_act,
                        ClickTarget::WearablesReconnect,
                    );
                    writer.push_action("[u]", " Unenroll", can_act, ClickTarget::WearablesUnenroll);
                }
                Tab::History if app.is_history_chart_inspecting() => {
                    let scrub_hint = if app.theme.ascii {
                        "[<-/->/h/l]"
                    } else {
                        "[←/→/h/l]"
                    };
                    writer.push_hint(scrub_hint, " Scrub");
                    writer.push_hint("[Home/End]", " Bounds");
                    writer.push_hint("[Esc/Enter]", " Exit");
                }
                Tab::History => {
                    let can_act = app.can_act_on_selected_session();
                    writer.push_action("[f]", " Feedback", can_act, ClickTarget::HistoryFeedback);
                    writer.push_action("[x]", " Export", can_act, ClickTarget::HistoryExport);
                    writer.push_action("[D]", " Delete", can_act, ClickTarget::HistoryDelete);
                    if connected && app.session_close_deadline_ms.is_some() {
                        writer.push_action("[E]", " End Session", true, ClickTarget::EndSession);
                    }
                    writer.push_hint("[j/k]", " Navigate");
                }
                Tab::Settings => {
                    writer.push_hint("[j/k]", " Navigate");
                    writer.push_action(
                        "[Space]",
                        " Toggle",
                        connected,
                        ClickTarget::SettingsToggleSelected,
                    );
                    writer.push_action("[E]", " Editor", true, ClickTarget::SettingsEditorHandoff);
                    writer.push_action(
                        "[B]",
                        " Rebase",
                        connected,
                        ClickTarget::SettingsOpenRebase,
                    );
                    writer.push_action("[P]", " Purge", connected, ClickTarget::SettingsOpenPurge);
                    writer.push_action("[W]", " Wipe", connected, ClickTarget::SettingsOpenWipe);
                    if app.has_active_calibration_boundary() {
                        writer.push_action(
                            "[U]",
                            " Undo Boundary",
                            connected,
                            ClickTarget::SettingsUndoBoundary,
                        );
                    }
                }
            }

            writer.push_action("[?]", " Help", true, ClickTarget::OpenHelp);
            writer.push_item_with_gap("[q]", " Quit", "", true, Some(ClickTarget::Quit));
        }
    }

    writer.finish(buf);
}
