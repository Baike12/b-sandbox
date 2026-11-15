//! b-sandbox REST service entry point.
//!
//! Usage:
//! ```text
//! sandbox-api --data-root <dir> [--port 8094] [--backend seatbelt|bwrap]
//! ```
//!
//! M1 binds `127.0.0.1` only (same posture as b-superagent's container);
//! auth is deferred to the M2 deployment milestone where the nginx front
//! already terminates the only exposed surface.

use std::path::PathBuf;
use std::sync::Arc;

use sandbox_core::SandboxBackend;

#[derive(Debug)]
struct Config {
    data_root: PathBuf,
    port: u16,
    backend: String,
}

fn parse_args() -> Config {
    let mut cfg = Config {
        data_root: PathBuf::from("/data/b-sandbox"),
        port: 8094,
        backend: "seatbelt".into(),
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
            "--backend" => cfg.backend = args.next().expect("--backend needs a value"),
            other => panic!("unknown argument {other:?} (expected --data-root/--port/--backend)"),
        }
    }
    cfg
}

fn build_backend(cfg: &Config) -> Result<Arc<dyn SandboxBackend>, String> {
    match cfg.backend.as_str() {
        "seatbelt" => Ok(Arc::new(sandbox_backend_seatbelt::SeatbeltBackend::new(
            cfg.data_root.clone(),
        ))),
        "bwrap" => Ok(Arc::new(sandbox_backend_bwrap::BwrapBackend::new(
            cfg.data_root.clone(),
        ))),
        other => Err(format!(
            "unknown backend {other:?} (expected seatbelt or bwrap)"
        )),
    }
}

fn main() {
    let cfg = parse_args();
    let backend = build_backend(&cfg).unwrap_or_else(|e| panic!("{e}"));
    let health = backend.health();
    if !health.available {
        // Fail loudly at startup: a service that cannot isolate must not
        // come up and silently serve.
        let reason = health.reason.as_deref().unwrap_or("unknown");
        panic!("backend {:?} unavailable: {reason}", backend.name());
    }
    eprintln!(
        "b-sandbox: backend={} data-root={} listening on 127.0.0.1:{}",
        backend.name(),
        cfg.data_root.display(),
        cfg.port
    );

    let state = Arc::new(sandbox_api::state::AppState::new(
        backend,
        cfg.data_root.clone(),
    ));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", cfg.port))
            .await
            .unwrap_or_else(|e| panic!("cannot bind 127.0.0.1:{}: {e}", cfg.port));
        axum::serve(listener, sandbox_api::routes::router(state))
            .await
            .expect("server error");
    });
}
