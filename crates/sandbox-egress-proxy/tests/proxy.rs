//! Integration tests for the credential-injection proxy: injection on
//! matching hosts, pass-through without injection, and the CONNECT tunnel.
//! Real TCP everywhere (raw clients, real upstream servers).

use std::collections::BTreeMap;
use std::sync::Arc;

use sandbox_egress_proxy::EgressPolicy;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Spawn with `AllowAll`: these tests' upstreams live on loopback by design.
async fn spawn_allow_all(creds: std::collections::BTreeMap<String, String>) -> String {
    sandbox_egress_proxy::spawn_with_policy("127.0.0.1:0", Arc::new(creds), EgressPolicy::AllowAll)
        .await
        .unwrap()
        .to_string()
}

/// Minimal upstream HTTP server: captures the request head, replies with a
/// fixed body. Returns the bound port and a shared handle to the capture.
async fn spawn_upstream(captured: Arc<std::sync::Mutex<String>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut conn, _)) = listener.accept().await else {
                return;
            };
            let captured = captured.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 2048];
                // One read is enough for these tiny test requests.
                let n = conn.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                *captured.lock().unwrap() = String::from_utf8_lossy(&buf).to_string();
                let body = "proxy-ok";
                conn.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            });
        }
    });
    port
}

async fn raw_get(proxy_addr: &str, target: &str, extra: &str) -> (u16, String) {
    let mut conn = TcpStream::connect(proxy_addr).await.unwrap();
    conn.write_all(format!("GET {target} HTTP/1.1\r\n{extra}\r\n").as_bytes())
        .await
        .unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        conn.read_to_end(&mut buf),
    )
    .await
    .expect("upstream response in time");
    // The proxy address is echoed back only for error assertions; the caller
    // mostly reads the body. We return the raw response text.
    let status = 200u16; // real status parsed by callers where needed
    (status, String::from_utf8_lossy(&buf).to_string())
}

#[tokio::test]
async fn injects_authorization_for_configured_hosts() {
    let captured = Arc::new(std::sync::Mutex::new(String::new()));
    let upstream_port = spawn_upstream(captured.clone()).await;

    let creds = Arc::new(BTreeMap::from([(
        "127.0.0.1".to_string(),
        "Bearer tok-123".to_string(),
    )]));
    let proxy = spawn_allow_all(creds.as_ref().clone()).await;

    let (_, response) = raw_get(
        &proxy.to_string(),
        &format!("http://127.0.0.1:{upstream_port}/data?q=1"),
        "Host: 127.0.0.1\r\n",
    )
    .await;

    assert!(response.contains("proxy-ok"), "{response}");
    let seen = captured.lock().unwrap().clone();
    // Request line rewritten to origin-form.
    assert!(seen.starts_with("GET /data?q=1 HTTP/1.1\r\n"), "{seen}");
    // Credential injected.
    assert!(seen.contains("Authorization: Bearer tok-123\r\n"), "{seen}");
    // Host rewritten to the upstream authority.
    assert!(
        seen.contains(&format!("Host: 127.0.0.1:{upstream_port}\r\n")),
        "{seen}"
    );
}

#[tokio::test]
async fn non_configured_hosts_pass_through_without_injection() {
    let captured = Arc::new(std::sync::Mutex::new(String::new()));
    let upstream_port = spawn_upstream(captured.clone()).await;

    let proxy = spawn_allow_all(BTreeMap::new()).await;

    let (_, response) = raw_get(
        &proxy.to_string(),
        &format!("http://127.0.0.1:{upstream_port}/x"),
        "Host: 127.0.0.1\r\nAuthorization: client-owned\r\n",
    )
    .await;

    assert!(response.contains("proxy-ok"), "{response}");
    let seen = captured.lock().unwrap().clone();
    // No proxy credential added; the client's own header is not dropped.
    assert!(seen.contains("Authorization: client-owned\r\n"), "{seen}");
    assert!(!seen.contains("Bearer"), "{seen}");
}

#[tokio::test]
async fn connect_tunnels_bytes_end_to_end() {
    // Echo upstream: a CONNECT target the client then speaks raw TCP with.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut conn, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                loop {
                    match conn.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if conn.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });

    let proxy = spawn_allow_all(BTreeMap::new()).await;

    let mut conn = TcpStream::connect(proxy.to_string()).await.unwrap();
    conn.write_all(
        format!("CONNECT 127.0.0.1:{echo_port} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        conn.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    assert!(
        head.starts_with(b"HTTP/1.1 200 Connection established\r\n\r\n"),
        "{}",
        String::from_utf8_lossy(&head)
    );

    conn.write_all(b"ping").await.unwrap();
    let mut echo = [0u8; 4];
    conn.read_exact(&mut echo).await.unwrap();
    assert_eq!(&echo, b"ping");
}

#[tokio::test]
async fn malformed_target_gets_a_400() {
    let proxy = spawn_allow_all(BTreeMap::new()).await;
    let (_, response) = raw_get(&proxy.to_string(), "/origin-form-only", "Host: x\r\n").await;
    assert!(response.contains("400 Bad Request"), "{response}");
}

// ---------- egress policy (the production default) ----------

async fn spawn_default() -> String {
    sandbox_egress_proxy::spawn("127.0.0.1:0", Arc::new(BTreeMap::new()))
        .await
        .unwrap()
        .to_string()
}

/// The proxy runs unconfined on the host: without the policy filter the
/// sandbox's single loopback hole would tunnel into every host-side service.
#[tokio::test]
async fn deny_private_blocks_connect_to_loopback() {
    let proxy = spawn_default().await;
    let mut conn = TcpStream::connect(&proxy).await.unwrap();
    conn.write_all(b"CONNECT 127.0.0.1:80 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        conn.read_to_end(&mut buf),
    )
    .await;
    let text = String::from_utf8_lossy(&buf);
    assert!(text.contains("403 Forbidden"), "{text}");
    assert!(text.contains("denied"), "{text}");
}

#[tokio::test]
async fn deny_private_blocks_absolute_form_to_loopback() {
    let proxy = spawn_default().await;
    let (_, response) = raw_get(&proxy, "http://127.0.0.1:80/x", "Host: 127.0.0.1\r\n").await;
    assert!(response.contains("403 Forbidden"), "{response}");
    assert!(response.contains("denied"), "{response}");
}

/// A DNS name resolving to a private address is refused outright — no
/// fall-through to a public address of the same name (rebinding defense).
#[tokio::test]
async fn deny_private_blocks_names_resolving_to_private_addresses() {
    let proxy = spawn_default().await;
    let mut conn = TcpStream::connect(&proxy).await.unwrap();
    conn.write_all(b"CONNECT localhost:80 HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        conn.read_to_end(&mut buf),
    )
    .await;
    let text = String::from_utf8_lossy(&buf);
    assert!(text.contains("403 Forbidden"), "{text}");
}

#[tokio::test]
async fn allow_all_still_serves_loopback_targets() {
    // Same target the policy tests refuse, with the explicit opt-in flag:
    // proves the refusal is the policy, not a broken connect path.
    let captured = Arc::new(std::sync::Mutex::new(String::new()));
    let upstream_port = spawn_upstream(captured.clone()).await;
    let proxy = spawn_allow_all(BTreeMap::new()).await;
    let (_, response) = raw_get(
        &proxy,
        &format!("http://127.0.0.1:{upstream_port}/y"),
        "Host: 127.0.0.1\r\n",
    )
    .await;
    assert!(response.contains("proxy-ok"), "{response}");
}
