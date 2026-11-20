use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

/// Minimum plausible heart-rate reading in bpm; values at or below this are treated as invalid.
pub const MIN_VALID_HR_BPM: f64 = 20.0;

/// Single validated point on a continuous trace segment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TracePoint {
    pub ts_ms: i64,
    pub value: f64,
}

/// Raw timestamped reading before gap and validity segmentation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TraceSample {
    pub ts_ms: i64,
    pub value: Option<f64>,
}

/// Series configuration passed to the shared time-series trace renderer.
#[derive(Debug, Clone)]
pub struct TraceSeriesSpec<'a> {
    pub segments: &'a [Vec<TracePoint>],
    pub y_min: f64,
    pub y_max: f64,
    pub style: Style,
    pub ascii_point_char: char,
}

#[derive(Debug, Clone, Copy)]
struct TimeRange {
    start_ms: i64,
    end_ms: i64,
}

impl TimeRange {
    fn norm(self, ts_ms: i64) -> f64 {
        if self.end_ms > self.start_ms {
            ((ts_ms - self.start_ms) as f64 / (self.end_ms - self.start_ms) as f64).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
}

/// Returns `true` when `bpm` is finite and above the invalid-signal floor (`> 20.0 bpm`).
pub fn is_valid_hr_bpm(bpm: f64) -> bool {
    bpm.is_finite() && bpm > MIN_VALID_HR_BPM
}

/// Maps `ts_ms` in `[t_start_ms, t_end_ms]` to a 0-based column index within `chart_w`,
/// matching the trace renderer's horizontal coordinate system for both Braille and ASCII modes.
pub fn time_to_column(
    ts_ms: i64,
    t_start_ms: i64,
    t_end_ms: i64,
    chart_w: u16,
    ascii: bool,
) -> u16 {
    if chart_w <= 1 {
        return 0;
    }
    let t_norm = TimeRange {
        start_ms: t_start_ms,
        end_ms: t_end_ms,
    }
    .norm(ts_ms);
    if ascii {
        (t_norm * (f64::from(chart_w - 1))).round() as u16
    } else {
        let px_w = usize::from(chart_w) * 2;
        let px = (t_norm * ((px_w - 1) as f64)).round() as usize;
        (px / 2).min(usize::from(chart_w - 1)) as u16
    }
}

const fn braille_bit(dx: usize, dy: usize) -> u8 {
    match (dx, dy) {
        (0, 0) => 0x01,
        (0, 1) => 0x02,
        (0, 2) => 0x04,
        (0, 3) => 0x40,
        (1, 0) => 0x08,
        (1, 1) => 0x10,
        (1, 2) => 0x20,
        (1, 3) => 0x80,
        _ => 0,
    }
}

/// Splits timestamped readings into continuous segments, breaking on missing/invalid
/// values or whenever consecutive valid readings exceed `max_gap_ms` (or go backwards).
pub fn build_segments(samples: &[TraceSample], max_gap_ms: i64) -> Vec<Vec<TracePoint>> {
    let mut segments: Vec<Vec<TracePoint>> = Vec::new();
    let mut current: Vec<TracePoint> = Vec::new();

    for sample in samples {
        let Some(val) = sample.value.filter(|v| v.is_finite()) else {
            if !current.is_empty() {
                segments.push(std::mem::take(&mut current));
            }
            continue;
        };

        if let Some(prev) = current.last() {
            let dt = sample.ts_ms.saturating_sub(prev.ts_ms);
            if sample.ts_ms < prev.ts_ms || dt > max_gap_ms {
                segments.push(std::mem::take(&mut current));
            }
        }

        current.push(TracePoint {
            ts_ms: sample.ts_ms,
            value: val,
        });
    }

    if !current.is_empty() {
        segments.push(current);
    }

    segments
}

/// Clips continuous segments to `[t_start_ms, t_end_ms]`, interpolating boundary crossings.
pub fn clip_segments(
    segments: &[Vec<TracePoint>],
    t_start_ms: i64,
    t_end_ms: i64,
) -> Vec<Vec<TracePoint>> {
    let mut clipped_all = Vec::new();

    if t_end_ms <= t_start_ms {
        for seg in segments {
            let kept: Vec<TracePoint> = seg
                .iter()
                .copied()
                .filter(|p| p.ts_ms == t_start_ms)
                .collect();
            if !kept.is_empty() {
                clipped_all.push(kept);
            }
        }
        return clipped_all;
    }

    for seg in segments {
        if seg.is_empty() {
            continue;
        }
        if seg.len() == 1 {
            let p = seg[0];
            if p.ts_ms >= t_start_ms && p.ts_ms <= t_end_ms {
                clipped_all.push(vec![p]);
            }
            continue;
        }

        let mut current: Vec<TracePoint> = Vec::new();
        for pair in seg.windows(2) {
            let p0 = pair[0];
            let p1 = pair[1];

            if (p0.ts_ms < t_start_ms && p1.ts_ms < t_start_ms)
                || (p0.ts_ms > t_end_ms && p1.ts_ms > t_end_ms)
            {
                if !current.is_empty() {
                    clipped_all.push(std::mem::take(&mut current));
                }
                continue;
            }

            let c0 = if p0.ts_ms < t_start_ms {
                interpolate_at(p0, p1, t_start_ms)
            } else {
                p0
            };
            let c1 = if p1.ts_ms > t_end_ms {
                interpolate_at(p0, p1, t_end_ms)
            } else {
                p1
            };

            if current.is_empty() {
                current.push(c0);
            } else if current.last().is_some_and(|last| last.ts_ms != c0.ts_ms) {
                clipped_all.push(std::mem::take(&mut current));
                current.push(c0);
            }
            current.push(c1);
        }

        if !current.is_empty() {
            clipped_all.push(current);
        }
    }

    clipped_all
}

fn interpolate_at(p0: TracePoint, p1: TracePoint, target_ts: i64) -> TracePoint {
    if p1.ts_ms <= p0.ts_ms {
        return p0;
    }
    let alpha = (target_ts - p0.ts_ms) as f64 / (p1.ts_ms - p0.ts_ms) as f64;
    TracePoint {
        ts_ms: target_ts,
        value: p0.value + alpha * (p1.value - p0.value),
    }
}

/// Computes dynamic HR Y-axis bounds from valid plotted points and active baselines.
///
/// Pads observed min/max by `±2.0 bpm`, enforces a minimum span of `12.0 bpm` centered on the
/// midpoint, and falls back to `50.0..100.0 bpm` when neither data nor baselines are present.
pub fn compute_hr_y_bounds(values: impl Iterator<Item = f64>, refs: &[Option<f64>]) -> (f64, f64) {
    let mut min_val = f64::INFINITY;
    let mut max_val = f64::NEG_INFINITY;
    let mut has_any = false;

    for v in values {
        if v.is_finite() {
            min_val = min_val.min(v);
            max_val = max_val.max(v);
            has_any = true;
        }
    }

    for r in refs.iter().flatten() {
        if r.is_finite() && *r > 0.0 {
            min_val = min_val.min(*r);
            max_val = max_val.max(*r);
            has_any = true;
        }
    }

    if !has_any {
        return (50.0, 100.0);
    }

    let padded_min = min_val - 2.0;
    let padded_max = max_val + 2.0;
    if padded_max - padded_min < 12.0 {
        let mid = (padded_min + padded_max) * 0.5;
        (mid - 6.0, mid + 6.0)
    } else {
        (padded_min, padded_max)
    }
}

/// Renders top and bottom HR bpm Y-axis labels (`{:>3.0}bpm`).
pub fn render_hr_y_labels(
    x: u16,
    y: u16,
    height: u16,
    min_bpm: f64,
    max_bpm: f64,
    style: Style,
    buf: &mut Buffer,
) {
    if height == 0 {
        return;
    }
    buf.set_string(x, y, format!("{max_bpm:>3.0}bpm"), style);
    buf.set_string(
        x,
        y + height.saturating_sub(1),
        format!("{min_bpm:>3.0}bpm"),
        style,
    );
}

/// Renders dashed horizontal baseline reference lines underneath data curves.
pub fn render_horizontal_ref_lines(
    plot_area: Rect,
    buf: &mut Buffer,
    y_min: f64,
    y_max: f64,
    refs: &[(Option<f64>, Style)],
    ascii: bool,
) {
    if plot_area.width == 0 || plot_area.height == 0 {
        return;
    }
    let range = (y_max - y_min).max(1.0);
    let ref_char = if ascii { '-' } else { '┄' };

    for &(val_opt, style) in refs {
        let Some(val) = val_opt.filter(|v| v.is_finite() && *v > 0.0) else {
            continue;
        };
        let norm = ((val - y_min) / range).clamp(0.0, 1.0);
        let row_offset =
            ((1.0 - norm) * (plot_area.height.saturating_sub(1) as f64)).round() as u16;
        let row_y = plot_area.y + row_offset;
        if row_y < plot_area.bottom() {
            for col in 0..plot_area.width {
                if let Some(c) = buf.cell_mut((plot_area.x + col, row_y)) {
                    c.set_char(ref_char).set_style(style);
                }
            }
        }
    }
}

/// Renders one or more clipped time-series traces into `area` using either `2×4` Unicode Braille
/// or continuous 7-bit ASCII strokes.
///
/// `series` should be ordered from lowest priority to highest priority (e.g. `[P(Sleep), HR]`):
/// - In Braille mode, dot masks are bitwise-ORed and higher-priority series set the foreground style.
/// - In ASCII mode, higher-priority series overwrite lower-priority glyphs on cell collisions.
pub fn render_trace_series(
    area: Rect,
    buf: &mut Buffer,
    t_start_ms: i64,
    t_end_ms: i64,
    series: &[TraceSeriesSpec<'_>],
    ascii: bool,
) {
    let width = usize::from(area.width);
    let height = usize::from(area.height);
    if width == 0 || height == 0 {
        return;
    }

    let time = TimeRange {
        start_ms: t_start_ms,
        end_ms: t_end_ms,
    };

    if ascii {
        render_ascii_series(area, buf, time, series);
    } else {
        render_braille_series(area, buf, time, series);
    }
}

fn map_point(
    point: TracePoint,
    time: TimeRange,
    y_min: f64,
    y_max: f64,
    grid_w: usize,
    grid_h: usize,
) -> (i32, i32) {
    let t_norm = time.norm(point.ts_ms);
    let y_span = y_max - y_min;
    let v_norm = if y_span > 0.0 {
        ((point.value - y_min) / y_span).clamp(0.0, 1.0)
    } else {
        0.5
    };
    let x = (t_norm * (grid_w.saturating_sub(1) as f64)).round() as i32;
    let y = ((1.0 - v_norm) * (grid_h.saturating_sub(1) as f64)).round() as i32;
    (x, y)
}

fn rasterize_segments(
    time: TimeRange,
    series: &[TraceSeriesSpec<'_>],
    grid_w: usize,
    grid_h: usize,
    mut plot_segment_step: impl FnMut(&TraceSeriesSpec<'_>, (i32, i32), Option<(i32, i32)>),
) {
    for spec in series {
        for seg in spec.segments {
            if seg.is_empty() {
                continue;
            }
            let first = map_point(seg[0], time, spec.y_min, spec.y_max, grid_w, grid_h);
            if seg.len() == 1 {
                plot_segment_step(spec, first, None);
                continue;
            }

            let mut prev = first;
            for &pt in &seg[1..] {
                let next = map_point(pt, time, spec.y_min, spec.y_max, grid_w, grid_h);
                plot_segment_step(spec, prev, Some(next));
                prev = next;
            }
        }
    }
}

fn render_braille_series(
    area: Rect,
    buf: &mut Buffer,
    time: TimeRange,
    series: &[TraceSeriesSpec<'_>],
) {
    let width = usize::from(area.width);
    let height = usize::from(area.height);
    let px_w = width * 2;
    let px_h = height * 4;

    let mut masks = vec![0u8; width * height];
    let mut styles: Vec<Option<Style>> = vec![None; width * height];

    rasterize_segments(
        time,
        series,
        px_w,
        px_h,
        |spec, prev, next_opt| match next_opt {
            None => plot_braille_pixel(
                prev.0,
                prev.1,
                width,
                height,
                spec.style,
                &mut masks,
                &mut styles,
            ),
            Some(next) => {
                draw_bresenham(prev.0, prev.1, next.0, next.1, |px, py| {
                    plot_braille_pixel(px, py, width, height, spec.style, &mut masks, &mut styles);
                });
            }
        },
    );

    for row in 0..height {
        for col in 0..width {
            let idx = row * width + col;
            let mask = masks[idx];
            if mask == 0 {
                continue;
            }
            let ch = char::from_u32(0x2800 + u32::from(mask)).unwrap_or('⣿');
            if let Some(cell) = buf.cell_mut((area.x + col as u16, area.y + row as u16)) {
                cell.set_char(ch);
                if let Some(style) = styles[idx] {
                    cell.set_style(style);
                }
            }
        }
    }
}

fn plot_braille_pixel(
    px: i32,
    py: i32,
    width: usize,
    height: usize,
    style: Style,
    masks: &mut [u8],
    styles: &mut [Option<Style>],
) {
    if px < 0 || py < 0 {
        return;
    }
    let col = (px as usize) / 2;
    let row = (py as usize) / 4;
    if col >= width || row >= height {
        return;
    }
    let dx = (px as usize) % 2;
    let dy = (py as usize) % 4;
    let idx = row * width + col;
    masks[idx] |= braille_bit(dx, dy);
    styles[idx] = Some(style);
}

fn render_ascii_series(
    area: Rect,
    buf: &mut Buffer,
    time: TimeRange,
    series: &[TraceSeriesSpec<'_>],
) {
    let width = usize::from(area.width);
    let height = usize::from(area.height);
    let mut grid: Vec<Option<(char, Style)>> = vec![None; width * height];

    rasterize_segments(
        time,
        series,
        width,
        height,
        |spec, prev, next_opt| match next_opt {
            None => plot_ascii_cell(
                prev.0,
                prev.1,
                spec.ascii_point_char,
                spec.style,
                width,
                height,
                &mut grid,
            ),
            Some(next) => {
                let stroke = ascii_edge_char(prev, next);
                draw_bresenham(prev.0, prev.1, next.0, next.1, |cx, cy| {
                    plot_ascii_cell(cx, cy, stroke, spec.style, width, height, &mut grid);
                });
                plot_ascii_cell(
                    prev.0,
                    prev.1,
                    spec.ascii_point_char,
                    spec.style,
                    width,
                    height,
                    &mut grid,
                );
                plot_ascii_cell(
                    next.0,
                    next.1,
                    spec.ascii_point_char,
                    spec.style,
                    width,
                    height,
                    &mut grid,
                );
            }
        },
    );

    for row in 0..height {
        for col in 0..width {
            if let Some((ch, style)) = grid[row * width + col]
                && let Some(cell) = buf.cell_mut((area.x + col as u16, area.y + row as u16))
            {
                cell.set_char(ch).set_style(style);
            }
        }
    }
}

fn ascii_edge_char(p0: (i32, i32), p1: (i32, i32)) -> char {
    let dx = p1.0 - p0.0;
    let dy = p1.1 - p0.1;
    if dx == 0 && dy == 0 {
        '*'
    } else if dy == 0 {
        '-'
    } else if dx == 0 {
        '|'
    } else if (dx > 0 && dy < 0) || (dx < 0 && dy > 0) {
        '/'
    } else {
        '\\'
    }
}

fn plot_ascii_cell(
    cx: i32,
    cy: i32,
    ch: char,
    style: Style,
    width: usize,
    height: usize,
    grid: &mut [Option<(char, Style)>],
) {
    if cx < 0 || cy < 0 {
        return;
    }
    let col = cx as usize;
    let row = cy as usize;
    if col >= width || row >= height {
        return;
    }
    grid[row * width + col] = Some((ch, style));
}

fn draw_bresenham(x0: i32, y0: i32, x1: i32, y1: i32, mut plot: impl FnMut(i32, i32)) {
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let mut x = x0;
    let mut y = y0;

    loop {
        plot(x, y);
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}
