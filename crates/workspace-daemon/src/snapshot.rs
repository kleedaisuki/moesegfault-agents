//! Quiescent-workspace checkpoints. Stop all workspace processes before calling create.
//! Capability-relative reads contain raced path changes; they cannot make a live tree atomic.
use anyhow::{Context, Result, ensure};
use cap_std::{ambient_authority, fs::Dir};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const MANIFEST: &str = ".magents-manifest.json";
pub const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
const MANIFEST_LIMIT: u64 = 16 * 1024 * 1024;
const EXTENSION_LIMIT: u64 = 64 * 1024;
const MAX_MTIME: u64 = 253_402_300_799;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotMetadata {
    pub format_version: u32,
    pub sha256: String,
    pub logical_bytes: u64,
    pub archive_bytes: u64,
    pub entries: u64,
}

pub struct SnapshotArtifact {
    pub path: PathBuf,
    pub metadata: SnapshotMetadata,
}
impl Drop for SnapshotArtifact {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
pub struct SnapshotStore {
    root: PathBuf,
    max_bytes: u64,
    max_entries: u64,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Record {
    size: u64,
    sha256: String,
    directory: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    fifo: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    entries: BTreeMap<String, Record>,
}

impl SnapshotStore {
    fn archive_limit(&self) -> u64 {
        self.max_bytes.min(MAX_ARCHIVE_BYTES)
    }
    pub fn incoming(&self) -> Result<tempfile::NamedTempFile> {
        Ok(tempfile::NamedTempFile::new_in(self.root.join("archives"))?)
    }

    pub fn open(root: PathBuf, max_bytes: u64, max_entries: u64) -> Result<Self> {
        ensure!(
            max_bytes > 0 && max_entries > 0,
            "snapshot limits must be positive"
        );
        fs::create_dir_all(root.join("generations"))?;
        fs::create_dir_all(root.join("archives"))?;
        set_mode(&root, 0o711)?;
        set_mode(&root.join("generations"), 0o711)?;
        set_mode(&root.join("archives"), 0o700)?;
        let store = Self {
            root,
            max_bytes,
            max_entries,
        };
        if !store.root.join("CURRENT").exists() {
            let id = Uuid::new_v4().to_string();
            fs::create_dir(store.root.join("generations").join(&id))?;
            sync_dir(&store.root.join("generations").join(&id))?;
            sync_dir(&store.root.join("generations"))?;
            store.publish(&id)?;
        }
        store.current()?;
        Ok(store)
    }

    pub fn current(&self) -> Result<PathBuf> {
        let id = fs::read_to_string(self.root.join("CURRENT"))?;
        ensure!(
            Uuid::parse_str(&id)?.to_string() == id,
            "invalid CURRENT generation"
        );
        let path = self.root.join("generations").join(id);
        ensure!(
            fs::symlink_metadata(&path)?.file_type().is_dir(),
            "CURRENT is not a directory"
        );
        Ok(path)
    }

    #[allow(dead_code)] // Compatibility wrapper for callers that only need workspace usage.
    pub fn logical_bytes(&self) -> Result<u64> {
        Ok(self.fingerprint()?.0)
    }

    /// Metadata-only dirty detection for a quiescent tree; never reads file content.
    /// Linux ctime detects writes even when a user restores the original mtime.
    pub fn fingerprint(&self) -> Result<(u64, String)> {
        let dir = Dir::open_ambient_dir(self.current()?, ambient_authority())?;
        let mut entries = 0;
        let mut bytes = 0;
        let mut hash = Sha256::new();
        hash.update(b"magents-workspace-metadata-v1\0");
        hash_metadata(&mut hash, &dir.dir_metadata()?)?;
        self.measure(&dir, Path::new(""), &mut entries, &mut bytes, &mut hash)?;
        Ok((bytes, hex::encode(hash.finalize())))
    }

    fn measure(
        &self,
        dir: &Dir,
        prefix: &Path,
        entries: &mut u64,
        bytes: &mut u64,
        hash: &mut Sha256,
    ) -> Result<()> {
        let mut names = Vec::new();
        for entry in dir.entries()? {
            *entries = entries.checked_add(1).context("snapshot entry overflow")?;
            ensure!(
                *entries <= self.max_entries,
                "snapshot entry limit exceeded"
            );
            names.push(entry?.file_name());
        }
        names.sort();
        for name in names {
            let path = prefix.join(&name);
            validate_path(&path)?;
            let md = dir.symlink_metadata(&name)?;
            if is_socket(&md.file_type()) {
                continue;
            }
            ensure!(
                md.is_dir()
                    || md.is_file()
                    || md.file_type().is_symlink()
                    || is_fifo(&md.file_type()),
                "unsupported workspace entry: {}",
                path.display()
            );
            hash_field(hash, path_key(&path).as_bytes());
            hash_metadata(hash, &md)?;
            if is_fifo(&md.file_type()) {
                continue;
            }
            if md.file_type().is_symlink() {
                let target = dir.read_link_contents(&name)?;
                validate_target(&path, &target)?;
                hash_field(hash, path_key(&target).as_bytes());
                continue;
            }
            if md.is_dir() {
                self.measure(&dir.open_dir(&name)?, &path, entries, bytes, hash)?;
                continue;
            }
            reject_cap_hardlinks(&md)?;
            *bytes = bytes
                .checked_add(md.len())
                .context("snapshot size overflow")?;
            ensure!(
                *bytes <= self.max_bytes,
                "snapshot expanded byte limit exceeded"
            );
        }
        Ok(())
    }
    pub fn create(&self) -> Result<SnapshotArtifact> {
        let dir = Dir::open_ambient_dir(self.current()?, ambient_authority())?;
        let mut temp = tempfile::NamedTempFile::new_in(self.root.join("archives"))?;
        // R2 independently verifies the later native file stream before its
        // pointer can publish; do not reread the archive just to derive this hash.
        let encoder = zstd::stream::write::Encoder::new(
            DigestWriter::new(LimitedWriter {
                inner: temp.as_file_mut(),
                left: self.archive_limit(),
            }),
            3,
        )?;
        let mut tar = tar::Builder::new(encoder);
        let mut records = BTreeMap::new();
        let mut bytes = 0;
        self.pack(&dir, Path::new(""), &mut tar, &mut records, &mut bytes)?;
        validate_links(&records)?;
        let entries = records.len() as u64;
        let manifest = serde_json::to_vec(&Manifest {
            version: 1,
            entries: records,
        })?;
        ensure!(
            manifest.len() as u64 <= MANIFEST_LIMIT,
            "snapshot manifest too large"
        );
        let mut header = header(false, manifest.len() as u64, 0o600);
        tar.append_data(&mut header, MANIFEST, manifest.as_slice())?;
        let mut writer = tar.into_inner()?.finish()?;
        writer.flush()?;
        let sha256 = writer.finish();
        temp.as_file().sync_all()?;
        let archive_bytes = temp.as_file().metadata()?.len();
        let metadata = SnapshotMetadata {
            format_version: 1,
            sha256,
            logical_bytes: bytes,
            archive_bytes,
            entries,
        };
        let path = self
            .root
            .join("archives")
            .join(format!("{}.tar.zst", Uuid::new_v4()));
        temp.persist(&path)?;
        sync_dir(&self.root.join("archives"))?;
        Ok(SnapshotArtifact { path, metadata })
    }

