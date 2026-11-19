use std::time::Duration;

use chrono::{DateTime, Local, Utc};

/// Clock abstraction enabling deterministic virtual-time testing via `tokio::time::pause()`.
pub trait DaemonClock: Send + Sync {
    /// Returns the current UTC timestamp.
    fn now_utc(&self) -> DateTime<Utc>;

    /// Returns the current local-timezone timestamp.
    fn now_local(&self) -> DateTime<Local> {
        self.now_utc().with_timezone(&Local)
    }

    /// Returns the current monotonic Tokio instant.
    fn now_instant(&self) -> tokio::time::Instant {
        tokio::time::Instant::now()
    }
}

/// Production wall-clock implementation.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl DaemonClock for SystemClock {
    fn now_utc(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Virtual clock anchored to a starting UTC instant and driven by Tokio's paused time.
#[derive(Debug, Clone)]
pub struct VirtualClock {
    anchor_utc: DateTime<Utc>,
    anchor_instant: tokio::time::Instant,
}

impl VirtualClock {
    /// Creates a `VirtualClock` whose `now_utc()` starts at `start_utc` and advances
    /// in lockstep with `tokio::time::advance(...)`.
    pub fn starting_at(start_utc: DateTime<Utc>) -> Self {
        Self {
            anchor_utc: start_utc,
            anchor_instant: tokio::time::Instant::now(),
        }
    }
}

impl DaemonClock for VirtualClock {
    fn now_utc(&self) -> DateTime<Utc> {
        let elapsed: Duration =
            tokio::time::Instant::now().saturating_duration_since(self.anchor_instant);
        let delta =
            chrono::Duration::from_std(elapsed).unwrap_or_else(|_| chrono::Duration::zero());
        self.anchor_utc + delta
    }
}
