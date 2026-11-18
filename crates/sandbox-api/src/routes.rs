//! REST routes for M1: sandbox lifecycle, exec, file data plane, events,
//! health. Resource shapes intentionally mirror E2B where it costs nothing;
//! the full E2B alignment (auth, /v2 routes, multipart files) is M2 work.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{MethodRouter, delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{ApiError, ApiJson, ApiResult, backend_unavailable, sandbox_not_found};
use crate::files::resolve_sandbox_path;
use crate::ledger::now_ms;
use crate::state::{SandboxRecord, SharedState, new_sandbox_id};
use sandbox_core::events::EventKind;
use sandbox_core::{Error, ExecOutcome, LifecycleState};

/// One route of the route table: (method, path, handler).
pub type RouteEntry = (&'static str, &'static str, MethodRouter<SharedState>);

/// The route table: the single source of truth for the served surface. The
/// router is built from it, and the OpenAPI contract test
/// (tests/contract.rs) proves table ↔ router ↔ spec equivalence — a new
/// handler that skips the table fails the gate.
pub fn route_table() -> Vec<RouteEntry> {
    vec![
        ("GET", "/health", get(health)),
        ("POST", "/sandboxes", post(create_sandbox)),
        ("GET", "/sandboxes", get(list_sandboxes)),
        ("GET", "/sandboxes/{id}", get(get_sandbox)),
        ("DELETE", "/sandboxes/{id}", delete(delete_sandbox)),
        ("POST", "/sandboxes/{id}/pause", post(pause_sandbox)),
        ("POST", "/sandboxes/{id}/resume", post(resume_sandbox)),
        ("POST", "/sandboxes/{id}/commit", post(commit_sandbox)),
        ("POST", "/sandboxes/{id}/exec", post(exec)),
        ("POST", "/sandboxes/{id}/files/read", post(file_read)),
        ("POST", "/sandboxes/{id}/files/write", post(file_write)),
        ("POST", "/sandboxes/{id}/files/list", post(file_list)),
        ("POST", "/sandboxes/{id}/files/stat", post(file_stat)),
        ("POST", "/sandboxes/{id}/files/mkdir", post(file_mkdir)),
        ("POST", "/sandboxes/{id}/files/remove", post(file_remove)),
        ("GET", "/sandboxes/{id}/events", get(events)),
        ("GET", "/sandboxes/{id}/events/stream", get(events_stream)),
        ("GET", "/metrics", get(metrics)),
    ]
}

/// (method, path pattern) pairs of the served surface — what the
/// observability middleware maps concrete request paths back to.
pub fn route_patterns() -> Vec<(&'static str, &'static str)> {
    route_table()
        .into_iter()
        .map(|(method, path, _)| (method, path))
        .collect()
}

pub fn router(state: SharedState) -> Router {
    let table = route_table();
    // Token auth wraps every route; the middleware itself opens /health.

    // Same-path entries with different methods fold into one MethodRouter —
    // axum rejects duplicate registrations of the same path.
    let paths: std::collections::BTreeSet<&'static str> =
        table.iter().map(|(_, path, _)| *path).collect();
    let mut router = Router::new();
    for path in paths {
        let handlers: Vec<MethodRouter<SharedState>> = table
            .iter()
            .filter(|(_, p, _)| *p == path)
            .map(|(_, _, handler)| handler.clone())
            .collect();
        let mut merged = handlers.first().expect("table path has handlers").clone();
        for handler in handlers.iter().skip(1) {
            merged = merged.merge(handler.clone());
        }
        router = router.route(path, merged);
    }
    router
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        // Outermost observing layer: observe sees even auth rejections and
        // unmatched paths, so the metrics picture is complete traffic.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            observe_middleware,
        ))
        .layer(axum::middleware::from_fn(body_limit_middleware))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        // Outermost guard: a handler panic must degrade to an envelope 500
        // on that one connection, not a dead socket.
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(
            panic_envelope,
        ))
        .with_state(state)
}

/// Request-body ceiling: `Content-Length` above this is answered 413 before
/// any bytes are read. Chunked bodies fall through to the extractor-level
/// DefaultBodyLimit, which the JSON envelope maps to the same 413.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

async fn body_limit_middleware(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let too_big = req
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|len| len > MAX_BODY_BYTES);
    if too_big {
        return crate::error::payload_too_large().into_response();
    }
    next.run(req).await
}

