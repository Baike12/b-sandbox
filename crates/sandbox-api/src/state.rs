//! Service state: the backend handle, the sandbox registry, and identity
//! minting.
//!
//! Concurrency model:
//! - The registry (`HashMap<String, Arc<SandboxRecord>>`) is behind one
//!   `RwLock`; handlers clone the `Arc` and drop the registry lock
//!   immediately, so execs never hold it.
//! - Per-sandbox mutable state (lifecycle, event log) lives behind its own
//!   mutexes inside the record.
//! - Sandbox ids are minted here from /dev/urandom — callers never name
//!   sandboxes in v1.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use sandbox_core::{EventLog, Lifecycle, SandboxBackend, SandboxContext};

use crate::ledger::{UsageLedger, now_ms};

pub type SharedState = Arc<AppState>;

pub struct AppState {
    pub backend: Arc<dyn SandboxBackend>,
    pub registry: RwLock<HashMap<String, Arc<SandboxRecord>>>,
    /// Idempotency-Key header → created sandbox id (M1: creation only).
    pub idempotency: RwLock<HashMap<String, String>>,
    /// Root for backend data (rootfs dirs live under data_root/sandboxes).
    pub data_root: std::path::PathBuf,
    /// Service-level counters (lock-free atomics; scrape derives gauges).
    pub metrics: crate::metrics::Metrics,
    /// Shared bearer token. None = open service (localhost + nginx posture);
    /// Some = every route except /health requires `Authorization: Bearer …`.
    pub auth_token: Option<String>,
    /// (method, path-pattern) pairs copied from the route table at build
    /// time; the observability middleware maps raw request paths back to
    /// patterns so metrics cardinality is bounded by the route table.
    pub route_patterns: Vec<(&'static str, &'static str)>,
    /// Set just before graceful shutdown drains the server; long-lived SSE
    /// tails watch it and end their streams so connections can close.
    pub shutdown: Arc<std::sync::atomic::AtomicBool>,
}

pub struct SandboxRecord {
    pub ctx: SandboxContext,
    pub lifecycle: Mutex<Lifecycle>,
    pub events: Mutex<EventLog>,
    /// Accounting side of the budget: what happened through the API.
    pub usage: Mutex<UsageLedger>,
    /// Idle TTL in milliseconds; the reaper terminates the sandbox after
    /// this much time without data-plane activity. `None` = no TTL.
    pub ttl_ms: Option<u64>,
}

impl SandboxRecord {
    pub fn new(ctx: SandboxContext, ttl_ms: Option<u64>) -> Self {
        Self {
            ctx,
            lifecycle: Mutex::new(Lifecycle::new()),
            events: Mutex::new(EventLog::new()),
            usage: Mutex::new(UsageLedger::new(now_ms())),
            ttl_ms,
        }
    }
}

impl AppState {
    pub fn new(backend: Arc<dyn SandboxBackend>, data_root: std::path::PathBuf) -> Self {
        Self {
            backend,
            registry: RwLock::new(HashMap::new()),
            idempotency: RwLock::new(HashMap::new()),
            data_root,
            metrics: Default::default(),
            auth_token: None,
            route_patterns: crate::routes::route_patterns(),
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Require a bearer token on every route except /health.
    pub fn with_auth_token(mut self, token: Option<String>) -> Self {
        self.auth_token = token;
        self
    }

    /// Clone the record handle out of the registry (releasing the registry
    /// lock immediately).
    pub fn get(&self, id: &str) -> Option<Arc<SandboxRecord>> {
        self.registry.read().unwrap().get(id).cloned()
    }
}

/// Mint a sandbox id: `sbx-<16 hex chars>` from /dev/urandom. The charset is
/// restricted so it is safe as a single path segment everywhere.
pub fn new_sandbox_id() -> String {
    let mut buf = [0u8; 8];
    // /dev/urandom exists on every unix target of this service.
    let mut file = std::fs::File::open("/dev/urandom")
        .unwrap_or_else(|e| panic!("cannot open /dev/urandom: {e}"));
    use std::io::Read;
    file.read_exact(&mut buf)
        .unwrap_or_else(|e| panic!("cannot read /dev/urandom: {e}"));
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    format!("sbx-{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_prefixed_and_charset_restricted() {
        for _ in 0..32 {
            let id = new_sandbox_id();
            assert!(id.starts_with("sbx-"));
            assert!(
                id[4..]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "hex only: {id}"
            );
            assert_eq!(id.len(), 4 + 16);
        }
    }

    #[test]
    fn ids_do_not_collide_in_a_small_sample() {
        let set: std::collections::HashSet<String> = (0..256).map(|_| new_sandbox_id()).collect();
        assert_eq!(set.len(), 256, "64-bit ids must not collide in 256 draws");
    }
}
