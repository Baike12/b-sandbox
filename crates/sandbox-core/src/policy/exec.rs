//! Exec policy: per-execution resource envelopes that process backends can
//! actually enforce. Every field here maps to a real enforcement point —
//! anything we cannot enforce does not get a field (fail-closed by omission).

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Hard ceiling on captured output per exec, regardless of what callers ask for.
/// Prevents a workload from flooding host memory through the capture pipe.
pub const MAX_OUTPUT_BYTES_CEILING: u64 = 64 * 1024 * 1024;

/// Hard ceiling on file descriptors per exec.
pub const MAX_OPEN_FILES_CEILING: u32 = 4096;

/// Per-execution policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecPolicy {
    /// Captured stdout+stderr bytes per exec before truncation.
    /// Default 1 MiB.
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: u64,
    /// File descriptor limit (RLIMIT_NOFILE) for exec'd processes.
    /// Default 256.
    #[serde(default = "default_max_open_files")]
    pub max_open_files: u32,
}

impl Default for ExecPolicy {
    fn default() -> Self {
        Self {
            max_output_bytes: default_max_output_bytes(),
            max_open_files: default_max_open_files(),
        }
    }
}

fn default_max_output_bytes() -> u64 {
    1024 * 1024
}

fn default_max_open_files() -> u32 {
    256
}

impl ExecPolicy {
    /// Validate bounds. No silent clamping: out-of-bounds requests are errors.
    pub fn validate(&self) -> Result<()> {
        if self.max_output_bytes == 0 {
            return Err(Error::Budget(
                "max_output_bytes must be greater than zero".into(),
            ));
        }
        if self.max_output_bytes > MAX_OUTPUT_BYTES_CEILING {
            return Err(Error::Budget(format!(
                "max_output_bytes {} exceeds ceiling {MAX_OUTPUT_BYTES_CEILING}",
                self.max_output_bytes
            )));
        }
        if self.max_open_files == 0 {
            return Err(Error::Budget(
                "max_open_files must be greater than zero".into(),
            ));
        }
        if self.max_open_files > MAX_OPEN_FILES_CEILING {
            return Err(Error::Budget(format!(
                "max_open_files {} exceeds ceiling {MAX_OPEN_FILES_CEILING}",
                self.max_open_files
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
        ExecPolicy::default().validate().unwrap();
    }

    #[test]
    fn zero_output_cap_rejected() {
        let policy = ExecPolicy {
            max_output_bytes: 0,
            ..ExecPolicy::default()
        };
        let err = policy.validate().unwrap_err();
        assert_eq!(err.code(), "budget_rejected");
    }

    #[test]
    fn output_cap_above_ceiling_rejected() {
        let policy = ExecPolicy {
            max_output_bytes: MAX_OUTPUT_BYTES_CEILING + 1,
            ..ExecPolicy::default()
        };
        let err = policy.validate().unwrap_err();
        assert_eq!(err.code(), "budget_rejected");
        assert!(err.to_string().contains("ceiling"));
    }

    #[test]
    fn zero_open_files_rejected() {
        let policy = ExecPolicy {
            max_open_files: 0,
            ..ExecPolicy::default()
        };
        assert_eq!(policy.validate().unwrap_err().code(), "budget_rejected");
    }

    #[test]
    fn open_files_above_ceiling_rejected() {
        let policy = ExecPolicy {
            max_open_files: MAX_OPEN_FILES_CEILING + 1,
            ..ExecPolicy::default()
        };
        assert_eq!(policy.validate().unwrap_err().code(), "budget_rejected");
    }

    #[test]
    fn boundary_values_accepted() {
        let policy = ExecPolicy {
            max_output_bytes: MAX_OUTPUT_BYTES_CEILING,
            max_open_files: MAX_OPEN_FILES_CEILING,
        };
        policy.validate().unwrap();
    }
}
