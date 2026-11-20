use ratatui::layout::{Position, Rect};

use super::app::Tab;

/// Interactive action triggered by a left mouse click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClickTarget {
    SelectTab(Tab),
    CancelGrace,
    SnoozeGrace,
}

/// Scrollable region targeted by mouse-wheel events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollTarget {
    WearablesTable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HitRegion<T> {
    rect: Rect,
    target: T,
}

/// Render-emitted hit-testing map built on every `TuiApp::render` pass (ADR-0010).
#[derive(Debug, Default, Clone)]
pub(crate) struct HitMap {
    modal_active: bool,
    clicks: Vec<HitRegion<ClickTarget>>,
    scrolls: Vec<HitRegion<ScrollTarget>>,
}

impl HitMap {
    /// Clears all recorded regions and resets modal shadowing.
    pub(crate) fn clear(&mut self) {
        self.modal_active = false;
        self.clicks.clear();
        self.scrolls.clear();
    }

    /// Marks whether a modal is active so background targets are shadowed.
    pub(crate) fn set_modal_active(&mut self, active: bool) {
        self.modal_active = active;
    }

    /// Records a clickable target rectangle if non-empty.
    pub(crate) fn push_click(&mut self, rect: Rect, target: ClickTarget) {
        push_region(&mut self.clicks, rect, target);
    }

    /// Records a scrollable target rectangle if non-empty.
    pub(crate) fn push_scroll(&mut self, rect: Rect, target: ScrollTarget) {
        push_region(&mut self.scrolls, rect, target);
    }

    /// Resolves a left click at `(col, row)` (topmost drawn wins), returning `None` when shadowed by a modal.
    pub(crate) fn click_at(&self, col: u16, row: u16) -> Option<ClickTarget> {
        if self.modal_active {
            return None;
        }
        region_at(&self.clicks, col, row)
    }

    /// Resolves a vertical wheel event at `(col, row)`, returning `None` when shadowed by a modal.
    pub(crate) fn scroll_at(&self, col: u16, row: u16) -> Option<ScrollTarget> {
        if self.modal_active {
            return None;
        }
        region_at(&self.scrolls, col, row)
    }
}

fn push_region<T>(regions: &mut Vec<HitRegion<T>>, rect: Rect, target: T) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    regions.push(HitRegion { rect, target });
}

fn region_at<T: Copy>(regions: &[HitRegion<T>], col: u16, row: u16) -> Option<T> {
    let pos = Position::new(col, row);
    regions
        .iter()
        .rev()
        .find(|region| region.rect.contains(pos))
        .map(|region| region.target)
}
