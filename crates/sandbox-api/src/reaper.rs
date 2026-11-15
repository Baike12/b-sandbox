//! Idle-TTL reaper: a background sweep that terminates sandboxes whose data
//! plane has been idle longer than their declared TTL. Paused sandboxes
//! still hold resources, so they expire on the same clock.
//!
//! Termination reuses the routes' single removal path — one writer for
//! termination. The sweep itself is a pure query (which ids are expired at
//! a given timestamp), so tests drive it deterministically without sleeps.

use std::sync::Arc;

use crate::ledger::now_ms;
use crate::state::{SandboxRecord, SharedState};

/// Sweep cadence: an idle sandbox may outlive its TTL by at most this.
pub const SWEEP_INTERVAL_MS: u64 = 15_000;

/// Records whose idle time has reached their TTL, in iteration order.
pub fn expired<'a>(records: impl Iterator<Item = &'a Arc<SandboxRecord>>, now: u64) -> Vec<String> {
    records
        .filter(|r| {
            r.ttl_ms.is_some_and(|ttl| {
                let idle = now.saturating_sub(r.usage.lock().unwrap().last_activity_ms());
                idle >= ttl
            })
        })
        .map(|r| r.ctx.id.clone())
        .collect()
}

/// Terminate every expired sandbox; returns how many were swept. A sandbox
/// removed concurrently between the query and its termination is skipped
/// (the shared removal path answers 404, which is not an error here).
pub async fn sweep(state: &SharedState) -> usize {
    let expired_ids = {
        let registry = state.registry.read().unwrap();
        expired(registry.values(), now_ms())
    };
    for id in &expired_ids {
        let _ = crate::routes::remove_sandbox(state, id, "idle_ttl_expired").await;
    }
    expired_ids.len()
}

/// Start the periodic sweep. Call once from `main` after the state exists.
pub fn spawn(state: SharedState) {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_millis(SWEEP_INTERVAL_MS));
        loop {
            interval.tick().await;
            sweep(&state).await;
        }
    });
}
