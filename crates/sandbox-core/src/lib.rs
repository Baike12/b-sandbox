//! sandbox-core: pure domain model for the b-sandbox service.
//!
//! This crate holds everything that must not touch platform APIs: the policy
//! model, the lifecycle state machine, budgets, the event log, and the
//! `SandboxBackend` seam. Isolation backends live in sibling crates and
//! implement the trait defined here.

/// Smoke test placeholder; replaced by real modules in subsequent commits.
pub fn workspace_is_wired() -> bool {
    true
}

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_is_wired() {
        assert!(super::workspace_is_wired());
    }
}
