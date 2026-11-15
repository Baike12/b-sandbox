//! The policy model and the sandbox spec built from it.
//!
//! A [`SandboxSpec`] is the complete, validated declaration a caller submits
//! when creating a sandbox. Rules of the model:
//! - Serialization is the API contract: `deny_unknown_fields` everywhere
//!   (hallucinated fields from agents are errors, not ignored), camelCase.
//! - Validation happens once, here, at the domain boundary; backends receive
//!   already-resolved policies plus their own capability checks.

pub mod exec;
pub mod fs;
pub mod net;

pub use exec::ExecPolicy;
pub use fs::{FsAccess, FsPolicy, ResolvedFsPolicy, normalize_relative};
pub use net::{Domain, EgressDefault, NetPolicy, NetTarget, ResolvedNetPolicy};

use serde::{Deserialize, Serialize};

use crate::budget::Budget;
use crate::error::Result;
use crate::snapshot::validate_template_name;

/// Everything a caller declares at sandbox creation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SandboxSpec {
    #[serde(default)]
    pub fs: FsPolicy,
    #[serde(default)]
    pub net: NetPolicy,
    #[serde(default)]
    pub exec: ExecPolicy,
    #[serde(default)]
    pub budget: Budget,
    /// Named rootfs template to materialize from (built by the commit
    /// endpoint). None = empty scratch rootfs. Validated here as a name;
    /// existence is the backend's concern (its data root holds templates).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}

impl SandboxSpec {
    /// Validate all sections. Returns resolved policies on success.
    pub fn validate(&self) -> Result<(ResolvedFsPolicy, ResolvedNetPolicy)> {
        let fs = self.fs.resolve()?;
        self.exec.validate()?;
        let net = self.net.resolve()?;
        self.budget.validate()?;
        if let Some(name) = &self.template {
            validate_template_name(name)?;
        }
        Ok((fs, net))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_spec_validates_to_deny_all() {
        let spec = SandboxSpec::default();
        let (fs, net) = spec.validate().unwrap();
        assert!(fs.readwrite.is_empty() && fs.readonly.is_empty() && fs.denied.is_empty());
        assert_eq!(net.egress_default, EgressDefault::Deny);
    }

    #[test]
    fn budget_section_validates_with_the_spec() {
        let json = r#"{"budget": {"memoryMb": 512, "pidsMax": 64}}"#;
        let spec: SandboxSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.budget.memory_mb, 512);
        assert_eq!(spec.budget.pids_max, 64);
        spec.validate().unwrap();

        let json = r#"{"budget": {"memoryMb": 0}}"#;
        let spec: SandboxSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.validate().unwrap_err().code(), "budget_rejected");
    }

    #[test]
    fn unknown_fields_are_hard_errors() {
        // Agents hallucinate fields; a sandbox that ignores them would run with
        // different semantics than the caller believes. This is fail-closed.
        let json = r#"{"fs": {}, "net": {}, "exec": {}, "gpu": true}"#;
        let err = serde_json::from_str::<SandboxSpec>(json).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn snake_case_variant_of_camel_field_is_unknown() {
        let json = r#"{"exec": {"max_output_bytes": 1024}}"#;
        let err = serde_json::from_str::<SandboxSpec>(json).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn camel_case_roundtrip() {
        let json = r#"{"exec": {"maxOutputBytes": 4096, "maxOpenFiles": 64}}"#;
        let spec: SandboxSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.exec.max_output_bytes, 4096);
        assert_eq!(spec.exec.max_open_files, 64);
        spec.validate().unwrap();
    }

    #[test]
    fn invalid_section_fails_whole_spec() {
        let json = r#"{"net": {"allow": [{"target": "10.0.0.0/8"}]}}"#;
        let spec: SandboxSpec = serde_json::from_str(json).unwrap();
        let err = spec.validate().unwrap_err();
        assert_eq!(err.code(), "policy_validation");
    }
}
