//! Linux Bubblewrap process backend.
//!
//! One sandbox = one scratch rootfs directory; every `exec` spawns `bwrap`
//! with an argv generated from the sandbox policy (see [`profile`]). The
//! backend compiles everywhere so the workspace builds on any host, but
//! `health()` reports availability only when the `bwrap` binary is present;
//! integration tests run behind `#[cfg(target_os = "linux")]`.
//!
//! Capability contract (same honest set as the Seatbelt tier in M1):
//! - `FS_READWRITE` / `FS_READONLY` / `FS_DENIED` via bind mounts and tmpfs
//!   masks (unit-tested in argv space, integration-tested on Linux).
//! - `NET_EGRESS_DEFAULT` via `--unshare-net`.
//! - `BUDGET_PIDS` / `BUDGET_NOFILE` via rlimits applied in `pre_exec` inside
//!   the sandbox.
//! - **No `BUDGET_MEMORY`** in M1: the declaration would be untestable on the
//!   current dev host. Linux cgroups-based memory budgets arrive in M2 with
//!   Linux CI.
//! - **No `NET_EGRESS_RULES`** in M1 (no slirp/iptables wiring yet).

pub mod profile;

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use sandbox_core::backend::EXEC_TIMEOUT_MS_CEILING;
use sandbox_core::error::{BackendFailureKind, Error, Result};
use sandbox_core::policy::SandboxSpec;
use sandbox_core::{
    BackendCapabilities, BackendHealth, ExecOutcome, ExecRequest, SandboxBackend, SandboxContext,
};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Bubblewrap backend implementation.
pub struct BwrapBackend {
    /// Directory under which sandbox rootfs directories are created.
    data_root: PathBuf,
}

impl BwrapBackend {
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
        }
    }

    fn rootfs_for(&self, id: &str) -> Result<PathBuf> {
        if id.is_empty()
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(Error::Policy(format!("invalid sandbox id {id:?}")));
        }
        let rootfs = self.data_root.join("sandboxes").join(id);
        if !rootfs.starts_with(&self.data_root) || rootfs == self.data_root {
            return Err(Error::Internal(format!(
                "rootfs path escaped data root: {}",
                rootfs.display()
            )));
        }
        Ok(rootfs)
    }

    fn bwrap_binary() -> &'static str {
        "bwrap"
    }
}

/// Read a stream into `buf` under a shared byte budget (same contract as the
/// Seatbelt backend: check-and-decrement under one lock, so concurrent
/// readers cannot overshoot the cap together).
async fn read_capped(
    mut pipe: impl tokio::io::AsyncRead + Unpin,
    budget: std::sync::Arc<std::sync::Mutex<i64>>,
    truncated: std::sync::Arc<std::sync::atomic::AtomicBool>,
    buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
) -> std::io::Result<()> {
    use std::sync::atomic::Ordering;

    let mut chunk = [0u8; 8192];
    loop {
        let take_limit = {
            let left = budget.lock().unwrap();
            if *left <= 0 {
                truncated.store(true, Ordering::Relaxed);
                return Ok(());
            }
            *left
        };
        let n = pipe.read(&mut chunk).await?;
        if n == 0 {
            return Ok(()); // EOF
        }
        let take = n.min(take_limit as usize);
        buf.lock().unwrap().extend_from_slice(&chunk[..take]);
        {
            let mut left = budget.lock().unwrap();
            *left -= take as i64;
            if take < n || *left <= 0 {
                truncated.store(true, Ordering::Relaxed);
                return Ok(());
            }
        }
    }
}

