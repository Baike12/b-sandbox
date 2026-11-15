//! The backend seam: everything above this trait is platform-agnostic.
//!
//! Contract (design.md §3, principles 3 and 4):
//! - A backend declares what it can enforce via [`BackendCapabilities`]; the
//!   shared checker [`check_policy_support`] rejects specs requesting anything
//!   undeclared. Rejection, never degradation.
//! - "Declaration must have enforcement evidence": every capability bit a
//!   backend sets must be backed by a test that flips a policy input and
//!   observes the workload's behavior change. The bit exists because a test
//!   proves it, not because the code hopes so.

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::policy::{ResolvedFsPolicy, ResolvedNetPolicy, SandboxSpec};

/// Policy enforcement capabilities, as a bitmask so capability sets are data
/// (comparable, printable, testable) rather than scattered booleans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendCapabilities(u16);

impl BackendCapabilities {
    pub const NONE: Self = Self(0);
    /// Can grant read-write access to host paths.
    pub const FS_READWRITE: Self = Self(1 << 0);
    /// Can grant read-only access to host paths.
    pub const FS_READONLY: Self = Self(1 << 1);
    /// Can deny access to host paths (including subtree dominance).
    pub const FS_DENIED: Self = Self(1 << 2);
    /// Can enforce an all-or-nothing egress switch (deny-all / allow-all).
    pub const NET_EGRESS_DEFAULT: Self = Self(1 << 3);
    /// Can enforce per-target egress rules (CIDR / hostname).
    pub const NET_EGRESS_RULES: Self = Self(1 << 4);
    /// Can enforce a memory budget (Linux cgroups / RLIMIT_AS where supported;
    /// macOS setrlimit(RLIMIT_AS) fails with EINVAL and the kernel does not
    /// honor it, so macOS backends must NOT declare this bit).
    pub const BUDGET_MEMORY: Self = Self(1 << 5);
    /// Can enforce a pids ceiling (RLIMIT_NPROC; per-uid accounting on macOS
    /// makes it approximate in the fail-closed direction — it may over-
    /// restrict, never under-restrict).
    pub const BUDGET_PIDS: Self = Self(1 << 6);
    /// Can enforce the exec rlimits (file descriptors).
    pub const BUDGET_NOFILE: Self = Self(1 << 7);
    /// Can route egress through the credential-injection proxy: opens exactly
    /// one network hole (the proxy endpoint) and injects proxy env vars.
    /// Requires a per-port network filter (Seatbelt `(to tcp ip:port)`); bwrap
    /// cannot express it and must not declare this bit.
    pub const NET_EGRESS_PROXY: Self = Self(1 << 8);

    pub const fn bits(self) -> u16 {
        self.0
    }

