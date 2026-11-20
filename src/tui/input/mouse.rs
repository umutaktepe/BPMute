use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

use super::super::app::{ModalState, Tab, TuiApp};
use super::super::event::TuiCommand;
use super::super::views::header::{GraceBannerLayout, tab_hit_ranges};

impl TuiApp {
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