fn set_rlimit(resource: i32, value: u64) -> std::io::Result<()> {
    let mut lim = unsafe {
        let mut lim = std::mem::zeroed::<libc::rlimit>();
        if libc::getrlimit(resource, &mut lim) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        lim
    };
    lim.rlim_cur = value.min(lim.rlim_max);
    let rc = unsafe { libc::setrlimit(resource, &lim) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[async_trait]
impl SandboxBackend for BwrapBackend {
    fn name(&self) -> &'static str {
        "bwrap"
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::FS_READWRITE
            .union(BackendCapabilities::FS_READONLY)
            .union(BackendCapabilities::FS_DENIED)
            .union(BackendCapabilities::NET_EGRESS_DEFAULT)
            .union(BackendCapabilities::BUDGET_PIDS)
            .union(BackendCapabilities::BUDGET_NOFILE)
    }

    fn health(&self) -> BackendHealth {
        match which_bwrap() {
            Ok(_) => BackendHealth::available(),
            Err(reason) => BackendHealth::unavailable(reason),
        }
    }

    async fn provision(&self, id: &str, spec: &SandboxSpec) -> Result<SandboxContext> {
        let (fs, net) = spec.validate()?;
        sandbox_core::check_policy_support(spec, self.capabilities())?;
        let rootfs = self.rootfs_for(id)?;
        tokio::fs::create_dir_all(&rootfs)
            .await
            .map_err(|e| Error::Backend {
                kind: BackendFailureKind::Io,
                message: format!("create rootfs {}: {e}", rootfs.display()),
            })?;
        for sub in ["tmp", "home"] {
            tokio::fs::create_dir_all(rootfs.join(sub))
                .await
                .map_err(|e| Error::Backend {
                    kind: BackendFailureKind::Io,
                    message: format!("create {sub}: {e}"),
                })?;
        }
        // Canonicalize so the profile rules, the exec cwd, and the file data
        // plane all speak the same (kernel-canonical) path spelling.
        let rootfs = tokio::fs::canonicalize(&rootfs)
            .await
            .map_err(|e| Error::Backend {
                kind: BackendFailureKind::Io,
                message: format!("canonicalize rootfs {}: {e}", rootfs.display()),
            })?;
        // A named template seeds the rootfs (reflink snapshot of a committed
        // sandbox). Missing templates are policy errors, never empty rootfs.
        if let Some(name) = &spec.template {
            sandbox_core::materialize_template(&self.data_root, name, &rootfs)?;
        }
        Ok(SandboxContext {
            id: id.to_string(),
            rootfs,
            spec: spec.clone(),
            fs,
            net,
        })
    }

    async fn exec(&self, ctx: &SandboxContext, req: ExecRequest) -> Result<ExecOutcome> {
        req.validate()?;

        // Working directory: sandbox-relative, must exist.
        let rel = sandbox_core::policy::normalize_relative(&req.cwd)?;
        let cwd = if rel.as_os_str().is_empty() {
            ctx.rootfs.clone()
        } else {
            ctx.rootfs.join(&rel)
        };
        if !cwd.is_dir() {
            return Err(Error::Policy(format!(
                "exec cwd {:?} does not exist in sandbox",
                req.cwd
            )));
        }

        let mut env = profile::base_env(&ctx.rootfs);
        for (k, v) in &req.env {
            env.push((k.clone(), v.clone()));
        }
        let argv = profile::generate(&ctx.fs, &ctx.net, &ctx.rootfs, &env);

        let mut cmd = Command::new(Self::bwrap_binary());
        cmd.args(&argv)
            .arg(&req.command) // after "--"
            .args(&req.args)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // Process group (for the timeout kill) and rlimits (applied inside
        // the sandbox — pre_exec runs in the bwrap'd child before it execs
        // the workload) both set on the underlying std command.
        let pids_max = ctx.spec.budget.pids_max as u64;
        let max_open_files = ctx.spec.exec.max_open_files as u64;
        {
            use std::os::unix::process::CommandExt as _;
            let std_cmd = cmd.as_std_mut();
            std_cmd.process_group(0);
            unsafe {
                std_cmd.pre_exec(move || {
                    set_rlimit(libc::RLIMIT_NPROC, pids_max)?;
                    set_rlimit(libc::RLIMIT_NOFILE, max_open_files)
                });
            }
        }

        let mut child = cmd.spawn().map_err(|e| Error::Backend {
            kind: BackendFailureKind::Spawn,
            message: format!("spawn bwrap {:?}: {e}", req.command),
        })?;
        let pid = child.id();

        let stdout_pipe = child
            .stdout
            .take()
            .ok_or_else(|| Error::Internal("stdout pipe missing".into()))?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| Error::Internal("stderr pipe missing".into()))?;

        let budget =
            std::sync::Arc::new(std::sync::Mutex::new(ctx.spec.exec.max_output_bytes as i64));
        let truncated = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let out_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let err_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        let out_reader = tokio::spawn(read_capped(
            stdout_pipe,
            budget.clone(),
            truncated.clone(),
            out_buf.clone(),
        ));
        let err_reader = tokio::spawn(read_capped(
            stderr_pipe,
            budget.clone(),
            truncated.clone(),
            err_buf.clone(),
        ));

        let timeout_ms = req.timeout_ms.unwrap_or(EXEC_TIMEOUT_MS_CEILING);
        let wait_result =
            tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), child.wait()).await;

        let (timed_out, exit_code) = match wait_result {
            Ok(Ok(status)) => (false, status.code()),
            Ok(Err(e)) => {
                return Err(Error::Backend {
                    kind: BackendFailureKind::Other,
                    message: format!("wait: {e}"),
                });
            }
            Err(_elapsed) => {
                // Kill the whole process group, then reap.
                if let Some(pid) = pid {
                    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                }
                let status = child.wait().await;
                (true, status.ok().and_then(|s| s.code()))
            }
        };

        let _ = tokio::join!(out_reader, err_reader);

        Ok(ExecOutcome {
            exit_code,
            timed_out,
            stdout: String::from_utf8_lossy(&out_buf.lock().unwrap()).into_owned(),
            stderr: String::from_utf8_lossy(&err_buf.lock().unwrap()).into_owned(),
            truncated: truncated.load(std::sync::atomic::Ordering::Relaxed),
        })
    }

    async fn stop(&self, _ctx: &SandboxContext) -> Result<()> {
        // M1 execs are synchronous; nothing left in flight.
        Ok(())
    }

    async fn deprovision(&self, ctx: &SandboxContext) -> Result<()> {
        let expected = tokio::fs::canonicalize(self.rootfs_for(&ctx.id)?)
            .await
            .map_err(|e| Error::Backend {
                kind: BackendFailureKind::Io,
                message: format!("canonicalize rootfs: {e}"),
            })?;
        if ctx.rootfs != expected {
            return Err(Error::Internal(
                "rootfs does not match backend data root".into(),
            ));
        }
        tokio::fs::remove_dir_all(&ctx.rootfs)
            .await
            .map_err(|e| Error::Backend {
                kind: BackendFailureKind::Io,
                message: format!("remove rootfs {}: {e}", ctx.rootfs.display()),
            })?;
        Ok(())
    }
}

/// Locate the bwrap binary; `Err` carries the human-readable unavailability
/// reason for /health.
fn which_bwrap() -> std::result::Result<PathBuf, String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(BwrapBackend::bwrap_binary());
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err("bwrap binary not found in PATH".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_reports_missing_bwrap_honestly() {
        // On a host without bwrap (e.g. this macOS dev machine) the backend
        // must report unavailable, not pretend to work.
        let backend = BwrapBackend::new(std::env::temp_dir());
        let health = backend.health();
        if which_bwrap().is_err() {
            assert!(!health.available);
            assert!(health.reason.is_some());
        } else {
            assert!(health.available);
        }
    }
}