/// Panic catcher: log the payload server-side, answer the fixed internal
/// envelope. The code matches `Error::Internal`'s — one internal vocabulary.
fn panic_envelope(err: Box<dyn std::any::Any + Send>) -> axum::response::Response {
    let detail = if let Some(s) = err.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = err.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    };
    tracing::error!(panic = %detail, "handler panicked; returning 500");
    crate::error::internal_panic().into_response()
}

/// Bearer-token gate. Open when no token is configured; `/health` stays
/// open either way (the deployment probe must not need secrets). Token
/// comparison is constant-time.
async fn auth_middleware(
    State(state): State<SharedState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, ApiError> {
    if req.uri().path() == "/health" || state.auth_token.is_none() {
        return Ok(next.run(req).await);
    }
    let ok = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|given| {
            ct_eq(
                given.as_bytes(),
                state.auth_token.as_deref().unwrap_or("").as_bytes(),
            )
        });
    if ok {
        Ok(next.run(req).await)
    } else {
        Err(crate::error::unauthorized())
    }
}

/// Constant-time byte equality (no early exit on the secret).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Observability middleware: one structured log line plus HTTP metrics for
/// every request reaching the router — including auth rejections and
/// unmatched paths. Latency measures time-to-response-headers (the SSE
/// handshake, not the stream lifetime).
async fn observe_middleware(
    State(state): State<SharedState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = req.method().as_str().to_owned();
    let path = req.uri().path().to_owned();
    let route = normalize_route(&state.route_patterns, &method, &path);
    let started = std::time::Instant::now();
    state.metrics.http_in_flight_inc();
    let response = next.run(req).await;
    state.metrics.http_in_flight_dec();
    let elapsed = started.elapsed();
    let status = response.status().as_u16();
    state.metrics.record_http(&method, route, status, elapsed);
    tracing::info!(
        method = %method,
        route = route,
        path = %path,
        status,
        duration_ms = elapsed.as_millis() as u64,
        "request"
    );
    response
}

/// Map a concrete request path back to its route-table pattern ("unmatched"
/// when nothing fits — 404s and the like). `{id}` matches one segment, so
/// metric cardinality stays bounded by the route table itself.
fn normalize_route(
    patterns: &[(&'static str, &'static str)],
    method: &str,
    path: &str,
) -> &'static str {
    patterns
        .iter()
        .find(|(m, p)| *m == method && path_matches(p, path))
        .map(|(_, p)| *p)
        .unwrap_or("unmatched")
}

