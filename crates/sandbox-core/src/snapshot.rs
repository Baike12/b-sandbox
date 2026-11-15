//! Rootfs templates and reflink snapshots.
//!
//! A template is a named rootfs snapshot under `<data-root>/templates/<name>`,
//! created by the commit endpoint and materialized at provision time.
//! File copies use the filesystem's native copy-on-write reflink when
//! available — `clonefile(2)` on macOS/APFS, `FICLONE` on Linux/btrfs-XFS —
//! falling back to a plain copy where the filesystem refuses (cross-device,
//! unsupported). Cloning is a performance property only; snapshot *correctness*
//! (content, permissions, symlinks) is identical either way and is what the
//! tests pin.
//!
//! Template names are data-plane path components and are treated as
//! adversarial: a strict charset rejects traversal before any path join.

use std::io;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Validate a template name: 1-63 chars of `[a-z0-9-]`, not starting or
/// ending with `-`. Single path segment — traversal and separators are
/// rejected before any filesystem call.
pub fn validate_template_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err(Error::Policy(format!(
            "template name {name:?} invalid: use 1-63 chars of [a-z0-9-], no leading/trailing dash"
        )))
    }
}

/// Directory holding all templates for a data root.
pub fn templates_dir(data_root: &Path) -> PathBuf {
    data_root.join("templates")
}

/// Validated path of one template.
pub fn template_path(data_root: &Path, name: &str) -> Result<PathBuf> {
    validate_template_name(name)?;
    Ok(templates_dir(data_root).join(name))
}

/// Materialize a template into an existing (empty) rootfs directory.
/// A missing template is a policy error, not a silent empty rootfs.
pub fn materialize_template(data_root: &Path, name: &str, rootfs: &Path) -> Result<()> {
    let src = template_path(data_root, name)?;
    if !src.is_dir() {
        return Err(Error::Policy(format!("template {name:?} does not exist")));
    }
    snapshot_tree(&src, rootfs)
}

/// Snapshot a directory tree into `dst` (which must exist). Reflink per file
/// with copy fallback; symlinks are recreated as symlinks, never followed.
pub fn snapshot_tree(src: &Path, dst: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src).map_err(io_err(src))? {
        let entry = entry.map_err(io_err(src))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let meta = entry.file_type().map_err(io_err(&from))?;
        if meta.is_dir() {
            std::fs::create_dir(&to).map_err(io_err(&to))?;
            snapshot_tree(&from, &to)?;
        } else if meta.is_symlink() {
            let target = std::fs::read_link(&from).map_err(io_err(&from))?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &to).map_err(io_err(&to))?;
            #[cfg(not(unix))]
            return Err(Error::Internal("symlinks require unix".into()));
        } else {
            clone_or_copy_file(&from, &to)?;
        }
    }
    Ok(())
}

/// Clone one file via the platform reflink; fall back to a plain copy when
/// the filesystem does not support it (ENOTSUP/EXDEV/EPERM/ENOSYS…).
pub fn clone_or_copy_file(src: &Path, dst: &Path) -> Result<()> {
    if clone_file(src, dst)? {
        return Ok(());
    }
    std::fs::copy(src, dst)
        .map(|_| ())
        .map_err(|e| Error::Internal(format!("copy {} → {}: {e}", src.display(), dst.display())))
}

/// Platform reflink. `Ok(true)` = cloned, `Ok(false)` = unsupported (caller
/// copies), `Err` = a real failure that must not be swallowed.
#[allow(unused_variables)]
fn clone_file(src: &Path, dst: &Path) -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        let c_src = cstring(src)?;
        let c_dst = cstring(dst)?;
        // clonefile(src, dst, flags) — 0 on success; unsupported or
        // cross-device filesystems are fallback conditions, not errors.
        let rc = unsafe { libc::clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) };
        if rc == 0 {
            return Ok(true);
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::ENOTSUP) | Some(libc::EXDEV) | Some(libc::EPERM) => Ok(false),
            Some(libc::EEXIST) => Err(Error::Internal(format!(
                "snapshot destination exists: {}",
                dst.display()
            ))),
            _ => Err(Error::Internal(format!(
                "clonefile {} → {}: {}",
                src.display(),
                dst.display(),
                io::Error::last_os_error()
            ))),
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use std::os::unix::io::AsRawFd;
        let src_f = std::fs::File::open(src).map_err(io_err(src))?;
        let dst_f = std::fs::File::create(dst).map_err(io_err(dst))?;
        // FICLONE ioctl: reflink on btrfs/XFS/NFS; ENOSYS/EINVAL/EXDEV → copy.
        let rc = unsafe { libc::ioctl(dst_f.as_raw_fd(), libc::FICLONE, src_f.as_raw_fd()) };
        if rc == 0 {
            return Ok(true);
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::ENOSYS)
            | Some(libc::EINVAL)
            | Some(libc::EXDEV)
            | Some(libc::EOPNOTSUPP) => Ok(false),
            _ => Err(Error::Internal(format!(
                "FICLONE {} → {}: {}",
                src.display(),
                dst.display(),
                io::Error::last_os_error()
            ))),
        }
    }
    #[cfg(not(unix))]
    {
        Ok(false)
    }
}

