//! Live integration test: the SDK drives the real router over real TCP.
//! This is the contract-in-action proof — every call here goes through
//! hyper's HTTP/1.1 client against the exact axum server the binary runs.

use std::path::PathBuf;
use std::sync::Arc;

use sandbox_core::policy::SandboxSpec;
use sandbox_core::{
    BackendCapabilities, BackendHealth, ExecOutcome, ExecRequest, SandboxBackend, SandboxContext,
};
use sandbox_sdk::{Client, ExecSpec};

/// Canned backend with a real rootfs under a shared data root, so template
/// commit + create-from-template work end-to-end over HTTP.
struct StubBackend {
    data_root: PathBuf,
}

#[async_trait::async_trait]
impl SandboxBackend for StubBackend {
    fn name(&self) -> &'static str {
        "stub"
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::FS_READWRITE
            .union(BackendCapabilities::FS_READONLY)
            .union(BackendCapabilities::FS_DENIED)
            .union(BackendCapabilities::NET_EGRESS_DEFAULT)
            .union(BackendCapabilities::NET_EGRESS_PROXY)
            .union(BackendCapabilities::BUDGET_PIDS)
            .union(BackendCapabilities::BUDGET_NOFILE)
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
        sandbox_core::check_policy_support(spec, self.capabilities())?;
        let rootfs = self.data_root.join("sandboxes").join(id);
        tokio::fs::create_dir_all(&rootfs).await.unwrap();
        let rootfs = tokio::fs::canonicalize(&rootfs).await.unwrap();
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

