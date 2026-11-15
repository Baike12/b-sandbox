//! Bubblewrap command generation: pure function from resolved policy to the
//! bwrap argv. Unit-tested in argv space; integration tests live behind
//! `#[cfg(target_os = "linux")]` (this crate compiles everywhere, runs there).
//!
//! Filesystem view construction (the MXC shape):
//! - read-only baseline binds for the system trees, `--try` variants so a
//!   missing tree never aborts the spawn;
//! - `--proc /proc`, `--dev /dev` so processes work;
//! - the sandbox rootfs is bound read-write at its own canonical path;
//! - policy grants mount after the baseline (later mounts win on overlap);
//! - policy denied paths are masked with `--tmpfs` (denial is viral: the
//!   whole subtree vanishes under the mask).
//!
//! Egress: `--unshare-net` for deny-all; nothing for allow (shared host net).
//! Rule-based egress is NOT declared in M1 (see backend capabilities).

use std::path::Path;

use sandbox_core::policy::{EgressDefault, ResolvedFsPolicy, ResolvedNetPolicy};

/// System trees every sandbox needs for the dynamic loader and shells.
const READONLY_BASELINE: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib64"];

/// Generate the full bwrap argv (without the `bwrap` program name itself).
pub fn generate(
    fs: &ResolvedFsPolicy,
    net: &ResolvedNetPolicy,
    rootfs: &Path,
    env: &[(String, String)],
) -> Vec<String> {
    let rootfs = rootfs.to_string_lossy().into_owned();
    let mut argv: Vec<String> = vec![
        // Isolation baseline.
        "--unshare-all".into(),
        "--new-session".into(),
        "--die-with-parent".into(),
        // Proc/dev so the workload can inspect itself and use /dev/null etc.
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
    ];

    // Read-only system baseline (--try: a missing tree must not abort).
    for path in READONLY_BASELINE {
        argv.push("--ro-bind-try".into());
        argv.push((*path).into());
        argv.push((*path).into());
    }

    // Rootfs: the implicit writable scratch area, mounted last of the baseline.
    argv.push("--bind".into());
    argv.push(rootfs.clone());
    argv.push(rootfs.clone());

    // Policy grants — after the baseline so they win any overlap.
    for p in &fs.readonly {
        let p = p.to_string_lossy().into_owned();
        argv.push("--ro-bind".into());
        argv.push(p.clone());
        argv.push(p);
    }
    for p in &fs.readwrite {
        let p = p.to_string_lossy().into_owned();
        argv.push("--bind".into());
        argv.push(p.clone());
        argv.push(p);
    }
    // Denied paths: mask the whole subtree with an empty tmpfs.
    for p in &fs.denied {
        argv.push("--tmpfs".into());
        argv.push(p.to_string_lossy().into_owned());
    }

    // Egress posture (capability NET_EGRESS_DEFAULT only).
    if net.egress_default == EgressDefault::Deny {
        argv.push("--unshare-net".into());
    }

    // Environment: cleared, then the minimal base, then per-exec env.
    argv.push("--clearenv".into());
    for (k, v) in env {
        argv.push("--setenv".into());
        argv.push(k.clone());
        argv.push(v.clone());
    }

    argv.push("--".into());
    argv
}

