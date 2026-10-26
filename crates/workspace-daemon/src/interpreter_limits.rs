//! Validate an admitted memory SKU against the kernel's effective cgroup limit.
//! This checks allocation; it does not allocate memory or resize a container.

use std::{
    collections::VecDeque,
    fs,
    io::{self, BufRead},
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

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AllocationProfile {
    memory_limit: MemoryLimit,
    application_class: String,
    image_source: String,
}

impl AllocationProfile {
    fn valid(&self) -> bool {
        let class = match self.memory_limit {
            MemoryLimit::OneGiB => "Session",
            MemoryLimit::FourGiB => "Session4G",
        };
        self.application_class == class
            && self.image_source.len() == 40
            && self
                .image_source
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    }
}

struct GuestObservation {
    cgroup_limit: Option<u64>,
    physical_bytes: u64,
}

#[derive(Serialize)]
#[serde(tag = "capacity_source", rename_all = "snake_case")]
pub(super) enum CapacityEvidence {
    Cgroup {
        allocation_bytes: u64,
        guest_physical_bytes: u64,
    },
    CloudflareVm {
        allocation_bytes: u64,
        guest_physical_bytes: u64,
        allocation: AllocationProfile,
    },
}

fn accepted_capacity(
    requested: MemoryLimit,
    guest: &GuestObservation,
    allocation: Option<&AllocationProfile>,
) -> Result<CapacityEvidence, &'static str> {
    if let Some(limit) = guest.cgroup_limit {
        compare(requested, Some(limit))?;
        return Ok(CapacityEvidence::Cgroup {
            allocation_bytes: limit,
            guest_physical_bytes: guest.physical_bytes,
        });
    }
    let allocation = allocation
        .filter(|allocation| allocation.valid())
        .ok_or("code_memory_profile_unavailable")?;
    compare(requested, Some(allocation.memory_limit.bytes()))?;
    // The allocation is the provider's immutable application/SKU, not MemTotal.
    // Both observed CF SKUs include additional guest OS memory. This bounded
    // envelope detects a wrong VM; it does not invent a finite cgroup ceiling.
    let overhead = guest
        .physical_bytes
        .checked_sub(requested.bytes())
        .ok_or("code_memory_profile_mismatch")?;
    if overhead > 256 * 1024 * 1024 {
        return Err("code_memory_profile_mismatch");
    }
    Ok(CapacityEvidence::CloudflareVm {
        allocation_bytes: requested.bytes(),
        guest_physical_bytes: guest.physical_bytes,
        allocation: allocation.clone(),
    })
}

fn observe() -> io::Result<(GuestObservation, Option<AllocationProfile>)> {
    let meminfo = fs::read_to_string("/proc/meminfo")?;
    let physical_bytes = meminfo
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            if fields.next()? != "MemTotal:" {
                return None;
            }
            let kib = fields.next()?.parse::<u64>().ok()?;
            if fields.next()? != "kB" {
                return None;
            }
            kib.checked_mul(1024)
        })
        .ok_or_else(invalid)?;
    let allocation = match std::env::var("MAGENTS_CF_ALLOCATION") {
        Ok(value) if value.len() <= 1024 => {
            Some(serde_json::from_str(&value).map_err(|_| invalid())?)
        }
        Ok(_) => return Err(invalid()),
        Err(std::env::VarError::NotPresent) => None,
        Err(_) => return Err(invalid()),
    };
    Ok((
        GuestObservation {
            cgroup_limit: current_limit()?,
            physical_bytes,
        },
        allocation,
    ))
}

pub(super) fn capacity() -> Option<CapacityEvidence> {
    let (guest, allocation) = observe().ok()?;
    [MemoryLimit::OneGiB, MemoryLimit::FourGiB]
        .into_iter()
        .find_map(|memory| accepted_capacity(memory, &guest, allocation.as_ref()).ok())
}

