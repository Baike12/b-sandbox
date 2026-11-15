//! Per-request overhead gate for the API layer. The mock backend isolates
//! the service machinery (routing, extraction, registry, ledger, metrics)
//! from backend cost: these bounds catch regressions like accidental
//! blocking, O(n) registry scans, or per-request allocations gone wrong.
//! Order-of-magnitude budgets calibrated from measured baselines; the
//! measured numbers live in the comments.

use std::time::Instant;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use sandbox_api::routes::router;
use sandbox_api::state::AppState;
use sandbox_core::policy::SandboxSpec;
use sandbox_core::{
    BackendCapabilities, BackendHealth, ExecOutcome, ExecRequest, SandboxBackend, SandboxContext,
};

/// Instant backend: isolates API-layer cost from isolation cost.
#[derive(Default)]
struct InstantBackend;

#[async_trait::async_trait]
impl SandboxBackend for InstantBackend {
    fn name(&self) -> &'static str {
        "instant"
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::FS_READWRITE
            .union(BackendCapabilities::FS_READONLY)
            .union(BackendCapabilities::FS_DENIED)
            .union(BackendCapabilities::NET_EGRESS_DEFAULT)
    }

    fn health(&self) -> BackendHealth {
        BackendHealth::available()
    }

    async fn provision(
        &self,
        id: &str,
        spec: &SandboxSpec,
    ) -> sandbox_core::Result<SandboxContext> {
        let (fs, net) = spec.validate()?;
        let tmp = tempfile::tempdir().unwrap();
        Ok(SandboxContext {
            id: id.to_string(),
            rootfs: std::fs::canonicalize(tmp.path()).unwrap(),
            spec: spec.clone(),
            fs,
            net,
        })
    }

    async fn exec(
        &self,
        _ctx: &SandboxContext,
        req: ExecRequest,
    ) -> sandbox_core::Result<ExecOutcome> {
        req.validate()?;
        Ok(ExecOutcome {
            exit_code: Some(0),
            timed_out: false,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
        })
    }

    async fn stop(&self, _ctx: &SandboxContext) -> sandbox_core::Result<()> {
        Ok(())
    }

    async fn deprovision(&self, _ctx: &SandboxContext) -> sandbox_core::Result<()> {
        Ok(())
    }
}

fn app() -> axum::Router {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = tmp.path().to_path_buf();
    std::mem::forget(tmp);
    router(std::sync::Arc::new(AppState::new(
        std::sync::Arc::new(InstantBackend),
        data_root,
    )))
}

async fn send(
    app: &mut axum::Router,
    method: &str,
    uri: &str,
    body: Option<String>,
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let req = builder.body(Body::from(body.unwrap_or_default())).unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// One full lifecycle cycle (create → exec → detail → delete, four requests)
/// through the router. Measured baseline 0.195 ms/cycle (2026-11-15, M2 dev
/// machine, release-less debug build); budget is ~10x for CI noise.
#[tokio::test]
async fn api_lifecycle_cycle_overhead_budget() {
    let mut app = app();

    // Warm-up: pay first-request setup costs outside the measurement.
    let (status, body) = send(&mut app, "POST", "/sandboxes", Some("{}".into())).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let runs = 200;
    let started = Instant::now();
    for _ in 0..runs {
        let (status, body) = send(&mut app, "POST", "/sandboxes", Some("{}".into())).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        let (status, _) = send(
            &mut app,
            "POST",
            &format!("/sandboxes/{id}/exec"),
            Some(r#"{"command":"true"}"#.into()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = send(&mut app, "GET", &format!("/sandboxes/{id}"), None).await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = send(&mut app, "DELETE", &format!("/sandboxes/{id}"), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    let per_cycle_ms = started.elapsed().as_millis() as f64 / runs as f64;
    eprintln!("mean create+exec+detail+delete cycle: {per_cycle_ms:.3} ms");
    assert!(
        per_cycle_ms < 2.0,
        "API cycle mean {per_cycle_ms:.3} ms blew the 2 ms order-of-magnitude budget (measured baseline 0.195 ms)"
    );
}