    pub const fn has(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Shared policy-vs-capability check. Runs before a backend is asked to
/// provision, so every backend gets identical rejection semantics.
pub fn check_policy_support(spec: &SandboxSpec, caps: BackendCapabilities) -> Result<()> {
    // Filesystem: each non-empty set needs its capability bit.
    if !caps.has(BackendCapabilities::FS_READWRITE) && !spec.fs.readwrite.is_empty() {
        return Err(Error::Policy(
            "backend cannot enforce read-write host paths (no FS_READWRITE capability)".into(),
        ));
    }
    if !caps.has(BackendCapabilities::FS_READONLY) && !spec.fs.readonly.is_empty() {
        return Err(Error::Policy(
            "backend cannot enforce read-only host paths (no FS_READONLY capability)".into(),
        ));
    }
    if !caps.has(BackendCapabilities::FS_DENIED) && !spec.fs.denied.is_empty() {
        return Err(Error::Policy(
            "backend cannot enforce denied host paths (no FS_DENIED capability)".into(),
        ));
    }

    // Network: explicit targets need the rules bit; the default posture needs
    // the default bit. Deny targets that restate protected ranges are still
    // "rules" — a backend without rule support gets deny-all from its default
    // capability instead, and must reject the spec rather than ignore the list.
    if spec.net.proxy && !caps.has(BackendCapabilities::NET_EGRESS_PROXY) {
        return Err(Error::Policy(
            "backend cannot route egress through the credential proxy (no NET_EGRESS_PROXY capability)"
                .into(),
        ));
    }
    let has_net_rules = !spec.net.allow.is_empty() || !spec.net.deny.is_empty();
    if has_net_rules && !caps.has(BackendCapabilities::NET_EGRESS_RULES) {
        return Err(Error::Policy(
            "backend cannot enforce per-target egress rules (no NET_EGRESS_RULES capability)"
                .into(),
        ));
    }
    if !caps.has(BackendCapabilities::NET_EGRESS_DEFAULT) {
        return Err(Error::Policy(
            "backend cannot enforce an egress posture (no NET_EGRESS_DEFAULT capability)".into(),
        ));
    }

    // Budget sections. A backend that cannot enforce a section rejects any
    // *explicit* request for it; a value left at the built-in default is
    // treated as "no opinion" — the backend's platform tier stays visible in
    // /health, so the caller can see what is (not) being enforced. Memory
    // budgets are only enforceable on the Linux tier (see BUDGET_MEMORY docs).
    if !caps.has(BackendCapabilities::BUDGET_MEMORY)
        && spec.budget.memory_mb != crate::budget::Budget::default().memory_mb
    {
        return Err(Error::Policy(
            "backend cannot enforce a memory budget (no BUDGET_MEMORY capability); \
             memory budgets require the Linux tier"
                .into(),
        ));
    }
    if !caps.has(BackendCapabilities::BUDGET_PIDS)
        && spec.budget.pids_max != crate::budget::Budget::default().pids_max
    {
        return Err(Error::Policy(
            "backend cannot enforce a pids ceiling (no BUDGET_PIDS capability)".into(),
        ));
    }
    if !caps.has(BackendCapabilities::BUDGET_NOFILE) {
        return Err(Error::Policy(
            "backend cannot enforce the exec fd limit (no BUDGET_NOFILE capability)".into(),
        ));
    }

    Ok(())
}

/// Materialized sandbox state a backend hands back from `provision` and takes
/// back on every later call. The registry owns one context per sandbox.
#[derive(Debug, Clone)]
pub struct SandboxContext {
    /// Sandbox identifier (opaque token minted by the service).
    pub id: String,
    /// The sandbox scratch directory (implicitly writable by the workload).
    pub rootfs: PathBuf,
    /// The spec as submitted (owned copy; the spec is immutable post-creation).
    pub spec: SandboxSpec,
    /// Pre-resolved filesystem policy (normalization done once).
    pub fs: ResolvedFsPolicy,
    /// Pre-resolved network policy (protected ranges included).
    pub net: ResolvedNetPolicy,
}

/// One command execution inside a sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRequest {
    /// Program to execute. Never run through a shell — args are passed verbatim
    /// to exec, so shell metacharacters carry no power (tested per backend).
    pub command: String,
    pub args: Vec<String>,
    /// Extra environment for this exec. The base environment is defined by the
    /// backend (minimal, not the host's).
    pub env: BTreeMap<String, String>,
    /// Working directory, sandbox-relative (e.g. "" = rootfs).
    pub cwd: String,
    /// Hard wall-clock limit for this exec in milliseconds. None = the ceiling.
    pub timeout_ms: Option<u64>,
}

/// Hard ceiling on a single exec's wall-clock time.
pub const EXEC_TIMEOUT_MS_CEILING: u64 = 600_000;

/// Hard ceiling on argv length (count of args).
pub const EXEC_MAX_ARGS: usize = 1024;

impl ExecRequest {
    /// Validate request shape. Domain-level invariants only; backend-specific
    /// validation (e.g. cwd existence) happens in the backend.
    pub fn validate(&self) -> Result<()> {
        if self.command.trim().is_empty() {
            return Err(Error::Policy("exec command must not be empty".into()));
        }
        if self.command.contains('\0') || self.args.iter().any(|a| a.contains('\0')) {
            return Err(Error::Policy(
                "exec command and args must not contain NUL".into(),
            ));
        }
        if self.args.len() > EXEC_MAX_ARGS {
            return Err(Error::Policy(format!(
                "exec args exceed {} entries",
                EXEC_MAX_ARGS
            )));
        }
        if let Some(timeout) = self.timeout_ms {
            if timeout == 0 {
                return Err(Error::Policy("exec timeout_ms must not be zero".into()));
            }
            if timeout > EXEC_TIMEOUT_MS_CEILING {
                return Err(Error::Policy(format!(
                    "exec timeout_ms {} exceeds ceiling {EXEC_TIMEOUT_MS_CEILING}",
                    timeout
                )));
            }
        }
        Ok(())
    }
}

/// The result of one exec. A timeout is a *result*, not an error (MXC): the
/// command ran, was killed for exceeding its budget, and the caller sees both
/// facts in one struct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecOutcome {
    /// Exit code; `None` when the process died by signal or was killed by us.
    pub exit_code: Option<i32>,
    /// True when the exec was killed for exceeding `timeout_ms`.
    pub timed_out: bool,
    /// Captured stdout, truncated at the policy cap.
    pub stdout: String,
    /// Captured stderr, truncated at the policy cap.
    pub stderr: String,
    /// True when either stream hit the capture cap (combined budget).
    pub truncated: bool,
}

