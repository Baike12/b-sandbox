//! Benchmarks — deliberately NOT gated (they measure, they do not assert).
//! Run explicitly:
//!   cargo test -p sandbox-api --test bench -- --ignored --nocapture
//! The numbers printed here feed docs/production-report.md; the gated
//! order-of-magnitude budgets live in tests/perf.rs, calibrated from these
//! measurements.

use std::path::PathBuf;
use std::sync::Arc;
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

/// Zero-cost backend for API-mechanics benchmarks: rootfs dirs under one
/// shared root (created and removed, never leaked).
struct BenchBackend {
    data_root: PathBuf,
}

impl BenchBackend {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sandboxes");
        std::fs::create_dir_all(&root).unwrap();
        // Leaked on purpose: the dir must outlive the test process.
        std::mem::forget(tmp);
        Self { data_root: root }
    }
}

#[async_trait::async_trait]
impl SandboxBackend for BenchBackend {
    fn name(&self) -> &'static str {
        "bench"
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
        let rootfs = self.data_root.join(id);
        tokio::fs::create_dir_all(&rootfs).await.unwrap();
        // Canonical rootfs (macOS /var → /private/var): the file data plane
        // compares canonicalized paths, so this must match.
        let rootfs = tokio::fs::canonicalize(&rootfs).await.unwrap();
        Ok(SandboxContext {
            id: id.to_string(),
            rootfs,
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
            stdout: "bench".into(),
            stderr: String::new(),
            truncated: false,
        })
    }

    async fn stop(&self, _ctx: &SandboxContext) -> sandbox_core::Result<()> {
        Ok(())
    }

    async fn deprovision(&self, ctx: &SandboxContext) -> sandbox_core::Result<()> {
        let _ = tokio::fs::remove_dir_all(&ctx.rootfs).await;
        Ok(())
    }
}

fn bench_router() -> axum::Router {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = tmp.path().to_path_buf();
    std::mem::forget(tmp);
    router(Arc::new(AppState::new(
        Arc::new(BenchBackend::new()),
        data_root,
    )))
}

