//! sandbox-core: pure domain model for the b-sandbox service.
//!
//! This crate holds everything that must not touch platform APIs: the policy
//! model, the lifecycle state machine, budgets, the event log, and the
//! `SandboxBackend` seam. Isolation backends live in sibling crates and
//! implement the trait defined here.

pub mod budget;
pub mod error;
pub mod events;
pub mod lifecycle;
pub mod policy;

pub use budget::Budget;
pub use error::{BackendFailureKind, Error, Result};
pub use events::{Event, EventKind, EventLog};
pub use lifecycle::LifecycleState;
