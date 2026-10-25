//! Code kernels communicate over Unix IPC and cannot create Internet sockets.
//! Compile before fork; installation in the child is irreversible and fail-closed.

#[cfg(target_os = "linux")]
pub(super) fn filter() -> anyhow::Result<seccompiler::BpfProgram> {
    use seccompiler::{
        SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter, SeccompRule,
    };
    use std::collections::BTreeMap;

    let mut rules = BTreeMap::new();
    for syscall in [libc::SYS_socket, libc::SYS_socketpair] {
        let rule = SeccompRule::new(vec![SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Ne,
            libc::AF_UNIX as u64,
        )?])?;
        rules.insert(syscall, vec![rule.clone()]);
        #[cfg(target_arch = "x86_64")]
        rules.insert(syscall | 0x4000_0000, vec![rule]);
    }
    // io_uring socket operations do not invoke the socket syscall. FD theft from
    // another process must not introduce an unfiltered Internet socket either.
    for syscall in [libc::SYS_io_uring_setup, libc::SYS_pidfd_getfd] {
        rules.insert(syscall, vec![]);
        #[cfg(target_arch = "x86_64")]
        rules.insert(syscall | 0x4000_0000, vec![]);
    }
    Ok(SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        std::env::consts::ARCH.try_into()?,
    )?
    .try_into()?)
}

#[cfg(target_os = "linux")]
pub(super) fn install(filter: &seccompiler::BpfProgram) -> std::io::Result<()> {
    seccompiler::apply_filter(filter).map_err(|_| std::io::Error::from_raw_os_error(libc::EPERM))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[test]
    fn compiled_filter_has_an_architecture_guard_and_is_bounded() {
        let program = super::filter().unwrap();
        assert!(program.len() < 256);
        assert!(program.iter().any(|instruction| {
            instruction.k == libc::SECCOMP_RET_KILL_PROCESS && instruction.code == 6
        }));
    }
}
