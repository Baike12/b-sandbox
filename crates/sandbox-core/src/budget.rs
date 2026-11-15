//! Sandbox budget: resource ceilings declared at creation and enforced by the
//! backend. Every field maps to a concrete enforcement point for at least one
//! backend; a budget field we cannot enforce anywhere does not exist here.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Hard ceiling on memory requests (process backends map this to RLIMIT_AS).
pub const MEMORY_MB_CEILING: u64 = 32 * 1024;
/// Hard ceiling on process counts (process backends map this to RLIMIT_NPROC).
pub const PIDS_MAX_CEILING: u32 = 2048;

/// Sandbox-level resource budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Budget {
    /// Address-space ceiling for sandbox processes, in MiB. Default 2048.
    #[serde(default = "default_memory_mb")]
    pub memory_mb: u64,
    /// Max concurrent processes/threads in the sandbox. Default 512.
    #[serde(default = "default_pids_max")]
    pub pids_max: u32,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            memory_mb: default_memory_mb(),
            pids_max: default_pids_max(),
        }
    }
}

fn default_memory_mb() -> u64 {
    2048
}

fn default_pids_max() -> u32 {
    512
}

impl Budget {
    /// Validate bounds. No silent clamping: out-of-bounds requests are errors.
    pub fn validate(&self) -> Result<()> {
        if self.memory_mb == 0 {
            return Err(Error::Budget("memory_mb must be greater than zero".into()));
        }
        if self.memory_mb > MEMORY_MB_CEILING {
            return Err(Error::Budget(format!(
                "memory_mb {} exceeds ceiling {MEMORY_MB_CEILING}",
                self.memory_mb
            )));
        }
        if self.pids_max == 0 {
            return Err(Error::Budget("pids_max must be greater than zero".into()));
        }
        if self.pids_max > PIDS_MAX_CEILING {
            return Err(Error::Budget(format!(
                "pids_max {} exceeds ceiling {PIDS_MAX_CEILING}",
                self.pids_max
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        Budget::default().validate().unwrap();
    }

    #[test]
    fn zero_memory_rejected() {
        let err = Budget {
            memory_mb: 0,
            ..Budget::default()
        }
        .validate()
        .unwrap_err();
        assert_eq!(err.code(), "budget_rejected");
    }

    #[test]
    fn memory_above_ceiling_rejected() {
        let budget = Budget {
            memory_mb: MEMORY_MB_CEILING + 1,
            ..Budget::default()
        };
        let err = budget.validate().unwrap_err();
        assert_eq!(err.code(), "budget_rejected");
        assert!(err.to_string().contains("ceiling"));
    }

    #[test]
    fn zero_pids_rejected() {
        let err = Budget {
            pids_max: 0,
            ..Budget::default()
        }
        .validate()
        .unwrap_err();
        assert_eq!(err.code(), "budget_rejected");
    }

    #[test]
    fn pids_above_ceiling_rejected() {
        let budget = Budget {
            pids_max: PIDS_MAX_CEILING + 1,
            ..Budget::default()
        };
        assert_eq!(budget.validate().unwrap_err().code(), "budget_rejected");
    }

    #[test]
    fn boundary_values_accepted() {
        Budget {
            memory_mb: MEMORY_MB_CEILING,
            pids_max: PIDS_MAX_CEILING,
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn unknown_fields_are_hard_errors() {
        let json = r#"{"memoryMb": 512, "diskGb": 10}"#;
        let err = serde_json::from_str::<Budget>(json).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }
}
