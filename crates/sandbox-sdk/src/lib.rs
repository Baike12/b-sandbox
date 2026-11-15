//! b-sandbox client SDK — the typed client for the REST contract documented
//! in `docs/openapi.yaml`. Policy types come from `sandbox-core` directly,
//! so the client and the service share one model: a spec change breaks the
//! SDK build instead of silently breaking callers.
//!
//! The integration test drives a live HTTP server over real TCP (the exact
//! router the binary serves), so SDK/API drift fails the gate, not prod.

use std::collections::BTreeMap;

use hyper::body::Bytes;
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use hyper::{Method, Request, Uri};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde::Deserialize;
use serde_json::json;

use sandbox_core::ExecOutcome;
use sandbox_core::policy::SandboxSpec;

pub type Result<T> = std::result::Result<T, SdkError>;

/// One API failure, decomposed the way the envelope promises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdkError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl SdkError {
    pub fn is_not_found(&self) -> bool {
        self.code == "sandbox_not_found"
    }

    pub fn is_unauthorized(&self) -> bool {
        self.code == "unauthorized"
    }
}

impl std::fmt::Display for SdkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}: {}", self.status, self.code, self.message)
    }
}

impl std::error::Error for SdkError {}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SandboxView {
    pub id: String,
    pub state: String,
    pub rootfs: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Usage {
    #[serde(rename = "createdAtMs")]
    pub created_at_ms: u64,
    #[serde(rename = "lastActivityMs")]
    pub last_activity_ms: u64,
    #[serde(rename = "execsStarted")]
    pub execs_started: u64,
    #[serde(rename = "execsFinished")]
    pub execs_finished: u64,
    #[serde(rename = "execsTimedOut")]
    pub execs_timed_out: u64,
    #[serde(rename = "execWallMs")]
    pub exec_wall_ms: u64,
    #[serde(rename = "outputBytes")]
    pub output_bytes: u64,
    #[serde(rename = "fileOps")]
    pub file_ops: u64,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SandboxDetail {
    pub id: String,
    pub state: String,
    pub rootfs: String,
    #[serde(rename = "timeoutMs")]
    pub timeout_ms: Option<u64>,
    pub usage: Usage,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Health {
    pub status: String,
    pub backend: serde_json::Value,
    pub sandboxes: u64,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct EventsPage {
    pub events: Vec<sandbox_core::Event>,
    #[serde(rename = "lastSeq")]
    pub last_seq: u64,
    pub dropped: u64,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub file_type: String,
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct FileContent {
    pub path: String,
    #[serde(rename = "contentBase64")]
    pub content_base64: String,
    pub size: u64,
}

/// One command run; mirrors the wire shape, `timeout_ms: None` = ceiling.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecSpec {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreateOptions {
    pub timeout_ms: Option<u64>,
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Client {
    base: String,
    token: Option<String>,
    http: hyper_util::client::legacy::Client<HttpConnector, http_body_util::Full<Bytes>>,
}

impl Client {
    /// `base` like `http://127.0.0.1:8094` (a trailing slash is fine).
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            token: None,
            http: hyper_util::client::legacy::Client::builder(TokioExecutor::new()).build_http(),
        }
    }

    /// Shared bearer token (see the OpenAPI securitySchemes note).
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    pub async fn health(&self) -> Result<Health> {
        self.get_json("/health").await
    }

    pub async fn create(&self, spec: &SandboxSpec) -> Result<SandboxView> {
        self.create_with(spec, CreateOptions::default()).await
    }

    pub async fn create_with(
        &self,
        spec: &SandboxSpec,
        opts: CreateOptions,
    ) -> Result<SandboxView> {
        let mut body = serde_json::to_value(spec).expect("spec serializes");
        if let Some(timeout) = opts.timeout_ms {
            body["timeoutMs"] = json!(timeout);
        }
        let mut req = self.request(Method::POST, "/sandboxes", Some(body));
        if let Some(key) = &opts.idempotency_key {
            req.headers_mut().insert(
                "idempotency-key",
                HeaderValue::from_str(key).expect("idempotency key is a header value"),
            );
        }
        self.send(req, decode_json).await
    }

    pub async fn list(&self) -> Result<Vec<SandboxView>> {
        #[derive(Deserialize)]
        struct Page {
            sandboxes: Vec<SandboxView>,
        }
        let page: Page = self.get_json("/sandboxes").await?;
        Ok(page.sandboxes)
    }

    pub async fn get(&self, id: &str) -> Result<SandboxDetail> {
        self.get_json(&format!("/sandboxes/{id}")).await
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        self.empty(Method::DELETE, &format!("/sandboxes/{id}"), None)
            .await
    }

    pub async fn pause(&self, id: &str) -> Result<()> {
        self.empty(Method::POST, &format!("/sandboxes/{id}/pause"), None)
            .await
    }

    pub async fn resume(&self, id: &str) -> Result<()> {
        self.empty(Method::POST, &format!("/sandboxes/{id}/resume"), None)
            .await
    }

    pub async fn commit(&self, id: &str, template_name: &str) -> Result<()> {
        self.empty(
            Method::POST,
            &format!("/sandboxes/{id}/commit"),
            Some(json!({"name": template_name})),
        )
        .await
    }

    pub async fn exec(&self, id: &str, spec: ExecSpec) -> Result<ExecOutcome> {
        let body = json!({
            "command": spec.command,
            "args": spec.args,
            "env": spec.env,
            "cwd": spec.cwd,
            "timeoutMs": spec.timeout_ms,
        });
        self.post_json(&format!("/sandboxes/{id}/exec"), body).await
    }

    pub async fn file_write(&self, id: &str, path: &str, content: &str) -> Result<()> {
        self.empty(
            Method::POST,
            &format!("/sandboxes/{id}/files/write"),
            Some(json!({"path": path, "content": content})),
        )
        .await
    }

    pub async fn file_read(&self, id: &str, path: &str) -> Result<FileContent> {
        self.post_json(
            &format!("/sandboxes/{id}/files/read"),
            json!({"path": path}),
        )
        .await
    }

    pub async fn file_list(&self, id: &str, path: &str) -> Result<Vec<FileEntry>> {
        #[derive(Deserialize)]
        struct Page {
            entries: Vec<FileEntry>,
        }
        let page: Page = self
            .post_json(
                &format!("/sandboxes/{id}/files/list"),
                json!({"path": path}),
            )
            .await?;
        Ok(page.entries)
    }

    pub async fn file_stat(&self, id: &str, path: &str) -> Result<FileEntry> {
        self.post_json(
            &format!("/sandboxes/{id}/files/stat"),
            json!({"path": path}),
        )
        .await
    }

    pub async fn file_mkdir(&self, id: &str, path: &str) -> Result<()> {
        self.empty(
            Method::POST,
            &format!("/sandboxes/{id}/files/mkdir"),
            Some(json!({"path": path})),
        )
        .await
    }

    pub async fn file_remove(&self, id: &str, path: &str) -> Result<()> {
        self.empty(
            Method::POST,
            &format!("/sandboxes/{id}/files/remove"),
            Some(json!({"path": path})),
        )
        .await
    }

    pub async fn events(&self, id: &str, after: u64) -> Result<EventsPage> {
        self.get_json(&format!("/sandboxes/{id}/events?after={after}"))
            .await
    }

    /// Raw Prometheus exposition text.
    pub async fn metrics(&self) -> Result<String> {
        let req = self.request(Method::GET, "/metrics", None);
        self.send(req, |status, body| match status {
            200 => Ok(String::from_utf8_lossy(body).into_owned()),
            _ => Err(decode_error(status, body)),
        })
        .await
    }

    // ---------- transport ----------

    fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Request<http_body_util::Full<Bytes>> {
        let uri: Uri = format!("{}{path}", self.base)
            .parse()
            .expect("request uri is well-formed");
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = &self.token {
            builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        match body {
            Some(v) => builder
                .header(CONTENT_TYPE, "application/json")
                .body(http_body_util::Full::new(Bytes::from(v.to_string())))
                .expect("request builds"),
            None => builder
                .body(http_body_util::Full::new(Bytes::new()))
                .expect("request builds"),
        }
    }

    async fn send<T>(
        &self,
        req: Request<http_body_util::Full<Bytes>>,
        decode: impl FnOnce(u16, &[u8]) -> Result<T>,
    ) -> Result<T> {
        let response = self.http.request(req).await.map_err(|e| SdkError {
            status: 0,
            code: "transport".into(),
            message: e.to_string(),
        })?;
        let status = response.status().as_u16();
        let bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .map_err(|e| SdkError {
                status,
                code: "transport".into(),
                message: e.to_string(),
            })?
            .to_bytes();
        decode(status, &bytes)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let req = self.request(Method::GET, path, None);
        self.send(req, decode_json).await
    }

    async fn post_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<T> {
        let req = self.request(Method::POST, path, Some(body));
        self.send(req, decode_json).await
    }

    async fn empty(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<()> {
        let req = self.request(method, path, body);
        self.send(req, |status, body| {
            if (200..300).contains(&status) {
                Ok(())
            } else {
                Err(decode_error(status, body))
            }
        })
        .await
    }
}

fn decode_json<T: serde::de::DeserializeOwned>(status: u16, body: &[u8]) -> Result<T> {
    if !(200..300).contains(&status) {
        return Err(decode_error(status, body));
    }
    serde_json::from_slice(body).map_err(|e| SdkError {
        status,
        code: "response_format".into(),
        message: e.to_string(),
    })
}

fn decode_error(status: u16, body: &[u8]) -> SdkError {
    #[derive(Deserialize)]
    struct Envelope {
        error: ErrorBody,
    }
    #[derive(Deserialize)]
    struct ErrorBody {
        code: String,
        message: String,
    }
    match serde_json::from_slice::<Envelope>(body) {
        Ok(env) => SdkError {
            status,
            code: env.error.code,
            message: env.error.message,
        },
        Err(_) => SdkError {
            status,
            code: "response_format".into(),
            message: String::from_utf8_lossy(body).into_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_carries_code_and_status() {
        let e = SdkError {
            status: 404,
            code: "sandbox_not_found".into(),
            message: "missing".into(),
        };
        assert_eq!(e.to_string(), "404 sandbox_not_found: missing");
        assert!(e.is_not_found());
        assert!(!e.is_unauthorized());
    }

    #[test]
    fn base_url_is_normalized() {
        let c = Client::new("http://127.0.0.1:8094/");
        assert_eq!(c.base, "http://127.0.0.1:8094");
    }
}
