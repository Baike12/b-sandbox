//! macOS Seatbelt process backend.
//!
//! One sandbox = one scratch rootfs directory + a Seatbelt profile compiled
//! from its policy. Every `exec` spawns a fresh process that applies the
//! profile in `pre_exec` (fork→exec seam), runs under rlimits derived from the
//! budget, and is reaped — or killed via its process group when the exec
//! times out.
//!
//! Capability contract (every bit has enforcement evidence in the tests):
//! - `FS_READWRITE` / `FS_READONLY` / `FS_DENIED`: profile rules; integration
//!   tests flip each set and observe writes/reads succeed or fail.
//! - `NET_EGRESS_DEFAULT`: `deny/allow network-outbound` line; integration
//!   tests connect to a live local listener both ways.
//! - `BUDGET_PIDS`: `RLIMIT_NPROC` set in `pre_exec`; the child reads it back.
//!   macOS accounts NPROC per-uid, so the ceiling is approximate in the
//!   fail-closed direction (may over-restrict, never under-restrict).
//! - `BUDGET_NOFILE`: `RLIMIT_NOFILE` set in `pre_exec`; child-visible.
//! - **No `BUDGET_MEMORY`**: macOS `setrlimit(RLIMIT_AS)` fails with EINVAL
//!   and the kernel does not honor the limit. Explicit memory budgets are
//!   rejected at validation (`check_policy_support`) instead of being
//!   pretended — memory budgeting arrives with the Linux cgroups tier.
//!
//! `stop()` is a no-op for M1: execs are synchronous, so nothing is left in
//! flight when the registry tears a sandbox down.

pub mod profile;

#[cfg(target_os = "macos")]
pub mod ffi;

use std::path::PathBuf;

use async_trait::async_trait;
use sandbox_core::error::{BackendFailureKind, Error, Result};
use sandbox_core::policy::SandboxSpec;
use sandbox_core::{
    BackendCapabilities, BackendHealth, ExecOutcome, ExecRequest, SandboxBackend, SandboxContext,
};

/// Seatbelt backend implementation.
pub struct SeatbeltBackend {
    /// Directory under which sandbox rootfs directories are created.
    data_root: PathBuf,
    /// `ip:port` of the credential-injection egress proxy, when the service
    /// runs one. Proxy-mode specs are rejected at provision without it.
    egress_proxy: Option<String>,
}

impl SeatbeltBackend {
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
            egress_proxy: None,
        }
    }

    /// Route proxy-mode sandboxes through the credential-injection proxy.
    pub fn with_egress_proxy(mut self, addr: impl Into<String>) -> Self {
        self.egress_proxy = Some(addr.into());
        self
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
        // Paranoia: the id path segment cannot traverse, but assert the shape
        // anyway — deprovision deletes trees, so this check guards the worst
        // possible bug in this backend.
        if !rootfs.starts_with(&self.data_root) || rootfs == self.data_root {
            return Err(Error::Internal(format!(
                "rootfs path escaped data root: {}",
                rootfs.display()
            )));
        }
        Ok(rootfs)
    }
}

