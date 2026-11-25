pub mod fake;
pub mod grace;
pub mod notifier;
pub mod pipeline;
pub mod system;
pub mod traits;

pub use fake::{FakeActionExecutor, FakeNotifier};
pub use grace::{GraceControlCmd, GraceCoordinator, GraceRunOutcome, GraceRunParams};
pub use notifier::DesktopNotifier;
pub use pipeline::{PipelineContext, PipelineResult, SleepActionPipeline};
pub use system::SystemActionExecutor;
pub use traits::{
    ActionError, ActionExecutor, BoxFuture, CommandOutput, GraceNoticeSpec, GracePhase,
    NotificationEvent, Notifier, PowerActionResult, Urgency, is_player_ignored,
};
