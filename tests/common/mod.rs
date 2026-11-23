pub mod daemon;
pub mod synthetic;
pub mod tui;

#[allow(
    unused_imports,
    reason = "shared test fixtures re-exported for convenience across integration test targets"
)]
pub use tui::{make_session_summary, sample_session_summary};
