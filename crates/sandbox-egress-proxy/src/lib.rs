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
//! Egress is policy-filtered (`EgressPolicy`): the default refuses loopback,
//! link-local, RFC1918/CGNAT, unique-local, and other reserved destinations,
//! because the proxy runs unconfined on the host — without the filter, the
//! sandbox's one loopback hole would be a tunnel into every host-side
//! service (databases, this API itself) and cloud metadata. DNS names are
//! resolved and checked before connecting; a name resolving to any blocked
//! address is refused outright (rebinding defense).
//!
//! The proxy runs on the host loopback next to the sandbox service; the
//! backend's network policy opens exactly one hole: this endpoint.

use std::collections::BTreeMap;
use std::io;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Upper bound on a request head; a bigger head is a protocol error.
pub const MAX_HEAD_BYTES: usize = 16 * 1024;

/// Deadline for reading the request head: a client that opens a socket and
/// never speaks must not hold a connection (and a semaphore slot) forever.
pub const HEAD_TIMEOUT: Duration = Duration::from_secs(5);

/// Concurrent-connection ceiling. Each in-flight connection holds tasks and
/// buffers; past this the proxy refuses work instead of growing without
/// bound — the same fail-closed posture as the API body limit.
pub const MAX_CONNECTIONS: usize = 1024;

/// Outbound connect budget: a dead upstream must fail fast enough that the
/// sandboxed client's own timeout governs, and a hung connect must not pin
/// a proxy connection slot forever.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// host (lowercase, no port) → Authorization header value.
pub type Credentials = Arc<BTreeMap<String, String>>;

/// Where the proxy may egress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EgressPolicy {
    /// Public internet only (see the module docs). The production default.
    #[default]
    DenyPrivate,
    /// No destination filtering — for tests and for deployments that
    /// deliberately proxy to internal services.
    AllowAll,
}

/// Accept loop. Returns on listener error; per-connection errors are logged,
/// never crash the proxy.
pub async fn serve(listener: TcpListener, credentials: Credentials) -> io::Result<()> {
    serve_with_policy(listener, credentials, EgressPolicy::DenyPrivate).await
}

/// [`serve`] with an explicit egress policy.
pub async fn serve_with_policy(
    listener: TcpListener,
    credentials: Credentials,
    policy: EgressPolicy,
) -> io::Result<()> {
    let capacity = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (mut conn, _peer) = listener.accept().await?;
        // try_acquire, not acquire: an at-capacity proxy must refuse new work
        // immediately (observable 503) instead of queueing unboundedly.
        let permit = match capacity.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                tracing::warn!("egress-proxy at capacity; refusing connection");
                let _ = respond(&mut conn, "503 Service Unavailable", "proxy at capacity").await;
                continue;
            }
        };
        let creds = credentials.clone();
        tokio::spawn(async move {
            // Hold the slot for the whole connection lifetime.
            let _permit = permit;
            if let Err(e) = handle(conn, creds, policy).await {
                tracing::warn!(error = %e, "egress-proxy connection dropped");
            }
        });
    }
}

/// Bind and serve on `addr`; returns the bound address (for port 0 tests).
pub async fn spawn(addr: &str, credentials: Credentials) -> io::Result<std::net::SocketAddr> {
    spawn_with_policy(addr, credentials, EgressPolicy::DenyPrivate).await
}

/// [`spawn`] with an explicit egress policy.
pub async fn spawn_with_policy(
    addr: &str,
    credentials: Credentials,
    policy: EgressPolicy,
) -> io::Result<std::net::SocketAddr> {
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    tokio::spawn(serve_with_policy(listener, credentials, policy));
    Ok(bound)
}

/// Parsed absolute-form request, ready to forward.
struct ForwardRequest {
    method: String,
    host: String,
    port: u16,
    path: String,
}

async fn handle(mut conn: TcpStream, creds: Credentials, policy: EgressPolicy) -> io::Result<()> {
    let (head, leftover) = read_head(&mut conn).await?;
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or_default().to_ascii_uppercase();
    let target = parts.next().unwrap_or_default().to_string();

    if method == "CONNECT" {
        tunnel(conn, &target, policy).await
    } else if let Some((host, port, path)) = parse_absolute_http(&target) {
        let req = ForwardRequest {
            method,
            host,
            port,
            path,
        };
        forward(conn, &head, &leftover, &req, &creds, policy).await
    } else {
        respond(
            &mut conn,
            "400 Bad Request",
            "proxy needs absolute-form http:// or CONNECT",
        )
        .await
    }
}

/// Resolve `host` and connect with the egress policy applied. Every resolved
/// address is checked: a host resolving to any blocked address fails the
/// whole connect (no fall-through to a public address — that is the DNS
/// rebinding defense). Connection attempts are bounded by CONNECT_TIMEOUT.
async fn connect_checked(host: &str, port: u16, policy: EgressPolicy) -> io::Result<TcpStream> {
    let addrs = tokio::net::lookup_host((host, port)).await.map_err(|e| {
        io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!("cannot resolve {host}: {e}"),
        )
    })?;
    let mut last_err: Option<io::Error> = None;
    for addr in addrs {
        if let Some(reason) = blocked_reason(addr.ip(), policy) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, reason));
        }
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) => last_err = Some(e),
            Err(_) => last_err = Some(io::Error::new(io::ErrorKind::TimedOut, "connect timed out")),
        }
    }
    Err(last_err
        .unwrap_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "no usable addresses")))
}

