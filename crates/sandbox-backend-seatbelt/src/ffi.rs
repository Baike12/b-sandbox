//! The single `unsafe` seam of this backend: libSystem's `sandbox_init`.
//!
//! `sandbox_init` is an undocumented but long-stable libSystem symbol (Apple's
//! own `sandbox-exec` uses exactly this path). Contract, verified against
//! `/usr/include` in the macOS SDK:
//!
//! ```c
//! int sandbox_init(const char *profile, uint64_t flags, char **errorbuf);
//! ```
//!
//! Returns **0 on success, non-zero on failure** (it is NOT a pointer/token —
//! treating a successful 0 return as a null token made every spawn fail with
//! EINVAL when this backend was first wired). On failure `errorbuf` points to
//! a human-readable reason the caller must release with `sandbox_free_error`.
//!
//! Invariant: we call it inside `pre_exec` — i.e. in the forked child, right
//! before `exec` — so the sandbox covers the whole workload lifetime. The
//! sandbox cannot be lifted afterwards; nothing to keep alive, nothing to
//! free on the success path.

use std::ffi::{CStr, CString, c_char};

use sandbox_core::error::{BackendFailureKind, Error, Result};

unsafe extern "C" {
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> i32;
    fn sandbox_free_error(errorbuf: *mut c_char);
}

/// Apply a Seatbelt profile to the current process. Must be called between
/// fork and exec (in `pre_exec`); the sandbox cannot be lifted afterwards.
pub fn apply(profile: &str) -> Result<()> {
    let c_profile = CString::new(profile).map_err(|e| Error::Backend {
        kind: BackendFailureKind::Spawn,
        message: format!("profile contains NUL byte: {e}"),
    })?;
    let mut errorbuf: *mut c_char = std::ptr::null_mut();
    let rc = unsafe { sandbox_init(c_profile.as_ptr(), 0, &mut errorbuf) };
    if rc != 0 {
        let message = if errorbuf.is_null() {
            format!("sandbox_init returned {rc} with no error message")
        } else {
            let msg = unsafe { CStr::from_ptr(errorbuf) }
                .to_string_lossy()
                .into_owned();
            unsafe { sandbox_free_error(errorbuf) };
            format!("sandbox_init returned {rc}: {msg}")
        };
        return Err(Error::Backend {
            kind: BackendFailureKind::Spawn,
            message,
        });
    }
    Ok(())
}
