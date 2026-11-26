use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::hit_map::{ClickTarget, HitMap};
use crate::ipc::protocol::EnrolledWearableStatus;

pub mod dashboard;
pub mod footer;
pub mod header;
pub mod help;
pub mod history;
mod history_chart;
mod history_modals;
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

/// Adjusts a list/table viewport scroll offset so `selected_idx` stays visible.
pub(crate) fn fit_scroll_offset(
    current_offset: usize,
    selected_idx: usize,
    total_items: usize,
    visible_rows: usize,
) -> usize {
    if visible_rows == 0 || total_items == 0 {
        return 0;
    }
    let clamped_selected = selected_idx.min(total_items - 1);
    let max_scroll = total_items.saturating_sub(visible_rows);
    let mut offset = current_offset.min(max_scroll);
    if clamped_selected < offset {
        offset = clamped_selected;
    } else if clamped_selected >= offset + visible_rows {
        offset = clamped_selected + 1 - visible_rows;
    }
    offset.min(max_scroll)
}

/// Builds a single-row `Line` while registering clickable spans into `hit_map`.
pub(crate) fn build_clickable_row_line<'a>(
    bounds: Rect,
    y: u16,
    items: Vec<(Span<'a>, Option<ClickTarget>)>,
    hit_map: &mut HitMap,
) -> Line<'a> {
    build_clickable_line(bounds, y, items, hit_map, false)
}

/// Builds a single-row `Line` while registering clickable modal spans into `hit_map`.
pub(crate) fn build_modal_clickable_row_line<'a>(
    bounds: Rect,
    y: u16,
    items: Vec<(Span<'a>, Option<ClickTarget>)>,
    hit_map: &mut HitMap,
) -> Line<'a> {
    build_clickable_line(bounds, y, items, hit_map, true)
}

fn build_clickable_line<'a>(
    bounds: Rect,
    y: u16,
    items: Vec<(Span<'a>, Option<ClickTarget>)>,
    hit_map: &mut HitMap,
    modal: bool,
) -> Line<'a> {
    let mut x = bounds.x;
    let mut spans = Vec::with_capacity(items.len());
    for (span, target) in items {
        let w = span.width() as u16;
        if let Some(click_target) = target
            && bounds.height > 0
        {
            if modal {
                hit_map.push_clipped_modal_row_click(bounds, x, y, w, click_target);
            } else {
                hit_map.push_clipped_row_click(bounds, x, y, w, click_target);
            }
        }
        x = x.saturating_add(w);
        spans.push(span);
    }
    Line::from(spans)
}

/// Formats a wearable battery percentage when the daemon and wearable are connected, otherwise `"--"`.
pub(crate) fn format_wearable_battery(
    daemon_connected: bool,
    wearable: Option<&EnrolledWearableStatus>,
) -> String {
    wearable
        .filter(|w| daemon_connected && w.status.is_connected())
        .and_then(|w| w.battery)
        .map(|b| format!("{b}%"))
        .unwrap_or_else(|| "--".to_string())
}
