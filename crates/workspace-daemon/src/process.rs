use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;

#[cfg(any(target_os = "linux", test))]
const UID: u32 = 1000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecRequest {
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "default_output")]
    pub max_output_bytes: usize,
}

fn default_timeout() -> u64 {
    120_000
}
fn default_output() -> usize {
    1024 * 1024
}

impl ExecRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.id.is_empty()
            || self.id.len() > 128
            || !self
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            return Err("id must contain 1 to 128 ASCII letters, digits, underscores or hyphens");
        }
        if self.command.is_empty() || self.command.len() > 24 * 1024 || self.command.contains('\0')
        {
            return Err("command must contain 1 to 24576 bytes without NUL");
        }
        if self.cwd.as_ref().is_some_and(|cwd| {
            cwd.contains('\0')
                || cwd.len() > 4096
                || std::path::Path::new(cwd).components().any(|part| {
                    !matches!(
                        part,
                        std::path::Component::Normal(_) | std::path::Component::CurDir
                    )
                })
        }) {
            return Err("cwd must be a relative path without parent components");
        }
        if !(1..=600_000).contains(&self.timeout_ms) {
            return Err("timeout_ms must be between 1 and 600000");
        }
        if !(1..=4 * 1024 * 1024).contains(&self.max_output_bytes) {
            return Err("max_output_bytes must be between 1 and 4194304");
        }
        Ok(())
    }
}

#[derive(Clone, Serialize)]
pub struct ExecResult {
    pub id: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub cancelled: bool,
    pub truncated: bool,
    pub duration_ms: u64,
}

pub fn require_supported_platform() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        anyhow::ensure!(
            unsafe { libc::geteuid() } == 0,
            "daemon must start as root to isolate bash UID"
        );
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) };
        anyhow::ensure!(
            fd >= 0,
            "kernel must support pidfds for safe workspace cleanup"
        );
        unsafe {
            libc::close(fd as i32);
        }
    }
    #[cfg(not(target_os = "linux"))]
    anyhow::bail!("workspace execution is Linux-only; use the container image");
    #[allow(unreachable_code)]
    Ok(())
}

pub fn prepare_workspace(path: &Path) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};
        let name = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        anyhow::ensure!(
            unsafe { libc::chown(name.as_ptr(), UID, UID) } == 0,
            "cannot assign workspace ownership"
        );
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    let _ = path;
    Ok(())
}

async fn capture(
    mut reader: impl AsyncRead + Unpin,
    budget: Arc<AtomicUsize>,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut retained = Vec::new();
    let mut buffer = [0; 8192];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let available = budget
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                Some(left.saturating_sub(count))
            })
            .unwrap();
        let keep = count.min(available);
        retained.extend_from_slice(&buffer[..keep]);
        truncated |= keep < count;
    }
    Ok((retained, truncated))
}

#[cfg(target_os = "linux")]
fn configure(command: &mut Command) {
    // The daemon remains root; arbitrary bash never shares its credentials or
    // environment. no_new_privs also disables setuid/file-capability escalation.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0
                || libc::setgroups(0, std::ptr::null()) != 0
                || libc::setgid(UID) != 0
                || libc::setuid(UID) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            for (resource, limit) in [
                (libc::RLIMIT_NPROC, 128),
                (libc::RLIMIT_NOFILE, 1024),
                (libc::RLIMIT_FSIZE, 512 * 1024 * 1024),
                (libc::RLIMIT_CORE, 0),
            ] {
                let value = libc::rlimit {
                    rlim_cur: limit,
                    rlim_max: limit,
                };
                if libc::setrlimit(resource, &value) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn configure(_: &mut Command) {}

fn kill_group(pid: u32) {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    let _ = pid;
}

/// Kill every process belonging to the dedicated workspace UID, including
/// children that called setsid(). pidfds prevent a PID-reuse signal race.
pub fn quiesce() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        for _ in 0..32 {
            let mut live = 0;
            for entry in std::fs::read_dir("/proc")? {
                live += usize::from(terminate_workspace_entry(entry?)?);
            }
            if live == 0 {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        tracing::error!(
            event = "workspace.cleanup_error",
            stage = "quiesce_exhausted",
            errno = 0
        );
        anyhow::bail!("workspace UID failed to quiesce; refusing checkpoint");
    }
    #[cfg(not(target_os = "linux"))]
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn proc_outcome<T>(stage: &'static str, result: std::io::Result<T>) -> anyhow::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        // An opened procfs file can return ESRCH when its task exits before
        // read; the same exit before open returns ENOENT. Neither is a failure
        // to terminate a writer. Permission/resource/unknown errors fail closed.
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) => {
            tracing::info!(
                event = "workspace.cleanup_race",
                stage,
                errno = error.raw_os_error().unwrap()
            );
            Ok(None)
        }
        Err(error) => {
            tracing::error!(
                event = "workspace.cleanup_error",
                stage,
                errno = error.raw_os_error().unwrap_or(0)
            );
            Err(error).context("workspace process cleanup failed")
        }
    }
}

#[cfg(target_os = "linux")]
fn open_pidfd(pid: i32) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) })
}

