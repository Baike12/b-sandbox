//! Credential-injection egress proxy.
//!
//! Sandboxes in `net.proxy` mode reach the internet only through this proxy.
//! It holds the credentials the workloads need (in memory, injected at the
//! HTTP layer) so tokens never enter the sandbox environment, the policy, or
//! the process argv:
//!
//! - Plain HTTP: the client speaks proxy-style absolute-form requests
//!   (`GET http://host/path`); the proxy rewrites the request line to
//!   origin-form, injects/overrides `Authorization` for configured hosts,
//!   and streams the exchange.
//! - HTTPS (`CONNECT`): byte tunnel. TLS is end-to-end, so the proxy cannot
//!   inject into it — HTTPS credentials belong in exec env vars, never in
//!   this proxy's table (documented posture, not a TODO).
//!
//! The proxy runs on the host loopback next to the sandbox service; the
//! backend's network policy opens exactly one hole: this endpoint.

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Upper bound on a request head; a bigger head is a protocol error.
pub const MAX_HEAD_BYTES: usize = 16 * 1024;

/// host (lowercase, no port) → Authorization header value.
pub type Credentials = Arc<BTreeMap<String, String>>;

/// Accept loop. Returns on listener error; per-connection errors are logged,
/// never crash the proxy.
pub async fn serve(listener: TcpListener, credentials: Credentials) -> io::Result<()> {
    loop {
        let (conn, _peer) = listener.accept().await?;
        let creds = credentials.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(conn, creds).await {
                eprintln!("egress-proxy: connection dropped: {e}");
            }
        });
    }
}

/// Bind and serve on `addr`; returns the bound address (for port 0 tests).
pub async fn spawn(addr: &str, credentials: Credentials) -> io::Result<std::net::SocketAddr> {
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    tokio::spawn(serve(listener, credentials));
    Ok(bound)
}

async fn handle(mut conn: TcpStream, creds: Credentials) -> io::Result<()> {
    let head = read_head(&mut conn).await?;
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or_default().to_ascii_uppercase();
    let target = parts.next().unwrap_or_default().to_string();

    if method == "CONNECT" {
        tunnel(conn, &target).await
    } else if let Some((host, port, path)) = parse_absolute_http(&target) {
        forward(conn, &head, &method, &host, port, &path, &creds).await
    } else {
        respond(
            &mut conn,
            "400 Bad Request",
            "proxy needs absolute-form http:// or CONNECT",
        )
        .await
    }
}

/// Read until the end of the request head (bounded).
async fn read_head(conn: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        let n = conn.read(&mut chunk).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "client closed early",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_HEAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request head too large",
            ));
        }
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(buf);
        }
    }
}

/// `http://host[:port]/path?query` → (host, port, origin-form path).
fn parse_absolute_http(target: &str) -> Option<(String, u16, String)> {
    let rest = target
        .strip_prefix("http://")
        .or_else(|| target.strip_prefix("HTTP://"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().ok()?),
        None => (authority, 80),
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_ascii_lowercase(), port, path.to_string()))
}

/// Plain-HTTP forward with credential injection.
async fn forward(
    mut client: TcpStream,
    head: &[u8],
    method: &str,
    host: &str,
    port: u16,
    path: &str,
    creds: &Credentials,
) -> io::Result<()> {
    let text = String::from_utf8_lossy(head);
    let mut head_lines = text.split("\r\n");
    head_lines.next(); // request line, rebuilt below

    let host_header = format!("{host}:{port}");
    let inject_value = creds.get(host);
    let mut rewritten = format!("{method} {path} HTTP/1.1\r\n");
    let mut has_host = false;
    for line in head_lines {
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        // The proxy owns these headers end-to-end.
        if lower.starts_with("proxy-authorization:")
            || lower.starts_with("proxy-connection:")
            || lower.starts_with("connection:")
        {
            continue;
        }
        if lower.starts_with("authorization:") {
            // For configured hosts the proxy replaces the header entirely;
            // otherwise the client's own header passes through untouched.
            if inject_value.is_none() {
                rewritten.push_str(line);
                rewritten.push_str("\r\n");
            }
            continue;
        }
        if lower.starts_with("host:") {
            has_host = true;
            rewritten.push_str(&format!("Host: {host_header}\r\n"));
            continue;
        }
        rewritten.push_str(line);
        rewritten.push_str("\r\n");
    }
    if !has_host {
        rewritten.push_str(&format!("Host: {host_header}\r\n"));
    }
    if let Some(value) = inject_value {
        rewritten.push_str(&format!("Authorization: {value}\r\n"));
    }
    // Keep-alive state is per-connection; both legs close when idle anyway.
    rewritten.push_str("Connection: close\r\n\r\n");

    let mut upstream = TcpStream::connect((host, port)).await?;
    upstream.write_all(rewritten.as_bytes()).await?;
    upstream.flush().await?;

    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// CONNECT tunnel: TLS stays end-to-end; bytes are copied both ways.
async fn tunnel(mut client: TcpStream, target: &str) -> io::Result<()> {
    let (host, port) = match target.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "CONNECT port invalid"))?,
        ),
        None => (target.to_string(), 443),
    };
    let mut upstream = TcpStream::connect((host.as_str(), port)).await?;
    client
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await?;
    client.flush().await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

async fn respond(conn: &mut TcpStream, status: &str, message: &str) -> io::Result<()> {
    let body = format!("{{\"error\":\"{message}\"}}");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    conn.write_all(response.as_bytes()).await?;
    conn.flush().await?;
    Ok(())
}