    fn pack<W: Write>(
        &self,
        dir: &Dir,
        prefix: &Path,
        tar: &mut tar::Builder<W>,
        records: &mut BTreeMap<String, Record>,
        bytes: &mut u64,
    ) -> Result<()> {
        for entry in dir.entries()? {
            let entry = entry?;
            let name = entry.file_name();
            let path = prefix.join(&name);
            validate_path(&path)?;
            ensure!(
                (records.len() as u64) < self.max_entries,
                "snapshot entry limit exceeded"
            );
            let md = dir.symlink_metadata(&name)?;
            if is_socket(&md.file_type()) {
                continue;
            }
            ensure!(
                md.is_dir()
                    || md.is_file()
                    || md.file_type().is_symlink()
                    || is_fifo(&md.file_type()),
                "unsupported workspace entry: {}",
                path.display()
            );
            let archive_path = Path::new("data").join(&path);
            let key = path_key(&path);
            if is_fifo(&md.file_type()) {
                let mut pipe = header(false, 0, mode(&md.permissions()));
                pipe.set_entry_type(tar::EntryType::Fifo);
                pipe.set_cksum();
                tar.append_data(&mut pipe, &archive_path, io::empty())?;
                records.insert(
                    key,
                    Record {
                        size: 0,
                        sha256: hex::encode(Sha256::digest([])),
                        directory: false,
                        fifo: true,
                        target: None,
                    },
                );
                continue;
            }
            if md.file_type().is_symlink() {
                let target = dir.read_link_contents(&name)?;
                validate_target(&path, &target)?;
                let mut link_header = header(false, 0, 0o777);
                link_header.set_entry_type(tar::EntryType::Symlink);
                tar.append_link(&mut link_header, &archive_path, &target)?;
                records.insert(key, link_record(&target));
                continue;
            }
            if md.is_dir() {
                let child = dir.open_dir(&name)?;
                let metadata = child.dir_metadata()?;
                tar.append_data(
                    &mut timed_header(
                        true,
                        0,
                        mode(&metadata.permissions()),
                        metadata.modified()?.into_std(),
                    ),
                    &archive_path,
                    io::empty(),
                )?;
                records.insert(
                    key,
                    Record {
                        size: 0,
                        sha256: hex::encode(Sha256::digest([])),
                        directory: true,
                        fifo: false,
                        target: None,
                    },
                );
                self.pack(&child, &path, tar, records, bytes)?;
                continue;
            }
            let mut file = dir.open(&name)?.into_std();
            let opened = file.metadata()?;
            ensure!(
                opened.is_file(),
                "workspace entry changed type: {}",
                path.display()
            );
            reject_hardlinks(&opened)?;
            *bytes = bytes
                .checked_add(opened.len())
                .context("snapshot size overflow")?;
            ensure!(
                *bytes <= self.max_bytes,
                "snapshot expanded byte limit exceeded"
            );
            let mut reader = HashReader {
                inner: (&mut file).take(opened.len()),
                hash: Sha256::new(),
                bytes: 0,
            };
            tar.append_data(
                &mut timed_header(false, opened.len(), std_mode(&opened), opened.modified()?),
                &archive_path,
                &mut reader,
            )?;
            ensure!(
                reader.bytes == opened.len(),
                "workspace file truncated during checkpoint"
            );
            let hash = hex::encode(reader.hash.finalize());
            ensure!(
                file.read(&mut [0])? == 0 && file.metadata()?.len() == opened.len(),
                "workspace file changed during checkpoint"
            );
            records.insert(
                key,
                Record {
                    size: opened.len(),
                    sha256: hash,
                    directory: false,
                    fifo: false,
                    target: None,
                },
            );
        }
        Ok(())
    }

