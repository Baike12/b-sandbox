//! Sandbox-relative path resolution for the file data plane.
//!
//! Security model: the workload's filesystem view is `rootfs`. A caller-
//! supplied relative path is lexically normalized (core `normalize_relative`),
//! joined under the rootfs, and — this is the part that actually catches
//! attacks — canonicalized and required to stay under the canonicalized
//! rootfs. Symlinks inside the sandbox pointing outside therefore do NOT
//! grant access. A canonicalized result escaping the rootfs is a
//! `policy_validation` error: at this boundary, traversal is an attack, not
//! a quirk.
//!
//! TOCTOU note: resolution and use are separate syscalls; a racing workload
//! could swap a path between them. The M1 threat model (single-tenant host,
//! localhost-only bind) accepts this; a hardening step (openat2-style
//! dirfd-relative ops) is future work and documented in the design doc.

use std::path::{Path, PathBuf};

use sandbox_core::error::{Error, Result};
use sandbox_core::policy::normalize_relative;

/// Resolve a sandbox-relative path against the canonicalized rootfs.
///
/// `allow_missing` controls behavior when the final component does not
/// exist: with it, the deepest existing ancestor is canonicalized and the
/// remaining components are appended (lexical, already traversal-checked);
/// without it, a missing path is a `policy_validation` error naming the path.
pub fn resolve_sandbox_path(rootfs: &Path, rel: &str, allow_missing: bool) -> Result<PathBuf> {
    let rel_norm = normalize_relative(rel)?;
    let joined = if rel_norm.as_os_str().is_empty() {
        rootfs.to_path_buf()
    } else {
        rootfs.join(&rel_norm)
    };

    if let Ok(existing) = std::fs::canonicalize(&joined) {
        return ensure_within(rootfs, existing);
    }

    // The final path is missing. Strict mode (allow_missing=false) treats
    // that as a client error (400 policy_validation); permissive mode
    // canonicalizes the deepest existing ancestor and re-attaches the
    // (already lexically safe) remainder, deepest-first.
    if !allow_missing {
        return Err(Error::Policy(format!(
            "sandbox path {rel:?} does not exist"
        )));
    }
    let mut missing_tail: Vec<std::ffi::OsString> = Vec::new();
    let mut probe = joined.clone();
    loop {
        if let Ok(existing) = std::fs::canonicalize(&probe) {
            let mut resolved = existing;
            for comp in missing_tail.iter().rev() {
                resolved.push(comp);
            }
            return ensure_within(rootfs, resolved);
        }
        let popped_name = probe.file_name().map(|n| n.to_os_string());
        if !probe.pop() || popped_name.is_none() {
            // Reached the root without finding anything existing.
            break;
        }
        missing_tail.push(popped_name.unwrap());
    }

    Err(Error::Policy(format!(
        "cannot resolve sandbox path {rel:?}: no existing ancestor inside the sandbox"
    )))
}

fn ensure_within(rootfs: &Path, resolved: PathBuf) -> Result<PathBuf> {
    if resolved.starts_with(rootfs) {
        Ok(resolved)
    } else {
        Err(Error::Policy(format!(
            "sandbox path escapes the sandbox root (denied): {resolved:?}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir_all(rootfs.join("data")).unwrap();
        std::fs::write(rootfs.join("data/file.txt"), "hello").unwrap();
        (tmp, rootfs)
    }

    #[test]
    fn existing_paths_resolve_canonically() {
        let (_tmp, rootfs) = fixture();
        let resolved = resolve_sandbox_path(&rootfs, "data/file.txt", false).unwrap();
        assert_eq!(resolved, rootfs.join("data/file.txt"));
        // The root itself and dot-forms resolve to the rootfs.
        assert_eq!(resolve_sandbox_path(&rootfs, "", false).unwrap(), rootfs);
        assert_eq!(
            resolve_sandbox_path(&rootfs, "data/./", false).unwrap(),
            rootfs.join("data")
        );
    }

    #[test]
    fn missing_paths_resolve_with_allow_missing() {
        let (_tmp, rootfs) = fixture();
        let resolved = resolve_sandbox_path(&rootfs, "data/brand-new/out.txt", true).unwrap();
        assert_eq!(resolved, rootfs.join("data/brand-new/out.txt"));

        let err = resolve_sandbox_path(&rootfs, "data/nope.bin", false).unwrap_err();
        assert_eq!(err.code(), "policy_validation");
    }

    #[test]
    fn lexical_traversal_is_rejected() {
        let (_tmp, rootfs) = fixture();
        for raw in ["..", "../escape", "data/../../escape"] {
            let err = resolve_sandbox_path(&rootfs, raw, true).unwrap_err();
            assert_eq!(err.code(), "policy_validation", "{raw:?}");
        }
    }

    #[test]
    fn symlink_escape_is_rejected() {
        let (_tmp, rootfs) = fixture();
        // Symlink inside the sandbox pointing at the host /etc.
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc", rootfs.join("host-etc")).unwrap();
        let err = resolve_sandbox_path(&rootfs, "host-etc/passwd", false).unwrap_err();
        assert_eq!(err.code(), "policy_validation");
        assert!(err.to_string().contains("escapes"));
    }

    #[test]
    fn absolute_paths_are_rejected() {
        let (_tmp, rootfs) = fixture();
        let err = resolve_sandbox_path(&rootfs, "/etc/passwd", false).unwrap_err();
        assert_eq!(err.code(), "policy_validation");
    }
}