/// Baseline environment passed into the sandbox (base layer only).
pub fn base_env(rootfs: &Path) -> Vec<(String, String)> {
    vec![
        (
            "PATH".into(),
            "/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin".into(),
        ),
        (
            "HOME".into(),
            rootfs.join("home").to_string_lossy().into_owned(),
        ),
        (
            "TMPDIR".into(),
            rootfs.join("tmp").to_string_lossy().into_owned(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandbox_core::policy::{FsPolicy, NetPolicy};
    use std::path::PathBuf;

    fn fixture(fs: FsPolicy, net: ResolvedNetPolicy, rootfs: &str) -> Vec<String> {
        // Mirror the backend: base env layer first, caller env on top.
        let mut env = base_env(Path::new(rootfs));
        env.push(("FOO".into(), "bar".into()));
        let resolved_fs = fs.resolve().unwrap();
        generate(&resolved_fs, &net, Path::new(rootfs), &env)
    }

    fn default_net() -> ResolvedNetPolicy {
        NetPolicy::default().resolve().unwrap()
    }

    fn contains(argv: &[String], needle: &str) -> bool {
        argv.iter().any(|a| a == needle)
    }

    #[test]
    fn isolation_baseline_present() {
        let argv = fixture(FsPolicy::default(), default_net(), "/tmp/r");
        for flag in ["--unshare-all", "--new-session", "--die-with-parent"] {
            assert!(contains(&argv, flag), "{flag} missing");
        }
        assert!(contains(&argv, "--proc") && contains(&argv, "/proc"));
        assert!(contains(&argv, "--dev") && contains(&argv, "/dev"));
    }

    #[test]
    fn readonly_baseline_uses_try_binds() {
        let argv = fixture(FsPolicy::default(), default_net(), "/tmp/r");
        let has = |p: &str| {
            argv.windows(3)
                .any(|w| w[0] == "--ro-bind-try" && w[1] == p && w[2] == p)
        };
        assert!(has("/usr"), "usr baseline bind");
        assert!(has("/lib"), "lib baseline bind");
    }

    #[test]
    fn rootfs_is_bound_read_write() {
        let argv = fixture(FsPolicy::default(), default_net(), "/tmp/r");
        let has = argv
            .windows(3)
            .any(|w| w[0] == "--bind" && w[1] == "/tmp/r" && w[2] == "/tmp/r");
        assert!(has, "rootfs bind missing");
    }

    #[test]
    fn policy_sets_render_their_mount_kinds() {
        let argv = fixture(
            FsPolicy {
                readwrite: vec!["/data/work".into()],
                readonly: vec!["/srv/docs".into()],
                denied: vec!["/srv/docs/secret".into()],
            },
            default_net(),
            "/tmp/r",
        );
        let has_rw = argv
            .windows(3)
            .any(|w| w[0] == "--bind" && w[1] == "/data/work" && w[2] == "/data/work");
        let has_ro = argv
            .windows(3)
            .any(|w| w[0] == "--ro-bind" && w[1] == "/srv/docs" && w[2] == "/srv/docs");
        let has_mask = argv
            .windows(2)
            .any(|w| w[0] == "--tmpfs" && w[1] == "/srv/docs/secret");
        assert!(has_rw && has_ro && has_mask, "argv: {argv:?}");
    }

    #[test]
    fn denied_mount_comes_after_its_readwrite_parent() {
        // Strictest-wins normalization drops readwrite descendants of denied
        // ancestors; here the denied child is inside readonly /srv/docs, and
        // the tmpfs mask must come later in argv so it wins the mount stack.
        let argv = fixture(
            FsPolicy {
                readonly: vec!["/srv/docs".into()],
                denied: vec!["/srv/docs/secret".into()],
                ..Default::default()
            },
            default_net(),
            "/tmp/r",
        );
        let ro_pos = argv
            .windows(3)
            .position(|w| w[0] == "--ro-bind" && w[1] == "/srv/docs")
            .expect("ro bind present");
        let mask_pos = argv
            .windows(2)
            .position(|w| w[0] == "--tmpfs" && w[1] == "/srv/docs/secret")
            .expect("mask present");
        assert!(mask_pos > ro_pos, "mask must mount after the parent bind");
    }

    #[test]
    fn egress_deny_adds_unshare_net_allow_does_not() {
        let deny = fixture(FsPolicy::default(), default_net(), "/tmp/r");
        assert!(contains(&deny, "--unshare-net"));

        let allow_net = NetPolicy {
            egress_default: EgressDefault::Allow,
            ..Default::default()
        }
        .resolve()
        .unwrap();
        let allow = fixture(FsPolicy::default(), allow_net, "/tmp/r");
        assert!(!contains(&allow, "--unshare-net"));
    }

    #[test]
    fn environment_cleared_then_base_then_caller() {
        let argv = fixture(FsPolicy::default(), default_net(), "/tmp/r");
        let clear_pos = argv
            .iter()
            .position(|a| a == "--clearenv")
            .expect("clearenv");
        let foo_pos = argv
            .windows(3)
            .position(|w| w[0] == "--setenv" && w[1] == "FOO")
            .expect("caller env");
        assert!(foo_pos > clear_pos, "env set after clearenv");
        // Base PATH present exactly once.
        assert_eq!(argv.iter().filter(|a| a.as_str() == "PATH").count(), 1);
    }

    #[test]
    fn base_env_points_into_rootfs() {
        let env = base_env(&PathBuf::from("/tmp/r"));
        let home = env.iter().find(|(k, _)| k == "HOME").unwrap();
        assert_eq!(home.1, "/tmp/r/home");
    }
}
