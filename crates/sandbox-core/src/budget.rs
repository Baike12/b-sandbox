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
///
/// Fields are `Option` because **presence is the signal**: a field the caller
/// left out means "no opinion, use the default"; a field the caller set means
/// "enforce this or reject the request". This distinction is load-bearing for
/// capability checking — a backend that cannot enforce a ceiling must reject
/// the caller who explicitly asks for one (`{"memoryMb": 2048}` must not
/// quietly become "no request" just because 2048 equals the default), which
/// the previous defaulted-fields model could not express.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Budget {
    /// Address-space ceiling for sandbox processes, in MiB. Absent = default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u64>,
    /// Max concurrent processes/threads in the sandbox. Absent = default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids_max: Option<u32>,
}

pub const DEFAULT_MEMORY_MB: u64 = 2048;
pub const DEFAULT_PIDS_MAX: u32 = 512;

impl Budget {
    /// The memory ceiling actually applied at enforcement points.
    pub fn effective_memory_mb(&self) -> u64 {
        self.memory_mb.unwrap_or(DEFAULT_MEMORY_MB)
    }

    /// The pids ceiling actually applied at enforcement points.
    pub fn effective_pids_max(&self) -> u32 {
        self.pids_max.unwrap_or(DEFAULT_PIDS_MAX)
    }

    /// Validate bounds. No silent clamping: out-of-bounds requests are errors.
    /// Absent fields carry no caller intent and need no validation.
    pub fn validate(&self) -> Result<()> {
        if let Some(memory_mb) = self.memory_mb {
            if memory_mb == 0 {
                return Err(Error::Budget("memory_mb must be greater than zero".into()));
            }
            if memory_mb > MEMORY_MB_CEILING {
                return Err(Error::Budget(format!(
                    "memory_mb {memory_mb} exceeds ceiling {MEMORY_MB_CEILING}"
                )));
            }
        }
        if let Some(pids_max) = self.pids_max {
            if pids_max == 0 {
                return Err(Error::Budget("pids_max must be greater than zero".into()));
            }
            if pids_max > PIDS_MAX_CEILING {
                return Err(Error::Budget(format!(
                    "pids_max {pids_max} exceeds ceiling {PIDS_MAX_CEILING}"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_budget_is_valid_and_effective_values_are_the_defaults() {
        let budget = Budget::default();
        budget.validate().unwrap();
        assert_eq!(budget.effective_memory_mb(), DEFAULT_MEMORY_MB);
        assert_eq!(budget.effective_pids_max(), DEFAULT_PIDS_MAX);
    }

    #[test]
    fn zero_memory_rejected() {
        let err = Budget {
            memory_mb: Some(0),
            pids_max: None,
        }
        .validate()
        .unwrap_err();
        assert_eq!(err.code(), "budget_rejected");
    }

    #[test]
    fn memory_above_ceiling_rejected() {
        let budget = Budget {
            memory_mb: Some(MEMORY_MB_CEILING + 1),
            pids_max: None,
        };
        let err = budget.validate().unwrap_err();
        assert_eq!(err.code(), "budget_rejected");
        assert!(err.to_string().contains("ceiling"));
    }

    #[test]
    fn zero_pids_rejected() {
        let err = Budget {
            memory_mb: None,
            pids_max: Some(0),
        }
        .validate()
        .unwrap_err();
        assert_eq!(err.code(), "budget_rejected");
    }

    #[test]
    fn pids_above_ceiling_rejected() {
        let budget = Budget {
            memory_mb: None,
            pids_max: Some(PIDS_MAX_CEILING + 1),
        };
        assert_eq!(budget.validate().unwrap_err().code(), "budget_rejected");
    }

    #[test]
    fn boundary_values_accepted() {
        Budget {
            memory_mb: Some(MEMORY_MB_CEILING),
            pids_max: Some(PIDS_MAX_CEILING),
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

    #[test]
    fn explicit_field_is_present_and_absent_field_serializes_away() {
        // Presence is the capability signal: an explicit field must survive a
        // serde round-trip (so the capability check sees it), and an absent
        // one must not appear on the wire.
        let explicit: Budget =
            serde_json::from_str(r#"{"memoryMb": 2048}"#).expect("explicit field parses");
        assert_eq!(explicit.memory_mb, Some(2048));
        let wire = serde_json::to_string(&Budget::default()).expect("serializes");
        assert_eq!(wire, "{}", "absent fields must not echo back as set");
        let round: Budget = serde_json::from_str(&wire).expect("empty parses");
        assert_eq!(round, Budget::default());
    }
}
