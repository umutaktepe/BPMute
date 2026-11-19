use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};

use super::super::app::TuiApp;

pub fn render_help(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let modal_w = 68u16.min(area.width.saturating_sub(2));
    let modal_h = 22u16.min(area.height.saturating_sub(2));

    let modal_x = area.x + (area.width.saturating_sub(modal_w)) / 2;
    let modal_y = area.y + (area.height.saturating_sub(modal_h)) / 2;
    let modal_area = Rect::new(modal_x, modal_y, modal_w, modal_h);

    Clear.render(modal_area, buf);

    let block = Block::default()
        .title(" BPMute Keyboard & Mouse Shortcuts [?] ")
        .title_style(app.theme.card_title())
        .borders(Borders::ALL)
        .border_style(app.theme.primary());

    let inner = block.inner(modal_area);
    block.render(modal_area, buf);

    let k = |s: &'static str| Span::styled(s, app.theme.key_hint());
    let t = |s: &'static str| Span::styled(s, app.theme.base());
    let h = |s: &'static str| Span::styled(s, app.theme.primary());

    let lines = vec![
        Line::from(vec![h(" Global Navigation:")]),
        Line::from(vec![
            t("   "),
            k("1–4          "),
            t("Switch tabs (Dashboard, History, Wearables, Settings)"),
        ]),
        Line::from(vec![
            t("   "),
            k("Tab/Shift+Tab"),
            t(" Next / Previous tab"),
        ]),
        Line::from(vec![t("   "), k("h / l, ← / → "), t("Next / Previous tab")]),
        Line::from(vec![
            t("   "),
            k("j / k, ↑ / ↓ "),
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
            t("Dismiss top alert banner"),
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
