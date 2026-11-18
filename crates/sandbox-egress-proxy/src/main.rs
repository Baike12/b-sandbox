//! Standalone proxy entry point. In-process use (the common deployment) goes
//! through `sandbox_api`'s `--egress-proxy`/`--egress-credentials` flags;
//! this binary exists for running the proxy on its own host/namespace.

use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug)]
struct Config {
    bind: String,
    credentials_path: String,
}

fn parse_args() -> Config {
    let mut cfg = Config {
        bind: "127.0.0.1:8095".into(),
        credentials_path: String::new(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => cfg.bind = args.next().expect("--bind needs a value"),
            "--credentials" => {
                cfg.credentials_path = args.next().expect("--credentials needs a value")
            }
            other => panic!("unknown argument {other:?} (expected --bind/--credentials)"),
        }
    }
    if cfg.credentials_path.is_empty() {
        panic!("--credentials <file.json> is required (host → Authorization value)");
    }
    cfg
}

fn load_credentials(path: &str) -> BTreeMap<String, String> {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read credentials file {path:?}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("credentials file {path:?}: {e}"))
}

fn main() {
    let cfg = parse_args();
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_env_var("B_SANDBOX_LOG")
        .with_default_directive("info".parse().expect("valid default directive"))
        .from_env_lossy();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();
    let credentials = Arc::new(load_credentials(&cfg.credentials_path));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(&cfg.bind)
            .await
            .unwrap_or_else(|e| panic!("cannot bind {}: {e}", cfg.bind));
        tracing::info!(
            bind = %cfg.bind,
            hosts = credentials.len(),
            "egress-proxy listening"
        );
        sandbox_egress_proxy::serve(listener, credentials)
            .await
            .expect("proxy error");
    });
}
