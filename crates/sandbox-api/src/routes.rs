//! REST routes for M1: sandbox lifecycle, exec, file data plane, events,
//! health. Resource shapes intentionally mirror E2B where it costs nothing;
//! the full E2B alignment (auth, /v2 routes, multipart files) is M2 work.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{ApiError, ApiJson, ApiResult, backend_unavailable, sandbox_not_found};
use crate::files::resolve_sandbox_path;
use crate::state::{SandboxRecord, SharedState, new_sandbox_id};
use sandbox_core::events::EventKind;
use sandbox_core::{Error, ExecOutcome, LifecycleState};

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/sandboxes", post(create_sandbox).get(list_sandboxes))
        .route("/sandboxes/{id}", get(get_sandbox).delete(delete_sandbox))
        .route("/sandboxes/{id}/exec", post(exec))
        .route("/sandboxes/{id}/files/read", post(file_read))
        .route("/sandboxes/{id}/files/write", post(file_write))
        .route("/sandboxes/{id}/files/list", post(file_list))
        .route("/sandboxes/{id}/files/stat", post(file_stat))
        .route("/sandboxes/{id}/files/mkdir", post(file_mkdir))
        .route("/sandboxes/{id}/files/remove", post(file_remove))
        .route("/sandboxes/{id}/events", get(events))
        .with_state(state)
}

// ---------- health ----------

async fn health(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let backend_health = state.backend.health();
    let count = state.registry.read().unwrap().len();
    Json(json!({
        "status": if backend_health.available { "ok" } else { "degraded" },
        "backend": {
            "name": state.backend.name(),
            "available": backend_health.available,
            "reason": backend_health.reason,
        },
        "sandboxes": count,
    }))
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

async fn create_sandbox(
    State(state): State<SharedState>,
    headers: HeaderMap,
    spec: ApiJson<sandbox_core::policy::SandboxSpec>,
) -> ApiResult<(StatusCode, Json<SandboxView>)> {
    // Idempotency-Key: replay returns the original sandbox (M1: creation
    // only, no body-hash matching).
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
    let ctx = state.backend.provision(&id, &spec).await?;
    let record = Arc::new(SandboxRecord::new(ctx));

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
    state
        .registry
        .write()
        .unwrap()
        .insert(id.clone(), record.clone());
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

async fn get_sandbox(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> ApiResult<Json<SandboxView>> {
    let record = state.get(&id).ok_or_else(|| sandbox_not_found(&id))?;
    Ok(Json(view_of(&record)))
}

async fn delete_sandbox(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    // Remove from the registry first so concurrent calls see it gone
    // (second DELETE → 404). Idempotency bindings die with the sandbox.
    let record = state
        .registry
        .write()
        .unwrap()
        .remove(&id)
        .ok_or_else(|| sandbox_not_found(&id))?;
    state.idempotency.write().unwrap().retain(|_, v| v != &id);

    // Lifecycle: running → terminated. Backend stop + deprovision then
    // physically remove the environment.
    let _ = record
        .lifecycle
        .lock()
        .unwrap()
        .transition(LifecycleState::Terminated);
    record
        .events
        .lock()
        .unwrap()
        .push(EventKind::SandboxTerminated, json!({}));

    state.backend.stop(&record.ctx).await?;
    state.backend.deprovision(&record.ctx).await?;
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

    let outcome = state.backend.exec(&record.ctx, request.clone()).await?;

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

fn io_error(e: std::io::Error, path: &std::path::Path) -> ApiError {
    ApiError::from(Error::Backend {
        kind: sandbox_core::error::BackendFailureKind::Io,
        message: format!("file op on {path:?}: {e}"),
    })
}
