use crate::ipc::protocol::error_codes;

/// Semantic CLI exit codes defined in ADR-0009.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    /// Command succeeded.
    Success = 0,
    /// Generic runtime, I/O, or IPC error.
    RuntimeError = 1,
    /// CLI usage error or configuration/input validation error.
    UsageOrConfigError = 2,
    /// Daemon offline or Unix domain socket unreachable.
    DaemonOffline = 3,
    /// Action precondition or state conflict (e.g. file already exists without --force).
    PreconditionConflict = 4,
    /// OS / D-Bus permission or backend refusal.
    PermissionOrBackendRefusal = 5,
}

impl ExitCode {
    /// Returns the numeric process exit code.
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Maps an IPC error code string to the corresponding semantic CLI exit code.
    pub fn from_ipc_error_code(code: &str) -> Self {
        match code {
            error_codes::INVALID_PARAMS
            | error_codes::INVALID_CONFIG
            | error_codes::VALIDATION_ERROR => Self::UsageOrConfigError,
            error_codes::PRECONDITION_FAILED
            | error_codes::CONFLICT
            | error_codes::ENROLLMENT_IN_PROGRESS => Self::PreconditionConflict,
            error_codes::PERMISSION_DENIED | error_codes::BACKEND_REFUSED => {
                Self::PermissionOrBackendRefusal
            }
            _ => Self::RuntimeError,
        }
    }
}

impl From<ExitCode> for std::process::ExitCode {
    fn from(value: ExitCode) -> Self {
        std::process::ExitCode::from(value.as_u8())
    }
}
