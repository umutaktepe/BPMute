---
status: accepted
---

# Render-Emitted HitMap as the Single Source of Truth for TUI Mouse Input

All TUI mouse hit-testing across views, header, global banner, footer, and modals uses a render-emitted `HitMap` populated during `TuiApp::render` instead of separate hand-maintained layout calculators or hardcoded row/column offsets. Each rendered interactive target (tabs, banner controls, buttons, checkboxes, cyclic selectors, text-input boxes, list/table rows, scrollable regions, and modal bounds) records its exact rendered `Rect` and target action into the `HitMap`, and `TuiApp::handle_mouse` resolves click and scroll events exclusively against that `HitMap`.

## Considered Options

- **Separate pure layout math per view (`header.rs` `tab_hit_ranges()` / `GraceBannerLayout` style):** Keeps `TuiApp::render(&self, ...)` immutable, but forces every view, responsive breakpoint, and centered modal to duplicate Ratatui `Layout` and `centered_rect` calculations in two places, making coordinate drift and silent mouse regressions inevitable whenever UI copy or constraints change.
- **Render-emitted `HitMap` (chosen):** Couples hit-region capture directly to the actual Ratatui render pass so resize breakpoints, dynamic labels, scroll offsets, and modal layering (`Clear` + centered boxes + footer actions) are always aligned with what is on screen.

## Consequences

- `TuiApp::render` updates the active `HitMap` on every draw (replacing the standalone `tab_hit_ranges()` and `GraceBannerLayout` split), and `render_footer` / modal renderers register layered regions so active modals shadow background widgets.
- Unrendered states (such as terminal sizes below the `60x18` resize threshold) produce an empty interactive `HitMap`, naturally suppressing stale background clicks.
- E2E tests in `tests/tui_e2e.rs` must draw (`terminal.draw(...)` / `app.render(...)`) before dispatching synthetic mouse events at rendered coordinates, matching real terminal execution.
