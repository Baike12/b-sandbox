//! Filesystem policy: which host paths a sandbox may read and write.
//!
//! Semantics (documented contract, enforced by every backend):
//! - Paths are absolute host paths.
//! - **Strictest wins.** A path claimed by two sets resolves to the strictest
//!   set (`denied > readonly > readwrite`). This is the MXC normalization rule.
//! - **Denial is viral.** A denied path dominates its whole subtree: descendants
//!   in more permissive sets are dropped, because backends where deny is final
//!   (Seatbelt) cannot express "denied parent, writable child" anyway. The model
//!   does not pretend that shape is expressible.
//! - The sandbox scratch rootfs is writable implicitly and is not part of this
//!   policy; these rules govern access to the *rest of the host*.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Raw filesystem policy as submitted by a caller.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FsPolicy {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readwrite: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readonly: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied: Vec<String>,
}

/// Normalized filesystem policy: every path appears in exactly one set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedFsPolicy {
    pub readwrite: BTreeSet<PathBuf>,
    pub readonly: BTreeSet<PathBuf>,
    pub denied: BTreeSet<PathBuf>,
}

impl ResolvedFsPolicy {
    /// Strictest access class that applies to `path`, considering ancestors.
    /// The scratch rootfs is outside this policy and is checked by callers.
    pub fn access_at(&self, path: &Path) -> FsAccess {
        let mut best = FsAccess::None;
        let candidates = ancestors_of(path);
        for candidate in candidates {
            if self.denied.contains(candidate) {
                return FsAccess::Denied;
            }
            if self.readonly.contains(candidate) && best < FsAccess::Readonly {
                best = FsAccess::Readonly;
            }
            if self.readwrite.contains(candidate) && best < FsAccess::Readwrite {
                best = FsAccess::Readwrite;
            }
        }
        best
    }
}

/// Access class a policy grants at a path. Ordered by strictness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FsAccess {
    None,
    Readonly,
    Readwrite,
    Denied,
}

/// Ancestors of `path` from itself up to `/`, lexically.
fn ancestors_of(path: &Path) -> Vec<&Path> {
    let mut out = vec![path];
    let mut cur = path;
    while let Some(parent) = cur.parent() {
        if parent == cur {
            break;
        }
        out.push(parent);
        cur = parent;
    }
    out
}

/// Lexically normalize an absolute path: resolve `.` and inner `..`, strip
/// trailing slashes. Returns an error if the path is relative or escapes the root.
pub fn normalize_absolute(raw: &str) -> Result<PathBuf> {
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(Error::Policy(format!(
            "filesystem policy paths must be absolute, got {raw:?}"
        )));
    }
    let mut out = PathBuf::from("/");
    for comp in path.components() {
        match comp {
            Component::RootDir | Component::Prefix(_) => {}
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return Err(Error::Policy(format!(
                        "filesystem policy path escapes root: {raw:?}"
                    )));
                }
            }
            Component::Normal(seg) => out.push(seg),
        }
    }
    Ok(out)
}

/// Lexically normalize a sandbox-relative path (exec cwd, files data plane).
/// The empty string maps to the sandbox root itself. Absolute paths and any
/// `..` that escapes the root are errors — at this boundary, traversal is an
/// attack, not a quirk.
pub fn normalize_relative(raw: &str) -> Result<PathBuf> {
    if raw.starts_with('/') {
        return Err(Error::Policy(format!(
            "sandbox-relative paths must not be absolute, got {raw:?}"
        )));
    }
    let path = Path::new(raw);
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::RootDir | Component::Prefix(_) => {
                return Err(Error::Policy(format!(
                    "sandbox-relative paths must not be absolute, got {raw:?}"
                )));
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return Err(Error::Policy(format!(
                        "sandbox-relative path escapes the sandbox root: {raw:?}"
                    )));
                }
            }
            Component::Normal(seg) => out.push(seg),
        }
    }
    Ok(out)
}

impl FsPolicy {
    /// Validate and normalize into [`ResolvedFsPolicy`].
    pub fn resolve(&self) -> Result<ResolvedFsPolicy> {
        let mut denied = BTreeSet::new();
        let mut readonly = BTreeSet::new();
        let mut readwrite = BTreeSet::new();
        for raw in &self.denied {
            denied.insert(normalize_absolute(raw)?);
        }
        for raw in &self.readonly {
            readonly.insert(normalize_absolute(raw)?);
        }
        for raw in &self.readwrite {
            readwrite.insert(normalize_absolute(raw)?);
        }

        // Strictest wins; denial is viral across the subtree.
        readwrite.retain(|p| !covered_by(&denied, p) && !covered_by(&readonly, p));
        readonly.retain(|p| !covered_by(&denied, p));
        // Same-path duplicates across sets collapse into the strictest set,
        // which the retain calls above already guarantee.

        Ok(ResolvedFsPolicy {
            readwrite,
            readonly,
            denied,
        })
    }
}

