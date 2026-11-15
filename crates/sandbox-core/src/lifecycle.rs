//! Sandbox lifecycle state machine.
//!
//! M1 states: `Provisioning → Running → Terminated`. Paused/Pausing arrive in
//! M2; the machine is a transition *table* so adding states extends the table
//! instead of reworking conditionals.
//!
//! Contract:
//! - Transitions not in the table are `Error::Lifecycle` (code
//!   `invalid_transition`), never silently coerced.
//! - `Terminated` is terminal: no transition out of it exists.
//! - Exec is legal only in `Running`.

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    /// Backend is materializing the sandbox (rootfs, policy, handles).
    Provisioning,
    /// Sandbox accepts execs and file operations.
    Running,
    /// Sandbox is destroyed. Terminal.
    Terminated,
}

impl LifecycleState {
    /// Name used in events and API responses (the wire contract).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Running => "running",
            Self::Terminated => "terminated",
        }
    }
}

/// The legal edges of the lifecycle graph.
const TRANSITIONS: &[(LifecycleState, LifecycleState)] = &[
    (LifecycleState::Provisioning, LifecycleState::Running),
    (LifecycleState::Provisioning, LifecycleState::Terminated),
    (LifecycleState::Running, LifecycleState::Terminated),
];

/// Whether `from -> to` is a legal transition.
pub fn is_legal(from: LifecycleState, to: LifecycleState) -> bool {
    TRANSITIONS.iter().any(|&(f, t)| f == from && t == to)
}

/// A sandbox's lifecycle, guarding the current state.
#[derive(Debug, Clone)]
pub struct Lifecycle {
    state: LifecycleState,
}

impl Lifecycle {
    /// A lifecycle starts in `Provisioning` — there is no way to construct a
    /// sandbox in any other state.
    pub fn new() -> Self {
        Self {
            state: LifecycleState::Provisioning,
        }
    }

    pub fn state(&self) -> LifecycleState {
        self.state
    }

    /// Attempt a transition. Illegal transitions are errors; the current state
    /// is left untouched on failure (all-or-nothing).
    pub fn transition(&mut self, to: LifecycleState) -> Result<LifecycleState> {
        if !is_legal(self.state, to) {
            return Err(Error::Lifecycle(format!(
                "cannot transition from {} to {}",
                self.state.as_str(),
                to.as_str()
            )));
        }
        self.state = to;
        Ok(to)
    }

    /// Exec is only legal while running. Every exec entry point checks this.
    pub fn can_exec(&self) -> bool {
        self.state == LifecycleState::Running
    }
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legal_transitions_succeed() {
        let mut lc = Lifecycle::new();
        assert_eq!(lc.state(), LifecycleState::Provisioning);
        lc.transition(LifecycleState::Running).unwrap();
        assert_eq!(lc.state(), LifecycleState::Running);
        lc.transition(LifecycleState::Terminated).unwrap();
        assert_eq!(lc.state(), LifecycleState::Terminated);
    }

    #[test]
    fn provisioning_can_fail_directly_to_terminated() {
        let mut lc = Lifecycle::new();
        lc.transition(LifecycleState::Terminated).unwrap();
    }

    #[test]
    fn illegal_transitions_are_rejected_and_leave_state_untouched() {
        // provisioning → provisioning (self-transition) is illegal.
        let mut lc = Lifecycle::new();
        let err = lc.transition(LifecycleState::Provisioning).unwrap_err();
        assert_eq!(err.code(), "invalid_transition");
        assert_eq!(lc.state(), LifecycleState::Provisioning);

        // running → running (self-transition) is illegal.
        let mut lc = Lifecycle::new();
        lc.transition(LifecycleState::Running).unwrap();
        let err = lc.transition(LifecycleState::Running).unwrap_err();
        assert_eq!(err.code(), "invalid_transition");
        assert_eq!(lc.state(), LifecycleState::Running);

        // terminated → running is illegal and covered by the terminal test.
    }

    #[test]
    fn terminated_is_terminal() {
        for to in [
            LifecycleState::Provisioning,
            LifecycleState::Running,
            LifecycleState::Terminated,
        ] {
            let mut lc = Lifecycle::new();
            lc.transition(LifecycleState::Terminated).unwrap();
            let err = lc.transition(to).unwrap_err();
            assert_eq!(err.code(), "invalid_transition", "{to:?} out of terminated");
            assert_eq!(lc.state(), LifecycleState::Terminated);
        }
    }

    #[test]
    fn exec_only_legal_while_running() {
        let mut lc = Lifecycle::new();
        assert!(!lc.can_exec());
        lc.transition(LifecycleState::Running).unwrap();
        assert!(lc.can_exec());
        lc.transition(LifecycleState::Terminated).unwrap();
        assert!(!lc.can_exec());
    }

    #[test]
    fn state_names_are_the_wire_contract() {
        assert_eq!(LifecycleState::Provisioning.as_str(), "provisioning");
        assert_eq!(LifecycleState::Running.as_str(), "running");
        assert_eq!(LifecycleState::Terminated.as_str(), "terminated");
    }
}