/// Why this address is unreachable through the proxy, under `policy`.
fn blocked_reason(ip: IpAddr, policy: EgressPolicy) -> Option<String> {
    if policy == EgressPolicy::AllowAll {
        return None;
    }
    // IPv4-mapped (and deprecated IPv4-compatible) IPv6 forms dial IPv4 at
    // connect time: `::ffff:127.0.0.1` is not loopback to Ipv6Addr::is_loopback,
    // but the kernel dials 127.0.0.1. Policy checks the effective v4 address.
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        other => other,
    };
    let blocked = match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_unspecified()
                || o[0] == 0 // this-network (0.0.0.0/8)
                || v4.is_private() // RFC1918
                || v4.is_loopback()
                || v4.is_link_local()
                || (o[0] == 100 && (o[1] & 0xc0) == 64) // CGNAT 100.64/10
                || (o[0] == 198 && (o[1] & 0xfe) == 18) // benchmarking 198.18/15
                || v4.is_broadcast()
                || v4.is_documentation()
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            v6.is_unspecified()
                || v6.is_loopback()
                || (seg[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (seg[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
                || (seg[0] == 0x2001 && seg[1] == 0x0db8) // documentation
        }
    };
    blocked.then(|| format!("egress to {ip} is denied (private/loopback/link-local ranges are unreachable through this proxy)"))
}

/// Read until the end of the request head (bounded, timed out). Returns the
/// head and the body bytes that arrived in the same TCP segments: a single
/// read rarely aligns with the header terminator, and re-parsing body bytes
/// as header lines is a smuggling channel (a `Authorization: …` line inside
/// a POST body would ride along as a header).
async fn read_head(conn: &mut TcpStream) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        let n = match tokio::time::timeout(HEAD_TIMEOUT, conn.read(&mut chunk)).await {
            Ok(n) => n?,
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "client sent no complete request head in time",
                ));
            }
        };
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
        if let Some(idx) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = buf[..idx + 4].to_vec();
            let leftover = buf[idx + 4..].to_vec();
            return Ok((head, leftover));
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

/// Header name of a request line, case-normalized: everything before the
/// first colon, trimmed (an `Authorization : x` line has the same name as a
/// plain `Authorization: x` to any upstream).
fn header_name(line: &str) -> &str {
    line.split(':').next().unwrap_or("").trim()
}

/// Credential for one (host, port) pair. A `host:port` key matches exactly;
/// a bare `host` key matches only the default HTTP port 80 — the secret must
/// not be handed to every listener on the same host.
fn credential_for<'a>(creds: &'a Credentials, host: &str, port: u16) -> Option<&'a String> {
    if let Some(value) = creds.get(&format!("{host}:{port}")) {
        return Some(value);
    }
    if port == 80 {
        return creds.get(host);
    }
    None
}

/// Plain-HTTP forward with credential injection.
async fn forward(
    mut client: TcpStream,
    head: &[u8],
    leftover: &[u8],
    req: &ForwardRequest,
    creds: &Credentials,
    policy: EgressPolicy,
) -> io::Result<()> {
    let text = String::from_utf8_lossy(head);
    let mut head_lines = text.split("\r\n");
    head_lines.next(); // request line, rebuilt below

    let host_header = format!("{}:{}", req.host, req.port);
    let inject_value = credential_for(creds, &req.host, req.port);
    let mut rewritten = format!("{} {} HTTP/1.1\r\n", req.method, req.path);
    let mut has_host = false;
    // After a stripped header, obs-fold continuation lines (leading SP/HTAB)
    // must go too — a folded fragment would recombine with the injected
    // Authorization into a smuggled second header.
    let mut skip_continuation = false;
    for line in head_lines {
        if line.is_empty() {
            continue;
        }
        if skip_continuation && (line.starts_with(' ') || line.starts_with('\t')) {
            continue;
        }
        skip_continuation = false;
        // Header name comparison is case-insensitive AND trim-aware: a client
        // `Authorization : evil` (space before the colon) is still an
        // Authorization header to the upstream and would ride alongside the
        // injected one as a smuggling channel.
        let name = header_name(line).to_ascii_lowercase();
        // The proxy owns these headers end-to-end.
        if matches!(
            name.as_str(),
            "proxy-authorization" | "proxy-connection" | "connection"
        ) {
            skip_continuation = true;
            continue;
        }
        if name == "authorization" {
            // For configured hosts the proxy replaces the header entirely;
            // otherwise the client's own header passes through untouched.
            if inject_value.is_none() {
                rewritten.push_str(line);
                rewritten.push_str("\r\n");
            } else {
                skip_continuation = true;
            }
            continue;
        }
        if name == "host" {
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

    let mut upstream = match connect_checked(&req.host, req.port, policy).await {
        Ok(stream) => stream,
        Err(e) => return respond_for_connect_error(&mut client, &e).await,
    };
    upstream.write_all(rewritten.as_bytes()).await?;
    // Body bytes that rode in with the head go upstream verbatim, before the
    // bidirectional relay takes over.
    if !leftover.is_empty() {
        upstream.write_all(leftover).await?;
    }
    upstream.flush().await?;

    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// CONNECT tunnel: TLS stays end-to-end; bytes are copied both ways.
/// Connect failures answer 403 (policy denial) or 502 (unreachable) instead
/// of dropping the connection — the sandboxed client gets an observable
/// refusal, and a blocked target never leaves a silent hang.
async fn tunnel(mut client: TcpStream, target: &str, policy: EgressPolicy) -> io::Result<()> {
    let (host, port) = match target.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "CONNECT port invalid"))?,
        ),
        None => (target.to_string(), 443),
    };
    let mut upstream = match connect_checked(host.as_str(), port, policy).await {
        Ok(stream) => stream,
        Err(e) => return respond_for_connect_error(&mut client, &e).await,
    };
    client
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await?;
    client.flush().await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// 403 for policy denials, 502 for everything else — with the reason as the