/// Legacy private requests had no memory field. New public bindings must always
/// send their admitted SKU; absence must never be reported as an allocation.
pub(super) fn verify(requested: Option<MemoryLimit>) -> Result<(), &'static str> {
    let Some(requested) = requested else {
        return Ok(());
    };
    let (guest, allocation) = observe().map_err(|_| "code_memory_profile_unavailable")?;
    accepted_capacity(requested, &guest, allocation.as_ref()).map(|_| ())
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
    effective_limit(&resolve_process(
        locate(&membership, &mounts)?,
        std::process::id(),
    )?)
}

fn resolve_process(mut cgroup: Cgroup, pid: u32) -> io::Result<Cgroup> {
    if cgroup.filename == "memory.max"
        && cgroup.current == cgroup.root
        && !contains_process(&cgroup.current, pid)?
    {
        cgroup.current = discover_process(&cgroup.root, pid)?;
    }
    Ok(cgroup)
}

fn contains_process(directory: &Path, pid: u32) -> io::Result<bool> {
    let file = fs::File::open(directory.join("cgroup.procs"))?;
    for (index, line) in io::BufReader::new(file).lines().enumerate() {
        if index >= 16384 {
            return Err(invalid());
        }
        if line?.trim().parse::<u32>().ok() == Some(pid) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn discover_process(root: &Path, pid: u32) -> io::Result<PathBuf> {
    let mut pending = VecDeque::from([(root.to_owned(), 0)]);
    let mut visited = 0;
    let mut found = None;
    while let Some((directory, depth)) = pending.pop_front() {
        visited += 1;
        if visited > 128 || depth > 8 {
            return Err(invalid());
        }
        if contains_process(&directory, pid)? && found.replace(directory.clone()).is_some() {
            return Err(invalid());
        }
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if pending.len() >= 128 {
                return Err(invalid());
            }
            pending.push_back((entry.path(), depth + 1));
        }
    }
    found.ok_or_else(invalid)
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
        let root = absolute_path(&unescape(fields[4])?)?;
        // cgroup namespaces commonly expose the member as `/` while mountinfo
        // still identifies a host-side subtree. In that case the mount is the
        // visible hierarchy root, not a path to join outside the mount.
        let current = if member == Path::new("/") {
            root.clone()
        } else {
            let hierarchy = absolute_path(&unescape(fields[3])?)?;
            let Ok(relative) = member.strip_prefix(&hierarchy) else {
                continue;
            };
            root.join(relative)
        };
        return Ok(Cgroup {
            current,
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
        let value = match fs::read_to_string(directory.join(cgroup.filename)) {
            Ok(value) => value,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && directory == cgroup.root
                    && cgroup.filename == "memory.max" =>
            {
                return Ok(limit);
            }
            Err(error) => return Err(error),
        };
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

    fn process_fixture() -> tempfile::TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.temp/code-cgroup");
        fs::create_dir_all(&root).unwrap();
        tempfile::tempdir_in(root).unwrap()
    }

    fn group(root: &Path, name: &str, pid: u32, limit: &str) -> PathBuf {
        let path = root.join(name);
        fs::create_dir(&path).unwrap();
        fs::write(path.join("cgroup.procs"), format!("{pid}\n")).unwrap();
        fs::write(path.join("memory.max"), limit).unwrap();
        path
    }

    #[test]
    fn capacity_sources_keep_platform_allocation_and_guest_memory_distinct() {
        let cases = [
            (MemoryLimit::OneGiB, "Session", 1242872_u64),
            (MemoryLimit::FourGiB, "Session4G", 4263920_u64),
        ];
        for (memory_limit, class, physical_kib) in cases {
            let guest = GuestObservation {
                cgroup_limit: None,
                physical_bytes: physical_kib * 1024,
            };
            let allocation = AllocationProfile {
                memory_limit,
                application_class: class.into(),
                image_source: "a".repeat(40),
            };
            let proof = serde_json::to_value(
                accepted_capacity(memory_limit, &guest, Some(&allocation)).unwrap(),
            )
            .unwrap();
            assert_eq!(proof["capacity_source"], "cloudflare_vm");
            assert_eq!(proof["allocation_bytes"], memory_limit.bytes());
            assert_eq!(proof["guest_physical_bytes"], guest.physical_bytes);
            assert_ne!(proof["allocation_bytes"], proof["guest_physical_bytes"]);
            assert!(accepted_capacity(memory_limit, &guest, None).is_err());
            assert!(
                accepted_capacity(
                    memory_limit,
                    &GuestObservation {
                        cgroup_limit: None,
                        physical_bytes: 16 * 1024 * 1024 * 1024
                    },
                    Some(&allocation)
                )
                .is_err()
            );
            let mut invalid = allocation.clone();
            invalid.image_source = "latest".into();
            assert!(accepted_capacity(memory_limit, &guest, Some(&invalid)).is_err());
            invalid = allocation;
            invalid.application_class = "unrelated".into();
            assert!(accepted_capacity(memory_limit, &guest, Some(&invalid)).is_err());
        }
        let guest = GuestObservation {
            cgroup_limit: Some(1024 * 1024 * 1024),
            physical_bytes: 8 * 1024 * 1024 * 1024,
        };
        let cf = AllocationProfile {
            memory_limit: MemoryLimit::FourGiB,
            application_class: "Session4G".into(),
            image_source: "b".repeat(40),
        };
        assert!(accepted_capacity(MemoryLimit::FourGiB, &guest, Some(&cf)).is_err());
        let proof = serde_json::to_value(
            accepted_capacity(MemoryLimit::OneGiB, &guest, Some(&cf)).unwrap(),
        )
        .unwrap();
        assert_eq!(proof["capacity_source"], "cgroup");
    }

    #[test]
    fn namespace_ancestor_mount_finds_the_actual_process_not_a_matching_sibling_limit() {
        let root = process_fixture();
        fs::write(root.path().join("cgroup.procs"), "\n").unwrap();
        let own = group(root.path(), "workload", 42, "4294967296\n");
        group(root.path(), "unrelated", 43, "1073741824\n");
        let mounts = format!(
            "66 65 0:22 /.. {} rw - cgroup2 cgroup2 rw\n",
            root.path().display()
        );
        let cgroup = resolve_process(locate("0::/\n", &mounts).unwrap(), 42).unwrap();
        assert_eq!(cgroup.current, own);
        assert_eq!(effective_limit(&cgroup).unwrap(), Some(4294967296));
        assert!(compare(MemoryLimit::FourGiB, effective_limit(&cgroup).unwrap()).is_ok());
        assert!(compare(MemoryLimit::OneGiB, effective_limit(&cgroup).unwrap()).is_err());
        fs::write(root.path().join("memory.max"), "2147483648\n").unwrap();
        assert_eq!(effective_limit(&cgroup).unwrap(), Some(2147483648));
        fs::remove_file(own.join("memory.max")).unwrap();
        assert!(effective_limit(&cgroup).is_err());
    }

    #[test]
    fn process_discovery_fails_closed_on_absence_ambiguity_and_traversal_budget() {
        let root = process_fixture();
        fs::write(root.path().join("cgroup.procs"), "1\n").unwrap();
        let own = group(root.path(), "workload", 42, "1073741824\n");
        assert!(discover_process(root.path(), 99).is_err());
        let duplicate = group(root.path(), "duplicate", 42, "4294967296\n");
        assert!(discover_process(root.path(), 42).is_err());
        fs::remove_dir_all(duplicate).unwrap();
        fs::write(own.join("cgroup.procs"), "1\n").unwrap();
        let mut deep = own;
        for index in 0..9 {
            deep = group(&deep, &format!("level{index}"), 1, "max\n");
        }
        fs::write(deep.join("cgroup.procs"), "42\n").unwrap();
        assert!(discover_process(root.path(), 42).is_err());
    }

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