async fn send(
    app: &axum::Router,
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

/// One create→exec→detail→delete cycle through the full router.
async fn one_cycle(app: &axum::Router) {
    let (status, body) = send(app, "POST", "/sandboxes", Some("{}".into())).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, _) = send(
        app,
        "POST",
        &format!("/sandboxes/{id}/exec"),
        Some(r#"{"command":"true"}"#.into()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(app, "GET", &format!("/sandboxes/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(app, "DELETE", &format!("/sandboxes/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[tokio::test]
#[ignore]
async fn bench_api_lifecycle_cycles_sequential() {
    let app = bench_router();
    // Warm-up.
    one_cycle(&app).await;

    let runs = 200;
    let started = Instant::now();
    for _ in 0..runs {
        one_cycle(&app).await;
    }
    let elapsed = started.elapsed();
    println!(
        "api lifecycle cycle (sequential): {:.1} µs/cycle | {:.0} cycles/s | {} total",
        elapsed.as_micros() as f64 / runs as f64,
        runs as f64 / elapsed.as_secs_f64(),
        elapsed.as_secs_f64()
    );
}

#[tokio::test]
#[ignore]
async fn bench_api_lifecycle_cycles_parallel() {
    let app = Arc::new(bench_router());
    const WORKERS: usize = 32;
    const CYCLES: usize = 10;

    let started = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..WORKERS {
        let app = app.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..CYCLES {
                one_cycle(&app).await;
            }
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }
    let elapsed = started.elapsed();
    let total = (WORKERS * CYCLES) as f64;
    println!(
        "api lifecycle cycles (parallel, {WORKERS} workers): {total:.0} cycles in {:.3} s | {:.0} cycles/s",
        elapsed.as_secs_f64(),
        total / elapsed.as_secs_f64()
    );
}

#[tokio::test]
#[ignore]
async fn bench_metrics_scrape_under_traffic() {
    let app = bench_router();
    // Generate traffic so counters and histograms are populated.
    for _ in 0..10 {
        one_cycle(&app).await;
    }

    let scrapes = 200;
    let started = Instant::now();
    let mut last_size = 0usize;
    for _ in 0..scrapes {
        let (_, body) = send(&app, "GET", "/metrics", None).await;
        last_size = body.len();
    }
    let elapsed = started.elapsed();
    println!(
        "metrics scrape: {:.1} µs/scrape | {last_size} B body | populated with 40 requests of traffic",
        elapsed.as_micros() as f64 / scrapes as f64
    );
}

#[tokio::test]
#[ignore]
async fn bench_file_data_plane_ops() {
    let app = bench_router();
    let (_, body) = send(&app, "POST", "/sandboxes", Some("{}".into())).await;
    let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let ops = 200;
    let payload = "x".repeat(1024);
    let started = Instant::now();
    for i in 0..ops {
        let (status, body) = send(
            &app,
            "POST",
            &format!("/sandboxes/{id}/files/write"),
            Some(serde_json::json!({"path": format!("f/{i}.txt"), "content": payload}).to_string()),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }
    let write_us = started.elapsed().as_micros() as f64 / ops as f64;

    let started = Instant::now();
    for i in 0..ops {
        let (status, _) = send(&app, "POST", &format!("/sandboxes/{id}/files/read"), {
            Some(serde_json::json!({"path": format!("f/{i}.txt")}).to_string())
        })
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let read_us = started.elapsed().as_micros() as f64 / ops as f64;
    println!(
        "file data plane (1 KiB payloads): write {:.1} µs/op | read {:.1} µs/op | {ops} ops each",
        write_us, read_us
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore]
async fn bench_seatbelt_exec_percentiles() {
    use sandbox_backend_seatbelt::SeatbeltBackend;

    let tmp = tempfile::tempdir().unwrap();
    let backend = SeatbeltBackend::new(tmp.path().to_path_buf());
    let ctx = backend
        .provision("bench-seatbelt", &SandboxSpec::default())
        .await
        .unwrap();
    let req = ExecRequest {
        command: "/bin/echo".into(),
        args: vec![],
        env: Default::default(),
        cwd: String::new(),
        timeout_ms: Some(10_000),
    };

    // Warm-up pays cold profile compilation.
    backend.exec(&ctx, req.clone()).await.unwrap();

    let runs = 30;
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let started = Instant::now();
        let outcome = backend.exec(&ctx, req.clone()).await.unwrap();
        assert_eq!(outcome.exit_code, Some(0));
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "seatbelt exec echo ({} runs): p50 {:.1} ms | p90 {:.1} ms | p99 {:.1} ms | max {:.1} ms",
        runs,
        percentile(&samples, 50.0),
        percentile(&samples, 90.0),
        percentile(&samples, 99.0),
        samples[samples.len() - 1]
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore]
async fn bench_seatbelt_concurrent_exec() {
    use sandbox_backend_seatbelt::SeatbeltBackend;

    let tmp = tempfile::tempdir().unwrap();
    let backend = Arc::new(SeatbeltBackend::new(tmp.path().to_path_buf()));
    let ctx = Arc::new(
        backend
            .provision("bench-seatbelt-conc", &SandboxSpec::default())
            .await
            .unwrap(),
    );
    let req = ExecRequest {
        command: "/bin/echo".into(),
        args: vec![],
        env: Default::default(),
        cwd: String::new(),
        timeout_ms: Some(10_000),
    };
    backend.exec(&ctx, req.clone()).await.unwrap(); // warm-up

    const CONCURRENCY: usize = 8;
    const ROUNDS: usize = 3;
    let started = Instant::now();
    let mut total = 0usize;
    for _ in 0..ROUNDS {
        let mut handles = Vec::new();
        for _ in 0..CONCURRENCY {
            let backend = backend.clone();
            let ctx = ctx.clone();
            let req = req.clone();
            handles.push(tokio::spawn(async move {
                backend.exec(&ctx, req).await.unwrap()
            }));
        }
        for handle in handles {
            handle.await.unwrap();
            total += 1;
        }
    }
    let elapsed = started.elapsed();
    println!(
        "seatbelt concurrent exec ({CONCURRENCY} at a time, {ROUNDS} rounds): {total} execs in {:.3} s | {:.0} exec/s",
        elapsed.as_secs_f64(),
        total as f64 / elapsed.as_secs_f64()
    );
}