/// error body (IPs and resolution failures carry no secrets).
async fn respond_for_connect_error(client: &mut TcpStream, e: &io::Error) -> io::Result<()> {
    let status = if e.kind() == io::ErrorKind::PermissionDenied {
        "403 Forbidden"
    } else {
        "502 Bad Gateway"
    };
    respond(client, status, &e.to_string()).await
}

async fn respond(conn: &mut TcpStream, status: &str, message: &str) -> io::Result<()> {
    // serde_json owns the escaping: error text embeds hostnames and OS
    // messages, and a raw `{message}` in the template would let a crafted
    // hostname break the JSON envelope or forge response headers.
    let body = serde_json::to_string(&serde_json::json!({ "error": message }))
        .unwrap_or_else(|_| "{\"error\":\"internal\"}".to_string());
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    conn.write_all(response.as_bytes()).await?;
    conn.flush().await?;
    Ok(())
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn v4(octets: [u8; 4]) -> IpAddr {
        IpAddr::V4(Ipv4Addr::from(octets))
    }

    #[test]
    fn bare_host_credentials_apply_only_to_port_80() {
        let creds: Credentials = Arc::new(BTreeMap::from([
            ("example.com".to_string(), "bare".to_string()),
            ("exact.example.com:8443".to_string(), "exact".to_string()),
        ]));
        assert_eq!(
            credential_for(&creds, "example.com", 80).map(String::as_str),
            Some("bare")
        );
        assert_eq!(credential_for(&creds, "example.com", 8080), None);
        assert_eq!(credential_for(&creds, "example.com", 443), None);
        assert_eq!(
            credential_for(&creds, "exact.example.com", 8443).map(String::as_str),
            Some("exact")
        );
        assert_eq!(credential_for(&creds, "exact.example.com", 80), None);
    }

    #[test]
    fn header_names_are_trimmed_before_comparison() {
        assert_eq!(header_name("Authorization: x"), "Authorization");
        assert_eq!(header_name("Authorization : x"), "Authorization");
        assert_eq!(header_name("\tauthorization:x"), "authorization");
        assert_eq!(header_name("no-colon"), "no-colon");
    }

    #[test]
    fn reserved_ranges_are_blocked_under_deny_private() {
        let cases: Vec<(IpAddr, bool)> = vec![
            (v4([127, 0, 0, 1]), true),       // loopback
            (v4([10, 0, 0, 5]), true),        // RFC1918
            (v4([192, 168, 1, 1]), true),     // RFC1918
            (v4([172, 16, 0, 9]), true),      // RFC1918
            (v4([100, 64, 1, 1]), true),      // CGNAT (not is_private)
            (v4([198, 18, 0, 1]), true),      // benchmarking (not is_private)
            (v4([169, 254, 169, 254]), true), // cloud metadata
            (v4([0, 0, 0, 0]), true),         // unspecified
            (v4([0, 1, 2, 3]), true),         // this-network
            (v4([8, 8, 8, 8]), false),        // public
            (v4([1, 1, 1, 1]), false),        // public
            (IpAddr::V6(Ipv6Addr::LOCALHOST), true),
            (IpAddr::V6(Ipv6Addr::UNSPECIFIED), true),
            (IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1)), true), // unique local
            (IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)), true), // link-local
            (
                IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0, 0, 0, 0, 0, 0x4700)),
                false,
            ), // public
        ];
        for (ip, want_blocked) in cases {
            assert_eq!(
                blocked_reason(ip, EgressPolicy::DenyPrivate).is_some(),
                want_blocked,
                "{ip}"
            );
            assert!(
                blocked_reason(ip, EgressPolicy::AllowAll).is_none(),
                "AllowAll must not filter: {ip}"
            );
        }
    }
}
