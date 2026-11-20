use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Widget};

use super::super::app::TuiApp;
use super::super::hit_map::{ClickTarget, HitMap, ScrollTarget};
use super::centered_rect;

pub(crate) fn render_help(app: &TuiApp, area: Rect, buf: &mut Buffer, hit_map: &mut HitMap) -> u16 {
    let modal_area = centered_rect(area, 68, 22);
    // Register backdrop close + screen-wide wheel scroll before modal body absorber
    hit_map.push_modal_click(area, ClickTarget::CloseModal);
    hit_map.push_modal_scroll(area, ScrollTarget::HelpModal);
    hit_map.push_modal_click(modal_area, ClickTarget::Consume);

    Clear.render(modal_area, buf);

    let block = app
        .theme
        .card_block(" BPMute Keyboard & Mouse Shortcuts [?] ")
        .border_style(app.theme.primary());

    let inner = block.inner(modal_area);
    block.render(modal_area, buf);

    if inner.width == 0 || inner.height == 0 {
        return 0;
    }

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

    let wrapped = wrap_lines(lines, inner.width as usize);
    let total_lines = u16::try_from(wrapped.len()).unwrap_or(u16::MAX);
    let max_scroll = total_lines.saturating_sub(inner.height);
    let scroll_y = app.help_scroll_offset.min(max_scroll);
    Paragraph::new(wrapped)
        .scroll((scroll_y, 0))
        .render(inner, buf);
    max_scroll
}

fn wrap_lines(lines: Vec<Line<'static>>, max_width: usize) -> Vec<Line<'static>> {
    if max_width == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        if line.width() <= max_width {
            out.push(line);
            continue;
        }
        let mut current_spans: Vec<Span<'static>> = Vec::new();
        let mut current_w = 0usize;

        for span in line.spans {
            let style = span.style;
            let text = span.content;
            if current_w + Span::raw(text.as_ref()).width() <= max_width {
                current_w += Span::raw(text.as_ref()).width();
                current_spans.push(Span::styled(text, style));
                continue;
            }
            for token in split_preserving_spaces(text.as_ref()) {
                let token_w = Span::raw(token).width();
                if current_w > 0 && current_w + token_w > max_width {
                    out.push(Line::from(std::mem::take(&mut current_spans)));
                    current_w = 0;
                    if token.trim().is_empty() {
                        continue;
                    }
                }
                current_w += token_w;
                current_spans.push(Span::styled(token.to_string(), style));
            }
        }
        if !current_spans.is_empty() {
            out.push(Line::from(current_spans));
        }
    }
    out
}

fn split_preserving_spaces(s: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut start = 0;
    let mut in_space = None;
    for (idx, ch) in s.char_indices() {
        let is_sp = ch == ' ';
        if let Some(prev) = in_space
            && prev != is_sp
        {
            tokens.push(&s[start..idx]);
            start = idx;
        }
        in_space = Some(is_sp);
    }
    if start < s.len() {
        tokens.push(&s[start..]);
    }
    tokens
}
