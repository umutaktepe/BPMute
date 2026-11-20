use ratatui::layout::Rect;

pub mod dashboard;
pub mod footer;
pub mod header;
pub mod help;
pub mod history;
pub mod settings;
pub mod wearables;

/// Computes a centered rectangle inside `area` clamped to fit with a small margin.
pub fn centered_rect(area: Rect, max_w: u16, max_h: u16) -> Rect {
    let w = max_w.min(area.width.saturating_sub(4)).max(1);
    let h = max_h.min(area.height.saturating_sub(2)).max(1);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}
