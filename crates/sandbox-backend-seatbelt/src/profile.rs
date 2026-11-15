//! Seatbelt profile generation: pure function from resolved policy to the
//! scheme string passed to `sandbox_init`.
//!
//! Profile shape (denying default, then explicit allows, denied paths LAST):
//!
//! 1. `(deny default)` — the fail-closed baseline.
//! 2. System read allowlist — enough for dyld/shells to start (MXC-proven list).
//! 3. `/dev` basics — null/zero/random only.
//! 4. Rootfs — implicit write access to the sandbox scratch directory.
//! 5. Policy paths — readonly / readwrite grants from the fs policy.
//! 6. Egress posture — one line: deny or allow `network-outbound` (capability:
//!    `NET_EGRESS_DEFAULT` only; Seatbelt cannot express CIDR/hostname rules).
//! 7. Denied paths — emitted last so they shadow any earlier allow that might
//!    overlap (deny is final in Seatbelt; ordering last is belt-and-braces).
//!
//! Known M1 weakenings (documented, tightened in M2):
//! - `(allow mach-lookup)` without name filters: launchd-backed services
//!   (timezone, locale) need it at startup. Name-filtering is M2 work.

use std::path::Path;

use sandbox_core::policy::{EgressDefault, ResolvedFsPolicy, ResolvedNetPolicy};

/// Read-only host paths every sandbox needs to start processes at all
/// (dynamic loader, shells, system frameworks). Verified empirically on
/// macOS 24.3 (arm64) with `sandbox-exec`: under a deny-default profile the
/// sandboxed process SIGABRTs at startup unless it can read the root directory
/// itself (`(literal "/")`) — subpaths alone are not enough.
const SYSTEM_READ_BASELINE: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr",
    "/System",
    "/Library",
    "/private/etc",
    "/private/var/db",
    "/private/var/select",
];

/// Paths the workload may execute from: the system bins plus any policy path
/// (exec of a readwrite path must work, e.g. a script placed there).
fn process_exec_paths() -> Vec<&'static str> {
    [
        "/bin",
        "/sbin",
        "/usr/bin",
        "/usr/sbin",
        "/usr/local/bin",
        "/opt/homebrew/bin",
    ]
    .into_iter()
    .collect()
}