fn path_matches(pattern: &str, path: &str) -> bool {
    let mut pattern = pattern.split('/');
    let mut path = path.split('/');
    loop {
        match (pattern.next(), path.next()) {
            (None, None) => return true,
            (Some(p), Some(s)) => {
                if !(p.starts_with('{') || p == s) {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ct_eq;
    use super::{normalize_route, path_matches, route_patterns};

    #[test]
    fn constant_time_equality_matches_only_equal_slices() {
        assert!(ct_eq(b"tok-1", b"tok-1"));
        assert!(!ct_eq(b"tok-1", b"tok-2"));
        assert!(!ct_eq(b"tok-1", b"tok-12"));
        assert!(!ct_eq(b"", b"a"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn concrete_paths_map_back_to_route_patterns() {
        let patterns = route_patterns();
        assert_eq!(
            normalize_route(&patterns, "POST", "/sandboxes/sbx-abc123/exec"),
            "/sandboxes/{id}/exec"
        );
        assert_eq!(
            normalize_route(&patterns, "GET", "/sandboxes"),
            "/sandboxes"
        );
        assert_eq!(normalize_route(&patterns, "GET", "/health"), "/health");
    }

    #[test]
    fn unmatched_paths_collapse_to_one_label() {
        let patterns = route_patterns();
        assert_eq!(normalize_route(&patterns, "GET", "/nope"), "unmatched");
        assert_eq!(normalize_route(&patterns, "PUT", "/sandboxes"), "unmatched");
        assert_eq!(
            normalize_route(&patterns, "POST", "/sandboxes/sbx-1/files/nope"),
            "unmatched"
        );
    }

    #[test]
    fn pattern_matching_requires_segment_count_equality() {
        assert!(path_matches(
            "/sandboxes/{id}/exec",
            "/sandboxes/sbx-1/exec"
        ));
        assert!(!path_matches("/sandboxes/{id}/exec", "/sandboxes/exec"));
        assert!(!path_matches("/sandboxes/{id}", "/sandboxes/a/b"));
        assert!(path_matches("/health", "/health"));
        assert!(!path_matches("/health", "/healthz"));
    }
}

// ---------- health ----------

async fn health(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let backend_health = state.backend.health();
    let count = state.registry.read().unwrap().len();
    Json(json!({
        "status": if backend_health.available { "ok" } else { "degraded" },
        "version": env!("CARGO_PKG_VERSION"),
        "backend": {
            "name": state.backend.name(),
            "available": backend_health.available,
            "reason": backend_health.reason,
        },
        "sandboxes": count,
    }))
}

async fn metrics(State(state): State<SharedState>) -> impl axum::response::IntoResponse {
    let active = state.registry.read().unwrap().len() as u64;
    let body = state.metrics.render(active);
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
}

// ---------- lifecycle ----------

#[derive(Debug, Serialize)]
struct SandboxView {
    id: String,
    state: &'static str,
    rootfs: String,
}

fn view_of(record: &SandboxRecord) -> SandboxView {
    SandboxView {
        id: record.ctx.id.clone(),
        state: record.lifecycle.lock().unwrap().state().as_str(),
        rootfs: record.ctx.rootfs.display().to_string(),
    }
}

/// Parsed creation body: the sandbox spec (the request object itself) plus
/// the service-level idle TTL extracted from it.
#[derive(Debug)]
struct CreateBody {
    spec: sandbox_core::policy::SandboxSpec,
    /// Idle TTL in ms: the reaper terminates the sandbox after this long
    /// without data-plane activity. Absent = no TTL.
    timeout_ms: Option<u64>,
}

/// Parse the create body. The wire object IS the spec; `timeoutMs` is the
/// one envelope field on top. Deliberately parsed via `from_value` instead
/// of serde flatten: flatten silently disables `deny_unknown_fields`, which
/// would reopen the unknown-field hole the spec exists to close.
fn parse_create_body(value: serde_json::Value) -> ApiResult<CreateBody> {
    let policy = |msg: String| ApiError::from(Error::Policy(msg));
    let mut map = match value {
        serde_json::Value::Object(map) => map,
        other => {
            return Err(policy(format!(
                "create body must be a JSON object, got {other}"
            )));
        }
    };
    let timeout_ms = match map.remove("timeoutMs") {
        None => None,
        Some(raw) => serde_json::from_value::<Option<u64>>(raw)
            .map_err(|e| policy(format!("timeout_ms: {e}")))?,
    };
    let spec = serde_json::from_value::<sandbox_core::policy::SandboxSpec>(map.into())
        .map_err(|e| policy(e.to_string()))?;
    Ok(CreateBody { spec, timeout_ms })
}

/// One week: long enough for any legitimate session, short enough that a
/// misconfigured TTL cannot accumulate zombies forever.
const TTL_MS_CEILING: u64 = 7 * 24 * 60 * 60 * 1000;

fn validate_ttl(timeout_ms: Option<u64>) -> ApiResult<()> {
    match timeout_ms {
        None => Ok(()),
        Some(0) => Err(ApiError::from(Error::Policy(
            "timeout_ms must be at least 1 ms".into(),
        ))),
        Some(ms) if ms > TTL_MS_CEILING => Err(ApiError::from(Error::Policy(format!(
            "timeout_ms exceeds the {TTL_MS_CEILING} ms ceiling"
        )))),
        Some(_) => Ok(()),
    }
}

async fn create_sandbox(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: ApiJson<serde_json::Value>,
) -> ApiResult<(StatusCode, Json<SandboxView>)> {
    let ApiJson(raw) = body;
    let create = parse_create_body(raw)?;
    validate_ttl(create.timeout_ms)?;

    // Idempotency-Key: replay returns the original sandbox (creation only,
    // no body-hash matching).
    if let Some(record) = idempotency_lookup(&state, &headers).and_then(|id| state.get(&id)) {
        return Ok((StatusCode::OK, Json(view_of(&record))));
    }

    let health = state.backend.health();
    if !health.available {
        return Err(backend_unavailable(
            health.reason.as_deref().unwrap_or("unknown"),
        ));
    }

    let id = new_sandbox_id();
    let ctx = state.backend.provision(&id, &create.spec).await?;
    let record = Arc::new(SandboxRecord::new(ctx, create.timeout_ms));

    // Provisioned → running; a failed transition here is a bug (a fresh
    // lifecycle is always Provisioning).
    record
        .lifecycle
        .lock()
        .unwrap()
        .transition(LifecycleState::Running)
        .map_err(ApiError::from)?;
    record
        .events
        .lock()
        .unwrap()
        .push(EventKind::SandboxCreated, json!({}));

    let view = view_of(&record);
    state.metrics.inc_sandboxes_created();
    state
        .registry
        .write()
        .unwrap()
        .insert(id.clone(), record.clone());
    tracing::info!(sandbox = %id, "sandbox created");
    if let Some(key) = idempotency_key(&headers) {
        state
            .idempotency
            .write()
            .unwrap()
            .insert(key.to_string(), id);
    }
    Ok((StatusCode::CREATED, Json(view)))
}

fn idempotency_key(headers: &HeaderMap) -> Option<&str> {
    headers.get("idempotency-key").and_then(|v| v.to_str().ok())
}

fn idempotency_lookup(state: &SharedState, headers: &HeaderMap) -> Option<String> {
    idempotency_key(headers).and_then(|k| state.idempotency.read().unwrap().get(k).cloned())
}

async fn list_sandboxes(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let registry = state.registry.read().unwrap();
    let mut sandboxes: Vec<SandboxView> = registry.values().map(|r| view_of(r)).collect();
    sandboxes.sort_by(|a, b| a.id.cmp(&b.id));
    Json(json!({ "sandboxes": sandboxes }))
}

/// Detail view: the summary view flattened plus the declared TTL and the
/// usage ledger.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SandboxDetailView {
    #[serde(flatten)]
    sandbox: SandboxView,
    timeout_ms: Option<u64>,
    usage: crate::ledger::UsageLedger,
}

async fn get_sandbox(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> ApiResult<Json<SandboxDetailView>> {
    let record = state.get(&id).ok_or_else(|| sandbox_not_found(&id))?;
    Ok(Json(SandboxDetailView {
        timeout_ms: record.ttl_ms,
        sandbox: view_of(&record),
        usage: record.usage.lock().unwrap().clone(),
    }))
}

async fn delete_sandbox(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    remove_sandbox(&state, &id, "client_request").await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The single writer of termination: out of the registry and idempotency
/// map, lifecycle → terminated, evented with its reason, then physically
/// deprovisioned. DELETE and the reaper both land here.
pub(crate) async fn remove_sandbox(state: &SharedState, id: &str, reason: &str) -> ApiResult<()> {
    // Remove from the registry first so concurrent calls see it gone
    // (second DELETE → 404, reaper never double-terminates).
    let record = state
        .registry
        .write()
        .unwrap()
        .remove(id)
        .ok_or_else(|| sandbox_not_found(id))?;
    state.idempotency.write().unwrap().retain(|_, v| v != id);

    // Lifecycle: running/paused → terminated. The termination event is
    // pushed BEFORE the lifecycle flips — the SSE tail drains events first
    // and then checks the lifecycle, so this ordering is what guarantees a
    // stream always delivers the terminated event before closing.
    record
        .events
        .lock()
        .unwrap()
        .push(EventKind::SandboxTerminated, json!({"reason": reason}));
    let _ = record
        .lifecycle
        .lock()
        .unwrap()
        .transition(LifecycleState::Terminated);

    state.metrics.inc_sandboxes_terminated();
    state.backend.stop(&record.ctx).await?;
    state.backend.deprovision(&record.ctx).await?;
    tracing::info!(sandbox = %id, reason, "sandbox terminated");
    Ok(())
}

// ---------- pause / resume ----------

async fn pause_sandbox(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let record = state.get(&id).ok_or_else(|| sandbox_not_found(&id))?;
    record
        .lifecycle
        .lock()
        .unwrap()
        .transition(LifecycleState::Paused)
        .map_err(ApiError::from)?;
    record
        .events
        .lock()
        .unwrap()
        .push(EventKind::SandboxPaused, json!({}));
    Ok(StatusCode::NO_CONTENT)
}

async fn resume_sandbox(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let record = state.get(&id).ok_or_else(|| sandbox_not_found(&id))?;
    record
        .lifecycle
        .lock()
        .unwrap()
        .transition(LifecycleState::Running)
        .map_err(ApiError::from)?;
    // Resume reopens the data plane; the caller gets a fresh idle window.
    record.usage.lock().unwrap().refresh_activity(now_ms());
    record
        .events
        .lock()
        .unwrap()
        .push(EventKind::SandboxResumed, json!({}));
    Ok(StatusCode::NO_CONTENT)
}

// ---------- commit (rootfs → template) ----------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommitBody {
    name: String,
}

/// Snapshot a running sandbox's rootfs into a named template. Names are
/// validated as single path components; existing templates are never
/// overwritten (a template is an immutable base other sandboxes build on).
async fn commit_sandbox(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    body: ApiJson<CommitBody>,
) -> ApiResult<StatusCode> {
    let ApiJson(body) = body;
    let record = state.get(&id).ok_or_else(|| sandbox_not_found(&id))?;
    require_running(&record, &id)?;

    let tpl_dir = sandbox_core::snapshot::template_path(&state.data_root, &body.name)?;
    if tpl_dir.exists() {
        return Err(ApiError::from(Error::Policy(format!(
            "template {:?} already exists; templates are immutable",
            body.name
        ))));
    }
    tokio::fs::create_dir_all(&tpl_dir)
        .await
        .map_err(|e| io_error(e, &tpl_dir))?;

    // The tree may be large: clone it off the async runtime threads.
    let rootfs = record.ctx.rootfs.clone();
    tokio::task::spawn_blocking(move || sandbox_core::snapshot::snapshot_tree(&rootfs, &tpl_dir))
        .await
        .map_err(|e| ApiError::from(Error::Internal(format!("commit task panicked: {e}"))))?
        .map_err(ApiError::from)?;

    record
        .events
        .lock()
        .unwrap()
        .push(EventKind::TemplateCommitted, json!({"name": body.name}));
    record.usage.lock().unwrap().refresh_activity(now_ms());
    Ok(StatusCode::NO_CONTENT)
}

// ---------- exec ----------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExecBody {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

async fn exec(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    body: ApiJson<ExecBody>,
) -> ApiResult<Json<ExecOutcome>> {
    let ApiJson(body) = body;
    let record = state.get(&id).ok_or_else(|| sandbox_not_found(&id))?;

    {
        let lifecycle = record.lifecycle.lock().unwrap();
        if !lifecycle.can_exec() {
            return Err(ApiError::from(Error::Lifecycle(format!(
                "sandbox {id:?} is not running (state: {})",
                lifecycle.state().as_str()
            ))));
        }
    }

    let request = sandbox_core::ExecRequest {
        command: body.command,
        args: body.args,
        env: body.env,
        cwd: body.cwd,
        timeout_ms: body.timeout_ms,
    };
    request.validate().map_err(ApiError::from)?;

    record
        .events
        .lock()
        .unwrap()
        .push(EventKind::ExecStarted, json!({"command": request.command}));
    record.usage.lock().unwrap().record_exec_started(now_ms());
    state.metrics.inc_execs_started();
    state.metrics.exec_active_inc();
    // The gauge slot is released on every exit path (backend errors
    // included) — a leaked in-flight slot would be a false capacity signal.
    let started = std::time::Instant::now();
    let _active = ExecGauge(&state.metrics);
    let outcome = state.backend.exec(&record.ctx, request.clone()).await?;
    let wall_ms = started.elapsed().as_millis() as u64;
    state.metrics.record_exec_duration(started.elapsed());
    let output_bytes = outcome.stdout.len() + outcome.stderr.len();
    record.usage.lock().unwrap().record_exec_outcome(
        outcome.timed_out,
        wall_ms,
        output_bytes,
        now_ms(),
    );
    state
        .metrics
        .record_exec_outcome(outcome.timed_out, outcome.truncated, output_bytes);

    let (kind, data) = if outcome.timed_out {
        (
            EventKind::ExecTimedOut,
            json!({"command": request.command, "timeout": true}),
        )
    } else {
        (
            EventKind::ExecFinished,
            json!({"command": request.command, "exitCode": outcome.exit_code}),
        )
    };
    record.events.lock().unwrap().push(kind, data);

    Ok(Json(outcome))
}

/// Drops the execs_active gauge slot when the exec future ends (value or
/// error), so the gauge can never leak an in-flight slot.
struct ExecGauge<'a>(&'a crate::metrics::Metrics);
impl Drop for ExecGauge<'_> {
    fn drop(&mut self) {
        self.0.exec_active_dec();
    }
}

/// Drops the event-streams gauge slot when an SSE tail task ends.
struct StreamGauge<'a>(&'a crate::metrics::Metrics);
impl Drop for StreamGauge<'_> {
    fn drop(&mut self) {
        self.0.event_streams_dec();
    }
}

// ---------- file data plane ----------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FilePathBody {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileWriteBody {
    path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileRemoveBody {
    path: String,
    #[serde(default)]
    recursive: bool,
}

/// Files operate only on running sandboxes: the data plane presumes a live
/// environment, matching exec.
fn require_running(record: &SandboxRecord, id: &str) -> ApiResult<()> {
    let lifecycle = record.lifecycle.lock().unwrap();
    if lifecycle.can_exec() {
        Ok(())
    } else {
        Err(ApiError::from(Error::Lifecycle(format!(
            "sandbox {id:?} is not running (state: {})",
            lifecycle.state().as_str()
        ))))
    }
}

/// One successful file operation = one counted operation (ledger + metrics).
fn record_file_op(state: &SharedState, record: &SandboxRecord) {
    record.usage.lock().unwrap().record_file_op(now_ms());
    state.metrics.inc_file_ops();
}

async fn file_read(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    body: ApiJson<FilePathBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let record = state.get(&id).ok_or_else(|| sandbox_not_found(&id))?;
    require_running(&record, &id)?;
    let path = resolve_sandbox_path(&record.ctx.rootfs, &body.path, false)?;
    let content = tokio::fs::read(&path)
        .await
        .map_err(|e| io_error(e, &path))?;
    record_file_op(&state, &record);
    Ok(Json(json!({
        "path": body.path,
        "contentBase64": crate::b64::encode(&content),
        "size": content.len(),
    })))
}

async fn file_write(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    body: ApiJson<FileWriteBody>,
) -> ApiResult<StatusCode> {
    let record = state.get(&id).ok_or_else(|| sandbox_not_found(&id))?;
    require_running(&record, &id)?;
    // The target may not exist yet; its parent must. Resolve with the
    // missing-tail rule so the escape check still covers the full chain.
    let path = resolve_sandbox_path(&record.ctx.rootfs, &body.path, true)?;
    // Writes create missing parents (E2B-compatible semantics).
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| io_error(e, parent))?;
    }
    tokio::fs::write(&path, body.content.as_bytes())
        .await
        .map_err(|e| io_error(e, &path))?;
    record_file_op(&state, &record);
    Ok(StatusCode::NO_CONTENT)
}

async fn file_list(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    body: ApiJson<FilePathBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let record = state.get(&id).ok_or_else(|| sandbox_not_found(&id))?;
    require_running(&record, &id)?;
    let path = resolve_sandbox_path(&record.ctx.rootfs, &body.path, false)?;
    let mut entries = Vec::new();
    let mut read_dir = tokio::fs::read_dir(&path)
        .await
        .map_err(|e| io_error(e, &path))?;
    while let Some(entry) = read_dir
        .next_entry()
        .await
        .map_err(|e| io_error(e, &path))?
    {
        let file_type = entry
            .file_type()
            .await
            .map_err(|e| io_error(e, &entry.path()))?;
        let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
        entries.push(json!({
            "name": entry.file_name().to_string_lossy(),
            "type": if file_type.is_dir() {
                "dir"
            } else if file_type.is_symlink() {
                "symlink"
            } else {
                "file"
            },
            "size": size,
        }));
    }
    entries.sort_by_key(|e| e["name"].as_str().unwrap_or("").to_string());
    record_file_op(&state, &record);
    Ok(Json(json!({ "entries": entries })))
}

async fn file_stat(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    body: ApiJson<FilePathBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let record = state.get(&id).ok_or_else(|| sandbox_not_found(&id))?;
    require_running(&record, &id)?;
    let path = resolve_sandbox_path(&record.ctx.rootfs, &body.path, false)?;
    let meta = tokio::fs::metadata(&path)
        .await
        .map_err(|e| io_error(e, &path))?;
    record_file_op(&state, &record);
    Ok(Json(json!({
        "path": body.path,
        "type": if meta.is_dir() { "dir" } else { "file" },
        "size": meta.len(),
    })))
}

async fn file_mkdir(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    body: ApiJson<FilePathBody>,
) -> ApiResult<StatusCode> {
    let record = state.get(&id).ok_or_else(|| sandbox_not_found(&id))?;
    require_running(&record, &id)?;
    let path = resolve_sandbox_path(&record.ctx.rootfs, &body.path, true)?;
    tokio::fs::create_dir_all(&path)
        .await
        .map_err(|e| io_error(e, &path))?;
    record_file_op(&state, &record);
    Ok(StatusCode::NO_CONTENT)
}

async fn file_remove(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    body: ApiJson<FileRemoveBody>,
) -> ApiResult<StatusCode> {
    let record = state.get(&id).ok_or_else(|| sandbox_not_found(&id))?;
    require_running(&record, &id)?;
    let path = resolve_sandbox_path(&record.ctx.rootfs, &body.path, false)?;
    let meta = tokio::fs::metadata(&path)
        .await
        .map_err(|e| io_error(e, &path))?;
    if meta.is_dir() {
        if body.recursive {
            tokio::fs::remove_dir_all(&path)
                .await
                .map_err(|e| io_error(e, &path))?;
        } else {
            tokio::fs::remove_dir(&path)
                .await
                .map_err(|e| io_error(e, &path))?;
        }
    } else {
        tokio::fs::remove_file(&path)
            .await
            .map_err(|e| io_error(e, &path))?;
    }
    record_file_op(&state, &record);
    Ok(StatusCode::NO_CONTENT)
}

// ---------- events ----------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EventsQuery {
    #[serde(default)]
    after: Option<u64>,
}

