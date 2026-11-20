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
                }
                None
            }
            _ => None,
        }
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
