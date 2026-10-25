//! Validate an admitted memory SKU against the kernel's effective cgroup limit.
//! This checks allocation; it does not allocate memory or resize a container.

use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub(super) enum MemoryLimit {
    #[serde(rename = "1g")]
    OneGiB,
    #[serde(rename = "4g")]
    FourGiB,
}

impl MemoryLimit {
    fn bytes(self) -> u64 {
        match self {
            Self::OneGiB => 1024 * 1024 * 1024,
            Self::FourGiB => 4 * 1024 * 1024 * 1024,
        }
    }
}

/// Legacy private requests had no memory field. New public bindings must always
/// send their admitted SKU; absence must never be reported as an allocation.
pub(super) fn verify(requested: Option<MemoryLimit>) -> Result<(), &'static str> {
    let Some(requested) = requested else {
        return Ok(());
    };
    let actual = current_limit().map_err(|_| "code_memory_profile_unavailable")?;
    compare(requested, actual)
}

fn compare(requested: MemoryLimit, actual: Option<u64>) -> Result<(), &'static str> {
    if actual == Some(requested.bytes()) {
        Ok(())
    } else {
        Err("code_memory_profile_mismatch")
    }
}

struct Cgroup {
    root: PathBuf,
    current: PathBuf,
    filename: &'static str,
}

fn current_limit() -> io::Result<Option<u64>> {
    let membership = fs::read_to_string("/proc/self/cgroup")?;
    let mounts = fs::read_to_string("/proc/self/mountinfo")?;
    let cgroup = locate(&membership, &mounts)?;
    effective_limit(&cgroup)
}

fn locate(membership: &str, mounts: &str) -> io::Result<Cgroup> {
    let v2 = membership.lines().find_map(|line| line.strip_prefix("0::"));
    if let Some(member) = v2 {
        return mount_for(member, mounts, "cgroup2", "memory.max");
    }
    let member = membership
        .lines()
        .find_map(|line| {
            let mut fields = line.splitn(3, ':');
            fields.next()?;
            let controllers = fields.next()?;
            controllers
                .split(',')
                .any(|name| name == "memory")
                .then(|| fields.next())
                .flatten()
        })
        .ok_or_else(invalid)?;
    mount_for(member, mounts, "cgroup", "memory.limit_in_bytes")
}

fn mount_for(
    member: &str,
    mounts: &str,
    filesystem: &str,
    filename: &'static str,
) -> io::Result<Cgroup> {
    let member = absolute_path(member)?;
    for line in mounts.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let mut info = right.split_whitespace();
        if info.next() != Some(filesystem) {
            continue;
        }
        info.next();
        if filesystem == "cgroup"
            && !info
                .next()
                .is_some_and(|options| options.split(',').any(|name| name == "memory"))
        {
            continue;
        }
        let fields = left.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 5 {
            continue;
        }
        let hierarchy = absolute_path(&unescape(fields[3])?)?;
        let root = absolute_path(&unescape(fields[4])?)?;
        // cgroup namespaces commonly expose the member as `/` while mountinfo
        // still identifies a host-side subtree. In that case the mount is the
        // visible hierarchy root, not a path to join outside the mount.
        let relative = if member == Path::new("/") {
            Path::new("")
        } else {
            let Ok(relative) = member.strip_prefix(&hierarchy) else {
                continue;
            };
            relative
        };
        return Ok(Cgroup {
            current: root.join(relative),
            root,
            filename,
        });
    }
    Err(invalid())
}

fn effective_limit(cgroup: &Cgroup) -> io::Result<Option<u64>> {
    if !cgroup.current.starts_with(&cgroup.root) {
        return Err(invalid());
    }
    let mut limit = None;
    for directory in cgroup.current.ancestors() {
        let value = fs::read_to_string(directory.join(cgroup.filename))?;
        if let Some(bytes) = parse_limit(&value)? {
            limit = Some(limit.map_or(bytes, |previous: u64| previous.min(bytes)));
        }
        if directory == cgroup.root {
            return Ok(limit);
        }
    }
    Err(invalid())
}

fn parse_limit(value: &str) -> io::Result<Option<u64>> {
    let value = value.trim();
    if value == "max" {
        return Ok(None);
    }
    let bytes = value.parse::<u64>().map_err(|_| invalid())?;
    if bytes == 0 {
        return Err(invalid());
    }
    Ok(Some(bytes))
}

