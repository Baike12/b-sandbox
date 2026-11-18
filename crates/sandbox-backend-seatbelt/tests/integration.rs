//! Integration tests for the Seatbelt backend. These run REAL sandboxed
//! processes on macOS — this is the enforcement-evidence suite: every
//! capability bit the backend declares is exercised here by flipping a policy
//! input and observing the workload's behavior change.

#![cfg(target_os = "macos")]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use sandbox_backend_seatbelt::SeatbeltBackend;
use sandbox_core::policy::{EgressDefault, FsPolicy, NetPolicy, NetTarget, SandboxSpec};
use sandbox_core::{ExecRequest, SandboxBackend};

fn exec_req(command: &str, args: &[&str]) -> ExecRequest {
    ExecRequest {
        command: command.to_string(),
        args: args.iter().map(|s| s.to_string()).collect(),
        env: BTreeMap::new(),
        cwd: String::new(),
        timeout_ms: Some(10_000),
    }
}

fn spec_with_fs(fs: FsPolicy) -> SandboxSpec {
    SandboxSpec {
        fs,
        ..Default::default()
    }
}

async fn fresh_backend() -> (SeatbeltBackend, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    (SeatbeltBackend::new(tmp.path().to_path_buf()), tmp)
}

#[tokio::test]
async fn echo_round_trip() {
    let (backend, _tmp) = fresh_backend().await;
    let ctx = backend
        .provision("echo-test", &SandboxSpec::default())
        .await
        .unwrap();
    let outcome = backend
        .exec(&ctx, exec_req("/bin/echo", &["hello", "world"]))
        .await
        .unwrap();
    assert_eq!(outcome.exit_code, Some(0));
    assert_eq!(outcome.stdout, "hello world\n");
    assert!(!outcome.timed_out);
    assert!(!outcome.truncated);
}

#[tokio::test]
async fn writes_inside_rootfs_are_allowed() {
    let (backend, tmp) = fresh_backend().await;
    let ctx = backend
        .provision("rw-rootfs", &SandboxSpec::default())
        .await
        .unwrap();
    let outcome = backend
        .exec(&ctx, exec_req("/bin/sh", &["-c", "echo hi > out.txt"]))
        .await
        .unwrap();
    assert_eq!(outcome.exit_code, Some(0), "stderr: {}", outcome.stderr);
    let written = std::fs::read_to_string(tmp.path().join("sandboxes/rw-rootfs/out.txt")).unwrap();
    assert_eq!(written.trim(), "hi");
}

#[tokio::test]
async fn writes_outside_rootfs_are_denied() {
    let (backend, _tmp) = fresh_backend().await;
    let ctx = backend
        .provision("deny-write", &SandboxSpec::default())
        .await
        .unwrap();
    let probe = "/tmp/b-sandbox-deny-probe";
    let _ = std::fs::remove_file(probe);
    let outcome = backend
        .exec(
            &ctx,
            exec_req("/bin/sh", &["-c", "echo x > /tmp/b-sandbox-deny-probe"]),
        )
        .await
        .unwrap();
    assert_ne!(outcome.exit_code, Some(0), "write outside rootfs must fail");
    assert!(std::fs::read(probe).is_err(), "probe file must not exist");
}

/// A temp dir expressed in kernel-canonical form, with its guard still alive.
/// The fs-policy contract requires canonical paths (Seatbelt matches the
/// caller's literal path string, and /var→/private symlink spellings only
/// resolve for /private), so tests hand both the policy and the workload the
/// same canonical spelling.
fn canonical_host() -> (tempfile::TempDir, std::path::PathBuf) {
    let host = tempfile::tempdir().unwrap();
    let path = std::fs::canonicalize(host.path()).unwrap();
    (host, path)
}

