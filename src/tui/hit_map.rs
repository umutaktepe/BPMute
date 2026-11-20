use ratatui::layout::{Position, Rect};

use super::app::Tab;

/// Interactive action triggered by a left mouse click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClickTarget {
    SelectTab(Tab),
    CancelGrace,
    SnoozeGrace,
    ToggleDiagnostics,
    OpenPausePopover,
    AcceptShift,
    IgnoreShift,
    WearablesEnroll,
    WearablesRename,
    WearablesReconnect,
    WearablesUnenroll,
    HistoryFeedback,
    HistoryExport,
    HistoryDelete,
    SettingsToggleSelected,
    SettingsEditorHandoff,
    SettingsOpenRebase,
    SettingsOpenPurge,
    SettingsOpenWipe,
    SettingsUndoBoundary,
    OpenHelp,
    Quit,
    CloseModal,
    ModalConfirm,
    ModalRescan,
    Consume,
}

/// Scrollable region targeted by mouse-wheel events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollTarget {
    WearablesTable,
    HelpModal,
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
    modal_clicks: Vec<HitRegion<ClickTarget>>,
    modal_scrolls: Vec<HitRegion<ScrollTarget>>,
}

impl HitMap {
    /// Clears all recorded regions and resets modal shadowing.
    pub(crate) fn clear(&mut self) {
        self.modal_active = false;
        self.clicks.clear();
        self.scrolls.clear();
        self.modal_clicks.clear();
        self.modal_scrolls.clear();
    }

    /// Marks whether a modal is active so background targets are shadowed.
    pub(crate) fn set_modal_active(&mut self, active: bool) {
        self.modal_active = active;
    }

    /// Records a clickable background target rectangle if non-empty.
    pub(crate) fn push_click(&mut self, rect: Rect, target: ClickTarget) {
        push_region(&mut self.clicks, rect, target);
    }

    /// Records a clickable modal-layer target rectangle if non-empty.
    pub(crate) fn push_modal_click(&mut self, rect: Rect, target: ClickTarget) {
        push_region(&mut self.modal_clicks, rect, target);
    }

    /// Records a scrollable background target rectangle if non-empty.
    pub(crate) fn push_scroll(&mut self, rect: Rect, target: ScrollTarget) {
        push_region(&mut self.scrolls, rect, target);
    }

    /// Records a scrollable modal-layer target rectangle if non-empty.
    pub(crate) fn push_modal_scroll(&mut self, rect: Rect, target: ScrollTarget) {
        push_region(&mut self.modal_scrolls, rect, target);
    }

    /// Resolves a left click at `(col, row)` (topmost drawn wins), shadowing background targets when a modal is active.
    pub(crate) fn click_at(&self, col: u16, row: u16) -> Option<ClickTarget> {
        self.resolve(&self.clicks, &self.modal_clicks, col, row)
    }

    /// Resolves a vertical wheel event at `(col, row)`, shadowing background targets when a modal is active.
    pub(crate) fn scroll_at(&self, col: u16, row: u16) -> Option<ScrollTarget> {
        self.resolve(&self.scrolls, &self.modal_scrolls, col, row)
    }

    fn resolve<T: Copy>(
        &self,
        background: &[HitRegion<T>],
        modal: &[HitRegion<T>],
        col: u16,
        row: u16,
    ) -> Option<T> {
        let regions = if self.modal_active { modal } else { background };
        region_at(regions, col, row)
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
