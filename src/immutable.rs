//! Best-effort file immutability so other tools (e.g. `git lfs install`) that
//! try to overwrite lhm's hook scripts fail loudly instead of silently
//! clobbering them.
//!
//! On macOS/BSD, sets the per-user immutable flag (`UF_IMMUTABLE`) via
//! `chflags(2)`. The file owner can set and clear it; no root needed.
//!
//! On Linux, sets `FS_IMMUTABLE_FL` via `ioctl(FS_IOC_SETFLAGS)`. This requires
//! `CAP_LINUX_IMMUTABLE` (root in practice), so it's a no-op for non-root user
//! installs. The protection is defense-in-depth, not a correctness requirement.
//!
//! On other platforms, both functions are no-ops returning `Ok(())`.

use std::path::Path;

/// Mark a file as immutable. Best-effort: returns `Err` only when the syscall
/// itself fails in a way callers may want to log; not finding a suitable
/// mechanism (e.g. unsupported FS) returns `Ok(())`.
pub fn set_immutable(path: &Path) -> Result<(), String> {
    set_flag(path, true)
}

/// Clear the immutable flag if set. Safe to call on files that aren't marked
/// immutable, or on platforms that don't support the flag.
pub fn clear_immutable(path: &Path) -> Result<(), String> {
    set_flag(path, false)
}

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
))]
fn set_flag(path: &Path, enable: bool) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path =
        CString::new(path.as_os_str().as_bytes()).map_err(|e| format!("invalid path {}: {e}", path.display()))?;

    // SAFETY: `c_path` is a valid NUL-terminated C string owned for the duration
    // of the call; `stat` is fully initialized by the kernel on success.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::stat(c_path.as_ptr(), &mut st) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!("stat({}) failed: {err}", path.display()));
    }

    let current = st.st_flags as libc::c_uint;
    let new_flags = if enable {
        current | libc::UF_IMMUTABLE
    } else {
        current & !libc::UF_IMMUTABLE
    };

    if new_flags == current {
        return Ok(());
    }

    // SAFETY: `c_path` is still valid; `chflags` only reads the path.
    let rc = unsafe { libc::chflags(c_path.as_ptr(), new_flags) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!("chflags({}) failed: {err}", path.display()));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_flag(path: &Path, enable: bool) -> Result<(), String> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    // ext-family filesystems support FS_IMMUTABLE_FL. libc doesn't expose the
    // flag constant; the kernel ABI defines it as 0x00000010.
    const FS_IMMUTABLE_FL: libc::c_long = 0x0000_0010;

    // O_NONBLOCK avoids hanging on FIFOs/devices that happen to share the name;
    // FS_IOC_*FLAGS only requires the fd to refer to an inode.
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("open({}) for ioctl failed: {e}", path.display()))?;

    let fd = f.as_raw_fd();
    let mut flags: libc::c_long = 0;
    // SAFETY: `flags` is a properly sized destination for FS_IOC_GETFLAGS.
    let rc = unsafe { libc::ioctl(fd, libc::FS_IOC_GETFLAGS, &mut flags as *mut libc::c_long) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!("FS_IOC_GETFLAGS({}) failed: {err}", path.display()));
    }

    let new_flags = if enable {
        flags | FS_IMMUTABLE_FL
    } else {
        flags & !FS_IMMUTABLE_FL
    };

    if new_flags == flags {
        return Ok(());
    }

    // SAFETY: `new_flags` is a properly sized source for FS_IOC_SETFLAGS.
    let rc = unsafe { libc::ioctl(fd, libc::FS_IOC_SETFLAGS, &new_flags as *const libc::c_long) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!("FS_IOC_SETFLAGS({}) failed: {err}", path.display()));
    }
    Ok(())
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "linux",
)))]
fn set_flag(_path: &Path, _enable: bool) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// On macOS this should succeed for the file owner without root.
    /// On Linux this requires CAP_LINUX_IMMUTABLE and typically fails (EPERM).
    /// The test only asserts behavior we can verify cross-platform: clearing
    /// an unset flag is a no-op, and set+clear round-trips on platforms where
    /// the syscall succeeds.
    #[test]
    fn test_clear_on_unmarked_file_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("file");
        fs::write(&p, "hi").unwrap();
        clear_immutable(&p).expect("clear on unmarked file should succeed");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_set_then_write_fails_on_macos() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hook");
        fs::write(&p, "first").unwrap();

        set_immutable(&p).expect("set_immutable should succeed on macOS for owner");

        let result = fs::write(&p, "overwrite");
        assert!(result.is_err(), "write to immutable file should fail");

        clear_immutable(&p).expect("clear_immutable should succeed");
        fs::write(&p, "ok now").expect("write should succeed after clearing");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_set_then_unlink_fails_on_macos() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hook");
        fs::write(&p, "first").unwrap();

        set_immutable(&p).expect("set_immutable should succeed on macOS for owner");

        let result = fs::remove_file(&p);
        assert!(result.is_err(), "unlinking an immutable file should fail");

        clear_immutable(&p).expect("clear_immutable should succeed");
        fs::remove_file(&p).expect("remove should succeed after clearing");
    }
}
