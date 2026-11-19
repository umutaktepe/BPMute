use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::tui::app::{GraceState, Tab, TuiApp};

pub fn render_header(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    // Top line: tabs + version
    let mut spans = Vec::new();
    let tabs = [
        (Tab::Dashboard, "1:Dashboard"),
        (Tab::History, "2:History"),
        (Tab::Wearables, "3:Wearables"),
        (Tab::Settings, "4:Settings"),
    ];

    for (t, label) in tabs {
        let is_active = app.active_tab == t;
        let style = app.theme.header_tab(is_active);
        spans.push(Span::styled(format!(" [{label}] "), style));
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

pub fn render_global_banner(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    // Check if Grace Period is active (highest priority global banner)
    if let Some(ref grace) = app.grace_state {
        render_grace_banner(grace, app, area, buf);
        return;
    }

    // Check if daemon is offline
    if !app.daemon_connected {
        render_offline_banner(app, area, buf);
    }
}

fn render_grace_banner(grace: &GraceState, app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let action_str = if grace.planned_actions.is_empty() {
        "None".to_string()
    } else {
        grace.planned_actions.join(", ")
    };

    let dry_tag = if grace.dry_run { " [DRY]" } else { "" };
    let banner_text = format!(
        " GRACE: {}s left{} | Actions: {} | [c] Cancel  [s] Snooze ",
        grace.remaining_sec, dry_tag, action_str
    );

    let style = app.theme.banner_grace();
    let p = Paragraph::new(banner_text).style(style);
    p.render(area, buf);
}

fn render_offline_banner(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let banner_text = " Daemon offline — retrying… [Hint: systemctl --user start bpmute] ";
    let style = app.theme.banner_offline();
    let p = Paragraph::new(banner_text).style(style);
    p.render(area, buf);
}
