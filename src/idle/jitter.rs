use std::collections::VecDeque;
use std::time::Duration;

/// X11 raw input event type for user activity detection (ADR-0006).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    RawMotion { dx: f64, dy: f64 },
    RawKeyPress,
    RawButtonPress,
}

/// Sliding-window pointer motion jitter filter and key/click detector.
///
/// Filters out sub-threshold pointer drift and optical sensor noise by requiring
/// cumulative motion of at least `threshold_px` within a sliding `window` duration.
/// Any keypress or mouse button click triggers activity detection immediately.
#[derive(Debug, Clone)]
pub struct PointerJitterFilter {
    threshold_px: f64,
    window_ms: i64,
    motion_history: VecDeque<(i64, f64)>, // (ts_ms, distance_px)
}

impl PointerJitterFilter {
    /// Creates a new filter with specified pixel threshold and time window.
    pub fn new(threshold_px: f64, window: Duration) -> Self {
        Self {
            threshold_px,
            window_ms: window.as_millis().min(i64::MAX as u128) as i64,
            motion_history: VecDeque::new(),
        }
    }

    /// Records an input event at `ts_ms` and returns `true` if physical user activity is confirmed.
    pub fn record_event(&mut self, event: InputEvent, ts_ms: i64) -> bool {
        match event {
            InputEvent::RawKeyPress | InputEvent::RawButtonPress => true,
            InputEvent::RawMotion { dx, dy } => {
                let dist = (dx * dx + dy * dy).sqrt();
                self.record_motion(dist, ts_ms)
            }
        }
    }

    fn record_motion(&mut self, dist_px: f64, ts_ms: i64) -> bool {
        let cutoff = ts_ms.saturating_sub(self.window_ms);
        while let Some(&(t, _)) = self.motion_history.front() {
            if t < cutoff {
                self.motion_history.pop_front();
            } else {
                break;
            }
        }

        self.motion_history.push_back((ts_ms, dist_px));
        let total: f64 = self.motion_history.iter().map(|(_, d)| *d).sum();
        total >= self.threshold_px
    }

    /// Clears any accumulated motion history.
    pub fn reset(&mut self) {
        self.motion_history.clear();
    }
}
