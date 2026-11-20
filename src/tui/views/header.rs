use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::tui::app::{GraceState, Tab, TuiApp};

const TABS: [(Tab, &str); 4] = [
    (Tab::Dashboard, " [1:Dashboard] "),
    (Tab::History, " [2:History] "),
    (Tab::Wearables, " [3:Wearables] "),
    (Tab::Settings, " [4:Settings] "),
];

/// Returns half-open `[start_col, end_col)` column hit ranges on row 0 matching `render_header`.
pub fn tab_hit_ranges() -> [(Tab, u16, u16); 4] {
    let mut x = 0u16;
    let mut out = [(Tab::Dashboard, 0, 0); 4];
    for (idx, (tab, label)) in TABS.iter().enumerate() {
        let len = label.len() as u16;
        out[idx] = (*tab, x, x + len);
        x += len + 1;
    }
    out
}

/// Computed text and exact clickable button column ranges for the global `Grace Period` banner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraceBannerLayout {
    pub text: String,
    pub cancel_cols: (u16, u16),
    pub snooze_cols: (u16, u16),
}

impl GraceBannerLayout {
    /// Builds banner text clipped cleanly for `width` while keeping `[c] Cancel` and `[s] Snooze` clickable.
    pub fn compute(grace: &GraceState, width: u16) -> Self {
        let dry_tag = if grace.dry_run { " [DRY]" } else { "" };
        let prefix = format!(" GRACE: {}s{} | ", grace.remaining_sec, dry_tag);
        let full_controls = "[c] Cancel (I'm Awake)  [s] Snooze ";
        let short_controls = "[c] Cancel  [s] Snooze ";

        let action_str = if grace.planned_actions.is_empty() {
            "None".to_string()
        } else {
            grace.planned_actions.join(", ")
        };

        let controls = if width >= 96 {
            full_controls
        } else {
            short_controls
        };

        let cancel_label = if controls == full_controls {
            "[c] Cancel (I'm Awake)"
        } else {
            "[c] Cancel"
        };
        let snooze_label = "[s] Snooze";

        let w = usize::from(width);
        let actions_full = format!("Actions: {action_str} | ");
        let text = if prefix.len() + actions_full.len() + controls.len() <= w {
            format!("{prefix}{actions_full}{controls}")
        } else {
            let ellipsis = if width > 0 { "..." } else { "" };
            let overhead =
                prefix.len() + controls.len() + "Actions: ".len() + ellipsis.len() + " | ".len();
            if w > overhead + 3 {
                let avail = w - overhead;
                let clipped: String = action_str.chars().take(avail).collect();
                format!("{prefix}Actions: {clipped}{ellipsis} | {controls}")
            } else {
                format!("{prefix}{controls}")
            }
        };

        let cancel_start = text.find(cancel_label).unwrap_or(0) as u16;
        let cancel_end = cancel_start + cancel_label.len() as u16;
        let snooze_start = text.find(snooze_label).unwrap_or(cancel_end as usize) as u16;
        let snooze_end = snooze_start + snooze_label.len() as u16;

        Self {
            text,
            cancel_cols: (cancel_start, cancel_end),
            snooze_cols: (snooze_start, snooze_end),
        }
    }
}

pub fn render_header(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let mut spans = Vec::new();
    for (t, label) in TABS {
        let is_active = app.active_tab == t;
        let style = app.theme.header_tab(is_active);
        spans.push(Span::styled(label, style));
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

    if !app.daemon_connected {
        render_offline_banner(app, area, buf);
        return;
    }

    if let Some(ref grace) = app.grace_state {
        render_grace_banner(grace, app, area, buf);
    }
}

fn render_grace_banner(grace: &GraceState, app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let layout = GraceBannerLayout::compute(grace, area.width);
    let style = app.theme.banner_grace();
    let p = Paragraph::new(layout.text).style(style);
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
