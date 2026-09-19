//! Errors surfaced by the SDK.
//!
//! The engine's error enum is large and carries internal variants that mean
//! nothing to an embedder. This module collapses it into a small [`ErrorKind`]
//! that callers can actually match on, keeping the engine's own message as the
//! payload. The kinds line up one-for-one with the string codes the Node and
//! Python SDKs raise, so the three SDKs agree on what a failure is called.

use std::fmt;

use smolvm::error::{AgentErrorKind, Error as EngineError};

/// What went wrong, in terms an embedder can branch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// A machine, image, rootfs, disk or mount source does not exist.
    NotFound,
    /// The machine is not in a state that permits the operation.
    InvalidState,
    /// No hypervisor is available on this host.
    HypervisorUnavailable,
    /// KVM is missing, or the process may not open it.
    KvmUnavailable,
    /// The name is taken, or the guest rejected the request as conflicting.
    Conflict,
    /// A disk, overlay or image store operation failed.
    Storage,
    /// A mount could not be set up, or its path is not usable.
    Mount,
    /// The machine configuration is not valid.
    Config,
    /// A command ran but failed.
    CommandFailed,
    /// The credential was missing, rejected or expired.
    Unauthorized,
    /// The request could not be delivered.
    Connection,
    /// The request outlived its deadline.
    Timeout,
    /// The operation exists, but not on this target.
    NotSupported,
    /// Anything the SDK does not classify further.
    Other,
}

impl ErrorKind {
    /// The stable string code, matching the Node and Python SDKs.
    pub fn code(self) -> &'static str {
        match self {
            Self::NotFound => "NOT_FOUND",
            Self::InvalidState => "INVALID_STATE",
            Self::HypervisorUnavailable => "HYPERVISOR_UNAVAILABLE",
            Self::KvmUnavailable => "KVM_UNAVAILABLE",
            Self::Conflict => "CONFLICT",
            Self::Storage => "STORAGE_ERROR",
            Self::Mount => "MOUNT_ERROR",
            Self::Config => "CONFIG_ERROR",
            Self::CommandFailed => "COMMAND_FAILED",
            Self::Unauthorized => "UNAUTHORIZED",
            Self::Connection => "CONNECTION",
            Self::Timeout => "TIMEOUT",
            Self::NotSupported => "NOT_SUPPORTED",
            Self::Other => "SMOLVM_ERROR",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// An SDK failure: a [`ErrorKind`] plus the engine's own description.
#[derive(Debug, Clone)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

impl Error {
    /// Build an error directly. Mostly useful in tests.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// What class of failure this is.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The stable string code for this failure.
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }

    /// The engine's description, without the code prefix.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.kind.code(), self.message)
    }
}

impl std::error::Error for Error {}

impl From<EngineError> for Error {
    fn from(err: EngineError) -> Self {
        let (kind, message) = match &err {
            EngineError::VmNotFound { name } => {
                (ErrorKind::NotFound, format!("VM not found: {name}"))
            }
            EngineError::InvalidState { expected, actual } => (
                ErrorKind::InvalidState,
                format!("Invalid state: expected {expected}, got {actual}"),
            ),
            EngineError::HypervisorUnavailable(reason) => (
                ErrorKind::HypervisorUnavailable,
                format!("Hypervisor unavailable: {reason}"),
            ),
            EngineError::Agent {
                operation,
                reason,
                kind,
            } => {
                let kind = match kind {
                    AgentErrorKind::NotFound => ErrorKind::NotFound,
                    AgentErrorKind::Conflict => ErrorKind::Conflict,
                    AgentErrorKind::Other => ErrorKind::Other,
                };
                (kind, format!("Agent error ({operation}): {reason}"))
            }
            EngineError::RootfsNotFound { path } => (
                ErrorKind::NotFound,
                format!("Rootfs not found: {}", path.display()),
            ),
            EngineError::DiskNotFound { path } => (
                ErrorKind::NotFound,
                format!("Disk not found: {}", path.display()),
            ),
            EngineError::MountSourceNotFound { path } => (
                ErrorKind::NotFound,
                format!("Mount source not found: {}", path.display()),
            ),
            EngineError::Storage { operation, reason } => (
                ErrorKind::Storage,
                format!("Storage ({operation}): {reason}"),
            ),
            EngineError::Mount { operation, reason } => {
                (ErrorKind::Mount, format!("Mount ({operation}): {reason}"))
            }
            EngineError::InvalidMountPath { reason } => {
                (ErrorKind::Mount, format!("Invalid mount path: {reason}"))
            }
            EngineError::Config { operation, reason } => {
                (ErrorKind::Config, format!("Config ({operation}): {reason}"))
            }
            EngineError::CommandFailed { command, reason } => (
                ErrorKind::CommandFailed,
                format!("Command '{command}' failed: {reason}"),
            ),
            EngineError::KvmUnavailable(reason) => (
                ErrorKind::KvmUnavailable,
                format!("KVM unavailable: {reason}"),
            ),
            EngineError::KvmPermission(reason) => (
                ErrorKind::KvmUnavailable,
                format!("KVM permission denied: {reason}"),
            ),
            _ => (ErrorKind::Other, err.to_string()),
        };
        Self { kind, message }
    }
}

impl From<smol_cloud::Error> for Error {
    fn from(err: smol_cloud::Error) -> Self {
        let kind = match err.kind() {
            smol_cloud::ErrorKind::NotFound => ErrorKind::NotFound,
            smol_cloud::ErrorKind::Unauthorized => ErrorKind::Unauthorized,
            smol_cloud::ErrorKind::Conflict => ErrorKind::Conflict,
            smol_cloud::ErrorKind::Timeout => ErrorKind::Timeout,
            smol_cloud::ErrorKind::Connection => ErrorKind::Connection,
            _ => ErrorKind::Other,
        };
        // Keep the correlation id in the message: a caller sees the message,
        // never the response headers, and support needs that id to find the
        // call.
        let message = match err.request_id() {
            Some(request_id) => format!("{} [request id: {request_id}]", err.message()),
            None => err.message().to_string(),
        };
        Self { kind, message }
    }
}

/// The SDK's result alias.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_vm_maps_to_not_found_and_keeps_its_name() {
        let error = Error::from(EngineError::VmNotFound {
            name: "ghost".into(),
        });
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert_eq!(error.code(), "NOT_FOUND");
        assert!(error.message().contains("ghost"));
        assert_eq!(error.to_string(), "[NOT_FOUND] VM not found: ghost");
    }

    #[test]
    fn a_conflicting_agent_error_maps_to_conflict() {
        let error = Error::from(EngineError::Agent {
            operation: "create".into(),
            reason: "name taken".into(),
            kind: AgentErrorKind::Conflict,
        });
        assert_eq!(error.kind(), ErrorKind::Conflict);
        assert!(error.message().contains("create"));
    }

    #[test]
    fn both_kvm_failures_read_as_one_kind() {
        for error in [
            EngineError::KvmUnavailable("no /dev/kvm".into()),
            EngineError::KvmPermission("not in group kvm".into()),
        ] {
            assert_eq!(Error::from(error).kind(), ErrorKind::KvmUnavailable);
        }
    }
}
