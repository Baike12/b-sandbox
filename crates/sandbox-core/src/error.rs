//! Typed error model for the sandbox domain.
//!
//! The enum is closed: every failure class the service can report is a variant,
//! and each variant carries a stable machine-readable code via [`Error::code`].
//! The API layer maps those codes to HTTP statuses; no other layer invents codes.
//!
//! Invariants:
//! - `Error::Internal` exists for "this is a bug, not a workload outcome". It must
//!   be logged in full server-side; its message never crosses the API boundary.
//! - Backend messages may contain workload-controlled text; the API layer is
//!   responsible for redaction when echoing them.

use thiserror::Error;

/// Why a backend operation failed. The kind — not the message — is contractual.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendFailureKind {
    /// The workload could not be spawned (fork/exec failure, profile setup failure).
    Spawn,
    /// The workload was spawned but could not be killed or reaped.
    Kill,
    /// I/O plumbing failed (pipes, output capture).
    Io,
    /// Any other backend-local failure.
    Other,
}

impl BackendFailureKind {
    /// Stable machine-readable code; the API maps this to HTTP semantics.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Spawn => "backend_spawn_failed",
            Self::Kill => "backend_kill_failed",
            Self::Io => "backend_io_failed",
            Self::Other => "backend_error",
        }
    }
}

/// The closed error type for the whole service.
#[derive(Debug, Error)]
pub enum Error {
    /// A policy was rejected at validation time. Fail-closed: this is the only
    /// correct answer when a backend cannot enforce a requested rule.
    #[error("policy validation failed: {0}")]
    Policy(String),

    /// An operation is illegal in the current lifecycle state.
    #[error("lifecycle violation: {0}")]
    Lifecycle(String),

    /// A budget request or its enforcement was rejected.
    #[error("budget rejected: {0}")]
    Budget(String),

    /// An isolation backend operation failed.
    #[error("backend error ({kind:?}): {message}")]
    Backend {
        kind: BackendFailureKind,
        message: String,
    },

    /// A bug or unrecoverable internal condition. Never shown to callers as-is.
    #[error("internal error: {0}")]
    Internal(String),
}

impl Error {
    /// Stable machine-readable code for this error. Codes are part of the API
    /// contract: renaming one is a breaking change.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Policy(_) => "policy_validation",
            Self::Lifecycle(_) => "invalid_transition",
            Self::Budget(_) => "budget_rejected",
            Self::Backend { kind, .. } => kind.code(),
            Self::Internal(_) => "internal",
        }
    }
}

/// Convenience alias for fallible domain operations.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_failure_codes_are_stable() {
        assert_eq!(BackendFailureKind::Spawn.code(), "backend_spawn_failed");
        assert_eq!(BackendFailureKind::Kill.code(), "backend_kill_failed");
        assert_eq!(BackendFailureKind::Io.code(), "backend_io_failed");
        assert_eq!(BackendFailureKind::Other.code(), "backend_error");
    }

    #[test]
    fn error_codes_cover_every_variant() {
        // One assertion per variant: adding a variant without a code breaks the
        // exhaustive match at compile time; removing one breaks this test.
        let cases: Vec<(Error, &str)> = vec![
            (Error::Policy("x".into()), "policy_validation"),
            (Error::Lifecycle("x".into()), "invalid_transition"),
            (Error::Budget("x".into()), "budget_rejected"),
            (
                Error::Backend {
                    kind: BackendFailureKind::Spawn,
                    message: "x".into(),
                },
                "backend_spawn_failed",
            ),
            (Error::Internal("x".into()), "internal"),
        ];
        for (err, code) in cases {
            assert_eq!(err.code(), code, "variant {:?} changed code", err);
        }
    }

    #[test]
    fn codes_are_unique() {
        let all = [
            "policy_validation",
            "invalid_transition",
            "budget_rejected",
            BackendFailureKind::Spawn.code(),
            BackendFailureKind::Kill.code(),
            BackendFailureKind::Io.code(),
            BackendFailureKind::Other.code(),
            "internal",
        ];
        let mut sorted = all.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len(), "duplicate error codes detected");
    }

    #[test]
    fn messages_render_for_humans() {
        let err = Error::Policy("deny-all may not be overridden".into());
        assert!(err.to_string().contains("deny-all"));
    }
}
