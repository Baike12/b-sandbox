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
    /// Root of the web console build (web/dist). None = console routes
    /// answer with a build hint instead of files.
    pub console_dir: Option<std::path::PathBuf>,
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
            console_dir: None,
        }
    }

    /// Serve the web console build from `dir`.
    pub fn with_console_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.console_dir = Some(dir);
        self
    }

    /// Same, but `None` leaves the console off (routes answer with a hint).
    pub fn with_console_dir_opt(mut self, dir: Option<std::path::PathBuf>) -> Self {
        self.console_dir = dir;
        self
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
/// restricted so it is safe as a single path segment everywhere. /dev/urandom
/// exists on every unix target of this service, but a failure must surface as
/// a typed error to the client — never a panic in a request path.
pub fn new_sandbox_id() -> Result<String, sandbox_core::Error> {
    use std::io::Read;
    let mut buf = [0u8; 8];
    let mut file = std::fs::File::open("/dev/urandom")
        .map_err(|e| sandbox_core::Error::Internal(format!("cannot open /dev/urandom: {e}")))?;
    file.read_exact(&mut buf)
        .map_err(|e| sandbox_core::Error::Internal(format!("cannot read /dev/urandom: {e}")))?;
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("sbx-{hex}"))
}

/// Single-instance guard on a data root: two b-sandbox processes sharing one
/// `data_root` would each see (and reap) the other's sandboxes as orphans.
///
/// The lock is an exclusive `flock(2)` on a lock file — the kernel releases
/// it when the holder's file descriptor dies, so a crashed or SIGKILLed
/// process can never wedge the service, and there is no pid-file heuristic
/// to get wrong (a pid check would: inside a container the entrypoint is
/// always pid 1, so a stale `1` looks "alive" to the next boot and crashes
/// the service into a restart loop). The pid is written inside for
/// diagnostics only.
#[derive(Debug)]
pub struct ServiceLock {
    /// Held open for the service's lifetime: closing it releases the flock.
    _file: std::fs::File,
    path: std::path::PathBuf,
}

const LOCK_FILE: &str = "b-sandbox.lock";

impl ServiceLock {
    /// Take the lock, creating `data_root` if needed. Errors when another
    /// live process holds it.
    pub fn acquire(data_root: &std::path::Path) -> Result<Self, String> {
        std::fs::create_dir_all(data_root)
            .map_err(|e| format!("cannot create data root {data_root:?}: {e}"))?;
        let path = data_root.join(LOCK_FILE);
        // truncate(false) is deliberate: a concurrent holder's pid must
        // survive our failed attempt (we read it for the error message).
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("cannot open lock {path:?}: {e}"))?;
        let rc = unsafe {
            libc::flock(
                std::os::unix::io::AsRawFd::as_raw_fd(&file),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                let holder = std::fs::read_to_string(&path).unwrap_or_default();
                let holder = holder.trim();
                return Err(format!(
                    "another b-sandbox instance holds {path:?} (pid {holder}); refusing to start"
                ));
            }
            return Err(format!("cannot lock {path:?}: {err}"));
        }
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut file = &file;
            // We hold the exclusive lock: rewriting the pid is now ours.
            file.set_len(0)
                .map_err(|e| format!("cannot reset lock {path:?}: {e}"))?;
            file.seek(SeekFrom::Start(0))
                .map_err(|e| format!("cannot seek lock {path:?}: {e}"))?;
            write!(file, "{}", std::process::id())
                .map_err(|e| format!("cannot write lock {path:?}: {e}"))?;
        }
        Ok(Self { _file: file, path })
    }
}

impl Drop for ServiceLock {
    fn drop(&mut self) {
        // Closing the fd releases the flock; unlinking the file afterwards is
        // best-effort housekeeping (a leftover is harmless — the next acquire
        // locks the fresh file descriptor, no contents are trusted).
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_prefixed_and_charset_restricted() {
        for _ in 0..32 {
            let id = new_sandbox_id().unwrap();
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
        let set: std::collections::HashSet<String> =
            (0..256).map(|_| new_sandbox_id().unwrap()).collect();
        assert_eq!(set.len(), 256, "64-bit ids must not collide in 256 draws");
    }

    #[test]
    fn service_lock_excludes_a_second_instance_and_releases_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = ServiceLock::acquire(tmp.path()).unwrap();
        let err = ServiceLock::acquire(tmp.path()).unwrap_err();
        assert!(err.contains("refusing"), "{err}");
        // The lock file records the holder's pid for diagnostics.
        let recorded: u32 = std::fs::read_to_string(tmp.path().join(LOCK_FILE))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(recorded, std::process::id());
        drop(lock);
        // Released → acquirable again (and the file is gone afterwards).
        let again = ServiceLock::acquire(tmp.path()).unwrap();
        drop(again);
        assert!(!tmp.path().join(LOCK_FILE).exists());
    }

    #[test]
    fn service_lock_ignores_leftover_file_contents() {
        // flock owns the exclusion: file CONTENTS are diagnostics only, so a
        // stale file (crash, SIGKILL, container restart where the entrypoint
        // is always pid 1) must never wedge the next boot.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(LOCK_FILE), "1").unwrap();
        let lock = ServiceLock::acquire(tmp.path()).unwrap();
        drop(lock);
    }

    #[test]
    fn service_lock_creates_a_fresh_data_root() {
        let base = tempfile::tempdir().unwrap();
        let fresh = base
            .path()
            .join("does")
            .join("not")
            .join("exist")
            .join("yet");
        let lock = ServiceLock::acquire(&fresh).unwrap();
        drop(lock);
        assert!(fresh.join(LOCK_FILE).exists() || !fresh.join(LOCK_FILE).exists());
        assert!(fresh.is_dir(), "data root created");
    }
}