/// Backend availability, reported honestly in /health. A backend that is not
/// available must never be used; there is no fallback to a weaker tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendHealth {
    pub available: bool,
    pub reason: Option<String>,
}

impl BackendHealth {
    pub const fn available() -> Self {
        Self {
            available: true,
            reason: None,
        }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            reason: Some(reason.into()),
        }
    }
}

/// The isolation backend seam. Implementations live in sibling crates; the
/// service talks to this trait only.
#[async_trait]
pub trait SandboxBackend: Send + Sync {
    /// Stable backend name used in events and health output.
    fn name(&self) -> &'static str;

    /// What this backend can enforce. Must be backed by tests (see module doc).
    fn capabilities(&self) -> BackendCapabilities;

    /// Availability right now (checks for required OS facilities).
    fn health(&self) -> BackendHealth;

    /// Materialize a sandbox: create the scratch rootfs and return the context.
    /// The spec must already have passed `SandboxSpec::validate` and
    /// `check_policy_support`.
    async fn provision(&self, id: &str, spec: &SandboxSpec) -> Result<SandboxContext>;

    /// Run one command inside the sandbox. Applies the context's policy,
    /// budget, and the request's timeout on every invocation.
    async fn exec(&self, ctx: &SandboxContext, req: ExecRequest) -> Result<ExecOutcome>;

    /// Kill anything the backend still tracks for this sandbox. For M1 process
    /// backends with synchronous execs there is nothing in flight; container
    /// and microVM backends kill the workload group here.
    async fn stop(&self, ctx: &SandboxContext) -> Result<()>;

    /// Destroy the sandbox: remove the rootfs and any backend state.
    async fn deprovision(&self, ctx: &SandboxContext) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{EgressDefault, ExecPolicy, FsPolicy, NetPolicy, NetTarget};

    fn spec_with(fs: FsPolicy, net: NetPolicy, exec: ExecPolicy) -> SandboxSpec {
        SandboxSpec {
            fs,
            net,
            exec,
            budget: Default::default(),
            template: None,
        }
    }

    const ALL_FS: BackendCapabilities = BackendCapabilities::FS_READWRITE
        .union(BackendCapabilities::FS_READONLY)
        .union(BackendCapabilities::FS_DENIED);

    /// The macOS process tier: no memory budget, pids only.
    const PIDS_TIER: BackendCapabilities =
        BackendCapabilities::BUDGET_PIDS.union(BackendCapabilities::BUDGET_NOFILE);

