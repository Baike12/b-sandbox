//! Cleanup sweeps, all funneled through the routes' single removal path —
//! one writer for termination:
//!
//! - **Idle-TTL reaper**: a background sweep that terminates sandboxes whose
//!   data plane has been idle longer than their declared TTL. Paused
//!   sandboxes still hold resources, so they expire on the same clock. The
//!   sweep itself is a pure query (which ids are expired at a given
//!   timestamp), so tests drive it deterministically without sleeps.
//! - **Crash recovery**: at startup, every directory under
//!   `data_root/sandboxes` belonged to a registry entry of the previous
//!   process; the registry is in-memory, so none of them are live. They are
//!   removed before the service accepts traffic.

use std::path::Path;
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

/// Terminate the given sandboxes through the shared removal path. Per-id
/// failures are logged, never propagated: each termination is independent
/// (a sandbox deleted concurrently is skipped, not an error).
pub async fn terminate_all(state: &SharedState, ids: &[String], reason: &str) -> usize {
    let mut terminated = 0;
    for id in ids {
        match crate::routes::remove_sandbox(state, id, reason).await {
            Ok(()) => terminated += 1,
            Err(e) => {
                tracing::warn!(sandbox = %id, reason, error = %e, "termination failed")
            }
        }
    }
    terminated
}

/// Terminate every expired sandbox; returns how many were swept.
pub async fn sweep(state: &SharedState) -> usize {
    let expired_ids = {
        let registry = state.registry.read().unwrap();
        expired(registry.values(), now_ms())
    };
    let terminated = terminate_all(state, &expired_ids, "idle_ttl_expired").await;
    for _ in 0..terminated {
        state.metrics.inc_sandboxes_reaped();
    }
    state.metrics.inc_reaper_sweeps();
    if terminated > 0 {
        tracing::info!(count = terminated, "idle-TTL reaper terminated sandboxes");
    }
    terminated
}

/// Start the periodic sweep. Call once from inside the async runtime.
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

/// Startup crash recovery: remove everything under `data_root/sandboxes`
/// (stale rootfs directories from a previous process) and return how many
/// were removed. Templates under `data_root/templates` are immutable and
/// never touched.
pub async fn recover_orphans(data_root: &Path) -> usize {
    let dir = data_root.join("sandboxes");
    let mut read_dir = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "cannot scan for orphaned sandboxes");
            return 0;
        }
    };
    let mut removed = 0usize;
    while let Ok(Some(entry)) = read_dir.next_entry().await {
        let path = entry.path();
        let result = if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            tokio::fs::remove_dir_all(&path).await
        } else {
            tokio::fs::remove_file(&path).await
        };
        match result {
            Ok(()) => removed += 1,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "orphan removal failed")
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn recover_orphans_removes_sandbox_dirs_but_not_templates() {
        let tmp = tempfile::tempdir().unwrap();
        let sandboxes = tmp.path().join("sandboxes");
        tokio::fs::create_dir_all(sandboxes.join("sbx-dead1"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(sandboxes.join("sbx-dead2"))
            .await
            .unwrap();
        tokio::fs::write(sandboxes.join("stray.tmp"), "x")
            .await
            .unwrap();
        let templates = tmp.path().join("templates");
        tokio::fs::create_dir_all(templates.join("tpl-1"))
            .await
            .unwrap();
        tokio::fs::write(templates.join("tpl-1/marker"), "keep")
            .await
            .unwrap();

        assert_eq!(recover_orphans(tmp.path()).await, 3);

        assert!(
            !sandboxes.exists()
                || tokio::fs::read_dir(&sandboxes)
                    .await
                    .unwrap()
                    .next_entry()
                    .await
                    .unwrap()
                    .is_none()
        );
        assert!(
            templates.join("tpl-1/marker").exists(),
            "templates are untouched"
        );
        // A second pass finds nothing.
        assert_eq!(recover_orphans(tmp.path()).await, 0);
    }

    #[tokio::test]
    async fn recover_orphans_on_fresh_data_root_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(recover_orphans(tmp.path()).await, 0);
    }
}
