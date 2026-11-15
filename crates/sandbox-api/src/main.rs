//! b-sandbox REST service entry point.
//!
//! Usage:
//! ```text
//! sandbox-api --data-root <dir> [--port 8094] [--bind <addr>] [--backend seatbelt|bwrap]
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
            other => {
                panic!("unknown argument {other:?} (expected --data-root/--port/--bind/--backend)")
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
    eprintln!("egress-proxy: listening on {addr}");
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

fn main() {
    let cfg = parse_args();
    match (&cfg.egress_proxy, &cfg.egress_credentials) {
        (Some(_), Some(_)) | (None, None) => {}
        _ => panic!("--egress-proxy and --egress-credentials must be given together"),
    }
    let backend = build_backend(&cfg).unwrap_or_else(|e| panic!("{e}"));
    let health = backend.health();
    if !health.available {
        // Fail loudly at startup: a service that cannot isolate must not
        // come up and silently serve.
        let reason = health.reason.as_deref().unwrap_or("unknown");
        panic!("backend {:?} unavailable: {reason}", backend.name());
    }
    eprintln!(
        "b-sandbox: backend={} data-root={} listening on {}:{}",
        backend.name(),
        cfg.data_root.display(),
        cfg.bind,
        cfg.port
    );

    let state = Arc::new(
        sandbox_api::state::AppState::new(backend, cfg.data_root.clone())
            .with_auth_token(cfg.auth_token),
    );
    // Idle-TTL reaper: terminates sandboxes whose declared TTL elapsed.
    sandbox_api::reaper::spawn(state.clone());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async move {
        if let (Some(addr), Some(cred_file)) = (&cfg.egress_proxy, &cfg.egress_credentials) {
            start_egress_proxy(addr, cred_file).await;
        }
        let listener = tokio::net::TcpListener::bind((cfg.bind, cfg.port))
            .await
            .unwrap_or_else(|e| panic!("cannot bind {}:{}: {e}", cfg.bind, cfg.port));
        axum::serve(listener, sandbox_api::routes::router(state))
            .await
            .expect("server error");
    });
}