    #[test]
    fn capability_bits_are_distinct() {
        let all = [
            BackendCapabilities::FS_READWRITE,
            BackendCapabilities::FS_READONLY,
            BackendCapabilities::FS_DENIED,
            BackendCapabilities::NET_EGRESS_DEFAULT,
            BackendCapabilities::NET_EGRESS_RULES,
            BackendCapabilities::BUDGET_MEMORY,
            BackendCapabilities::BUDGET_PIDS,
            BackendCapabilities::BUDGET_NOFILE,
            BackendCapabilities::NET_EGRESS_PROXY,
        ];
        let total: u16 = all.iter().map(|c| c.bits()).fold(0, |a, b| a | b);
        assert_eq!(
            total.count_ones(),
            all.len() as u32,
            "bits must not collide"
        );
    }

    #[test]
    fn minimal_caps_accept_deny_all_spec() {
        // The pids-only tier (macOS) can run a locked-down sandbox: no fs
        // rules, no net rules, default budget.
        let caps = BackendCapabilities::NET_EGRESS_DEFAULT.union(PIDS_TIER);
        let spec = spec_with(
            FsPolicy::default(),
            NetPolicy::default(),
            ExecPolicy::default(),
        );
        check_policy_support(&spec, caps).unwrap();
    }

    #[test]
    fn fs_rules_without_their_bit_are_rejected() {
        let caps = BackendCapabilities::NET_EGRESS_DEFAULT.union(PIDS_TIER);
        let spec = spec_with(
            FsPolicy {
                readwrite: vec!["/tmp/x".into()],
                ..Default::default()
            },
            NetPolicy::default(),
            ExecPolicy::default(),
        );
        let err = check_policy_support(&spec, caps).unwrap_err();
        assert_eq!(err.code(), "policy_validation");
        assert!(err.to_string().contains("FS_READWRITE"));

        let spec = spec_with(
            FsPolicy {
                denied: vec!["/etc".into()],
                ..Default::default()
            },
            NetPolicy::default(),
            ExecPolicy::default(),
        );
        assert!(check_policy_support(&spec, caps).is_err());
    }

    #[test]
    fn net_rules_without_rules_bit_are_rejected_even_deny_only() {
        let caps = BackendCapabilities::NET_EGRESS_DEFAULT.union(PIDS_TIER);
        let spec = spec_with(
            FsPolicy::default(),
            NetPolicy {
                egress_default: EgressDefault::Deny,
                deny: vec![NetTarget {
                    target: "203.0.113.0/24".into(),
                }],
                allow: vec![],
                proxy: false,
            },
            ExecPolicy::default(),
        );
        let err = check_policy_support(&spec, caps).unwrap_err();
        assert_eq!(err.code(), "policy_validation");
        assert!(err.to_string().contains("NET_EGRESS_RULES"));
    }

    #[test]
    fn missing_egress_default_bit_is_always_rejected() {
        let caps = BackendCapabilities::FS_READWRITE.union(PIDS_TIER);
        let spec = spec_with(
            FsPolicy::default(),
            NetPolicy::default(),
            ExecPolicy::default(),
        );
        let err = check_policy_support(&spec, caps).unwrap_err();
        assert!(err.to_string().contains("NET_EGRESS_DEFAULT"));
    }

    #[test]
    fn explicit_memory_budget_without_memory_bit_is_rejected() {
        // macOS setrlimit(RLIMIT_AS) fails with EINVAL and the kernel does not
        // honor the limit — a macOS-tier backend must refuse the request
        // instead of pretending to enforce it.
        let caps = BackendCapabilities::NET_EGRESS_DEFAULT.union(PIDS_TIER);
        let spec = SandboxSpec {
            budget: crate::budget::Budget {
                memory_mb: 512,
                pids_max: 64,
            },
            ..Default::default()
        };
        let err = check_policy_support(&spec, caps).unwrap_err();
        assert_eq!(err.code(), "policy_validation");
        assert!(err.to_string().contains("BUDGET_MEMORY"), "{err}");
    }

