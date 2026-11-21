use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::tui::app::{GraceState, OpFeedback, Tab, TuiApp};
use crate::tui::hit_map::{ClickTarget, HitMap};

const TABS: [(Tab, &str); 4] = [
    (Tab::Dashboard, " [1:Dashboard] "),
    (Tab::History, " [2:History] "),
    (Tab::Wearables, " [3:Wearables] "),
    (Tab::Settings, " [4:Settings] "),
];

fn push_row_click(hit_map: &mut HitMap, area: Rect, x: u16, width: u16, target: ClickTarget) {
    if x < area.right() {
        let visible_w = width.min(area.right().saturating_sub(x));
        hit_map.push_click(Rect::new(x, area.y, visible_w, 1), target);
    }
}

pub(crate) fn render_header(app: &TuiApp, area: Rect, buf: &mut Buffer, hit_map: &mut HitMap) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let mut spans = Vec::new();
    let mut x = area.x;
    for (t, label) in TABS {
        let is_active = app.active_tab == t;
        let style = app.theme.header_tab(is_active);
        let span = Span::styled(label, style);
        let label_w = span.width() as u16;
        push_row_click(hit_map, area, x, label_w, ClickTarget::SelectTab(t));
        x = x.saturating_add(label_w).saturating_add(1);
        spans.push(span);
        spans.push(Span::raw(" "));
    }

    let version_str = format!("BPMute v{} ", env!("CARGO_PKG_VERSION"));
    let header_line = Line::from(spans);
    buf.set_line(area.x, area.y, &header_line, area.width);

    let ver_style = app.theme.muted();
    if area.width > version_str.len() as u16 + 40 {
        let ver_x = area.right().saturating_sub(version_str.len() as u16);
        buf.set_string(ver_x, area.y, &version_str, ver_style);
    }
}

pub(crate) fn render_global_banner(
    app: &TuiApp,
    area: Rect,
    buf: &mut Buffer,
    hit_map: &mut HitMap,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    if !app.daemon_connected {
        render_offline_banner(app, area, buf);
        return;
    }

    if let Some(ref grace) = app.grace_state {
        render_grace_banner(grace, app, area, buf, hit_map);
    } else if let Some(ref fb) = app.op_feedback {
        render_op_feedback_banner(fb, app, area, buf);
    }
}

fn render_op_feedback_banner(fb: &OpFeedback, app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let prefix = if fb.is_error { " Error: " } else { " " };
    let max_len = usize::from(area.width).saturating_sub(prefix.len() + 1);
    let msg: String = fb.message.chars().take(max_len).collect();
    let text = format!("{prefix}{msg} ");
    let p = Paragraph::new(text).style(app.theme.banner_op_feedback(fb.is_error));
    p.render(area, buf);
}

fn render_grace_banner(
    grace: &GraceState,
    app: &TuiApp,
    area: Rect,
    buf: &mut Buffer,
    hit_map: &mut HitMap,
) {
    let dry_tag = if grace.dry_run { " [DRY]" } else { "" };
    let phase_label = match grace.phase {
        crate::actions::GracePhase::Countdown => "GRACE",
        crate::actions::GracePhase::ActivityHold => "ACTIVITY HOLD",
    };
    let prefix = format!(" {phase_label}: {}s{} | ", grace.remaining_sec, dry_tag);
    let full_controls = "[c] Cancel (I'm Awake)  [s] Snooze ";
    let short_controls = "[c] Cancel  [s] Snooze ";

    let action_str = if grace.planned_actions.is_empty() {
        "None".to_string()
    } else {
        grace.planned_actions.join(", ")
    };

    let (controls, cancel_label) = if area.width >= 96 {
        (full_controls, "[c] Cancel (I'm Awake)")
    } else {
        (short_controls, "[c] Cancel")
    };
    let snooze_label = "[s] Snooze";

    let w = usize::from(area.width);
    let actions_full = format!("Actions: {action_str} | ");
    let leading = if prefix.len() + actions_full.len() + controls.len() <= w {
        format!("{prefix}{actions_full}")
    } else {
        let ellipsis = if area.width > 0 { "..." } else { "" };
        let overhead =
            prefix.len() + controls.len() + "Actions: ".len() + ellipsis.len() + " | ".len();
        if w > overhead + 3 {
            let avail = w - overhead;
            let clipped: String = action_str.chars().take(avail).collect();
            format!("{prefix}Actions: {clipped}{ellipsis} | ")
        } else {
            prefix
        }
    };

    let leading_span = Span::raw(leading);
    let cancel_span = Span::raw(cancel_label);
    let gap_span = Span::raw("  ");
    let snooze_span = Span::raw(snooze_label);
    let trailing_span = Span::raw(" ");

    let cancel_x = area.x.saturating_add(leading_span.width() as u16);
    let cancel_w = cancel_span.width() as u16;
    let snooze_x = cancel_x
        .saturating_add(cancel_w)
        .saturating_add(gap_span.width() as u16);
    let snooze_w = snooze_span.width() as u16;

    push_row_click(hit_map, area, cancel_x, cancel_w, ClickTarget::CancelGrace);
    push_row_click(hit_map, area, snooze_x, snooze_w, ClickTarget::SnoozeGrace);

    let line = Line::from(vec![
        leading_span,
        cancel_span,
        gap_span,
        snooze_span,
        trailing_span,
    ]);
    let style = app.theme.banner_grace();
    let p = Paragraph::new(line).style(style);
    p.render(area, buf);
}

fn render_offline_banner(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let banner_text = if app.theme.ascii {
        " Daemon offline - retrying... [Hint: systemctl --user start bpmute] "
    } else {
        " Daemon offline — retrying… [Hint: systemctl --user start bpmute] "
    };
    let style = app.theme.banner_offline();
    let p = Paragraph::new(banner_text).style(style);
    p.render(area, buf);
}