fn cstring(p: &Path) -> Result<std::ffi::CString> {
    std::ffi::CString::new(p.as_os_str().as_encoded_bytes())
        .map_err(|_| Error::Internal(format!("path has interior NUL: {}", p.display())))
}

fn io_err(ctx: impl AsRef<Path>) -> impl FnOnce(io::Error) -> Error {
    let ctx = ctx.as_ref().to_path_buf();
    move |e| Error::Internal(format!("{}: {e}", ctx.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, content: &[u8]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn template_names_reject_traversal_and_noise() {
        for bad in [
            "",
            "..",
            "../evil",
            "a/b",
            "a\\b",
            "UPPER",
            " leading",
            "-lead",
            "trail-",
            "dot.name",
            "with space",
            &"a".repeat(64),
        ] {
            let err = validate_template_name(bad).unwrap_err();
            assert_eq!(err.code(), "policy_validation", "{bad:?}");
        }
        for good in ["base", "python-3-12", "a1", &"x".repeat(63)] {
            validate_template_name(good).unwrap();
        }
    }

    #[test]
    fn snapshot_tree_copies_content_dirs_and_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        write(&src.join("a.txt"), b"alpha");
        write(&src.join("nested/deep/b.txt"), b"beta");
        #[cfg(unix)]
        std::os::unix::fs::symlink("a.txt", src.join("link-to-a")).unwrap();

        let dst = tmp.path().join("dst");
        std::fs::create_dir(&dst).unwrap();
        snapshot_tree(&src, &dst).unwrap();

        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"alpha");
        assert_eq!(
            std::fs::read(dst.join("nested/deep/b.txt")).unwrap(),
            b"beta"
        );
        #[cfg(unix)]
        {
            let link = std::fs::read_link(dst.join("link-to-a")).unwrap();
            assert_eq!(link, Path::new("a.txt"), "symlink recreated, not followed");
            assert_eq!(std::fs::read(dst.join("link-to-a")).unwrap(), b"alpha");
        }
    }

    #[test]
    fn materialize_requires_an_existing_template() {
        let tmp = tempfile::tempdir().unwrap();
        let err = materialize_template(tmp.path(), "nope", tmp.path().join("root").as_path())
            .unwrap_err();
        assert_eq!(err.code(), "policy_validation");
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn materialize_reproduces_the_template_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let tpl = templates_dir(tmp.path()).join("base");
        write(&tpl.join("hello.txt"), b"hello template");
        write(&tpl.join("bin/run.sh"), b"#!/bin/sh\necho hi\n");

        let rootfs = tmp.path().join("sbx-root");
        std::fs::create_dir(&rootfs).unwrap();
        materialize_template(tmp.path(), "base", &rootfs).unwrap();

        assert_eq!(
            std::fs::read_to_string(rootfs.join("hello.txt")).unwrap(),
            "hello template"
        );
        assert_eq!(
            std::fs::read_to_string(rootfs.join("bin/run.sh")).unwrap(),
            "#!/bin/sh\necho hi\n"
        );
    }

    #[test]
    fn template_path_is_contained_in_templates_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let p = template_path(tmp.path(), "ok-name").unwrap();
        assert_eq!(p, tmp.path().join("templates").join("ok-name"));
        // Traversal names never reach a path join.
        assert!(template_path(tmp.path(), "../escape").is_err());
    }

    #[test]
    fn clone_or_copy_produces_identical_content() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("f.bin");
        let payload: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        write(&src, &payload);
        let dst = tmp.path().join("g.bin");
        clone_or_copy_file(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), payload);
    }
}