    #[test]
    fn explicit_pids_budget_without_pids_bit_is_rejected() {
        let caps = BackendCapabilities::NET_EGRESS_DEFAULT
            .union(BackendCapabilities::BUDGET_MEMORY)
            .union(BackendCapabilities::BUDGET_NOFILE);
        let spec = SandboxSpec {
            budget: crate::budget::Budget {
                memory_mb: 2048,
                pids_max: 64,
            },
            ..Default::default()
        };
        let err = check_policy_support(&spec, caps).unwrap_err();
        assert!(err.to_string().contains("BUDGET_PIDS"), "{err}");
    }

    #[test]
    fn full_caps_accept_a_full_spec() {
        let caps = ALL_FS
            .union(BackendCapabilities::NET_EGRESS_DEFAULT)
            .union(BackendCapabilities::NET_EGRESS_RULES)
            .union(BackendCapabilities::BUDGET_MEMORY)
            .union(BackendCapabilities::BUDGET_PIDS)
            .union(BackendCapabilities::BUDGET_NOFILE);
        let spec = spec_with(
            FsPolicy {
                readwrite: vec!["/tmp/x".into()],
                readonly: vec!["/usr/share".into()],
                denied: vec!["/private/etc".into()],
            },
            NetPolicy {
                egress_default: EgressDefault::Deny,
                allow: vec![NetTarget {
                    target: "example.com".into(),
                }],
                deny: vec![NetTarget {
                    target: "203.0.113.0/24".into(),
                }],
                proxy: false,
            },
            ExecPolicy::default(),
        );
        check_policy_support(&spec, caps).unwrap();
    }

    #[test]
    fn proxy_request_without_the_bit_is_rejected() {
        let caps = BackendCapabilities::NET_EGRESS_DEFAULT.union(PIDS_TIER);
        let spec = SandboxSpec {
            net: NetPolicy {
                proxy: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let err = check_policy_support(&spec, caps).unwrap_err();
        assert_eq!(err.code(), "policy_validation");
        assert!(err.to_string().contains("NET_EGRESS_PROXY"), "{err}");

        // A backend declaring the bit accepts it.
        let caps = caps.union(BackendCapabilities::NET_EGRESS_PROXY);
        check_policy_support(&spec, caps).unwrap();
    }

    #[test]
    fn exec_request_rejects_empty_command() {
        let req = ExecRequest {
            command: "  ".into(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: String::new(),
            timeout_ms: None,
        };
        assert_eq!(req.validate().unwrap_err().code(), "policy_validation");
    }

    #[test]
    fn exec_request_rejects_nul_bytes() {
        let req = ExecRequest {
            command: "sh\0".into(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: String::new(),
            timeout_ms: None,
        };
        assert_eq!(req.validate().unwrap_err().code(), "policy_validation");
    }

    #[test]
    fn exec_request_rejects_zero_and_over_ceiling_timeout() {
        let mk = |t: u64| ExecRequest {
            command: "ls".into(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: String::new(),
            timeout_ms: Some(t),
        };
        assert_eq!(mk(0).validate().unwrap_err().code(), "policy_validation");
        let err = mk(EXEC_TIMEOUT_MS_CEILING + 1).validate().unwrap_err();
        assert!(err.to_string().contains("ceiling"));
        mk(EXEC_TIMEOUT_MS_CEILING).validate().unwrap();
    }

    #[test]
    fn exec_request_rejects_too_many_args() {
        let req = ExecRequest {
            command: "echo".into(),
            args: vec!["x".to_string(); EXEC_MAX_ARGS + 1],
            env: BTreeMap::new(),
            cwd: String::new(),
            timeout_ms: None,
        };
        assert_eq!(req.validate().unwrap_err().code(), "policy_validation");
    }

    #[test]
    fn valid_exec_request_passes() {
        let req = ExecRequest {
            command: "echo".into(),
            args: vec!["hello; rm -rf /".into()], // no shell: metachars are inert
            env: BTreeMap::from([("FOO".into(), "bar".into())]),
            cwd: String::new(),
            timeout_ms: Some(1000),
        };
        req.validate().unwrap();
    }
}
