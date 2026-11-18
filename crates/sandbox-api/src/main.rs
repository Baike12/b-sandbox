//! b-sandbox REST service entry point.
//!
//! Usage:
//! ```text
//! sandbox-api --data-root <dir> [--port 8094] [--bind <addr>]
//!             [--backend seatbelt|bwrap] [--auth-token <token>]
//!             [--egress-proxy <ip:port> --egress-credentials <file.json>]
//! ```
//!
//! Default bind is `127.0.0.1` (the dev posture: nothing exposed). A
//! container deployment must pass `--bind 0.0.0.0` because a published port
//! forwards to the container IP, not its loopback; the host-side publish
//! address stays loopback in compose, so nginx remains the only facade.
//! Auth is deferred to the milestone that fronts the service with nginx
//! terminating the only exposed surface.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use sandbox_core::SandboxBackend;

#[derive(Debug)]
struct Config {
    data_root: PathBuf,
    port: u16,
    bind: IpAddr,
    backend: String,
    auth_token: Option<String>,
    /// `ip:port` to run the credential-injection proxy on. Requires
    /// --egress-credentials; proxy-mode sandboxes are refused without both.
    egress_proxy: Option<String>,
    egress_credentials: Option<String>,
    /// Web console build directory (web/dist). None → console routes answer
    /// with a build hint.
    console_dir: Option<PathBuf>,
}

/// Structured logging: `tracing` to stderr, level from `B_SANDBOX_LOG`
/// (default `info`) — the variable is ours, not the tracing default
/// `RUST_LOG`, so operators follow one documented knob. ANSI colors are
/// disabled: the container log driver captures raw bytes and a scraper
/// should not need escape-sequence stripping. Uninitialized subscribers are
/// no-ops, so library tests stay quiet unless they opt in.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_env_var("B_SANDBOX_LOG")
        .with_default_directive("info".parse().expect("valid default directive"))
        .from_env_lossy();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();
}

fn parse_args() -> Config {
    let mut cfg = Config {
        data_root: PathBuf::from("/data/b-sandbox"),
        port: 8094,
        bind: IpAddr::from([127, 0, 0, 1]),
        backend: "seatbelt".into(),
        auth_token: None,
        egress_proxy: None,
        egress_credentials: None,
        // Serve the console when a build exists next to the repo root —
        // zero-config for the common layout, overridable, and absent in a
        // bare deploy without a console build.
        console_dir: std::env::current_dir()
            .ok()
            .map(|cwd| cwd.join("web/dist"))
            .filter(|dir| dir.join("index.html").is_file()),
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-root" => {
                cfg.data_root = args
                    .next()
                    .map(PathBuf::from)
                    .expect("--data-root needs a value")
            }
            "--port" => {
                cfg.port = args
                    .next()
                    .expect("--port needs a value")
                    .parse()
                    .expect("--port must be a number")
            }
            "--bind" => {
                cfg.bind = args
                    .next()
                    .expect("--bind needs a value")
                    .parse()
                    .expect("--bind must be an IP address")
            }
            "--backend" => cfg.backend = args.next().expect("--backend needs a value"),
            "--auth-token" => {
                cfg.auth_token = Some(args.next().expect("--auth-token needs a value"))
            }
            "--egress-proxy" => {
                cfg.egress_proxy = Some(args.next().expect("--egress-proxy needs a value"))
            }
            "--egress-credentials" => {
                cfg.egress_credentials =
                    Some(args.next().expect("--egress-credentials needs a value"))
            }
            "--console-dir" => {
                cfg.console_dir = Some(
                    args.next()
                        .map(PathBuf::from)
                        .expect("--console-dir needs a value"),
                )
            }
            other => {
                panic!(
                    "unknown argument {other:?} (expected --data-root/--port/--bind/--backend/--console-dir)"
                )
            }
        }
    }
    // The token flows through the environment in production (argv is visible
    // in `ps`); the flag exists for local experiments.
    if cfg.auth_token.is_none() {
        cfg.auth_token = std::env::var("B_SANDBOX_AUTH_TOKEN").ok();
    }
    cfg
}

/// Bind the credential-injection proxy on the service runtime. Secrets live
/// in the credentials file (0600, outside the repo) and in the proxy's
/// memory — never in the policy, the env, or the sandbox.
async fn start_egress_proxy(addr: &str, credentials_path: &str) {
    let raw = std::fs::read_to_string(credentials_path)
        .unwrap_or_else(|e| panic!("cannot read credentials file {credentials_path:?}: {e}"));
    let credentials: BTreeMap<String, String> =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("credentials file: {e}"));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("egress proxy cannot bind {addr}: {e}"));
    tracing::info!(addr, "egress-proxy listening");
    tokio::spawn(async move {
        sandbox_egress_proxy::serve(listener, Arc::new(credentials))
            .await
            .expect("proxy error");
    });
}