fn subpaths(paths: &[&str]) -> String {
    paths
        .iter()
        .map(|p| format!(r#"(subpath "{p}")"#))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Generate the Seatbelt scheme for one sandbox. `egress_proxy` is the
/// `ip:port` of the credential-injection proxy; it is required when the
/// resolved policy is in proxy mode (validated at provision, before this
/// function runs).
pub fn generate(
    fs: &ResolvedFsPolicy,
    net: &ResolvedNetPolicy,
    rootfs: &Path,
    egress_proxy: Option<&str>,
) -> String {
    let rootfs = rootfs.to_string_lossy();
    let mut scheme = String::with_capacity(4096);

    scheme.push_str("(version 1)\n");
    scheme.push_str("(deny default)\n");

    // 2. System read baseline + the root directory itself (see
    //    SYSTEM_READ_BASELINE doc: without stat("/") the process aborts).
    scheme.push_str(&format!(
        "(allow file-read* (literal \"/\") {})\n",
        subpaths(SYSTEM_READ_BASELINE)
    ));

    // 3. /dev basics.
    scheme.push_str(
        "(allow file-read* file-write* (literal \"/dev/null\") (literal \"/dev/zero\")\n\
         \x20                     (literal \"/dev/random\") (literal \"/dev/urandom\"))\n",
    );
    scheme.push_str("(allow sysctl-read)\n");
    scheme.push_str("(allow process-fork)\n");
    // Documented M1 weakening; see module doc.
    scheme.push_str("(allow mach-lookup)\n");

    // Exec universe: system bins (needed to run anything) plus policy paths.
    scheme.push_str(&format!(
        "(allow process-exec {})\n",
        subpaths(&process_exec_paths())
    ));

    // 4. Rootfs: the implicit writable scratch area.
    scheme.push_str(&format!(
        "(allow file-read* file-write* (subpath \"{rootfs}\"))\n"
    ));

    // 5. Policy grants. Normalization guarantees these are disjoint from the
    // denied set, so ordering between them does not matter.
    if !fs.readonly.is_empty() {
        let paths: Vec<String> = fs
            .readonly
            .iter()
            .map(|p| format!(r#"(subpath "{}")"#, p.to_string_lossy()))
            .collect();
        scheme.push_str(&format!("(allow file-read* {})\n", paths.join(" ")));
    }
    if !fs.readwrite.is_empty() {
        let paths: Vec<String> = fs
            .readwrite
            .iter()
            .map(|p| format!(r#"(subpath "{}")"#, p.to_string_lossy()))
            .collect();
        scheme.push_str(&format!(
            "(allow file-read* file-write* {})\n",
            paths.join(" ")
        ));
    }
    // Policy paths may also need to be executable.
    if !fs.readonly.is_empty() || !fs.readwrite.is_empty() {
        let mut exec_paths: Vec<String> = fs
            .readonly
            .iter()
            .chain(fs.readwrite.iter())
            .map(|p| format!(r#"(subpath "{}")"#, p.to_string_lossy()))
            .collect();
        exec_paths.extend(
            process_exec_paths()
                .into_iter()
                .map(|p| format!(r#"(subpath "{p}")"#)),
        );
        scheme.push_str(&format!("(allow process-exec {})\n", exec_paths.join(" ")));
    }

    // 6. Egress posture (capability NET_EGRESS_DEFAULT; proxy mode needs
    // NET_EGRESS_PROXY). `system-socket` is required on modern macOS for
    // socket(2) under a deny-default profile (MXC reached the same wall);
    // deny-all intentionally omits it so the workload cannot even create
    // sockets. Proxy mode: exactly one hole — the proxy endpoint — with
    // credentials injected by the proxy, never visible to the workload.
    if net.proxy {
        // Seatbelt only accepts `*` or `localhost` as the network-filter host;
        // the backend validates the proxy address is loopback, so the single
        // hole is `localhost:<port>` (proven by the proxy-mode integration
        // test: allowed target connects, everything else is denied).
        let port = egress_proxy
            .and_then(|a| a.rsplit_once(':').map(|(_, p)| p.to_string()))
            .unwrap_or_else(|| "0".into());
        scheme.push_str("(deny network-outbound)\n");
        scheme.push_str(&format!(
            "(allow network-outbound (to tcp \"localhost:{port}\"))\n(allow system-socket)\n"
        ));
    } else {
        match net.egress_default {
            EgressDefault::Deny => scheme.push_str("(deny network-outbound)\n"),
            EgressDefault::Allow => {
                scheme.push_str("(allow network-outbound)\n(allow system-socket)\n")
            }
        }
    }

    // 7. Denied paths last — they shadow any overlapping earlier allow.
    for p in &fs.denied {
        let p = p.to_string_lossy();
        scheme.push_str(&format!(
            "(deny file-read* file-read-metadata file-write* (subpath \"{p}\"))\n"
        ));
    }

    scheme
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandbox_core::policy::FsPolicy;

    fn fixture(fs: FsPolicy, net: ResolvedNetPolicy, rootfs: &str) -> String {
        generate(&fs.resolve().unwrap(), &net, Path::new(rootfs), None)
    }

    fn default_net() -> ResolvedNetPolicy {
        sandbox_core::policy::NetPolicy::default()
            .resolve()
            .unwrap()
    }

    #[test]
    fn baseline_is_deny_default() {
        let scheme = fixture(FsPolicy::default(), default_net(), "/tmp/r");
        assert!(scheme.starts_with("(version 1)\n(deny default)\n"));
    }

    #[test]
    fn rootfs_is_writable() {
        let scheme = fixture(FsPolicy::default(), default_net(), "/tmp/r");
        assert!(scheme.contains(r#"(allow file-read* file-write* (subpath "/tmp/r"))"#));
    }

    #[test]
    fn readwrite_path_gets_read_write_exec() {
        let scheme = fixture(
            FsPolicy {
                readwrite: vec!["/tmp/data".into()],
                ..Default::default()
            },
            default_net(),
            "/tmp/r",
        );
        assert!(scheme.contains(r#"(allow file-read* file-write* (subpath "/tmp/data"))"#));
        assert!(scheme.contains(r#"(allow process-exec (subpath "/tmp/data")"#));
    }

    #[test]
    fn readonly_path_gets_read_only() {
        let scheme = fixture(
            FsPolicy {
                readonly: vec!["/usr/share/docs".into()],
                ..Default::default()
            },
            default_net(),
            "/tmp/r",
        );
        assert!(scheme.contains(r#"(allow file-read* (subpath "/usr/share/docs"))"#));
        assert!(
            !scheme.contains(r#"(allow file-read* file-write* (subpath "/usr/share/docs"))"#),
            "readonly path must not gain write"
        );
    }

    #[test]
    fn denied_path_emitted_last_and_dominates() {
        let scheme = fixture(
            FsPolicy {
                denied: vec!["/private/etc/secret".into()],
                ..Default::default()
            },
            default_net(),
            "/tmp/r",
        );
        let deny_line_pos = scheme
            .rfind(r#"(deny file-read* file-read-metadata file-write* (subpath "/private/etc/secret"))"#)
            .expect("deny line present");
        // Everything after it is only further deny lines.
        let tail = &scheme[deny_line_pos..];
        assert!(tail.lines().all(|l| l.trim_start().starts_with("(deny")));
        // It comes after the baseline allows.
        assert!(scheme.rfind("(allow file-read*").expect("baseline") < deny_line_pos);
    }

    #[test]
    fn egress_default_deny_and_allow_render_differently() {
        let deny_scheme = fixture(FsPolicy::default(), default_net(), "/tmp/r");
        assert!(deny_scheme.contains("(deny network-outbound)"));
        assert!(!deny_scheme.contains("(allow network-outbound)"));

        let allow_net = sandbox_core::policy::NetPolicy {
            egress_default: EgressDefault::Allow,
            ..Default::default()
        }
        .resolve()
        .unwrap();
        let allow_scheme = fixture(FsPolicy::default(), allow_net, "/tmp/r");
        assert!(allow_scheme.contains("(allow network-outbound)"));
    }

    #[test]
    fn proxy_mode_opens_only_the_proxy_endpoint() {
        let net = sandbox_core::policy::NetPolicy {
            proxy: true,
            ..Default::default()
        }
        .resolve()
        .unwrap();
        let scheme = generate(
            &FsPolicy::default().resolve().unwrap(),
            &net,
            Path::new("/tmp/r"),
            Some("127.0.0.1:8095"),
        );
        assert!(scheme.contains("(deny network-outbound)\n"));
        assert!(
            scheme.contains("(allow network-outbound (to tcp \"localhost:8095\"))"),
            "{scheme}"
        );
        assert!(scheme.contains("(allow system-socket)"));
        // No blanket allow.
        assert!(!scheme.contains("(allow network-outbound)\n"));
    }

    #[test]
    fn system_baseline_present_for_sh_startup() {
        let scheme = fixture(FsPolicy::default(), default_net(), "/tmp/r");
        // /usr covers /usr/lib etc.
        for p in ["/bin", "/usr", "/System", "/private/etc"] {
            assert!(
                scheme.contains(&format!(r#"(subpath "{p}")"#)),
                "{p} in baseline"
            );
        }
        // The root literal is startup-critical (see SYSTEM_READ_BASELINE doc).
        assert!(
            scheme.contains(r#"(literal "/")"#),
            "root literal in baseline"
        );
    }
}
