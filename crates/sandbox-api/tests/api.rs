//! Black-box integration tests for the REST API, driven through the exact
//! router the binary serves (tower oneshot, no network). API mechanics use a
//! canned mock backend so they run on every host; the macOS end-to-end test
//! at the bottom wires the real Seatbelt backend.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use sandbox_api::routes::router;
use sandbox_api::state::AppState;
use sandbox_core::policy::SandboxSpec;
use sandbox_core::{
    BackendCapabilities, BackendHealth, ExecOutcome, ExecRequest, SandboxBackend, SandboxContext,
};

/// Canned backend for API mechanics: real rootfs dirs (so the file data plane
/// and its traversal defenses are exercised for real), canned exec outcomes.
/// Enforcement evidence lives in the backend test suites, not here.
struct MockBackend {
    data_root: PathBuf,
    available: bool,
}

impl MockBackend {
    fn available() -> Self {
        Self {
            data_root: std::env::temp_dir(),
            available: true,
        }
    }

    fn unavailable() -> Self {
        Self {
            data_root: std::env::temp_dir(),
            available: false,
        }
    }
}

#[async_trait::async_trait]
impl SandboxBackend for MockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn capabilities(&self) -> BackendCapabilities {
        // Everything declared: the mock must accept whatever the API layer
        // passes through so mechanics tests never fail on policy gates.
        BackendCapabilities::FS_READWRITE
            .union(BackendCapabilities::FS_READONLY)
            .union(BackendCapabilities::FS_DENIED)
            .union(BackendCapabilities::NET_EGRESS_DEFAULT)
            .union(BackendCapabilities::NET_EGRESS_RULES)
            .union(BackendCapabilities::BUDGET_MEMORY)
            .union(BackendCapabilities::BUDGET_PIDS)
            .union(BackendCapabilities::BUDGET_NOFILE)
    }

    fn health(&self) -> BackendHealth {
        if self.available {
            BackendHealth::available()
        } else {
            BackendHealth::unavailable("mock is down")
        }
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
            stdout: format!("mock ran {:?}", req.command),
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

fn app(backend: MockBackend) -> axum::Router {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = tmp.path().to_path_buf();
    // The router owns the state; leak the tempdir so data outlives the test
    // (tests are short-lived; this keeps the helper signature simple).
    std::mem::forget(tmp);
    router(Arc::new(AppState::new(Arc::new(backend), data_root)))
}

async fn call(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let req = builder
        .body(Body::from(body.map(|v| v.to_string()).unwrap_or_default()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

fn create_body() -> Value {
    json!({})
}

async fn create_sandbox(app: axum::Router) -> (axum::Router, String) {
    let (status, body) = call(app.clone(), "POST", "/sandboxes", Some(create_body())).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    (app, body["id"].as_str().unwrap().to_string())
}

// ---------- health ----------

#[tokio::test]
async fn health_reports_backend_and_counts() {
    let app = app(MockBackend::available());
    let (status, body) = call(app, "GET", "/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["backend"]["name"], "mock");
    assert_eq!(body["backend"]["available"], true);
    assert_eq!(body["sandboxes"], 0);
}

#[tokio::test]
async fn health_reports_degraded_when_backend_is_down() {
    let app = app(MockBackend::unavailable());
    let (status, body) = call(app, "GET", "/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "degraded");
    assert_eq!(body["backend"]["available"], false);
}

// ---------- lifecycle ----------

#[tokio::test]
async fn create_list_get_delete_roundtrip() {
    let app = app(MockBackend::available());
    let (app, id) = create_sandbox(app).await;
    assert!(id.starts_with("sbx-"));

    let (status, body) = call(app.clone(), "GET", "/sandboxes", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["sandboxes"].as_array().unwrap().len(), 1);
    assert_eq!(body["sandboxes"][0]["id"], id.as_str());
    assert_eq!(body["sandboxes"][0]["state"], "running");

    let (status, body) = call(app.clone(), "GET", &format!("/sandboxes/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "running");

    let (status, _) = call(app.clone(), "DELETE", &format!("/sandboxes/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = call(app.clone(), "GET", &format!("/sandboxes/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "sandbox_not_found");

    let (status, _) = call(app.clone(), "DELETE", &format!("/sandboxes/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_rejects_unknown_spec_fields() {
    let app = app(MockBackend::available());
    let (status, body) = call(
        app,
        "POST",
        "/sandboxes",
        Some(json!({"fs": {}, "net": {}, "exec": {}, "budget": {}, "gpu": true})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "policy_validation");
}

#[tokio::test]
async fn create_on_unavailable_backend_is_503() {
    let app = app(MockBackend::unavailable());
    let (status, body) = call(app, "POST", "/sandboxes", Some(create_body())).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "backend_unavailable");
}

#[tokio::test]
async fn idempotency_key_replays_the_same_sandbox() {
    let app = app(MockBackend::available());
    // Two separate calls with the same key must return the same id.
    let make = |app: axum::Router| {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/sandboxes")
                .header("content-type", "application/json")
                .header("idempotency-key", "k-1")
                .body(Body::from(create_body().to_string()))
                .unwrap(),
        )
    };
    let response = make(app.clone()).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let first: Value = serde_json::from_slice(&bytes).unwrap();

    let response = make(app).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let second: Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(first["id"], second["id"]);
}

// ---------- exec ----------

#[tokio::test]
async fn exec_returns_outcome_and_records_events() {
    let app = app(MockBackend::available());
    let (app, id) = create_sandbox(app).await;

    let (status, body) = call(
        app.clone(),
        "POST",
        &format!("/sandboxes/{id}/exec"),
        Some(json!({"command": "echo", "args": ["hi"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["exitCode"], 0);
    assert_eq!(body["stdout"], "mock ran \"echo\"");

    let (status, events) = call(app, "GET", &format!("/sandboxes/{id}/events"), None).await;
    assert_eq!(status, StatusCode::OK);
    let kinds: Vec<&str> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        vec!["sandbox_created", "exec_started", "exec_finished"]
    );
    assert_eq!(events["lastSeq"], 3);
}

#[tokio::test]
async fn exec_on_missing_sandbox_is_404() {
    let app = app(MockBackend::available());
    let (status, body) = call(
        app,
        "POST",
        "/sandboxes/sbx-missing/exec",
        Some(json!({"command": "echo"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "sandbox_not_found");
}

#[tokio::test]
async fn exec_rejects_bad_requests() {
    let app = app(MockBackend::available());
    let (app, id) = create_sandbox(app).await;

    // Empty command.
    let (status, body) = call(
        app.clone(),
        "POST",
        &format!("/sandboxes/{id}/exec"),
        Some(json!({"command": "   "})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "policy_validation");

    // Unknown field.
    let (status, _) = call(
        app.clone(),
        "POST",
        &format!("/sandboxes/{id}/exec"),
        Some(json!({"command": "echo", "shell": true})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Timeout over the ceiling.
    let (status, body) = call(
        app,
        "POST",
        &format!("/sandboxes/{id}/exec"),
        Some(json!({"command": "echo", "timeoutMs": 999_999_999})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("ceiling")
    );
}

// ---------- file data plane ----------

#[tokio::test]
async fn file_write_read_list_stat_remove_roundtrip() {
    let app = app(MockBackend::available());
    let (app, id) = create_sandbox(app).await;
    let base = format!("/sandboxes/{id}/files");

    let (status, _) = call(
        app.clone(),
        "POST",
        &format!("{base}/write"),
        Some(json!({"path": "data/notes.txt", "content": "hello sandbox"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = call(
        app.clone(),
        "POST",
        &format!("{base}/read"),
        Some(json!({"path": "data/notes.txt"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // "hello sandbox" in base64.
    assert_eq!(body["contentBase64"], "aGVsbG8gc2FuZGJveA==");
    assert_eq!(body["size"], 13);

    let (status, body) = call(
        app.clone(),
        "POST",
        &format!("{base}/list"),
        Some(json!({"path": "data"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["notes.txt"]);

    let (status, body) = call(
        app.clone(),
        "POST",
        &format!("{base}/stat"),
        Some(json!({"path": "data"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["type"], "dir");

    let (status, _) = call(
        app.clone(),
        "POST",
        &format!("{base}/remove"),
        Some(json!({"path": "data/notes.txt"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = call(
        app,
        "POST",
        &format!("{base}/read"),
        Some(json!({"path": "data/notes.txt"})),
    )
    .await;
    // A missing file is a client error, not a server fault.
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "policy_validation");
}

#[tokio::test]
async fn file_paths_reject_traversal_attacks() {
    let app = app(MockBackend::available());
    let (app, id) = create_sandbox(app).await;
    let base = format!("/sandboxes/{id}/files");

    for attack in [
        "../escape",
        "../../etc/passwd",
        "/etc/passwd",
        "data/../../x",
    ] {
        let (status, body) = call(
            app.clone(),
            "POST",
            &format!("{base}/read"),
            Some(json!({"path": attack})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "read {attack:?} must be denied: {body}"
        );
        assert_eq!(body["error"]["code"], "policy_validation");

        let (status, _) = call(
            app.clone(),
            "POST",
            &format!("{base}/write"),
            Some(json!({"path": attack, "content": "x"})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "write {attack:?} must be denied"
        );
    }
}

#[tokio::test]
async fn symlink_escape_is_denied() {
    let app = app(MockBackend::available());
    let (app, id) = create_sandbox(app).await;

    // Create an escape symlink inside the sandbox rootfs directly on disk;
    // the rootfs location comes from the detail view.
    let (_status, body) = call(app.clone(), "GET", &format!("/sandboxes/{id}"), None).await;
    let rootfs = PathBuf::from(body["rootfs"].as_str().unwrap());
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc", rootfs.join("host-etc")).unwrap();

    let (status, body) = call(
        app,
        "POST",
        &format!("/sandboxes/{id}/files/read"),
        Some(json!({"path": "host-etc/passwd"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "symlink escape must be denied"
    );
    assert_eq!(body["error"]["code"], "policy_validation");
}

// ---------- events cursor ----------

#[tokio::test]
async fn events_after_cursor_reads_forward() {
    let app = app(MockBackend::available());
    let (app, id) = create_sandbox(app).await;
    call(
        app.clone(),
        "POST",
        &format!("/sandboxes/{id}/exec"),
        Some(json!({"command": "one"})),
    )
    .await;
    call(
        app.clone(),
        "POST",
        &format!("/sandboxes/{id}/exec"),
        Some(json!({"command": "two"})),
    )
    .await;

    let (status, body) = call(app, "GET", &format!("/sandboxes/{id}/events?after=2"), None).await;
    assert_eq!(status, StatusCode::OK);
    let events = body["events"].as_array().unwrap();
    assert!(events.len() >= 2, "at least the second exec pair");
    assert_eq!(events[0]["seq"], 3);
}

// ---------- usage ledger ----------

#[tokio::test]
async fn usage_ledger_tracks_a_full_cycle() {
    let app = app(MockBackend::available());
    let (app, id) = create_sandbox(app).await;

    // One exec: mock stdout is `mock ran "echo"` — 15 bytes.
    let (status, _) = call(
        app.clone(),
        "POST",
        &format!("/sandboxes/{id}/exec"),
        Some(json!({"command": "echo"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Two file ops that count; one denied op that must not.
    let (status, _) = call(
        app.clone(),
        "POST",
        &format!("/sandboxes/{id}/files/write"),
        Some(json!({"path": "notes.txt", "content": "hi"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        app.clone(),
        "POST",
        &format!("/sandboxes/{id}/files/read"),
        Some(json!({"path": "notes.txt"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        app.clone(),
        "POST",
        &format!("/sandboxes/{id}/files/read"),
        Some(json!({"path": "missing.txt"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, body) = call(app, "GET", &format!("/sandboxes/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let usage = &body["usage"];
    assert_eq!(usage["execsStarted"], 1);
    assert_eq!(usage["execsFinished"], 1);
    assert_eq!(usage["execsTimedOut"], 0);
    assert_eq!(usage["outputBytes"], 15);
    assert_eq!(usage["fileOps"], 2);
    assert!(usage["createdAtMs"].as_u64().unwrap() > 0);
    assert!(usage["lastActivityMs"].as_u64().unwrap() >= usage["createdAtMs"].as_u64().unwrap());
}

// ---------- macOS end-to-end with the real backend ----------

#[cfg(target_os = "macos")]
#[tokio::test]
async fn seatbelt_end_to_end_echo_through_the_api() {
    let tmp = tempfile::tempdir().unwrap();
    let backend = Arc::new(sandbox_backend_seatbelt::SeatbeltBackend::new(
        tmp.path().to_path_buf(),
    ));
    let app = router(Arc::new(AppState::new(backend, tmp.path().to_path_buf())));

    let (status, body) = call(app.clone(), "POST", "/sandboxes", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_string();

    let (status, body) = call(
        app.clone(),
        "POST",
        &format!("/sandboxes/{id}/exec"),
        Some(json!({"command": "/bin/echo", "args": ["through", "the", "api"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["stdout"], "through the api\n");
    assert_eq!(body["exitCode"], 0);

    // Write a file through the API, read it back with a sandboxed process.
    let (status, _) = call(
        app.clone(),
        "POST",
        &format!("/sandboxes/{id}/files/write"),
        Some(json!({"path": "api-written.txt", "content": "from the api"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = call(
        app.clone(),
        "POST",
        &format!("/sandboxes/{id}/exec"),
        Some(json!({"command": "/bin/cat", "args": ["api-written.txt"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["stdout"], "from the api");

    let (status, _) = call(app, "DELETE", &format!("/sandboxes/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}