fn absolute_path(value: &str) -> io::Result<PathBuf> {
    let path = PathBuf::from(value);
    if !path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
    {
        return Err(invalid());
    }
    Ok(path)
}

fn unescape(value: &str) -> io::Result<String> {
    let mut output = Vec::with_capacity(value.len());
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            output.push(byte);
            continue;
        }
        let escape = [bytes.next(), bytes.next(), bytes.next()];
        output.push(match escape {
            [Some(b'0'), Some(b'4'), Some(b'0')] => b' ',
            [Some(b'0'), Some(b'1'), Some(b'1')] => b'\t',
            [Some(b'0'), Some(b'1'), Some(b'2')] => b'\n',
            [Some(b'1'), Some(b'3'), Some(b'4')] => b'\\',
            _ => return Err(invalid()),
        });
    }
    String::from_utf8(output).map_err(|_| invalid())
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid memory cgroup hierarchy",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_allocation_is_required_not_just_a_label_or_lower_bound() {
        assert!(compare(MemoryLimit::OneGiB, Some(1024 * 1024 * 1024)).is_ok());
        assert!(compare(MemoryLimit::FourGiB, Some(4 * 1024 * 1024 * 1024)).is_ok());
        assert!(compare(MemoryLimit::FourGiB, Some(1024 * 1024 * 1024)).is_err());
        assert!(compare(MemoryLimit::OneGiB, Some(4 * 1024 * 1024 * 1024)).is_err());
        assert!(compare(MemoryLimit::OneGiB, None).is_err());
        assert!(compare(MemoryLimit::OneGiB, Some(9_223_372_036_854_771_712)).is_err());
        assert!(serde_json::from_str::<MemoryLimit>("\"16g\"").is_err());
        assert!(serde_json::from_str::<MemoryLimit>("\"64g\"").is_err());
        assert!(verify(None).is_ok());
    }

    #[test]
    fn effective_limit_includes_visible_ancestors() {
        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("session");
        fs::create_dir(&child).unwrap();
        fs::write(root.path().join("memory.max"), "1073741824\n").unwrap();
        fs::write(child.join("memory.max"), "max\n").unwrap();
        let cgroup = Cgroup {
            root: root.path().into(),
            current: child.clone(),
            filename: "memory.max",
        };
        assert_eq!(effective_limit(&cgroup).unwrap(), Some(1024 * 1024 * 1024));
        fs::write(child.join("memory.max"), "536870912\n").unwrap();
        assert_eq!(effective_limit(&cgroup).unwrap(), Some(512 * 1024 * 1024));
        fs::write(root.path().join("memory.max"), "max\n").unwrap();
        fs::write(child.join("memory.max"), "max\n").unwrap();
        assert_eq!(effective_limit(&cgroup).unwrap(), None);
        fs::remove_file(child.join("memory.max")).unwrap();
        assert!(effective_limit(&cgroup).is_err());
    }

    #[test]
    fn mount_root_and_cgroup_namespaces_are_resolved_without_path_escape() {
        let mount = "1 0 0:1 /host/session /sys/fs/cgroup rw - cgroup2 cgroup rw\n";
        let namespaced = locate("0::/\n", mount).unwrap();
        assert_eq!(namespaced.current, Path::new("/sys/fs/cgroup"));
        let nested = locate("0::/host/session/code\n", mount).unwrap();
        assert_eq!(nested.current, Path::new("/sys/fs/cgroup/code"));
        assert!(locate("0::/host/other\n", mount).is_err());
        assert!(locate("0::/../outside\n", mount).is_err());
        assert!(locate("0::relative\n", mount).is_err());
        assert_eq!(unescape("/space\\040here").unwrap(), "/space here");
        assert!(unescape("/bad\\999").is_err());
    }

    #[test]
    fn v1_memory_controller_and_invalid_limits_fail_closed() {
        let mount = "1 0 0:1 / /sys/fs/cgroup/memory rw - cgroup cgroup rw,memory\n";
        let located = locate("4:cpu:/test\n5:memory:/session\n", mount).unwrap();
        assert_eq!(located.current, Path::new("/sys/fs/cgroup/memory/session"));
        assert_eq!(located.filename, "memory.limit_in_bytes");
        for invalid in ["", "-1", "0", "unlimited", "18446744073709551616"] {
            assert!(parse_limit(invalid).is_err());
        }
    }
}