#[async_trait]
impl SandboxBackend for SeatbeltBackend {
    fn name(&self) -> &'static str {
        "seatbelt"
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::FS_READWRITE
            .union(BackendCapabilities::FS_READONLY)
            .union(BackendCapabilities::FS_DENIED)
            .union(BackendCapabilities::NET_EGRESS_DEFAULT)
            // Per-port egress hole for the credential proxy (Seatbelt
            // `(to tcp ip:port)`); proven by the proxy-mode integration test.
            .union(BackendCapabilities::NET_EGRESS_PROXY)
            // macOS tier: pids + nofile only. Memory budgets are not
            // enforceable here (see BUDGET_MEMORY docs in sandbox-core).
            .union(BackendCapabilities::BUDGET_PIDS)
            .union(BackendCapabilities::BUDGET_NOFILE)
    }

    fn health(&self) -> BackendHealth {
        #[cfg(target_os = "macos")]
        {
            BackendHealth::available()
        }
        #[cfg(not(target_os = "macos"))]
        {
            BackendHealth::unavailable("seatbelt backend is macOS-only")
        }
    }

    async fn provision(&self, id: &str, spec: &SandboxSpec) -> Result<SandboxContext> {
        // Fail-closed belt-and-braces: never trust the caller's checks.
        let (fs, net) = spec.validate()?;
        sandbox_core::check_policy_support(spec, self.capabilities())?;
        if spec.net.proxy {
            // Seatbelt's network filter only accepts `*` or `localhost` as
            // the host, so the single egress hole must be a loopback proxy.
            match self.egress_proxy.as_deref() {
                None => {
                    return Err(Error::Policy(
                        "net.proxy requested but this service runs no egress proxy".into(),
                    ));
                }
                Some(addr) => macos_exec::validate_loopback_addr(addr)?,
            }
        }
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
        // Seatbelt evaluates rules against RESOLVED paths. On macOS /var and
        // /tmp are symlinks into /private, so an unresolved rootfs path (e.g.
        // /var/folders/...) would silently fail to match the profile rule and
        // the workload would abort at startup. Canonicalize once, here, and
        // store the resolved path in the context.
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
        #[cfg(target_os = "macos")]
        {
            return macos_exec::exec(ctx, req, self.egress_proxy.as_deref()).await;
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = ctx;
            Err(Error::Backend {
                kind: BackendFailureKind::Other,
                message: "seatbelt backend is macOS-only".into(),
            })
        }
    }

    async fn stop(&self, _ctx: &SandboxContext) -> Result<()> {
        // M1 execs are synchronous: by the time exec returns, no process tree
        // is left. Container/microVM backends kill their workload group here.
        Ok(())
    }

    async fn deprovision(&self, ctx: &SandboxContext) -> Result<()> {
        // ctx.rootfs is the canonicalized path (see provision); canonicalize
        // the expectation the same way before comparing.
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

#[cfg(target_os = "macos")]
mod macos_exec {
    use std::collections::BTreeSet;
    use std::io;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use sandbox_core::backend::EXEC_TIMEOUT_MS_CEILING;
    use sandbox_core::error::{BackendFailureKind, Error, Result};
    use sandbox_core::policy::{ResolvedFsPolicy, normalize_relative};
    use sandbox_core::{ExecOutcome, ExecRequest, SandboxContext};
    use tokio::io::AsyncReadExt;
    use tokio::process::Command;

    use super::ffi;
    use super::profile;

    /// Read a stream into `buf` under a shared byte budget. The budget check
    /// and decrement happen under one lock, so concurrent stdout/stderr
    /// readers can never overshoot the cap together. On exhaustion, reading
    /// stops and `truncated` is set — dropping the pipe is the backpressure.
    async fn read_capped(
        mut pipe: impl tokio::io::AsyncRead + Unpin,
        budget: Arc<Mutex<i64>>,
        truncated: Arc<AtomicBool>,
        buf: Arc<Mutex<Vec<u8>>>,
    ) -> io::Result<()> {
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

    /// Lower only the *soft* limit; the hard limit stays untouched (lowering
    /// the hard limit requires privilege and fails on stock macOS with EINVAL).
    /// The value is clamped below the existing hard limit so a caller asking
    /// for more than the host allows gets the host max, not a failed spawn.
    fn set_rlimit(resource: i32, value: u64) -> io::Result<()> {
        let mut lim = unsafe {
            let mut lim = std::mem::zeroed::<libc::rlimit>();
            if libc::getrlimit(resource, &mut lim) != 0 {
                return Err(io::Error::last_os_error());
            }
            lim
        };
        lim.rlim_cur = value.min(lim.rlim_max);
        let rc = unsafe { libc::setrlimit(resource, &lim) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Seatbelt matches the path string as the workload passes it — it does
    /// NOT re-resolve symlinks against the profile. A workload that opens
    /// `/var/folders/...` is denied by a rule that only allows the resolved
    /// `/private/var/folders/...`, and vice versa. So both spellings of every
    /// policy path are allowed (or denied): the submitted form and the
    /// kernel-resolved form. If the two spellings end up in conflicting sets,
    /// the later rule wins (deny-last ordering keeps that fail-closed).
    /// Validate `ip:port` is a loopback endpoint (Seatbelt network filters only
    /// permit `*` or `localhost`; anything else would silently become localhost).
    pub(super) fn validate_loopback_addr(addr: &str) -> Result<()> {
        let err = || {
            Error::Policy(format!(
                "egress proxy address {addr:?} invalid: must be 127.0.0.1:PORT or [::1]:PORT on the loopback"
            ))
        };
        let (ip_raw, port_raw) = addr.rsplit_once(':').ok_or_else(err)?;
        let ip_raw = ip_raw.trim_start_matches('[').trim_end_matches(']');
        let ip: std::net::IpAddr = ip_raw.parse().map_err(|_| err())?;
        if !ip.is_loopback() {
            return Err(err());
        }
        let port: u16 = port_raw.parse().map_err(|_| err())?;
        if port == 0 {
            return Err(err());
        }
        Ok(())
    }

    /// Proxy-mode execs get standard proxy env vars pointing at the service's
    /// credential-injection proxy. Empty NO_PROXY: nothing bypasses the proxy.
    fn proxy_env(ctx: &SandboxContext, egress_proxy: Option<&str>) -> Vec<(String, String)> {
        if !ctx.net.proxy {
            return Vec::new();
        }
        match egress_proxy {
            None => Vec::new(), // unreachable: provision rejects proxy mode without one
            Some(addr) => {
                let url = format!("http://{addr}");
                ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
                    .iter()
                    .map(|k| (k.to_string(), url.clone()))
                    .chain([("NO_PROXY".to_string(), String::new())])
                    .collect()
            }
        }
    }

    fn resolve_policy_paths(fs: &ResolvedFsPolicy) -> ResolvedFsPolicy {
        fn resolve_set(set: &BTreeSet<PathBuf>) -> BTreeSet<PathBuf> {
            let mut out = BTreeSet::new();
            for p in set {
                out.insert(p.clone());
                if let Ok(resolved) = std::fs::canonicalize(p) {
                    out.insert(resolved);
                }
            }
            out
        }
        ResolvedFsPolicy {
            readwrite: resolve_set(&fs.readwrite),
            readonly: resolve_set(&fs.readonly),
            denied: resolve_set(&fs.denied),
        }
    }

    pub(super) async fn exec(
        ctx: &SandboxContext,
        req: ExecRequest,
        egress_proxy: Option<&str>,
    ) -> Result<ExecOutcome> {
        // Working directory: sandbox-relative, must exist.
        let rel = normalize_relative(&req.cwd)?;
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

        let scheme = profile::generate(
            &resolve_policy_paths(&ctx.fs),
            &ctx.net,
            &ctx.rootfs,
            egress_proxy,
        );
        if std::env::var_os("SANDBOX_DEBUG_PROFILE").is_some() {
            eprintln!("---- generated scheme ----\n{scheme}---- end scheme ----");
        }
        let pids_max = ctx.spec.budget.pids_max as u64;
        let max_open_files = ctx.spec.exec.max_open_files as u64;

        let home = ctx.rootfs.join("home").to_string_lossy().into_owned();
        let tmpdir = ctx.rootfs.join("tmp").to_string_lossy().into_owned();
        let mut cmd = Command::new(&req.command);
        cmd.args(&req.args)
            .env_clear()
            .envs([
                (
                    "PATH",
                    "/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:/opt/homebrew/bin",
                ),
                ("HOME", home.as_str()),
                ("TMPDIR", tmpdir.as_str()),
            ])
            .envs(proxy_env(ctx, egress_proxy))
            .envs(req.env.iter())
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        // The fork→exec seam: setsid + rlimits + Seatbelt profile apply to the
        // child only. Any failure here aborts the spawn.
        {
            let scheme = scheme.clone();
            use std::os::unix::process::CommandExt as _;
            unsafe {
                cmd.as_std_mut().pre_exec(move || {
                    if libc::setsid() < 0 {
                        return Err(io::Error::other(format!(
                            "pre_exec setsid: {}",
                            io::Error::last_os_error()
                        )));
                    }
                    set_rlimit(libc::RLIMIT_NPROC, pids_max)
                        .map_err(|e| io::Error::other(format!("pre_exec setrlimit NPROC: {e}")))?;
                    set_rlimit(libc::RLIMIT_NOFILE, max_open_files)
                        .map_err(|e| io::Error::other(format!("pre_exec setrlimit NOFILE: {e}")))?;
                    ffi::apply(&scheme)
                        .map_err(|e| io::Error::other(format!("pre_exec sandbox_init: {e}")))
                });
            }
        }

        let mut child = cmd.spawn().map_err(|e| Error::Backend {
            kind: BackendFailureKind::Spawn,
            message: format!("spawn {:?}: {e}", req.command),
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

        let budget = Arc::new(Mutex::new(ctx.spec.exec.max_output_bytes as i64));
        let truncated = Arc::new(AtomicBool::new(false));
        let out_buf = Arc::new(Mutex::new(Vec::new()));
        let err_buf = Arc::new(Mutex::new(Vec::new()));

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
            tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait()).await;

        let (timed_out, exit_code) = match wait_result {
            Ok(Ok(status)) => (false, status.code()),
            Ok(Err(e)) => {
                return Err(Error::Backend {
                    kind: BackendFailureKind::Other,
                    message: format!("wait: {e}"),
                });
            }
            Err(_elapsed) => {
                // Kill the whole process group (setsid made pgid == pid), then
                // reap so nothing is left behind.
                if let Some(pid) = pid {
                    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                }
                let status = child.wait().await;
                (true, status.ok().and_then(|s| s.code()))
            }
        };

        // Readers finish once the (killed or exited) process closes its pipes.
        let _ = tokio::join!(out_reader, err_reader);

        Ok(ExecOutcome {
            exit_code,
            timed_out,
            stdout: String::from_utf8_lossy(&out_buf.lock().unwrap()).into_owned(),
            stderr: String::from_utf8_lossy(&err_buf.lock().unwrap()).into_owned(),
            truncated: truncated.load(Ordering::Relaxed),
        })
    }
}
