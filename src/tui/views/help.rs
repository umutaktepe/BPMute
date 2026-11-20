use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Widget};

use super::super::app::TuiApp;
use super::centered_rect;

pub fn render_help(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let modal_area = centered_rect(area, 68, 22);
    Clear.render(modal_area, buf);

    let block = app
        .theme
        .card_block(" BPMute Keyboard & Mouse Shortcuts [?] ")
        .border_style(app.theme.primary());

    let inner = block.inner(modal_area);
    block.render(modal_area, buf);

    let k = |s: &'static str| Span::styled(s, app.theme.key_hint());
    let t = |s: &'static str| Span::styled(s, app.theme.base());
    let h = |s: &'static str| Span::styled(s, app.theme.primary());

    let tab_range = if app.theme.ascii { "1-4" } else { "1–4" };
    let nav_lr = if app.theme.ascii {
        "h / l, <-/-> "
    } else {
        "h / l, ← / → "
    };
    let nav_ud = if app.theme.ascii {
        "j / k, Up/Dn "
    } else {
        "j / k, ↑ / ↓ "
    };

    let lines = vec![
        Line::from(vec![h(" Global Navigation:")]),
        Line::from(vec![
            t("   "),
            k(tab_range),
            t("          Switch tabs (Dashboard, History, Wearables, Settings)"),
        ]),
        Line::from(vec![
            t("   "),
            k("Tab/Shift+Tab"),
            t(" Next / Previous tab"),
        ]),
        Line::from(vec![t("   "), k(nav_lr), t("Next / Previous tab")]),
        Line::from(vec![
            t("   "),
            k(nav_ud),
            t("Select list items / scroll tables"),
        ]),
        Line::from(vec![
            t("   "),
            k("Esc          "),
            t("Close open modal / popover"),
        ]),
        Line::from(vec![
            t("   "),
            k("q            "),
            t("Quit TUI (leaves daemon running in background)"),
        ]),
        Line::from(""),
        Line::from(vec![h(" Dashboard:")]),
        Line::from(vec![
            t("   "),
            k("d            "),
            t("Toggle Engine Diagnostics pane"),
        ]),
        Line::from(vec![
            t("   "),
            k("p            "),
            t("Open Pause / Arming popover"),
        ]),
        Line::from(vec![
            t("   "),
            k("a / i        "),
            t("Accept / Ignore 5-session persistent shift prompt"),
        ]),
        Line::from(vec![
            t("   "),
            k("x            "),
            t("Dismiss top dismissible alert"),
        ]),
        Line::from(vec![
            t("   "),
            k("c / s        "),
            t("Cancel (I'm Awake) / Snooze during active Grace Period"),
        ]),
        Line::from(""),
        Line::from(vec![h(" Wearables Tab:")]),
        Line::from(vec![
            t("   "),
            k("e            "),
            t("Enroll new wearable (30s scan & Agent1 bonding modal)"),
        ]),
        Line::from(vec![
            t("   "),
            k("n            "),
            t("Rename alias of selected wearable"),
        ]),
        Line::from(vec![
            t("   "),
            k("c            "),
            t("Manual reconnect to selected wearable"),
        ]),
        Line::from(vec![
            t("   "),
            k("u            "),
            t("Unenroll selected wearable (with opt-in checkboxes)"),
        ]),
        Line::from(""),
        Line::from(vec![t(
            " Mouse: Click tabs/buttons/rows or scroll with mouse wheel.",
        )]),
    ];

    Paragraph::new(lines).render(inner, buf);
}