#[cfg(target_os = "linux")]
fn signal_pidfd(fd: &std::os::fd::OwnedFd) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let sent = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if sent < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn workspace_writer(status: &[u8]) -> anyhow::Result<bool> {
    let field = |prefix: &[u8]| {
        status
            .split(|byte| *byte == b'\n')
            .find_map(|line| line.strip_prefix(prefix))
            .and_then(|line| {
                line.split(u8::is_ascii_whitespace)
                    .find(|part| !part.is_empty())
            })
    };
    let uid = field(b"Uid:")
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse::<u32>().ok())
        .context("process status missing UID")?;
    if uid != UID {
        return Ok(false);
    }
    let state = field(b"State:").context("process status missing state")?;
    Ok(!matches!(state, b"Z" | b"X" | b"x"))
}

#[cfg(target_os = "linux")]
fn terminate_workspace_entry(entry: std::fs::DirEntry) -> anyhow::Result<bool> {
    let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
        return Ok(false);
    };
    let Some(fd) = proc_outcome("pidfd_open", open_pidfd(pid))? else {
        return Ok(false);
    };
    // Only ASCII kernel fields matter. Process names can contain non-UTF-8
    // bytes, so decoding the entire status document would be another false 500.
    let Some(status) = proc_outcome("process_status", std::fs::read(entry.path().join("status")))?
    else {
        return Ok(false);
    };
    if !workspace_writer(&status)? {
        return Ok(false);
    }
    // Count only a successfully signaled live task; ESRCH is already dead.
    Ok(proc_outcome("pidfd_signal", signal_pidfd(&fd))?.is_some())
}

