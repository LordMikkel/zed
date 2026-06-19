use anyhow::{Context as _, Result};

#[cfg(target_os = "linux")]
use std::{collections::HashMap, os::fd::RawFd, path::PathBuf};

#[cfg(target_os = "linux")]
type FdSnapshot = HashMap<RawFd, PathBuf>;

/// Marks file descriptors opened while the guard is alive as `FD_CLOEXEC`.
#[must_use = "the guard must remain alive while file descriptors are opened"]
pub struct CloseOnExecGuard {
    #[cfg(target_os = "linux")]
    before: Option<FdSnapshot>,
}

impl CloseOnExecGuard {
    pub fn new() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            before: match open_fds_snapshot() {
                Ok(before) => Some(before),
                Err(error) => {
                    log::debug!("failed to snapshot open file descriptors: {error}");
                    None
                }
            },
        }
    }
}

impl Drop for CloseOnExecGuard {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(before) = self.before.take()
            && let Err(error) = mark_fds_changed_since(&before)
        {
            log::debug!("failed to set new fds close-on-exec: {error}");
        }
    }
}

/// Marks every currently open file descriptor (except stdin/stdout/stderr)
/// as `FD_CLOEXEC`. Use before spawning a child process to prevent fd leaks
/// from GPU drivers, Wayland compositor, or third-party libraries.
#[cfg(target_os = "linux")]
pub fn mark_open_fds_close_on_exec() -> Result<()> {
    let entries = std::fs::read_dir(PROC_SELF_FD).context("read /proc/self/fd")?;

    for entry in entries {
        let entry = entry?;
        let Some(fd) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<RawFd>().ok())
        else {
            continue;
        };

        if fd <= 2 {
            continue;
        }

        set_fd_close_on_exec(fd)?;
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn mark_open_fds_close_on_exec() -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_fd_close_on_exec(fd: RawFd) -> Result<()> {
    // SAFETY: `fd` comes from `/proc/self/fd` and is only used for the duration
    // of these calls. A concurrent close is handled as `EBADF` below.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EBADF) {
            return Ok(());
        }
        return Err(error.into());
    }

    if flags & libc::FD_CLOEXEC != 0 {
        return Ok(());
    }

    // SAFETY: The same descriptor and flags are passed to the libc operation.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EBADF) {
            return Ok(());
        }
        return Err(error.into());
    }

    Ok(())
}

#[cfg(target_os = "linux")]
const PROC_SELF_FD: &str = "/proc/self/fd";

#[cfg(target_os = "linux")]
fn open_fds_snapshot() -> Result<FdSnapshot> {
    let entries = std::fs::read_dir(PROC_SELF_FD).context("read /proc/self/fd")?;

    let mut snapshot = FdSnapshot::default();
    for entry in entries {
        let entry = entry?;
        let Some(fd) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<RawFd>().ok())
        else {
            continue;
        };
        let target = match std::fs::read_link(entry.path()) {
            Ok(target) => target,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("read link for file descriptor {}", fd));
            }
        };

        snapshot.insert(fd, target);
    }

    Ok(snapshot)
}

#[cfg(target_os = "linux")]
fn mark_fds_changed_since(before: &FdSnapshot) -> Result<()> {
    let after = open_fds_snapshot()?;

    for (fd, target) in after {
        if before.get(&fd) == Some(&target) {
            continue;
        }

        set_fd_close_on_exec(fd)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    fn open_without_close_on_exec(path: &std::path::Path) -> anyhow::Result<std::fs::File> {
        use std::ffi::CString;
        use std::os::fd::FromRawFd;
        use std::os::unix::ffi::OsStrExt;

        let path = CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: `path` is a valid NUL-terminated path and no output
        // pointers are passed to `open`.
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY) };
        if fd == -1 {
            return Err(std::io::Error::last_os_error().into());
        }

        // SAFETY: `fd` is a valid, uniquely owned descriptor returned by
        // `open`, so ownership can be transferred to `File`.
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }

    #[cfg(target_os = "linux")]
    fn has_close_on_exec(fd: std::os::fd::RawFd) -> anyhow::Result<bool> {
        // SAFETY: The caller keeps the owned file descriptor alive.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags == -1 {
            return Err(std::io::Error::last_os_error().into());
        }

        Ok(flags & libc::FD_CLOEXEC != 0)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fd_guard_prevents_new_fd_inheritance() -> anyhow::Result<()> {
        use std::os::fd::AsRawFd;

        let temp_dir = tempfile::tempdir()?;
        let path = temp_dir.path().join("fd-test");
        std::fs::write(&path, b"test")?;

        let file = {
            let _fd_guard = super::CloseOnExecGuard::new();
            open_without_close_on_exec(&path)?
        };

        assert!(has_close_on_exec(file.as_raw_fd())?);

        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mark_open_fds_prevents_existing_fd_inheritance() -> anyhow::Result<()> {
        use std::os::fd::AsRawFd;

        let temp_dir = tempfile::tempdir()?;
        let path = temp_dir.path().join("fd-test-all");
        std::fs::write(&path, b"test")?;
        let file = open_without_close_on_exec(&path)?;

        super::mark_open_fds_close_on_exec()?;

        assert!(has_close_on_exec(file.as_raw_fd())?);

        Ok(())
    }
}
