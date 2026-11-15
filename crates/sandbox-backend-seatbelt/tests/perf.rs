//! Latency budget gates for the real Seatbelt backend. These are
//! order-of-magnitude guards calibrated from measured baselines (the
//! measured numbers live in the comments next to each budget), not
//! micro-benchmarks: they fail on 10x regressions (accidental blocking,
//! process storms, double sandboxing) and tolerate normal machine noise.

#![cfg(target_os = "macos")]

use std::time::Instant;

use sandbox_backend_seatbelt::SeatbeltBackend;
use sandbox_core::policy::SandboxSpec;
use sandbox_core::{ExecRequest, SandboxBackend};

fn exec_req(command: &str) -> ExecRequest {
    ExecRequest {
        command: command.to_string(),
        args: vec![],
        env: Default::default(),
        cwd: String::new(),
        timeout_ms: Some(10_000),
    }
}

/// One sequential echo exec through the real backend: fork + sandbox_init +
/// exec + reap. Measured baseline 5.2 ms/exec warm (2026-11-15, M2 dev
/// machine); budget is ~10x for CI noise.
#[tokio::test]
async fn exec_echo_round_trip_latency_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let backend = SeatbeltBackend::new(tmp.path().to_path_buf());
    let ctx = backend
        .provision("perf-echo", &SandboxSpec::default())
        .await
        .unwrap();

    // Warm-up (first exec pays profile compilation cold caches).
    let outcome = backend.exec(&ctx, exec_req("/bin/echo")).await.unwrap();
    assert_eq!(outcome.exit_code, Some(0));

    let runs = 10;
    let started = Instant::now();
    for _ in 0..runs {
        let outcome = backend.exec(&ctx, exec_req("/bin/echo")).await.unwrap();
        assert_eq!(outcome.exit_code, Some(0));
    }
    let mean_ms = started.elapsed().as_millis() as f64 / runs as f64;
    eprintln!("mean exec round trip: {mean_ms:.1} ms");
    assert!(
        mean_ms < 50.0,
        "echo exec mean {mean_ms:.1} ms blew the 50 ms order-of-magnitude budget (measured baseline 5.2 ms)"
    );
    let _ = backend.deprovision(&ctx).await;
}
