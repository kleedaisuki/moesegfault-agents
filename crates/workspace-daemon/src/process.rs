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

#[cfg(target_os = "linux")]
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
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        for _ in 0..32 {
            let mut live = 0;
            for entry in std::fs::read_dir("/proc")? {
                let entry = entry?;
                let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                    continue;
                };
                let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
                if fd < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::ESRCH) {
                        continue;
                    }
                    return Err(error).context("open process handle for workspace cleanup");
                }
                let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
                let status = match std::fs::read_to_string(entry.path().join("status")) {
                    Ok(status) => status,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(err) => {
                        return Err(err).context("inspect process ownership for workspace cleanup");
                    }
                };
                let user = status
                    .lines()
                    .find_map(|line| line.strip_prefix("Uid:"))
                    .and_then(|line| line.split_whitespace().next())
                    .and_then(|s| s.parse::<u32>().ok());
                if user != Some(UID)
                    || status
                        .lines()
                        .any(|line| line.starts_with("State:") && line.contains("Z (zombie)"))
                {
                    continue;
                }
                live += 1;
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
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() != Some(libc::ESRCH) {
                        return Err(err).context("terminate workspace process");
                    }
                }
            }
            if live == 0 {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        anyhow::bail!("workspace UID failed to quiesce; refusing checkpoint");
    }
    #[cfg(not(target_os = "linux"))]
    Ok(())
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