fn build_backend(cfg: &Config) -> Result<Arc<dyn SandboxBackend>, String> {
    match cfg.backend.as_str() {
        "seatbelt" => {
            let backend = sandbox_backend_seatbelt::SeatbeltBackend::new(cfg.data_root.clone());
            let backend = match &cfg.egress_proxy {
                Some(addr) => backend.with_egress_proxy(addr.clone()),
                None => backend,
            };
            Ok(Arc::new(backend))
        }
        "bwrap" => {
            // bwrap cannot enforce a single-hole egress policy (no per-port
            // network filter), so it declares no NET_EGRESS_PROXY capability
            // and proxy-mode specs are rejected at provision.
            if cfg.egress_proxy.is_some() {
                return Err("--egress-proxy is only supported by the seatbelt backend".into());
            }
            Ok(Arc::new(sandbox_backend_bwrap::BwrapBackend::new(
                cfg.data_root.clone(),
            )))
        }
        other => Err(format!(
            "unknown backend {other:?} (expected seatbelt or bwrap)"
        )),
    }
}

/// Resolve when SIGINT or SIGTERM arrives (`docker stop` sends SIGTERM).
/// Then: flip the shutdown flag (SSE tails end their streams), terminate
/// every live sandbox through the shared removal path, and return — axum
/// drains the remaining connections and serve() exits.
async fn shutdown_signal(state: sandbox_api::state::SharedState) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install ctrl_c handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    state
        .shutdown
        .store(true, std::sync::atomic::Ordering::Relaxed);
    tracing::info!("shutdown signal received; terminating sandboxes and draining");
    let ids: Vec<String> = state.registry.read().unwrap().keys().cloned().collect();
    let terminated = sandbox_api::reaper::terminate_all(&state, &ids, "server_shutdown").await;
    tracing::info!(count = terminated, "all live sandboxes terminated");
}

fn main() {
    let cfg = parse_args();
    match (&cfg.egress_proxy, &cfg.egress_credentials) {
        (Some(_), Some(_)) | (None, None) => {}
        _ => panic!("--egress-proxy and --egress-credentials must be given together"),
    }
    let backend = build_backend(&cfg).unwrap_or_else(|e| panic!("{e}"));
    init_tracing();
    let health = backend.health();
    if !health.available {
        // Fail loudly at startup: a service that cannot isolate must not
        // come up and silently serve.
        let reason = health.reason.as_deref().unwrap_or("unknown");
        panic!("backend {:?} unavailable: {reason}", backend.name());
    }
    // Single instance per data root: two processes would reap each other's
    // sandboxes as orphans. A stale lock (dead pid) is stolen automatically.
    let _service_lock =
        sandbox_api::state::ServiceLock::acquire(&cfg.data_root).unwrap_or_else(|e| panic!("{e}"));
    tracing::info!(
        backend = backend.name(),
        version = env!("CARGO_PKG_VERSION"),
        data_root = %cfg.data_root.display(),
        bind = %cfg.bind,
        port = cfg.port,
        "b-sandbox starting"
    );

    let state = Arc::new(
        sandbox_api::state::AppState::new(backend, cfg.data_root.clone())
            .with_auth_token(cfg.auth_token)
            .with_console_dir_opt(cfg.console_dir.clone()),
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async move {
        // Crash recovery first: stale rootfs directories from a previous
        // process are removed before anything can observe them.
        let orphans = sandbox_api::reaper::recover_orphans(&cfg.data_root).await;
        if orphans > 0 {
            tracing::warn!(
                count = orphans,
                "removed orphaned sandbox directories from the previous run"
            );
        }
        state.metrics.add_orphans_removed(orphans as u64);

        // Idle-TTL reaper: terminates sandboxes whose declared TTL elapsed.
        // (Inside the runtime — tokio::spawn requires a running executor.)
        sandbox_api::reaper::spawn(state.clone());

        if let (Some(addr), Some(cred_file)) = (&cfg.egress_proxy, &cfg.egress_credentials) {
            start_egress_proxy(addr, cred_file).await;
        }
        let listener = tokio::net::TcpListener::bind((cfg.bind, cfg.port))
            .await
            .unwrap_or_else(|e| panic!("cannot bind {}:{}: {e}", cfg.bind, cfg.port));
        tracing::info!(bind = %cfg.bind, port = cfg.port, "listening");
        axum::serve(listener, sandbox_api::routes::router(state.clone()))
            .with_graceful_shutdown(shutdown_signal(state))
            .await
            .expect("server error");
        tracing::info!("server drained; exiting");
    });
}