/// True if any set entry is `p` itself or an ancestor of `p`.
fn covered_by(set: &BTreeSet<PathBuf>, p: &Path) -> bool {
    set.iter().any(|rule| p.starts_with(rule))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(rw: &[&str], ro: &[&str], denied: &[&str]) -> Result<ResolvedFsPolicy> {
        FsPolicy {
            readwrite: rw.iter().map(|s| s.to_string()).collect(),
            readonly: ro.iter().map(|s| s.to_string()).collect(),
            denied: denied.iter().map(|s| s.to_string()).collect(),
        }
        .resolve()
    }

    #[test]
    fn relative_paths_are_rejected() {
        let err = resolve(&["relative/path"], &[], &[]).unwrap_err();
        assert_eq!(err.code(), "policy_validation");
        assert!(err.to_string().contains("absolute"));
    }

    #[test]
    fn dot_dot_that_stays_inside_root_normalizes() {
        let resolved = resolve(&["/data/../data/work"], &[], &[]).unwrap();
        assert!(resolved.readwrite.contains(Path::new("/data/work")));
    }

    #[test]
    fn dot_dot_escaping_root_is_rejected() {
        let err = resolve(&["/.."], &[], &[]).unwrap_err();
        assert_eq!(err.code(), "policy_validation");
    }

    #[test]
    fn trailing_slash_and_dot_components_normalize() {
        let resolved = resolve(&["/data/work/./"], &[], &[]).unwrap();
        assert!(resolved.readwrite.contains(Path::new("/data/work")));
    }

    #[test]
    fn same_path_in_two_sets_strictest_wins() {
        let resolved = resolve(&["/shared"], &["/shared"], &[]).unwrap();
        assert!(resolved.readonly.contains(Path::new("/shared")));
        assert!(!resolved.readwrite.contains(Path::new("/shared")));

        let resolved = resolve(&["/shared"], &[], &["/shared"]).unwrap();
        assert!(resolved.denied.contains(Path::new("/shared")));
        assert!(resolved.readwrite.is_empty());
    }

    #[test]
    fn denial_is_viral_over_the_subtree() {
        let resolved = resolve(&["/etc/allowed-child"], &[], &["/etc"]).unwrap();
        assert!(resolved.denied.contains(Path::new("/etc")));
        assert!(
            resolved.readwrite.is_empty(),
            "denied ancestor must drop rw child"
        );
    }

    #[test]
    fn readonly_ancestor_drops_readwrite_descendant() {
        let resolved = resolve(&["/system/child"], &["/system"], &[]).unwrap();
        assert!(resolved.readonly.contains(Path::new("/system")));
        assert!(resolved.readwrite.is_empty());
    }

    #[test]
    fn disjoint_sets_survive_resolution() {
        let resolved = resolve(&["/data/work"], &["/usr/share"], &["/private/etc"]).unwrap();
        assert_eq!(resolved.readwrite.len(), 1);
        assert_eq!(resolved.readonly.len(), 1);
        assert_eq!(resolved.denied.len(), 1);
    }

    #[test]
    fn access_at_resolves_through_ancestors() {
        let resolved = resolve(&["/data"], &[], &["/data/secret"]).unwrap();
        assert_eq!(
            resolved.access_at(Path::new("/data/file.txt")),
            FsAccess::Readwrite
        );
        assert_eq!(
            resolved.access_at(Path::new("/data/secret/key.pem")),
            FsAccess::Denied
        );
        assert_eq!(resolved.access_at(Path::new("/usr/bin/ls")), FsAccess::None);
    }

    #[test]
    fn access_at_ordering_is_strictness() {
        // Denied beats readonly beats readwrite beats none.
        assert!(FsAccess::Denied > FsAccess::Readwrite);
        assert!(FsAccess::Readwrite > FsAccess::Readonly);
        assert!(FsAccess::Readonly > FsAccess::None);
    }

    #[test]
    fn relative_normalization_rejects_absolute() {
        let err = normalize_relative("/etc/passwd").unwrap_err();
        assert_eq!(err.code(), "policy_validation");
    }

    #[test]
    fn relative_normalization_resolves_inner_traversal() {
        assert_eq!(normalize_relative("").unwrap(), PathBuf::from(""));
        assert_eq!(normalize_relative("a/./b").unwrap(), PathBuf::from("a/b"));
        assert_eq!(normalize_relative("a/../b").unwrap(), PathBuf::from("b"));
    }

    #[test]
    fn relative_normalization_rejects_escape() {
        for raw in ["..", "../x", "a/../../b"] {
            let err = normalize_relative(raw).unwrap_err();
            assert_eq!(err.code(), "policy_validation", "{raw:?} must be rejected");
        }
    }
}