async fn events(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let record = state.get(&id).ok_or_else(|| sandbox_not_found(&id))?;
    let events_log = record.events.lock().unwrap();
    let after = query.after.unwrap_or(0);
    let events: Vec<serde_json::Value> = events_log
        .events_after(after)
        .iter()
        .map(|e| serde_json::to_value(e).expect("events serialize"))
        .collect();
    Ok(Json(json!({
        "events": events,
        "lastSeq": events_log.last_seq(),
        "dropped": events_log.dropped(),
    })))
}

// ---------- event stream (SSE) ----------

/// The SSE response type: channel stream wrapped by the keep-alive stream.
pub type EventSse = axum::response::Sse<
    axum::response::sse::KeepAliveStream<
        tokio_stream::wrappers::ReceiverStream<
            Result<axum::response::sse::Event, std::convert::Infallible>,
        >,
    >,
>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EventsStreamQuery {
    #[serde(default)]
    after: Option<u64>,
}

/// Live tail of the sandbox event stream as Server-Sent Events. Replay
/// starts after `?after=` or the `Last-Event-ID` header (SSE reconnect
/// semantics), then follows the log. The per-sandbox log is the single
/// source; the tail task reads it on a short tick — no second event state.
async fn events_stream(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(query): axum::extract::Query<EventsStreamQuery>,
) -> ApiResult<EventSse> {
    use axum::response::sse::{Event as SseEvent, KeepAlive};

    let record = state.get(&id).ok_or_else(|| sandbox_not_found(&id))?;
    let reconnect_cursor = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let mut cursor = reconnect_cursor.or(query.after).unwrap_or(0);

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<SseEvent, std::convert::Infallible>>(64);
    state.metrics.event_streams_inc();
    let tail_state = state.clone();
    tokio::spawn(async move {
        // One stream = one counted slot, released when the tail ends for any
        // reason (client disconnect, sandbox termination, server shutdown).
        let _stream = StreamGauge(&tail_state.metrics);
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(200));
        loop {
            ticker.tick().await;
            let pending = { record.events.lock().unwrap().events_after(cursor) };
            for event in pending {
                cursor = event.seq;
                let kind = serde_json::to_string(&event.kind)
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_string();
                let item = SseEvent::default()
                    .id(event.seq.to_string())
                    .event(kind)
                    .data(event.data.to_string());
                if tx.send(Ok(item)).await.is_err() {
                    // The client disconnected; the record drops with this task.
                    return;
                }
            }
            // Only after draining: a terminated sandbox or a draining server
            // ends the tail. The terminated event itself is delivered first
            // because remove_sandbox pushes it before flipping the lifecycle.
            if tail_state
                .shutdown
                .load(std::sync::atomic::Ordering::Relaxed)
                || record.lifecycle.lock().unwrap().state() == LifecycleState::Terminated
            {
                return;
            }
        }
    });

    Ok(
        axum::response::Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx))
            .keep_alive(KeepAlive::default()),
    )
}

fn io_error(e: std::io::Error, path: &std::path::Path) -> ApiError {
    ApiError::from(Error::Backend {
        kind: sandbox_core::error::BackendFailureKind::Io,
        message: format!("file op on {path:?}: {e}"),
    })
}
