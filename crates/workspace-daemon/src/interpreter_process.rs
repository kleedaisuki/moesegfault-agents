//! The interpreter has a different UID from Bash and the root supervisor.

use std::path::Path;

use anyhow::{Context, ensure};
use tokio::process::Command;

#[cfg(any(target_os = "linux", test))]
const UID: u32 = 1001;

pub(super) fn directory(path: &Path, private: bool) -> anyhow::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};
        let name = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        ensure!(
            unsafe { libc::chown(name.as_ptr(), UID, UID) } == 0,
            "assign code directory"
        );
        std::fs::set_permissions(
            path,
            std::fs::Permissions::from_mode(if private { 0o700 } else { 0o711 }),
        )?;
    }
    let _ = private;
    Ok(())
}

pub(super) fn configure(command: &mut Command) {
    #[cfg(target_os = "linux")]
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0
                || libc::setgroups(0, std::ptr::null()) != 0
                || libc::setgid(UID) != 0
                || libc::setuid(UID) != 0
                || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
            {
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
    let _ = command;
}

pub(super) fn pause() -> anyhow::Result<()> {
    signal_all(Signal::Pause)
}

pub(super) fn resume() -> anyhow::Result<()> {
    signal_all(Signal::Resume)
}

pub(super) fn stop() -> anyhow::Result<()> {
    signal_all(Signal::Kill)
}

enum Signal {
    Pause,
    Resume,
    Kill,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, PartialEq, Eq)]
enum Task {
    Absent,
    Running,
    Stopped,
}

#[cfg(any(target_os = "linux", test))]
fn task(status: &[u8]) -> anyhow::Result<Task> {
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
        .context("code process status missing UID")?;
    if uid != UID {
        return Ok(Task::Absent);
    }
    Ok(
        match field(b"State:").context("code process status missing state")? {
            b"Z" | b"X" | b"x" => Task::Absent,
            b"T" | b"t" => Task::Stopped,
            _ => Task::Running,
        },
    )
}

fn signal_all(signal: Signal) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        ensure!(
            unsafe { libc::geteuid() } == 0,
            "code supervisor must be root"
        );
        let (signal, wait) = match signal {
            Signal::Pause => (libc::SIGSTOP, true),
            Signal::Resume => (libc::SIGCONT, false),
            Signal::Kill => (libc::SIGKILL, true),
        };
        for _ in 0..32 {
            let mut pending = 0;
            for entry in std::fs::read_dir("/proc")? {
                pending += usize::from(signal_entry(entry?, signal)?);
            }
            if !wait || pending == 0 {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        anyhow::bail!("code processes did not quiesce");
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = signal;
        anyhow::bail!("code execution is Linux-only");
    }
}

#[cfg(target_os = "linux")]
fn disappeared(error: std::io::Error) -> anyhow::Result<Option<()>> {
    if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) {
        return Ok(None);
    }
    Err(error).context("code process operation failed")
}

#[cfg(target_os = "linux")]
fn signal_entry(entry: std::fs::DirEntry, signal: i32) -> anyhow::Result<bool> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
        return Ok(false);
    };
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        disappeared(std::io::Error::last_os_error())?;
        return Ok(false);
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    let status = match std::fs::read(entry.path().join("status")) {
        Ok(status) => status,
        Err(error) => {
            disappeared(error)?;
            return Ok(false);
        }
    };
    let state = task(&status)?;
    if state == Task::Absent || (signal == libc::SIGSTOP && state == Task::Stopped) {
        return Ok(false);
    }
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result < 0 {
        disappeared(std::io::Error::last_os_error())?;
        return Ok(false);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_interpreter_uid_is_frozen() {
        for uid in [0, 1000, 1002] {
            assert_eq!(
                task(format!("State:\tS\nUid:\t{uid}\n").as_bytes()).unwrap(),
                Task::Absent
            );
        }
        assert_eq!(
            task(b"Name:\t\xff\nState:\tS\nUid:\t1001\n").unwrap(),
            Task::Running
        );
        assert_eq!(
            task(b"State:\tT (stopped)\nUid:\t1001\n").unwrap(),
            Task::Stopped
        );
        assert_eq!(
            task(b"State:\tZ (zombie)\nUid:\t1001\n").unwrap(),
            Task::Absent
        );
        assert!(task(b"State:\tS\n").is_err());
    }
}