#[tokio::test]
async fn readonly_host_path_can_be_read_but_not_written() {
    let (_guard, host) = canonical_host();
    std::fs::write(host.join("doc.txt"), "topsecret").unwrap();

    let (backend, _tmp) = fresh_backend().await;
    let fs = FsPolicy {
        readonly: vec![host.to_string_lossy().into_owned()],
        ..Default::default()
    };
    let ctx = backend
        .provision("ro-host", &spec_with_fs(fs))
        .await
        .unwrap();

    let doc = format!("{}/doc.txt", host.display());
    let read = backend
        .exec(&ctx, exec_req("/bin/cat", &[&doc]))
        .await
        .unwrap();
    assert_eq!(read.exit_code, Some(0), "stderr: {}", read.stderr);
    assert_eq!(read.stdout.trim(), "topsecret");

    let write = backend
        .exec(
            &ctx,
            exec_req("/bin/sh", &["-c", &format!("echo x >> {doc}")]),
        )
        .await
        .unwrap();
    assert_ne!(
        write.exit_code,
        Some(0),
        "readonly path must not be writable"
    );
    assert_eq!(std::fs::read_to_string(&doc).unwrap(), "topsecret");
}

#[tokio::test]
async fn readwrite_host_path_is_writable() {
    let (_guard, host) = canonical_host();

    let (backend, _tmp) = fresh_backend().await;
    let fs = FsPolicy {
        readwrite: vec![host.to_string_lossy().into_owned()],
        ..Default::default()
    };
    let ctx = backend
        .provision("rw-host", &spec_with_fs(fs))
        .await
        .unwrap();

    let file = format!("{}/out.txt", host.display());
    let write = backend
        .exec(
            &ctx,
            exec_req("/bin/sh", &["-c", &format!("echo sandboxed > {file}")]),
        )
        .await
        .unwrap();
    assert_eq!(write.exit_code, Some(0), "stderr: {}", write.stderr);
    assert_eq!(std::fs::read_to_string(&file).unwrap().trim(), "sandboxed");
}

#[tokio::test]
async fn denied_subtree_blocks_even_inside_a_readwrite_path() {
    let (_guard, host) = canonical_host();
    std::fs::create_dir_all(host.join("secret")).unwrap();
    std::fs::write(host.join("secret/key.pem"), "k").unwrap();
    std::fs::write(host.join("public.txt"), "p").unwrap();

    let (backend, _tmp) = fresh_backend().await;
    let fs = FsPolicy {
        readwrite: vec![host.to_string_lossy().into_owned()],
        denied: vec![format!("{}/secret", host.display())],
        ..Default::default()
    };
    let ctx = backend
        .provision("denied-sub", &spec_with_fs(fs))
        .await
        .unwrap();

    let public = format!("{}/public.txt", host.display());
    let secret = format!("{}/secret/key.pem", host.display());

    let read_public = backend
        .exec(&ctx, exec_req("/bin/cat", &[&public]))
        .await
        .unwrap();
    assert_eq!(read_public.exit_code, Some(0));

    let read_secret = backend
        .exec(&ctx, exec_req("/bin/cat", &[&secret]))
        .await
        .unwrap();
    assert_ne!(
        read_secret.exit_code,
        Some(0),
        "denied subtree must be unreadable"
    );
}

#[tokio::test]
async fn network_is_denied_by_default_even_to_a_live_listener() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let (backend, _tmp) = fresh_backend().await;
    let ctx = backend
        .provision("net-deny", &SandboxSpec::default())
        .await
        .unwrap();

    let outcome = backend
        .exec(
            &ctx,
            exec_req(
                "/usr/bin/curl",
                &["--max-time", "3", &format!("http://127.0.0.1:{port}/")],
            ),
        )
        .await
        .unwrap();
    assert_ne!(
        outcome.exit_code,
        Some(0),
        "curl must fail under deny-all egress"
    );
    assert!(
        !outcome.stdout.contains("HTTP"),
        "no HTTP exchange may happen; stdout was: {}",
        outcome.stdout
    );
    // The listener must still be alive and reachable from the host itself.
    // (Connect through the backlog — no blocking accept here: under deny-all
    // egress no sandbox connection ever arrives, and a blocking accept would
    // deadlock the test.)
    let addr = listener.local_addr().unwrap();
    std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(1))
        .expect("host must still reach its own listener");
}

