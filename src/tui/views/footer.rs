use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::tui::app::{ModalState, Tab, TuiApp};

pub fn render_footer(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let mut spans = Vec::new();
    let key_style = app.theme.key_hint();
    let live_key_style = if app.daemon_connected {
        key_style
    } else {
        app.theme.muted()
    };
    let text_style = app.theme.base();
    let live_text_style = if app.daemon_connected {
        text_style
    } else {
        app.theme.muted()
    };

    match app.modal {
        ModalState::Help => {
            spans.push(Span::styled("[Esc/?]", key_style));
            spans.push(Span::styled(" Close Help  ", text_style));
            spans.push(Span::styled("[q]", key_style));
            spans.push(Span::styled(" Quit  ", text_style));
        }
        ModalState::PausePopover { .. } => {
            spans.push(Span::styled("[1-5/j/k]", key_style));
            spans.push(Span::styled(" Select  ", text_style));
            spans.push(Span::styled("[Enter]", key_style));
            spans.push(Span::styled(" Confirm  ", text_style));
            spans.push(Span::styled("[Esc]", key_style));
            spans.push(Span::styled(" Cancel  ", text_style));
        }
        ModalState::EnrollmentModal { .. } => {
            let nav_hint = if app.theme.ascii {
                "[j/k]"
            } else {
                "[j/k/↑/↓]"
            };
            spans.push(Span::styled(nav_hint, key_style));
            spans.push(Span::styled(" Select Wearable  ", text_style));
            spans.push(Span::styled("[Enter]", key_style));
            spans.push(Span::styled(" Enroll  ", text_style));
            spans.push(Span::styled("[r]", key_style));
            spans.push(Span::styled(" Rescan  ", text_style));
            spans.push(Span::styled("[Esc]", key_style));
            spans.push(Span::styled(" Cancel  ", text_style));
        }
        ModalState::RenameModal { .. } => {
            spans.push(Span::styled("[Enter]", key_style));
            spans.push(Span::styled(" Save Alias  ", text_style));
            spans.push(Span::styled("[Esc]", key_style));
            spans.push(Span::styled(" Cancel  ", text_style));
        }
        ModalState::UnenrollModal { .. } => {
            let btn_hint = if app.theme.ascii {
                "[Tab/<-/->]"
            } else {
                "[Tab/←/→]"
            };
            spans.push(Span::styled("[1/2/Space]", key_style));
            spans.push(Span::styled(" Toggle  ", text_style));
            spans.push(Span::styled(btn_hint, key_style));
            spans.push(Span::styled(" Button  ", text_style));
            spans.push(Span::styled("[Enter]", key_style));
            spans.push(Span::styled(" Confirm  ", text_style));
            spans.push(Span::styled("[Esc]", key_style));
            spans.push(Span::styled(" Cancel  ", text_style));
        }
        ModalState::FeedbackModal { .. } => {
            let cycle_hint = if app.theme.ascii {
                "[<-/->/Space]"
            } else {
                "[←/→/Space]"
            };
            spans.push(Span::styled("[Tab]", key_style));
            spans.push(Span::styled(" Field  ", text_style));
            spans.push(Span::styled(cycle_hint, key_style));
            spans.push(Span::styled(" Option  ", text_style));
            spans.push(Span::styled("[Enter]", key_style));
            spans.push(Span::styled(" Submit  ", text_style));
            spans.push(Span::styled("[Esc]", key_style));
            spans.push(Span::styled(" Cancel  ", text_style));
        }
        ModalState::ExportTraceModal { .. } => {
            let cycle_hint = if app.theme.ascii {
                "[<-/->/Space]"
            } else {
                "[←/→/Space]"
            };
            spans.push(Span::styled("[Tab]", key_style));
            spans.push(Span::styled(" Field  ", text_style));
            spans.push(Span::styled(cycle_hint, key_style));
            spans.push(Span::styled(" Toggle  ", text_style));
            spans.push(Span::styled("[Enter]", key_style));
            spans.push(Span::styled(" Export  ", text_style));
            spans.push(Span::styled("[Esc]", key_style));
            spans.push(Span::styled(" Cancel  ", text_style));
        }
        ModalState::DeleteSessionModal { .. } => {
            let btn_hint = if app.theme.ascii {
                "[Tab/<-/->]"
            } else {
                "[Tab/←/→]"
            };
            spans.push(Span::styled(btn_hint, key_style));
            spans.push(Span::styled(" Button  ", text_style));
            spans.push(Span::styled("[Enter]", key_style));
            spans.push(Span::styled(" Confirm Delete  ", text_style));
            spans.push(Span::styled("[Esc]", key_style));
            spans.push(Span::styled(" Cancel  ", text_style));
        }
        ModalState::SoftRebaselineModal { .. } => {
            spans.push(Span::styled("[Tab]", key_style));
            spans.push(Span::styled(" Field  ", text_style));
            spans.push(Span::styled("[Space]", key_style));
            spans.push(Span::styled(" Toggle  ", text_style));
            spans.push(Span::styled("[Enter]", key_style));
            spans.push(Span::styled(" Submit  ", text_style));
            spans.push(Span::styled("[Esc]", key_style));
            spans.push(Span::styled(" Cancel  ", text_style));
        }
        ModalState::PurgeRawModal { .. } => {
            spans.push(Span::styled("[Enter]", key_style));
            spans.push(Span::styled(" Confirm Purge  ", text_style));
            spans.push(Span::styled("[Esc]", key_style));
            spans.push(Span::styled(" Cancel  ", text_style));
        }
        ModalState::HardWipeModal { .. } => {
            spans.push(Span::styled("[Enter]", key_style));
            spans.push(Span::styled(" Confirm Wipe  ", text_style));
            spans.push(Span::styled("[Esc]", key_style));
            spans.push(Span::styled(" Cancel  ", text_style));
        }
        ModalState::None => {
            spans.push(Span::styled("[1-4]", key_style));
            spans.push(Span::styled(" Tabs  ", text_style));

            match app.active_tab {
                Tab::Dashboard => {
                    spans.push(Span::styled("[d]", key_style));
                    spans.push(Span::styled(" Diagnostics  ", text_style));
                    spans.push(Span::styled("[p]", live_key_style));
                    spans.push(Span::styled(" Pause  ", live_text_style));
                    if app.daemon_connected && app.grace_state.is_some() {
                        spans.push(Span::styled("[c]", key_style));
                        spans.push(Span::styled(" Cancel  ", text_style));
                        spans.push(Span::styled("[s]", key_style));
                        spans.push(Span::styled(" Snooze  ", text_style));
                    }
                    if app.daemon_connected && app.has_persistent_shift_alert() {
                        spans.push(Span::styled("[a]", key_style));
                        spans.push(Span::styled(" Accept Shift  ", text_style));
                        spans.push(Span::styled("[i]", key_style));
                        spans.push(Span::styled(" Ignore Shift  ", text_style));
                    }
                }
                Tab::Wearables => {
                    spans.push(Span::styled("[e]", live_key_style));
                    spans.push(Span::styled(" Enroll  ", live_text_style));
                    spans.push(Span::styled("[n]", live_key_style));
                    spans.push(Span::styled(" Rename  ", live_text_style));
                    spans.push(Span::styled("[c]", live_key_style));
                    spans.push(Span::styled(" Reconnect  ", live_text_style));
                    spans.push(Span::styled("[u]", live_key_style));
                    spans.push(Span::styled(" Unenroll  ", live_text_style));
                }
                Tab::History => {
                    spans.push(Span::styled("[f]", live_key_style));
                    spans.push(Span::styled(" Feedback  ", live_text_style));
                    spans.push(Span::styled("[x]", live_key_style));
                    spans.push(Span::styled(" Export  ", live_text_style));
                    spans.push(Span::styled("[D]", live_key_style));
                    spans.push(Span::styled(" Delete  ", live_text_style));
                    spans.push(Span::styled("[j/k]", key_style));
                    spans.push(Span::styled(" Navigate  ", text_style));
                }
                Tab::Settings => {
                    spans.push(Span::styled("[j/k]", key_style));
                    spans.push(Span::styled(" Navigate  ", text_style));
                    spans.push(Span::styled("[Space]", live_key_style));
                    spans.push(Span::styled(" Toggle  ", live_text_style));
                    spans.push(Span::styled("[E]", key_style));
                    spans.push(Span::styled(" Editor  ", text_style));
                    spans.push(Span::styled("[B]", live_key_style));
                    spans.push(Span::styled(" Rebase  ", live_text_style));
                    spans.push(Span::styled("[P]", live_key_style));
                    spans.push(Span::styled(" Purge  ", live_text_style));
                    spans.push(Span::styled("[W]", live_key_style));
                    spans.push(Span::styled(" Wipe  ", live_text_style));
                    if app
                        .storage_stats
                        .as_ref()
                        .and_then(|s| s.active_calibration_boundary.as_ref())
                        .is_some()
                    {
                        spans.push(Span::styled("[U]", live_key_style));
                        spans.push(Span::styled(" Undo Boundary  ", live_text_style));
                    }
                }
            }

            spans.push(Span::styled("[?]", key_style));
            spans.push(Span::styled(" Help  ", text_style));
            spans.push(Span::styled("[q]", key_style));
            spans.push(Span::styled(" Quit", text_style));
        }
    }

    let line = Line::from(spans);
    buf.set_line(area.x, area.y, &line, area.width);
}