    pub fn restore(&self, archive_path: &Path, expected_sha256: &str) -> Result<SnapshotMetadata> {
        // Copy first: validation and extraction must consume the same immutable private file.
        let mut private = tempfile::NamedTempFile::new_in(self.root.join("archives"))?;
        let mut source = File::open(archive_path)?.take(self.archive_limit().saturating_add(1));
        let archive_bytes = io::copy(&mut source, private.as_file_mut())?;
        ensure!(
            archive_bytes <= self.archive_limit(),
            "snapshot compressed byte limit exceeded"
        );
        ensure!(
            hash_file(private.path())? == expected_sha256,
            "snapshot archive SHA-256 mismatch"
        );
        let staging = tempfile::Builder::new()
            .prefix(".restore-")
            .tempdir_in(self.root.join("generations"))?;
        let mut decoder = zstd::stream::read::Decoder::new(File::open(private.path())?)?;
        decoder.window_log_max(23)?;
        // Bound tar extension headers too, before the tar parser allocates their payload.
        let expanded_limit = self
            .max_bytes
            .saturating_add(self.max_entries.saturating_mul(8192))
            .saturating_add(MANIFEST_LIMIT);
        let guarded = TarGuard::new(
            decoder.take(expanded_limit),
            self.max_bytes,
            self.max_entries,
        );
        let mut archive = tar::Archive::new(guarded);
        let mut records = BTreeMap::new();
        let mut modes = Vec::new();
        let mut links = Vec::new();
        let mut manifest = None;
        let mut bytes = 0u64;
        for entry in archive.entries()? {
            let mut entry = entry?;
            ensure!(manifest.is_none(), "manifest must be final archive entry");
            let path = entry.path()?.into_owned();
            let kind = entry.header().entry_type();
            ensure!(
                kind.is_file() || kind.is_dir() || kind.is_symlink() || kind.is_fifo(),
                "unsupported archive entry: {}",
                path.display()
            );
            if path == Path::new(MANIFEST) {
                ensure!(
                    kind.is_file() && entry.size() <= MANIFEST_LIMIT,
                    "invalid manifest"
                );
                let mut data = Vec::new();
                entry.read_to_end(&mut data)?;
                manifest = Some(serde_json::from_slice::<Manifest>(&data)?);
                continue;
            }
            let relative = path.strip_prefix("data").context("entry outside data/")?;
            validate_path(relative)?;
            ensure!(
                (records.len() as u64) < self.max_entries,
                "snapshot entry limit exceeded"
            );
            let key = path_key(relative);
            ensure!(!records.contains_key(&key), "duplicate archive entry");
            let size = entry.size();
            let modified = restored_mtime(entry.header().mtime()?)?;
            if kind.is_symlink() {
                ensure!(size == 0, "symlink has file payload");
                let target = entry
                    .link_name()?
                    .context("symlink target missing")?
                    .into_owned();
                validate_target(relative, &target)?;
                records.insert(key, link_record(&target));
                links.push((relative.to_owned(), target));
                continue;
            }
            ensure!(
                !(kind.is_dir() || kind.is_fifo()) || size == 0,
                "non-file entry has file payload"
            );
            bytes = bytes.checked_add(size).context("snapshot size overflow")?;
            ensure!(
                bytes <= self.max_bytes,
                "snapshot expanded byte limit exceeded"
            );
            let destination = staging.path().join(relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let digest = if kind.is_fifo() {
                create_fifo(&destination, entry.header().mode()? & 0o777)?;
                hex::encode(Sha256::digest([]))
            } else if kind.is_dir() {
                fs::create_dir_all(&destination)?;
                hex::encode(Sha256::digest([]))
            } else {
                let mut file = File::options()
                    .write(true)
                    .create_new(true)
                    .open(&destination)?;
                let mut reader = HashReader {
                    inner: &mut entry,
                    hash: Sha256::new(),
                    bytes: 0,
                };
                io::copy(&mut reader, &mut file)?;
                ensure!(reader.bytes == size, "truncated snapshot entry");
                file.sync_all()?;
                hex::encode(reader.hash.finalize())
            };
            modes.push((
                destination,
                entry.header().mode()? & 0o777,
                modified,
                !kind.is_fifo(),
            ));
            records.insert(
                key,
                Record {
                    size,
                    sha256: digest,
                    directory: kind.is_dir(),
                    fifo: kind.is_fifo(),
                    target: None,
                },
            );
        }
        let manifest = manifest.context("snapshot manifest missing")?;
        ensure!(
            manifest.version == 1 && manifest.entries == records,
            "snapshot manifest does not match archive"
        );
        validate_links(&records)?;
        for (relative, target) in links {
            let destination = staging.path().join(relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            create_symlink(&target, &destination)?;
        }
        let entries = records.len() as u64;
        prepare_tree(staging.path())?;
        // Apply restrictive directory modes only after descendants are complete.
        modes.sort_by_key(|(path, _, _, _)| std::cmp::Reverse(path.components().count()));
        for (path, permissions, modified, syncable) in modes {
            let file = if syncable && (cfg!(unix) || path.is_file()) {
                Some(sync_file(&path)?)
            } else {
                None
            };
            set_mode(&path, permissions)?;
            if let Some(file) = file {
                file.set_modified(modified)?;
                file.sync_all()?;
            }
        }
        let id = Uuid::new_v4().to_string();
        fs::rename(staging.path(), self.root.join("generations").join(&id))?;
        sync_dir(&self.root.join("generations"))?;
        self.publish(&id)?;
        Ok(SnapshotMetadata {
            format_version: 1,
            sha256: expected_sha256.to_owned(),
            logical_bytes: bytes,
            archive_bytes,
            entries,
        })
    }

    fn publish(&self, id: &str) -> Result<()> {
        let mut temp = tempfile::NamedTempFile::new_in(&self.root)?;
        temp.write_all(id.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist(self.root.join("CURRENT"))?;
        sync_dir(&self.root)
    }
}

fn is_false(value: &bool) -> bool {
    !value
}
fn is_fifo(kind: &cap_std::fs::FileType) -> bool {
    #[cfg(unix)]
    {
        use cap_std::fs::FileTypeExt;
        kind.is_fifo()
    }
    #[cfg(not(unix))]
    {
        let _ = kind;
        false
    }
}
fn is_socket(kind: &cap_std::fs::FileType) -> bool {
    #[cfg(unix)]
    {
        use cap_std::fs::FileTypeExt;
        kind.is_socket()
    }
    #[cfg(not(unix))]
    {
        let _ = kind;
        false
    }
}
fn create_fifo(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        ensure!(
            unsafe { libc::mkfifo(path.as_ptr(), mode & 0o777) } == 0,
            "cannot restore FIFO"
        );
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        anyhow::bail!("restoring FIFO requires Unix")
    }
}
fn hash_field(hash: &mut Sha256, value: &[u8]) {
    hash.update((value.len() as u64).to_le_bytes());
    hash.update(value);
}
fn hash_metadata(hash: &mut Sha256, metadata: &cap_std::fs::Metadata) -> Result<()> {
    let kind = if metadata.is_dir() {
        b'd'
    } else if metadata.is_file() {
        b'f'
    } else if is_fifo(&metadata.file_type()) {
        b'p'
    } else {
        b'l'
    };
    hash.update([kind]);
    hash.update(metadata.len().to_le_bytes());
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt;
        hash.update(metadata.dev().to_le_bytes());
        hash.update(metadata.ino().to_le_bytes());
        hash.update(metadata.mode().to_le_bytes());
        hash.update(metadata.ctime().to_le_bytes());
        hash.update(metadata.ctime_nsec().to_le_bytes());
        hash.update(metadata.mtime().to_le_bytes());
        hash.update(metadata.mtime_nsec().to_le_bytes());
    }
    #[cfg(not(unix))]
    {
        hash.update([u8::from(metadata.permissions().readonly())]);
        let modified = metadata
            .modified()?
            .into_std()
            .duration_since(std::time::UNIX_EPOCH);
        let (before_epoch, duration) = match modified {
            Ok(duration) => (false, duration),
            Err(error) => (true, error.duration()),
        };
        hash.update([u8::from(before_epoch)]);
        hash.update(duration.as_nanos().to_le_bytes());
    }
    Ok(())
}
#[cfg(unix)]
fn reject_cap_hardlinks(metadata: &cap_std::fs::Metadata) -> Result<()> {
    use cap_std::fs::MetadataExt;
    validate_hardlink_owner(metadata.nlink(), Some(metadata.uid()))
}
#[cfg(not(unix))]
fn reject_cap_hardlinks(_: &cap_std::fs::Metadata) -> Result<()> {
    unavailable_hardlink_policy()
}
#[cfg_attr(not(unix), allow(dead_code))]
fn validate_hardlink_owner(links: u64, owner: Option<u32>) -> Result<()> {
    ensure!(
        links <= 1 || owner == Some(1000),
        "hard-linked files must belong to workspace UID 1000"
    );
    Ok(())
}
#[cfg(not(unix))]
fn unavailable_hardlink_policy() -> Result<()> {
    // Windows smoke fixtures are trusted. Production snapshots require the
    // Unix ownership check and daemon execution is Linux-only.
    #[cfg(test)]
    {
        Ok(())
    }
    #[cfg(not(test))]
    {
        anyhow::bail!("workspace snapshots require Unix ownership metadata")
    }
}
fn link_record(target: &Path) -> Record {
    let target = path_key(target);
    Record {
        size: 0,
        sha256: hex::encode(Sha256::digest(target.as_bytes())),
        directory: false,
        fifo: false,
        target: Some(target),
    }
}
fn validate_target(link: &Path, target: &Path) -> Result<()> {
    ensure!(!target.as_os_str().is_empty(), "empty symlink target");
    let mut depth = link.parent().map_or(0, |p| p.components().count());
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => (),
            Component::ParentDir => {
                ensure!(
                    depth > 0,
                    "symlink target escapes workspace: {}",
                    link.display()
                );
                depth -= 1;
            }
            _ => anyhow::bail!("absolute symlink target is unsupported: {}", link.display()),
        }
        ensure!(depth <= 128, "symlink target exceeds maximum depth");
    }
    Ok(())
}
fn validate_links(records: &BTreeMap<String, Record>) -> Result<()> {
    use std::collections::{HashMap, VecDeque};
    let paths = records
        .iter()
        .map(|(path, record)| Ok((decode_path(path)?, record)))
        .collect::<Result<Vec<_>>>()?;
    let links = paths
        .iter()
        .filter_map(|(path, record)| {
            record
                .target
                .as_ref()
                .map(|target| Ok((path.clone(), decode_path(target)?)))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    for (path, _) in &paths {
        for parent in path.ancestors().skip(1) {
            ensure!(
                !links.contains_key(parent),
                "archive entry descends through a symlink: {}",
                path.display()
            );
        }
    }
    for (path, target) in &links {
        validate_target(path, target)?;
        let mut resolved = path.parent().unwrap_or(Path::new("")).to_owned();
        let mut pending =
            VecDeque::from_iter(target.components().map(|c| c.as_os_str().to_owned()));
        let mut expansions = 0;
        while let Some(part) = pending.pop_front() {
            if part == "." {
                continue;
            }
            if part == ".." {
                ensure!(
                    resolved.pop(),
                    "chained symlink escapes workspace: {}",
                    path.display()
                );
                continue;
            }
            resolved.push(part);
            if let Some(next) = links.get(&resolved) {
                expansions += 1;
                ensure!(
                    expansions <= 40,
                    "symlink cycle or excessive chain: {}",
                    path.display()
                );
                resolved.pop();
                for part in next.components().rev() {
                    pending.push_front(part.as_os_str().to_owned());
                }
            }
        }
    }
    Ok(())
}
#[cfg(unix)]
fn decode_path(value: &str) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Ok(std::ffi::OsString::from_vec(hex::decode(value)?).into())
}
#[cfg(not(unix))]
fn decode_path(value: &str) -> Result<PathBuf> {
    Ok(String::from_utf8(hex::decode(value)?)?.into())
}
#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}
#[cfg(not(unix))]
fn create_symlink(_: &Path, _: &Path) -> Result<()> {
    anyhow::bail!("restoring symlinks requires a Unix workspace host")
}
fn validate_path(path: &Path) -> Result<()> {
    let mut depth = 0;
    for component in path.components() {
        ensure!(
            matches!(component, Component::Normal(_)),
            "unsafe snapshot path: {}",
            path.display()
        );
        depth += 1;
    }
    ensure!(
        depth > 0 && depth <= 128,
        "snapshot path depth outside 1..=128"
    );
    Ok(())
}
fn header(directory: bool, size: u64, mode: u32) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(if directory {
        tar::EntryType::Directory
    } else {
        tar::EntryType::Regular
    });
    header.set_size(size);
    header.set_mode(mode & 0o777);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    header
}
fn timed_header(directory: bool, size: u64, mode: u32, modified: SystemTime) -> tar::Header {
    let mut header = header(directory, size, mode);
    // GNU tar stores integral seconds. Pre-epoch times use the portable epoch
    // fallback rather than signed/nonstandard encodings or PAX extensions.
    header.set_mtime(
        modified
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs().min(MAX_MTIME)),
    );
    header.set_cksum();
    header
}
fn restored_mtime(seconds: u64) -> Result<SystemTime> {
    ensure!(seconds <= MAX_MTIME, "snapshot mtime exceeds year 9999");
    UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds))
        .context("snapshot mtime is not representable")
}