#[tokio::test]
async fn egress_allow_lets_curl_reach_a_live_listener() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let (backend, _tmp) = fresh_backend().await;
    let net = NetPolicy {
        egress_default: EgressDefault::Allow,
        ..Default::default()
    };
    let ctx = backend
        .provision(
            "net-allow",
            &SandboxSpec {
                net,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // Serve one request in the background, then run curl inside the sandbox.
    // Non-blocking with a deadline: if the sandbox denies the connection, the
    // server gives up instead of deadlocking the test run.
    let server = tokio::task::spawn_blocking(move || {
        use std::io::{Read, Write};
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match listener.accept() {
                Ok((mut sock, _)) => {
                    sock.set_nonblocking(false).ok();
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf);
                    let _ = sock.write_all(b"HTTP/1.0 200 OK\r\n\r\nok");
                    return;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() > deadline {
                        panic!("no sandbox connection within 5s");
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("accept failed: {e}"),
            }
        }
    });

    let outcome = backend
        .exec(
            &ctx,
            exec_req(
                "/usr/bin/curl",
                &["--max-time", "3", &format!("http://127.0.0.1:{port}/")],
            ),
        )
        .await
        .unwrap();
    assert_eq!(outcome.exit_code, Some(0), "stderr: {}", outcome.stderr);
    assert!(
        outcome.stdout.contains("ok"),
        "curl should have received the body"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn exec_timeout_kills_the_process_group() {
    let (backend, _tmp) = fresh_backend().await;
    let ctx = backend
        .provision("timeout", &SandboxSpec::default())
        .await
        .unwrap();
    let mut req = exec_req("/bin/sleep", &["5"]);
    req.timeout_ms = Some(400);
    let start = Instant::now();
    let outcome = backend.exec(&ctx, req).await.unwrap();
    let elapsed = start.elapsed();
    assert!(outcome.timed_out, "exec must be reported as timed out");
    assert!(
        elapsed < Duration::from_secs(3),
        "kill must happen promptly, took {elapsed:?}"
    );
    assert!(
        outcome.exit_code.is_none()
            || outcome.exit_code == Some(137)
            || outcome.exit_code == Some(9)
    );
}

#[tokio::test]
async fn output_flood_is_capped_and_flagged() {
    let (backend, _tmp) = fresh_backend().await;
    let ctx = backend
        .provision("flood", &SandboxSpec::default())
        .await
        .unwrap();
    // ~1.4 MB of output against the 1 MiB default cap.
    let outcome = backend
        .exec(&ctx, exec_req("/bin/sh", &["-c", "seq 1 200000"]))
        .await
        .unwrap();
    assert!(outcome.truncated, "flood must be flagged as truncated");
    assert!(
        outcome.stdout.len() <= 1024 * 1024,
        "captured output must not exceed the cap, was {}",
        outcome.stdout.len()
    );
}

#[tokio::test]
async fn pids_budget_is_visible_in_the_child() {
    let (backend, _tmp) = fresh_backend().await;
    let spec = SandboxSpec {
        budget: sandbox_core::Budget {
            memory_mb: sandbox_core::budget::Budget::default().memory_mb,
            pids_max: 64,
        },
        ..Default::default()
    };
    let ctx = backend.provision("rlimits", &spec).await.unwrap();
    let outcome = backend
        .exec(&ctx, exec_req("/bin/sh", &["-c", "ulimit -u"]))
        .await
        .unwrap();
    assert_eq!(outcome.exit_code, Some(0), "stderr: {}", outcome.stderr);
    assert_eq!(
        outcome.stdout.trim(),
        "64",
        "child must see the pids ceiling"
    );
}

#[tokio::test]
async fn explicit_memory_budget_is_rejected_on_macos_tier() {
    // The honest-tier contract: macOS cannot enforce a memory budget
    // (setrlimit(RLIMIT_AS) EINVALs), so an explicit request must fail
    // validation instead of running unenforced.
    let (backend, _tmp) = fresh_backend().await;
    let spec = SandboxSpec {
        budget: sandbox_core::Budget {
            memory_mb: 512,
            pids_max: sandbox_core::budget::Budget::default().pids_max,
        },
        ..Default::default()
    };
    let err = backend.provision("no-mem", &spec).await.unwrap_err();
    assert_eq!(err.code(), "policy_validation");
    assert!(err.to_string().contains("BUDGET_MEMORY"), "{err}");
}

#[tokio::test]
async fn deprovision_removes_the_rootfs() {
    let (backend, tmp) = fresh_backend().await;
    let ctx = backend
        .provision("deprov", &SandboxSpec::default())
        .await
        .unwrap();
    let rootfs = tmp.path().join("sandboxes/deprov");
    assert!(rootfs.is_dir());
    backend.deprovision(&ctx).await.unwrap();
    assert!(!rootfs.exists());
}

#[tokio::test]
async fn egress_rules_are_rejected_not_degraded() {
    // The capability contract in action: Seatbelt declares no
    // NET_EGRESS_RULES, so a spec with explicit rules must fail validation
    // instead of being silently downgraded to deny-all.
    let (backend, _tmp) = fresh_backend().await;
    let spec = SandboxSpec {
        net: NetPolicy {
            egress_default: EgressDefault::Deny,
            allow: vec![NetTarget {
                target: "example.com".into(),
            }],
            deny: vec![],
            proxy: false,
        },
        ..Default::default()
    };
    let err = backend.provision("no-rules", &spec).await.unwrap_err();
    assert_eq!(err.code(), "policy_validation");
    assert!(err.to_string().contains("NET_EGRESS_RULES"), "{err}");
}

#[tokio::test]
async fn args_are_passed_verbatim_no_shell_interpretation() {
    // Exec does not go through a shell: metacharacters in args are inert data.
    let (backend, _tmp) = fresh_backend().await;
    let ctx = backend
        .provision("no-shell", &SandboxSpec::default())
        .await
        .unwrap();
    let payload = "a; rm -rf /`whoami`$(id)";
    let outcome = backend
        .exec(&ctx, exec_req("/bin/echo", &[payload]))
        .await
        .unwrap();
    assert_eq!(outcome.exit_code, Some(0));
    assert_eq!(
        outcome.stdout.trim(),
        payload,
        "arg must round-trip verbatim"
    );
}

#[tokio::test]
async fn relative_cwd_inside_rootfs_works() {
    let (backend, tmp) = fresh_backend().await;
    let ctx = backend
        .provision("cwd-test", &SandboxSpec::default())
        .await
        .unwrap();
    std::fs::create_dir_all(tmp.path().join("sandboxes/cwd-test/sub")).unwrap();
    let mut req = exec_req("/bin/pwd", &[]);
    req.cwd = "sub".into();
    let outcome = backend.exec(&ctx, req).await.unwrap();
    // pwd prints the kernel-resolved path (/var/folders → /private/var/folders),
    // matching the canonicalized rootfs stored in the context.
    let expected = std::fs::canonicalize(tmp.path().join("sandboxes/cwd-test/sub")).unwrap();
    assert_eq!(outcome.stdout.trim(), expected.to_string_lossy());
}

#[tokio::test]
async fn absolute_cwd_is_rejected() {
    let (backend, _tmp) = fresh_backend().await;
    let ctx = backend
        .provision("cwd-abs", &SandboxSpec::default())
        .await
        .unwrap();
    let mut req = exec_req("/bin/pwd", &[]);
    req.cwd = "/etc".into();
    let err = backend.exec(&ctx, req).await.unwrap_err();
    assert_eq!(err.code(), "policy_validation");
}

#[tokio::test]
async fn traversal_cwd_is_rejected() {
    let (backend, _tmp) = fresh_backend().await;
    let ctx = backend
        .provision("cwd-traversal", &SandboxSpec::default())
        .await
        .unwrap();
    let mut req = exec_req("/bin/pwd", &[]);
    req.cwd = "../outside".into();
    let err = backend.exec(&ctx, req).await.unwrap_err();
    assert_eq!(err.code(), "policy_validation");
}

#[tokio::test]
async fn template_seeds_the_rootfs() {
    let (backend, tmp) = fresh_backend().await;

    // Build a template by hand (the commit endpoint does this via the API).
    let tpl = tmp.path().join("templates").join("py-env");
    std::fs::create_dir_all(tpl.join("lib")).unwrap();
    std::fs::write(tpl.join("lib/seed.txt"), "seeded").unwrap();

    let ctx = backend
        .provision(
            "from-template",
            &SandboxSpec {
                template: Some("py-env".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let outcome = backend
        .exec(&ctx, exec_req("/bin/cat", &["lib/seed.txt"]))
        .await
        .unwrap();
    assert_eq!(outcome.stdout, "seeded");
    assert_eq!(outcome.exit_code, Some(0));

    // An unknown template is a policy error, not an empty rootfs.
    let err = backend
        .provision(
            "from-missing",
            &SandboxSpec {
                template: Some("missing-tpl".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "policy_validation");
}

/// Minimal upstream HTTP server that captures the request head and asserts
/// the proxy injected the credential. Returns the bound port.
async fn spawn_upstream(captured: Arc<std::sync::Mutex<String>>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut conn, _)) = listener.accept().await else {
                return;
            };
            let captured = captured.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let n = conn.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                *captured.lock().unwrap() = String::from_utf8_lossy(&buf).to_string();
                conn.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nproxy-ok",
                )
                .await
                .unwrap();
            });
        }
    });
    port
}