pub async fn execute(
    request: ExecRequest,
    workspace: PathBuf,
    cancel: CancellationToken,
) -> anyhow::Result<ExecResult> {
    require_supported_platform()?;
    let started = Instant::now();
    // Resolve through a capability and retain the descriptor until spawn; bash
    // cannot redirect cwd through a symlink outside its workspace generation.
    let root = cap_std::fs::Dir::open_ambient_dir(&workspace, cap_std::ambient_authority())?;
    let cwd = root.open_dir(request.cwd.as_deref().unwrap_or("."))?;
    #[cfg(target_os = "linux")]
    let current_dir = {
        use std::os::fd::AsRawFd;
        PathBuf::from(format!("/proc/self/fd/{}", cwd.as_raw_fd()))
    };
    #[cfg(not(target_os = "linux"))]
    let current_dir = {
        let _ = cwd;
        workspace.clone()
    };
    let mut command = Command::new("/bin/bash");
    command
        .args(["--noprofile", "--norc", "-c", &request.command])
        .current_dir(current_dir)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", &workspace)
        .env("LANG", "C.UTF-8")
        .env("TERM", "dumb")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure(&mut command);
    let mut child = command.spawn().context("spawn bash")?;
    let pid = child.id().context("bash missing PID")?;
    let budget = Arc::new(AtomicUsize::new(request.max_output_bytes));
    let stdout = tokio::spawn(capture(child.stdout.take().unwrap(), budget.clone()));
    let stderr = tokio::spawn(capture(child.stderr.take().unwrap(), budget));
    let mut timed_out = false;
    let mut cancelled = false;
    let status = tokio::select! {
        result = child.wait() => result?,
        _ = tokio::time::sleep(Duration::from_millis(request.timeout_ms)) => {
            timed_out = true;
            kill_group(pid);
            let _ = child.start_kill();
            child.wait().await?
        },
        _ = cancel.cancelled() => {
            cancelled = true;
            kill_group(pid);
            let _ = child.start_kill();
            child.wait().await?
        },
    };
    kill_group(pid);
    tokio::task::spawn_blocking(quiesce).await??;
    let (stdout, out_cut) = stdout.await??;
    let (stderr, err_cut) = stderr.await??;
    let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    tracing::info!(event = "workspace.exec_complete", execution_id = %request.id, duration_ms, timed_out, cancelled, truncated = out_cut || err_cut);
    Ok(ExecResult {
        id: request.id,
        exit_code: status.code(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        timed_out,
        cancelled,
        truncated: out_cut || err_cut,
        duration_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_ignores_only_process_disappearance() {
        for stage in ["pidfd_open", "process_status", "pidfd_signal"] {
            for errno in [libc::ENOENT, libc::ESRCH] {
                assert!(
                    proc_outcome::<()>(stage, Err(std::io::Error::from_raw_os_error(errno)))
                        .unwrap()
                        .is_none()
                );
            }
            for errno in [
                libc::EPERM,
                libc::EACCES,
                libc::EIO,
                libc::EMFILE,
                libc::ENFILE,
                libc::ENOSYS,
                libc::EINVAL,
            ] {
                assert!(
                    proc_outcome::<()>(stage, Err(std::io::Error::from_raw_os_error(errno)))
                        .is_err()
                );
            }
        }
        assert_eq!(proc_outcome("process_status", Ok(42)).unwrap(), Some(42));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn already_open_status_reports_esrch_after_its_process_exits() {
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        // This test only touches its own child. It never runs UID-wide cleanup
        // and is safe on an ordinary unprivileged CI runner.
        let mut child = Child(
            std::process::Command::new("/bin/sleep")
                .arg("30")
                .spawn()
                .unwrap(),
        );
        let path = format!("/proc/{}/status", child.0.id());
        let fd = open_pidfd(child.0.id() as i32).unwrap();
        let mut status = std::fs::File::open(&path).unwrap();
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        let mut bytes = Vec::new();
        let error = std::io::Read::read_to_end(&mut status, &mut bytes).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ESRCH));
        assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(
            proc_outcome::<()>("process_status", Err(error))
                .unwrap()
                .is_none()
        );
        assert!(
            proc_outcome("process_status", std::fs::read(path))
                .unwrap()
                .is_none()
        );
        assert!(
            proc_outcome("pidfd_signal", signal_pidfd(&fd))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn process_names_are_not_decoded_as_utf8_or_treated_as_kernel_fields() {
        assert!(
            workspace_writer(b"Name:\t\xff\nState:\tS (sleeping)\nUid:\t1000\t1000\t1000\t1000\n")
                .unwrap()
        );
        for state in ["Z (zombie)", "X (dead)", "x (dead)"] {
            assert!(
                !workspace_writer(format!("State:\t{state}\nUid:\t1000\n").as_bytes()).unwrap()
            );
        }
        assert!(!workspace_writer(b"Name:\tUid:\t1000\nState:\tS\nUid:\t0\n").unwrap());
        assert!(workspace_writer(b"Name:\tunknown\nState:\tS\n").is_err());
    }

    #[tokio::test]
    async fn bounded_capture_still_drains() {
        let (bytes, truncated) = capture(&b"abcdefgh"[..], Arc::new(AtomicUsize::new(3)))
            .await
            .unwrap();
        assert_eq!(bytes, b"abc");
        assert!(truncated);
    }

    #[test]
    fn request_limits_are_explicit() {
        let mut request: ExecRequest =
            serde_json::from_str(r#"{"id":"run-1","command":"true"}"#).unwrap();
        assert!(request.validate().is_ok());
        request.timeout_ms = 600_001;
        assert!(request.validate().is_err());
        request.timeout_ms = 1;
        request.id = "../x".into();
        assert!(request.validate().is_err());
    }
}
