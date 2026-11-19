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
    let text_style = app.theme.base();

    // Check if modal is active
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
            spans.push(Span::styled("[j/k/↑/↓]", key_style));
            spans.push(Span::styled(" Select Wearable  ", text_style));
            spans.push(Span::styled("[Enter]", key_style));
            spans.push(Span::styled(" Enroll  ", text_style));
            spans.push(Span::styled("[Tab]", key_style));
            spans.push(Span::styled(" Toggle Alias Input  ", text_style));
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
            spans.push(Span::styled("[1/2/Space]", key_style));
            spans.push(Span::styled(" Toggle Checkbox  ", text_style));
            spans.push(Span::styled("[Tab/←/→]", key_style));
            spans.push(Span::styled(" Select Button  ", text_style));
            spans.push(Span::styled("[Enter]", key_style));
            spans.push(Span::styled(" Confirm  ", text_style));
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
                    spans.push(Span::styled("[p]", key_style));
                    spans.push(Span::styled(" Pause  ", text_style));
                    if app.grace_state.is_some() {
                        spans.push(Span::styled("[c]", key_style));
                        spans.push(Span::styled(" Cancel  ", text_style));
                        spans.push(Span::styled("[s]", key_style));
                        spans.push(Span::styled(" Snooze  ", text_style));
                    }
                    if app.has_persistent_shift_alert() {
                        spans.push(Span::styled("[a]", key_style));
                        spans.push(Span::styled(" Accept Shift  ", text_style));
                        spans.push(Span::styled("[i]", key_style));
                        spans.push(Span::styled(" Ignore Shift  ", text_style));
                    }
                }
                Tab::Wearables => {
                    spans.push(Span::styled("[e]", key_style));
                    spans.push(Span::styled(" Enroll  ", text_style));
                    spans.push(Span::styled("[n]", key_style));
                    spans.push(Span::styled(" Rename  ", text_style));
                    spans.push(Span::styled("[c]", key_style));
                    spans.push(Span::styled(" Reconnect  ", text_style));
                    spans.push(Span::styled("[u]", key_style));
                    spans.push(Span::styled(" Unenroll  ", text_style));
                }
                Tab::History => {
                    spans.push(Span::styled("[j/k]", key_style));
                    spans.push(Span::styled(" Navigate  ", text_style));
                }
                Tab::Settings => {
                    spans.push(Span::styled("[j/k]", key_style));
                    spans.push(Span::styled(" Navigate  ", text_style));
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