/// Enforcement evidence for NET_EGRESS_PROXY: the sandbox reaches the
/// internet ONLY through the proxy; the credential arrives at the upstream
/// injected, and direct egress to any other target is denied.
#[tokio::test]
async fn proxy_mode_egress_goes_through_the_credential_proxy() {
    let captured = Arc::new(std::sync::Mutex::new(String::new()));
    let upstream = spawn_upstream(captured.clone()).await;

    let creds = Arc::new(std::collections::BTreeMap::from([(
        "127.0.0.1".to_string(),
        "Bearer tok-123".to_string(),
    )]));
    // AllowAll: this test's upstream intentionally lives on loopback. The
    // production default (DenyPrivate) is proven in the egress-proxy crate
    // (deny_private_blocks_* tests).
    let proxy = sandbox_egress_proxy::spawn_with_policy(
        "127.0.0.1:0",
        creds,
        sandbox_egress_proxy::EgressPolicy::AllowAll,
    )
    .await
    .unwrap();

    let (backend, _tmp) = fresh_backend().await;
    let backend = backend.with_egress_proxy(proxy.to_string());
    let spec = SandboxSpec {
        net: NetPolicy {
            proxy: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let ctx = backend.provision("via-proxy", &spec).await.unwrap();

    // The workload sees no credential anywhere; the upstream sees it.
    let outcome = backend
        .exec(
            &ctx,
            exec_req(
                "/usr/bin/curl",
                &["-s", &format!("http://127.0.0.1:{upstream}/")],
            ),
        )
        .await
        .unwrap();
    assert_eq!(outcome.exit_code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "proxy-ok");
    let seen = captured.lock().unwrap().clone();
    assert!(seen.contains("Authorization: Bearer tok-123\r\n"), "{seen}");

    // Direct egress (proxy env cleared for this exec) to a non-proxy target
    // is denied by the profile: curl fails fast with exit 7, no timeout.
    let mut direct = exec_req("/usr/bin/curl", &["-s", "http://203.0.113.5/"]);
    direct.env.insert("NO_PROXY".to_string(), "*".to_string());
    direct.env.insert("no_proxy".to_string(), "*".to_string());
    let denied = backend.exec(&ctx, direct).await.unwrap();
    assert_ne!(denied.exit_code, Some(0));
    assert!(!denied.timed_out);
    assert!(!captured.lock().unwrap().contains("203.0.113.5"));
}