#[cfg(test)]
std::thread_local! {
    static CHECKSUM_FILE_READ_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn hash_file(path: &Path) -> Result<String> {
    let mut reader = HashReader {
        inner: File::open(path)?,
        hash: Sha256::new(),
        bytes: 0,
    };
    io::copy(&mut reader, &mut io::sink())?;
    #[cfg(test)]
    CHECKSUM_FILE_READ_BYTES.with(|bytes| bytes.set(bytes.get().saturating_add(reader.bytes)));
    Ok(hex::encode(reader.hash.finalize()))
}
struct HashReader<R> {
    inner: R,
    hash: Sha256,
    bytes: u64,
}

struct DigestWriter<W> {
    inner: W,
    hash: Sha256,
}

impl<W> DigestWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hash: Sha256::new(),
        }
    }

    fn finish(self) -> String {
        hex::encode(self.hash.finalize())
    }
}

impl<W: Write> Write for DigestWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(data)?;
        self.hash.update(&data[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

// Validate framing before tar sees a header: its GNU longname parser allocates the
// declared payload before yielding an entry to our extraction checks.
struct TarGuard<R> {
    inner: R,
    block: [u8; 512],
    position: usize,
    payload: u64,
    headers: u64,
    max_headers: u64,
    max_bytes: u64,
    ended: bool,
}
impl<R: Read> TarGuard<R> {
    fn new(inner: R, max_bytes: u64, max_entries: u64) -> Self {
        Self {
            inner,
            block: [0; 512],
            position: 512,
            payload: 0,
            headers: 0,
            max_headers: max_entries.saturating_mul(3).saturating_add(1),
            max_bytes,
            ended: false,
        }
    }

    fn next_header(&mut self) -> io::Result<()> {
        self.inner.read_exact(&mut self.block)?;
        self.position = 0;
        if self.block.iter().all(|byte| *byte == 0) {
            self.ended = true;
            return Ok(());
        }
        self.headers = self.headers.saturating_add(1);
        if self.headers > self.max_headers {
            return Err(io::Error::other("snapshot raw header limit exceeded"));
        }
        let header = tar::Header::from_byte_slice(&self.block);
        let checksum = self
            .block
            .iter()
            .enumerate()
            .map(|(index, byte)| {
                if (148..156).contains(&index) {
                    u32::from(b' ')
                } else {
                    u32::from(*byte)
                }
            })
            .sum::<u32>();
        if header.cksum()? != checksum {
            return Err(io::Error::other("snapshot tar header checksum mismatch"));
        }
        let size = header.entry_size()?;
        match header.entry_type().as_byte() {
            b'L' | b'K' if size <= EXTENSION_LIMIT => (),
            b'L' | b'K' => return Err(io::Error::other("snapshot GNU extension exceeds 64 KiB")),
            b'0' | 0 if size <= self.max_bytes.max(MANIFEST_LIMIT) => (),
            b'2' | b'5' | b'6' if size == 0 => (),
            b'x' | b'g' => return Err(io::Error::other("snapshot PAX extensions are unsupported")),
            _ => {
                return Err(io::Error::other(
                    "unsupported snapshot tar type or entry size",
                ));
            }
        }
        self.payload = size
            .checked_add(511)
            .and_then(|n| n.checked_div(512))
            .and_then(|n| n.checked_mul(512))
            .ok_or_else(|| io::Error::other("snapshot tar payload overflow"))?;
        Ok(())
    }
}
impl<R: Read> Read for TarGuard<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.position < 512 {
            let n = out.len().min(512 - self.position);
            out[..n].copy_from_slice(&self.block[self.position..self.position + n]);
            self.position += n;
            return Ok(n);
        }
        if self.payload != 0 {
            let limit = self.payload.min(out.len() as u64) as usize;
            let n = self.inner.read(&mut out[..limit])?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated snapshot tar payload",
                ));
            }
            self.payload -= n as u64;
            return Ok(n);
        }
        if self.ended {
            return Ok(0);
        }
        self.next_header()?;
        self.read(out)
    }
}
impl<R: Read> Read for HashReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(out)?;
        self.hash.update(&out[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
}
struct LimitedWriter<W> {
    inner: W,
    left: u64,
}
impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.len() as u64 > self.left {
            return Err(io::Error::other("snapshot compressed byte limit exceeded"));
        }
        let n = self.inner.write(data)?;
        self.left -= n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
#[cfg(unix)]
fn path_key(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    hex::encode(path.as_os_str().as_bytes())
}
#[cfg(not(unix))]
fn path_key(path: &Path) -> String {
    hex::encode(path.to_string_lossy().replace('\\', "/").as_bytes())
}
#[cfg(unix)]
fn reject_hardlinks(md: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    validate_hardlink_owner(md.nlink(), Some(md.uid()))
}
#[cfg(not(unix))]
fn reject_hardlinks(_: &fs::Metadata) -> Result<()> {
    unavailable_hardlink_policy()
}
#[cfg(unix)]
fn std_mode(md: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode() & 0o777
}
#[cfg(not(unix))]
fn std_mode(_: &fs::Metadata) -> u32 {
    0o644
}
#[cfg(unix)]
fn mode(p: &cap_std::fs::Permissions) -> u32 {
    use cap_std::fs::PermissionsExt;
    p.mode() & 0o777
}
#[cfg(not(unix))]
fn mode(_: &cap_std::fs::Permissions) -> u32 {
    0o755
}
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}
#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) -> Result<()> {
    Ok(())
}
#[cfg(unix)]
fn sync_dir(path: &Path) -> Result<()> {
    sync_file(path)?.sync_all()?;
    Ok(())
}
#[cfg(not(unix))]
fn sync_dir(_: &Path) -> Result<()> {
    Ok(())
}
fn sync_file(path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    {
        File::options().read(true).write(true).open(path)
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        File::options()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }
}
fn prepare_tree(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            prepare_tree(&entry?.path())?;
        }
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        // Never follow restored links while changing ownership.
        if unsafe { libc::geteuid() } == 0
            && unsafe { libc::lchown(name.as_ptr(), 1000, 1000) } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        if metadata.is_file() || metadata.is_dir() {
            File::open(path)?.sync_all()?;
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        if !metadata.is_file() && !metadata.is_dir() {
            return Ok(());
        }
        if path.is_dir() {
            sync_dir(path)?;
        } else {
            sync_file(path)?.sync_all()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ShortWriter {
        bytes: Vec<u8>,
        limit: usize,
        fail: bool,
    }

    impl Write for ShortWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.fail {
                return Err(io::Error::other("injected write failure"));
            }
            let written = bytes.len().min(self.limit);
            self.bytes.extend_from_slice(&bytes[..written]);
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn compressed_digest_tracks_only_successful_writes_across_every_byte_value() {
        let bytes = (0u8..=255).cycle().take(8193).collect::<Vec<_>>();
        for limit in [1, 3, 17, 4096, bytes.len()] {
            for width in [1, 7, 512, bytes.len()] {
                let mut writer = DigestWriter::new(ShortWriter {
                    bytes: Vec::new(),
                    limit,
                    fail: false,
                });
                for part in bytes.chunks(width) {
                    writer.write_all(part).unwrap();
                }
                writer.flush().unwrap();
                assert_eq!(writer.inner.bytes, bytes);
                assert_eq!(writer.finish(), hex::encode(Sha256::digest(&bytes)));
            }
        }
        let mut writer = DigestWriter::new(ShortWriter {
            bytes: Vec::new(),
            limit: 3,
            fail: false,
        });
        assert_eq!(writer.write(b"accepted").unwrap(), 3);
        writer.inner.fail = true;
        assert!(writer.write(b"not accepted").is_err());
        assert_eq!(writer.finish(), hex::encode(Sha256::digest(b"acc")));
    }

    #[test]
    fn archive_digest_includes_zstd_finalization_and_matches_independent_readback() {
        let temp = crate::test_dir();
        let store = SnapshotStore::open(temp.path().to_owned(), 2 * 1024 * 1024, 8).unwrap();
        for bytes in [0, 1, 8193, 1024 * 1024] {
            let data = (0u8..=255).cycle().take(bytes).collect::<Vec<_>>();
            let file = store.current().unwrap().join("binary");
            fs::write(&file, &data).unwrap();
            CHECKSUM_FILE_READ_BYTES.with(|read| read.set(0));
            let artifact = store.create().unwrap();
            assert_eq!(
                CHECKSUM_FILE_READ_BYTES.with(std::cell::Cell::get),
                0,
                "Creating an archive must not reopen it for a checksum pass"
            );
            assert_eq!(artifact.metadata.sha256, hash_file(&artifact.path).unwrap());
            assert_eq!(artifact.metadata.entries, 1);
            assert_eq!(artifact.metadata.logical_bytes, bytes as u64);
            store
                .restore(&artifact.path, &artifact.metadata.sha256)
                .unwrap();
            assert_eq!(
                fs::read(store.current().unwrap().join("binary")).unwrap(),
                data
            );
        }
    }

    fn write_archive_copy(path: &Path, bytes: &[u8], streaming: bool) -> (String, u64, u128) {
        let started = std::time::Instant::now();
        let mut output = File::create(path).unwrap();
        let digest = if streaming {
            let mut writer = DigestWriter::new(&mut output);
            for chunk in bytes.chunks(128 * 1024) {
                writer.write_all(chunk).unwrap();
            }
            writer.flush().unwrap();
            Some(writer.finish())
        } else {
            for chunk in bytes.chunks(128 * 1024) {
                output.write_all(chunk).unwrap();
            }
            output.flush().unwrap();
            None
        };
        output.sync_all().unwrap();
        let (digest, read_bytes) = match digest {
            Some(digest) => (digest, 0),
            None => {
                let mut reader = HashReader {
                    inner: File::open(path).unwrap(),
                    hash: Sha256::new(),
                    bytes: 0,
                };
                io::copy(&mut reader, &mut io::sink()).unwrap();
                (hex::encode(reader.hash.finalize()), reader.bytes)
            }
        };
        (digest, read_bytes, started.elapsed().as_nanos())
    }

    #[test]
    fn checkpoint_digest_and_manifest_cost_receipt() {
        let temp = crate::test_dir();
        let store =
            SnapshotStore::open(temp.path().join("workspace"), 16 * 1024 * 1024, 8).unwrap();
        let mut seed = 0x1234_5678_9abc_def0_u64;
        let data = (0..8 * 1024 * 1024)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect::<Vec<_>>();
        fs::write(store.current().unwrap().join("artifact"), &data).unwrap();
        CHECKSUM_FILE_READ_BYTES.with(|read| read.set(0));
        let artifact = store.create().unwrap();
        let production_reads = CHECKSUM_FILE_READ_BYTES.with(std::cell::Cell::get);
        assert_eq!(production_reads, 0);
        let compressed = fs::read(&artifact.path).unwrap();
        let samples = 5;
        let mut baseline_ns = Vec::new();
        let mut streaming_ns = Vec::new();
        let mut baseline_reads = 0;
        let mut streaming_reads = 0;
        // Both paths use the same hot compressed bytes and syscall chunk size;
        // alternating order avoids crediting either path for first-use setup.
        for n in 0..samples {
            for streaming in if n % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let path = temp.path().join(format!("copy-{n}-{streaming}"));
                let (digest, reads, nanos) = write_archive_copy(&path, &compressed, streaming);
                assert_eq!(digest, artifact.metadata.sha256);
                if streaming {
                    streaming_reads += reads;
                    streaming_ns.push(nanos);
                } else {
                    baseline_reads += reads;
                    baseline_ns.push(nanos);
                }
            }
        }
        assert_eq!(baseline_reads, compressed.len() as u64 * samples);
        assert_eq!(streaming_reads, 0);

        let records = (0..10_000)
            .map(|n| {
                (
                    format!("{n:08x}"),
                    Record {
                        size: 1,
                        sha256: "0".repeat(64),
                        directory: false,
                        fifo: false,
                        target: None,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let count = records.len();
        let manifest = Manifest {
            version: 1,
            entries: records,
        };
        let encoded = serde_json::to_vec(&manifest).unwrap();
        let mut parse_ns = Vec::new();
        let mut count_ns = Vec::new();
        for _ in 0..samples {
            let started = std::time::Instant::now();
            let restored: Manifest =
                serde_json::from_slice(std::hint::black_box(&encoded)).unwrap();
            assert_eq!(restored.entries.len(), count);
            parse_ns.push(started.elapsed().as_nanos());
            let started = std::time::Instant::now();
            assert_eq!(std::hint::black_box(&manifest).entries.len(), count);
            count_ns.push(started.elapsed().as_nanos());
        }
        let output = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.cache/perf/v3");
        fs::create_dir_all(&output).unwrap();
        let receipt = serde_json::json!({
            "schema_version":1,
            "fixture":"same compressed checkpoint, 128KiB writes, fsync, warm page cache, alternating order",
            "source_sha":std::env::var("GITHUB_SHA").ok(),
            "debug_assertions":cfg!(debug_assertions),
            "samples":samples,
            "logical_bytes":artifact.metadata.logical_bytes,
            "archive_bytes":compressed.len(),
            "production_create_checksum_read_bytes":production_reads,
            "baseline":{"post_write_read_bytes":baseline_reads,"elapsed_ns":baseline_ns},
            "streaming":{"post_write_read_bytes":streaming_reads,"elapsed_ns":streaming_ns},
            "manifest":{"entries":count,"serialized_bytes":encoded.len(),"baseline_parse_ns":parse_ns,"direct_count_ns":count_ns},
            "equivalent_sha256":true,
            "scope":"archive write/fsync/digest stage; excludes packing, compression, HTTP/R2 transfer and end-to-end latency",
        });
        fs::write(
            output.join("snapshot-io.json"),
            serde_json::to_vec_pretty(&receipt).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn created_archives_always_fit_the_restore_transport_limit() {
        let temp = crate::test_dir();
        for logical_limit in [1024, MAX_ARCHIVE_BYTES, MAX_ARCHIVE_BYTES * 2] {
            let store = SnapshotStore::open(temp.path().to_owned(), logical_limit, 1).unwrap();
            assert_eq!(store.archive_limit(), logical_limit.min(MAX_ARCHIVE_BYTES));
        }
    }

    #[test]
    fn hardlinks_require_the_dedicated_workspace_owner() {
        assert!(validate_hardlink_owner(2, Some(1000)).is_ok());
        assert!(validate_hardlink_owner(2, Some(0)).is_err());
        assert!(validate_hardlink_owner(2, Some(1001)).is_err());
        assert!(validate_hardlink_owner(2, None).is_err());
        assert!(validate_hardlink_owner(1, None).is_ok());
    }

    #[test]
    fn file_and_directory_mtime_round_trip_at_second_precision() {
        let temp = crate::test_dir();
        let store = store(temp.path());
        let original = store.current().unwrap();
        let nested = original.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("file"), b"build artifact").unwrap();
        let file_time = UNIX_EPOCH + Duration::from_secs(1_700_000_123);
        let dir_time = UNIX_EPOCH + Duration::from_secs(1_600_000_456);
        sync_file(&nested.join("file"))
            .unwrap()
            .set_modified(file_time)
            .unwrap();
        #[cfg(unix)]
        sync_file(&nested).unwrap().set_modified(dir_time).unwrap();
        let snapshot = store.create().unwrap();
        store
            .restore(&snapshot.path, &snapshot.metadata.sha256)
            .unwrap();
        let restored = store.current().unwrap().join("nested");
        assert_eq!(
            fs::metadata(restored.join("file"))
                .unwrap()
                .modified()
                .unwrap(),
            file_time
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&restored).unwrap().modified().unwrap(),
            dir_time
        );
        #[cfg(not(unix))]
        let _ = dir_time;
    }

    #[test]
    fn archive_timestamps_have_checked_portable_bounds() {
        assert!(restored_mtime(u64::MAX).is_err());
        assert_eq!(restored_mtime(0).unwrap(), UNIX_EPOCH);
        assert_eq!(
            timed_header(false, 0, 0o600, UNIX_EPOCH - Duration::from_secs(1))
                .mtime()
                .unwrap(),
            0
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fifo_round_trips_without_opening_it_and_sockets_are_ephemeral() {
        use std::os::fd::AsRawFd;
        use std::os::unix::{
            fs::{FileTypeExt, PermissionsExt},
            net::UnixListener,
        };
        let temp = crate::test_dir();
        let store = store(temp.path());
        let original = store.current().unwrap();
        create_fifo(&original.join("pipe"), 0o640).unwrap();
        set_mode(&original.join("pipe"), 0o640).unwrap();
        fs::write(original.join("data"), b"persisted").unwrap();
        // A workspace path may exceed sockaddr_un's 108-byte limit; binding
        // relative to an open directory keeps the fixture inside the repo.
        let directory = File::open(&original).unwrap();
        let socket_path = format!("/proc/self/fd/{}/ipc.sock", directory.as_raw_fd());
        let _socket = UnixListener::bind(socket_path).unwrap();
        let snapshot = store.create().unwrap();
        assert_eq!(snapshot.metadata.entries, 2);
        store
            .restore(&snapshot.path, &snapshot.metadata.sha256)
            .unwrap();
        let restored = store.current().unwrap();
        let metadata = fs::symlink_metadata(restored.join("pipe")).unwrap();
        assert!(metadata.file_type().is_fifo());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o640);
        assert!(!restored.join("ipc.sock").exists());
        assert_eq!(fs::read(restored.join("data")).unwrap(), b"persisted");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn workspace_owned_hardlinks_materialize_as_independent_files() {
        use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
        let temp = crate::test_dir();
        let store = store(temp.path());
        let root = store.current().unwrap();
        fs::write(root.join("first"), b"shared bytes").unwrap();
        fs::hard_link(root.join("first"), root.join("second")).unwrap();
        if unsafe { libc::geteuid() } == 0 {
            // Root-owned linked inodes must never be read with supervisor rights.
            assert!(
                store
                    .create()
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("UID 1000")
            );
            let path = std::ffi::CString::new(root.join("first").as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::chown(path.as_ptr(), 1000, 1000) }, 0);
        }
        if fs::metadata(root.join("first")).unwrap().uid() != 1000 {
            assert!(store.create().is_err());
            return;
        }
        let artifact = store.create().unwrap();
        store
            .restore(&artifact.path, &artifact.metadata.sha256)
            .unwrap();
        let restored = store.current().unwrap();
        assert_eq!(fs::read(restored.join("first")).unwrap(), b"shared bytes");
        assert_eq!(fs::read(restored.join("second")).unwrap(), b"shared bytes");
        assert_ne!(
            fs::metadata(restored.join("first")).unwrap().ino(),
            fs::metadata(restored.join("second")).unwrap().ino()
        );
        assert_eq!(fs::metadata(restored.join("first")).unwrap().uid(), 1000);
        assert_eq!(fs::metadata(restored.join("second")).unwrap().uid(), 1000);
    }
    fn store(root: &Path) -> SnapshotStore {
        SnapshotStore::open(root.to_owned(), 1024 * 1024, 100).unwrap()
    }
    #[test]
    fn round_trip_and_reopen() {
        let tmp = crate::test_dir();
        let store = store(tmp.path());
        let original = store.current().unwrap();
        fs::create_dir(original.join("nested")).unwrap();
        fs::write(original.join("nested/a"), b"hello").unwrap();
        let snapshot = store.create().unwrap();
        fs::write(original.join("nested/a"), b"modified").unwrap();
        let metadata = store
            .restore(&snapshot.path, &snapshot.metadata.sha256)
            .unwrap();
        assert_eq!(metadata, snapshot.metadata);
        assert_eq!(
            fs::read(store.current().unwrap().join("nested/a")).unwrap(),
            b"hello"
        );
        assert_eq!(fs::read(original.join("nested/a")).unwrap(), b"modified");
        assert_eq!(
            self::store(tmp.path()).current().unwrap(),
            store.current().unwrap()
        );
    }
    #[test]
    fn wrong_hash_does_not_switch_current() {
        let tmp = crate::test_dir();
        let store = store(tmp.path());
        let old = store.current().unwrap();
        let snapshot = store.create().unwrap();
        assert!(store.restore(&snapshot.path, "bad").is_err());
        assert_eq!(old, store.current().unwrap());
    }
    #[test]
    fn quota_failure_leaves_no_artifact() {
        let tmp = crate::test_dir();
        let store = SnapshotStore::open(tmp.path().to_owned(), 1024, 1).unwrap();
        fs::write(store.current().unwrap().join("a"), vec![0; 1025]).unwrap();
        assert!(store.create().is_err());
        assert_eq!(
            fs::read_dir(tmp.path().join("archives")).unwrap().count(),
            0
        );
    }

    #[test]
    fn forged_manifest_and_link_preserve_current() {
        let tmp = crate::test_dir();
        let store = store(tmp.path());
        let old = store.current().unwrap();
        for link in [false, true] {
            let path = tmp.path().join("forged.zst");
            let encoder =
                zstd::stream::write::Encoder::new(File::create(&path).unwrap(), 1).unwrap();
            let mut tar = tar::Builder::new(encoder);
            let mut entry = header(false, 1, 0o644);
            if link {
                entry.set_entry_type(tar::EntryType::Symlink);
                entry.set_size(0);
                entry.set_link_name("/etc/passwd").unwrap();
                tar.append_data(&mut entry, "data/link", io::empty())
                    .unwrap();
            } else {
                tar.append_data(&mut entry, "data/a", &b"x"[..]).unwrap();
                let manifest = br#"{"version":1,"entries":{}}"#;
                tar.append_data(
                    &mut header(false, manifest.len() as u64, 0o600),
                    MANIFEST,
                    &manifest[..],
                )
                .unwrap();
            }
            tar.into_inner()
                .unwrap()
                .finish()
                .unwrap()
                .sync_all()
                .unwrap();
            assert!(store.restore(&path, &hash_file(&path).unwrap()).is_err());
            assert_eq!(store.current().unwrap(), old);
        }
    }

    #[test]
    fn measurement_obeys_limits() {
        let tmp = crate::test_dir();
        let store = store(tmp.path());
        fs::write(store.current().unwrap().join("a"), b"12345").unwrap();
        assert_eq!(store.logical_bytes().unwrap(), 5);
    }

    #[test]
    fn fingerprints_are_stable_and_detect_same_size_writes() {
        let tmp = crate::test_dir();
        let store = store(tmp.path());
        let file = store.current().unwrap().join("content");
        for size in [1, 17, 128] {
            fs::write(&file, vec![b'a'; size]).unwrap();
            let before = store.fingerprint().unwrap();
            for _ in 0..5 {
                assert_eq!(store.fingerprint().unwrap(), before);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            fs::write(&file, vec![b'b'; size]).unwrap();
            let after = store.fingerprint().unwrap();
            assert_eq!(before.0, after.0);
            assert_ne!(before.1, after.1);
        }
    }

    #[test]
    fn empty_file_churn_changes_fingerprint_without_changing_bytes() {
        let tmp = crate::test_dir();
        let store = store(tmp.path());
        let before = store.fingerprint().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let empty = store.current().unwrap().join("empty");
        fs::write(&empty, []).unwrap();
        let added = store.fingerprint().unwrap();
        assert_eq!(before.0, added.0);
        assert_ne!(before.1, added.1);
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::remove_file(empty).unwrap();
        let deleted = store.fingerprint().unwrap();
        assert_eq!(before.0, deleted.0);
        assert_ne!(before.1, deleted.1);
        assert_ne!(added.1, deleted.1);
    }

    #[cfg(unix)]
    #[test]
    fn ctime_detects_content_change_with_restored_mtime() {
        let tmp = crate::test_dir();
        let store = store(tmp.path());
        let path = store.current().unwrap().join("file");
        fs::write(&path, b"a").unwrap();
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        let before = store.fingerprint().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&path, b"b").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(mtime))
            .unwrap();
        assert_ne!(before.1, store.fingerprint().unwrap().1);
    }

    #[cfg(unix)]
    #[test]
    fn fingerprints_include_safe_symlink_targets() {
        use std::os::unix::fs::symlink;
        let tmp = crate::test_dir();
        let store = store(tmp.path());
        let link = store.current().unwrap().join("link");
        symlink("a", &link).unwrap();
        let before = store.fingerprint().unwrap();
        assert_eq!(store.fingerprint().unwrap(), before);
        fs::remove_file(&link).unwrap();
        symlink("b", &link).unwrap();
        let after = store.fingerprint().unwrap();
        assert_eq!(before.0, after.0);
        assert_ne!(before.1, after.1);
    }

    #[test]
    fn oversized_extensions_fail_before_payload_allocation() {
        let tmp = crate::test_dir();
        let store = store(tmp.path());
        let old = store.current().unwrap();
        for kind in *b"LKxg" {
            let mut header = header(false, 512 * 1024 * 1024, 0o600);
            header.set_entry_type(tar::EntryType::new(kind));
            header.set_path("extension").unwrap();
            header.set_cksum();
            let compressed = zstd::stream::encode_all(&header.as_bytes()[..], 1).unwrap();
            assert!(compressed.len() < 512);
            let path = tmp.path().join("extension.zst");
            fs::write(&path, compressed).unwrap();
            let error = store
                .restore(&path, &hash_file(&path).unwrap())
                .unwrap_err();
            assert!(error.to_string().contains("extension"), "{error:#}");
            assert_eq!(store.current().unwrap(), old);
        }
    }

    #[test]
    fn raw_header_count_is_bounded_before_tar_parsing() {
        let mut bytes = Vec::new();
        for _ in 0..5 {
            let mut header = header(false, 0, 0o600);
            header.set_path("a").unwrap();
            header.set_cksum();
            bytes.extend_from_slice(header.as_bytes());
        }
        let mut guard = TarGuard::new(bytes.as_slice(), 1024, 1);
        let error = io::copy(&mut guard, &mut io::sink()).unwrap_err();
        assert!(error.to_string().contains("raw header limit"));
    }

    #[test]
    fn excessive_zstd_window_is_rejected_before_tar() {
        let tmp = crate::test_dir();
        let store = store(tmp.path());
        let path = tmp.path().join("window.zst");
        // Valid empty frame declaring a 1 GiB window, with no content-size field.
        fs::write(&path, [0x28, 0xb5, 0x2f, 0xfd, 0, 0xa0, 1, 0, 0]).unwrap();
        let error = store
            .restore(&path, &hash_file(&path).unwrap())
            .unwrap_err();
        assert!(error.to_string().contains("memory"), "{error:#}");
    }

    #[test]
    fn dropping_artifact_removes_private_archive() {
        let tmp = crate::test_dir();
        let store = store(tmp.path());
        let artifact = store.create().unwrap();
        let path = artifact.path.clone();
        assert!(path.is_file());
        drop(artifact);
        assert!(!path.exists());
    }

    #[test]
    fn symlink_graph_rejects_chained_escape_cycles_and_descendants() {
        let mut records = BTreeMap::new();
        records.insert(path_key(Path::new("a")), link_record(Path::new(".")));
        records.insert(
            path_key(Path::new("b")),
            link_record(Path::new("a/../secret")),
        );
        assert!(validate_target(Path::new("b"), Path::new("a/../secret")).is_ok());
        assert!(validate_links(&records).is_err());
        records.clear();
        records.insert(path_key(Path::new("a")), link_record(Path::new("b")));
        records.insert(path_key(Path::new("b")), link_record(Path::new("a")));
        assert!(validate_links(&records).is_err());
        records.clear();
        records.insert(path_key(Path::new("a")), link_record(Path::new("target")));
        records.insert(
            path_key(Path::new("a/child")),
            link_record(Path::new("target")),
        );
        assert!(validate_links(&records).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn relative_symlinks_round_trip_without_dereferencing() {
        use std::os::unix::fs::symlink;
        let tmp = crate::test_dir();
        let store = store(tmp.path());
        let root = store.current().unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        fs::write(root.join("program"), b"contents").unwrap();
        symlink("../program", root.join("bin/program")).unwrap();
        symlink("bin/program", root.join("command")).unwrap();
        symlink("missing", root.join("dangling")).unwrap();
        let snapshot = store.create().unwrap();
        assert_eq!(snapshot.metadata.logical_bytes, 8);
        store
            .restore(&snapshot.path, &snapshot.metadata.sha256)
            .unwrap();
        let restored = store.current().unwrap();
        assert_eq!(
            fs::read_link(restored.join("bin/program")).unwrap(),
            Path::new("../program")
        );
        assert_eq!(fs::read(restored.join("command")).unwrap(), b"contents");
        assert_eq!(
            fs::read_link(restored.join("dangling")).unwrap(),
            Path::new("missing")
        );
    }
    #[cfg(unix)]
    #[test]
    fn raw_names_and_unsupported_links() {
        use std::os::unix::{
            ffi::OsStringExt,
            fs::{MetadataExt, symlink},
        };
        let tmp = crate::test_dir();
        let store = store(tmp.path());
        let name = std::ffi::OsString::from_vec(vec![0xff, b'a']);
        fs::write(store.current().unwrap().join(&name), b"raw").unwrap();
        let snap = store.create().unwrap();
        store.restore(&snap.path, &snap.metadata.sha256).unwrap();
        assert_eq!(
            fs::read(store.current().unwrap().join(&name)).unwrap(),
            b"raw"
        );
        symlink("/etc/passwd", store.current().unwrap().join("escape")).unwrap();
        assert!(store.create().is_err());
        fs::remove_file(store.current().unwrap().join("escape")).unwrap();
        fs::hard_link(
            store.current().unwrap().join(&name),
            store.current().unwrap().join("hard"),
        )
        .unwrap();
        let owner = fs::metadata(store.current().unwrap().join(&name))
            .unwrap()
            .uid();
        assert_eq!(store.create().is_ok(), owner == 1000);
    }
}