    async fn exec(
        &self,
        _ctx: &SandboxContext,
        req: ExecRequest,
    ) -> sandbox_core::Result<ExecOutcome> {
        req.validate()?;
        Ok(ExecOutcome {
            exit_code: Some(0),
            timed_out: false,
            stdout: format!("stub ran {}", req.command),
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

/// Serve the real router on 127.0.0.1:0; returns the base URL. The data-root
/// tempdir is leaked — the test process is short-lived.
async fn spawn_server(token: Option<&str>) -> String {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = tmp.path().to_path_buf();
    std::mem::forget(tmp);
    let backend = Arc::new(StubBackend {
        data_root: data_root.clone(),
    });
    let state = Arc::new(
        sandbox_api::state::AppState::new(backend, data_root)
            .with_auth_token(token.map(str::to_string)),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, sandbox_api::routes::router(state))
            .await
            .unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn full_lifecycle_over_real_tcp() {
    let base = spawn_server(None).await;
    let client = Client::new(&base);

    let health = client.health().await.unwrap();
    assert_eq!(health.status, "ok");
    assert_eq!(health.sandboxes, 0);

    // Create + exec.
    let sandbox = client.create(&SandboxSpec::default()).await.unwrap();
    assert!(sandbox.id.starts_with("sbx-"));
    assert_eq!(sandbox.state, "running");

    let outcome = client
        .exec(
            &sandbox.id,
            ExecSpec {
                command: "echo".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(outcome.exit_code, Some(0));
    assert_eq!(outcome.stdout, "stub ran echo");

    // Files.
    client
        .file_write(&sandbox.id, "data/notes.txt", "hello sdk")
        .await
        .unwrap();
    let content = client
        .file_read(&sandbox.id, "data/notes.txt")
        .await
        .unwrap();
    // "hello sdk" in base64.
    assert_eq!(content.content_base64, "aGVsbG8gc2Rr");
    let entries = client.file_list(&sandbox.id, "data").await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "notes.txt");

    // Usage ledger moved with the exec + file ops.
    let detail = client.get(&sandbox.id).await.unwrap();
    assert_eq!(detail.usage.execs_started, 1);
    // write + read + list all counted.
    assert_eq!(detail.usage.file_ops, 3);
    assert_eq!(detail.usage.output_bytes, "stub ran echo".len() as u64);

    // Events with the after-cursor.
    let page = client.events(&sandbox.id, 0).await.unwrap();
    assert_eq!(page.last_seq, 3); // created, exec_started, exec_finished
    let tail = client.events(&sandbox.id, 2).await.unwrap();
    assert_eq!(tail.events.len(), 1);
    assert_eq!(tail.events[0].seq, 3);

    // Pause / resume.
    client.pause(&sandbox.id).await.unwrap();
    assert_eq!(client.get(&sandbox.id).await.unwrap().state, "paused");
    client.resume(&sandbox.id).await.unwrap();
    assert_eq!(client.get(&sandbox.id).await.unwrap().state, "running");

    // Commit → create-from-template.
    client.commit(&sandbox.id, "sdk-tpl").await.unwrap();
    let other = client
        .create_with(
            &SandboxSpec {
                template: Some("sdk-tpl".into()),
                ..Default::default()
            },
            sandbox_sdk::CreateOptions {
                timeout_ms: Some(60_000),
                idempotency_key: None,
            },
        )
        .await
        .unwrap();
    let inherited = client.file_read(&other.id, "data/notes.txt").await.unwrap();
    assert_eq!(inherited.content_base64, "aGVsbG8gc2Rr");
    assert_eq!(
        client.get(&other.id).await.unwrap().timeout_ms,
        Some(60_000)
    );

    // Metrics text is reachable.
    let metrics = client.metrics().await.unwrap();
    assert!(
        metrics.contains("bsandbox_sandboxes_created_total 2"),
        "{metrics}"
    );

    // Delete + 404 mapping.
    client.delete(&sandbox.id).await.unwrap();
    client.delete(&other.id).await.unwrap();
    let err = client.get(&sandbox.id).await.unwrap_err();
    assert_eq!(err.status, 404);
    assert!(err.is_not_found());

    // List is empty again.
    assert!(client.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn token_auth_and_error_envelope_over_real_tcp() {
    let base = spawn_server(Some("tok-42")).await;
    let client = Client::new(&base);

    // No token → 401 with the envelope code.
    let err = client.create(&SandboxSpec::default()).await.unwrap_err();
    assert_eq!(err.status, 401);
    assert!(err.is_unauthorized());

    // Wrong token → 401. Right token → full access.
    let err = Client::new(&base)
        .with_token("wrong")
        .create(&SandboxSpec::default())
        .await
        .unwrap_err();
    assert!(err.is_unauthorized());

    let authed = Client::new(&base).with_token("tok-42");
    let health = authed.health().await.unwrap();
    assert_eq!(health.status, "ok");
    let sandbox = authed.create(&SandboxSpec::default()).await.unwrap();
    assert!(sandbox.id.starts_with("sbx-"));
}

#[tokio::test]
async fn policy_validation_reaches_the_client_as_a_typed_error() {
    let base = spawn_server(None).await;
    // Unknown spec field (hallucinated by an agent) → 400 policy_validation.
    // Typed specs cannot carry unknown fields, so this call is raw by design.
    let body = serde_json::json!({"gpu": true});
    let response = reqwest_like_raw_post(&base, &body).await;
    assert_eq!(response.0, 400);
    assert_eq!(response.1["error"]["code"], "policy_validation");
}

/// Minimal raw POST used only to prove the envelope shape the SDK decodes.
async fn reqwest_like_raw_post(base: &str, body: &serde_json::Value) -> (u16, serde_json::Value) {
    use hyper::header::CONTENT_TYPE;
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http();
    let req = hyper::Request::builder()
        .method(hyper::Method::POST)
        .uri(format!("{base}/sandboxes"))
        .header(CONTENT_TYPE, "application/json")
        .body(http_body_util::Full::new(hyper::body::Bytes::from(
            body.to_string(),
        )))
        .unwrap();
    let response = client.request(req).await.unwrap();
    let status = response.status().as_u16();
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
