//! Immutable code artifacts are copied only while every interpreter process is stopped.

use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, ensure};
use cap_std::{ambient_authority, fs::Dir};
use serde::Serialize;
use sha2::{Digest, Sha256};

pub(super) const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FILES: usize = 128;
const MAX_ENTRIES: usize = 4096;

#[derive(Clone, Debug, Serialize)]
pub(super) struct Artifact {
    pub id: String,
    pub filename: String,
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub mime_type: String,
}

#[derive(Clone)]
pub(super) struct StoredArtifact {
    pub metadata: Artifact,
    pub file: PathBuf,
}

pub(super) struct Captured {
    pub files: Vec<StoredArtifact>,
    pub error: Option<&'static str>,
}

#[derive(Serialize)]
pub(super) struct FileInfo {
    filename: String,
    path: String,
    cursor: String,
    bytes: u64,
    kind: &'static str,
    exportable: bool,
}

#[derive(Serialize)]
pub(super) struct FilePage {
    pub files: Vec<FileInfo>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

pub(super) fn list(
    directory: &Path,
    after: Option<&str>,
    limit: usize,
) -> anyhow::Result<FilePage> {
    ensure!((1..=100).contains(&limit), "code file page limit");
    ensure!(
        after.is_none_or(
            |value| value.len() <= 8192 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        ),
        "invalid code file cursor"
    );
    let after = after.map(str::to_ascii_lowercase);
    let root = Dir::open_ambient_dir(directory, ambient_authority())?;
    let mut found = Vec::new();
    enumerate(&root, Path::new(""), 0, &mut found)?;
    found.sort_by(|left, right| left.cursor.cmp(&right.cursor));
    let mut files = found
        .into_iter()
        .filter(|file| {
            after
                .as_deref()
                .is_none_or(|after| file.cursor.as_str() > after)
        })
        .take(limit + 1)
        .collect::<Vec<_>>();
    let has_more = files.len() > limit;
    files.truncate(limit);
    let next_cursor = files.last().map(|file| file.cursor.clone());
    Ok(FilePage {
        files,
        has_more,
        next_cursor,
    })
}

fn enumerate(
    directory: &Dir,
    prefix: &Path,
    depth: usize,
    files: &mut Vec<FileInfo>,
) -> anyhow::Result<()> {
    ensure!(
        depth <= 32 && files.len() <= 100_000,
        "code file listing limit"
    );
    for entry in directory.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let path = prefix.join(&name);
        let metadata = directory.symlink_metadata(&name)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            enumerate(&directory.open_dir(&name)?, &path, depth + 1, files)?;
        } else {
            ensure!(files.len() < 100_000, "code file listing limit");
            let regular = metadata.is_file() && !metadata.file_type().is_symlink();
            files.push(FileInfo {
                filename: name.to_string_lossy().into_owned(),
                cursor: hex::encode(path.as_os_str().as_encoded_bytes()),
                path: path.to_string_lossy().into_owned(),
                bytes: metadata.len(),
                kind: if regular {
                    "file"
                } else if metadata.file_type().is_symlink() {
                    "symlink"
                } else {
                    "unsupported"
                },
                exportable: regular && metadata.len() <= MAX_FILE_BYTES,
            });
        }
    }
    Ok(())
}

pub(super) fn export(
    directory: &Path,
    spool: &Path,
    context: &str,
    cursor: &str,
) -> anyhow::Result<StoredArtifact> {
    ensure!(
        !cursor.is_empty() && cursor.len() <= 8192,
        "invalid code file cursor"
    );
    let raw = hex::decode(cursor)?;
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(raw))
    };
    #[cfg(not(unix))]
    let path = PathBuf::from(String::from_utf8(raw)?);
    ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_))),
        "invalid code file cursor"
    );
    let mut parent = Dir::open_ambient_dir(directory, ambient_authority())?;
    let mut parts = path.components().peekable();
    while let Some(part) = parts.next() {
        let name = part.as_os_str();
        let metadata = parent.symlink_metadata(name)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "code files cannot be links"
        );
        if parts.peek().is_some() {
            ensure!(metadata.is_dir(), "invalid code file directory");
            parent = parent.open_dir(name)?;
            continue;
        }
        ensure!(metadata.is_file(), "code file is not regular");
        let mut scan = Scan {
            context,
            spool,
            files: vec![],
            entries: 0,
            bytes: 0,
        };
        scan.file(&parent, name, path.clone(), metadata.len())?;
        return scan.files.pop().context("code file export missing");
    }
    anyhow::bail!("invalid code file cursor")
}

struct Scan<'a> {
    context: &'a str,
    spool: &'a Path,
    files: Vec<StoredArtifact>,
    entries: usize,
    bytes: u64,
}

pub(super) fn capture(directory: &Path, spool: &Path, context: &str) -> Captured {
    let mut scan = Scan {
        context,
        spool,
        files: vec![],
        entries: 0,
        bytes: 0,
    };
    let result = (|| -> anyhow::Result<()> {
        let root = Dir::open_ambient_dir(directory, ambient_authority())?;
        scan.walk(&root, Path::new(""), 0)
    })();
    Captured {
        files: scan.files,
        error: result
            .err()
            .map(|_| "artifact_export_limit_or_unsupported_entry"),
    }
}

impl Scan<'_> {
    fn walk(&mut self, directory: &Dir, prefix: &Path, depth: usize) -> anyhow::Result<()> {
        ensure!(depth <= 32, "code artifact depth limit");
        let mut names = directory
            .entries()?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?;
        names.sort();
        for name in names {
            self.entries += 1;
            ensure!(self.entries <= MAX_ENTRIES, "code artifact entry limit");
            let path = prefix.join(&name);
            let metadata = directory.symlink_metadata(&name)?;
            ensure!(
                !metadata.file_type().is_symlink(),
                "code artifacts cannot be links"
            );
            if metadata.is_dir() {
                self.walk(&directory.open_dir(&name)?, &path, depth + 1)?;
            } else {
                ensure!(metadata.is_file(), "code artifact must be a regular file");
                self.file(directory, &name, path, metadata.len())?;
            }
        }
        Ok(())
    }

    fn file(
        &mut self,
        directory: &Dir,
        name: &std::ffi::OsStr,
        path: PathBuf,
        bytes: u64,
    ) -> anyhow::Result<()> {
        ensure!(bytes <= MAX_FILE_BYTES, "code artifact exceeds 16 MiB");
        ensure!(
            self.files.len() < MAX_FILES,
            "code artifact file count limit"
        );
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .context("code artifact size overflow")?;
        ensure!(
            self.bytes <= MAX_TOTAL_BYTES,
            "code artifact total exceeds 64 MiB"
        );
        let mut source = directory.open(name)?;
        let mut output = tempfile::NamedTempFile::new_in(self.spool)?;
        let mut hash = Sha256::new();
        let mut copied = 0_u64;
        let mut buffer = [0; 64 * 1024];
        loop {
            let count = source.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            copied = copied
                .checked_add(count as u64)
                .context("code artifact size overflow")?;
            ensure!(copied <= bytes, "code artifact changed during export");
            hash.update(&buffer[..count]);
            output.write_all(&buffer[..count])?;
        }
        ensure!(copied == bytes, "code artifact changed during export");
        output.as_file().sync_all()?;
        let sha256 = hex::encode(hash.finalize());
        let mut identity = Sha256::new();
        identity.update(self.context.as_bytes());
        identity.update([0]);
        identity.update(path.as_os_str().as_encoded_bytes());
        identity.update([0]);
        identity.update(sha256.as_bytes());
        let id = format!("pyart_{}", hex::encode(identity.finalize()));
        let target = self.spool.join(&id);
        if target.exists() {
            ensure!(
                std::fs::metadata(&target)?.len() == bytes,
                "code artifact spool conflict"
            );
        } else {
            output
                .persist_noclobber(&target)
                .context("publish code artifact")?;
        }
        let metadata = Artifact {
            id,
            filename: name.to_string_lossy().into_owned(),
            path: path.to_string_lossy().into_owned(),
            bytes,
            sha256,
            mime_type: mime(&path).into(),
        };
        self.files.push(StoredArtifact {
            metadata,
            file: target,
        });
        Ok(())
    }
}

fn mime(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("pdf") => "application/pdf",
        Some("txt" | "py" | "md") => "text/plain",
        Some("csv") => "text/csv",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

pub(super) fn open(file: &StoredArtifact) -> anyhow::Result<File> {
    let metadata = std::fs::symlink_metadata(&file.file)?;
    ensure!(
        metadata.is_file() && !metadata.is_symlink(),
        "invalid code artifact spool"
    );
    ensure!(
        metadata.len() == file.metadata.bytes,
        "invalid code artifact size"
    );
    Ok(File::open(&file.file)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifacts_are_immutable_content_bound_copies() {
        let root = tempfile::tempdir().unwrap();
        let spool = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("plot.png"), [0, 255, 1]).unwrap();
        let first = capture(root.path(), spool.path(), "cntr_test");
        assert!(first.error.is_none());
        let file = &first.files[0];
        assert_eq!(file.metadata.bytes, 3);
        assert_eq!(file.metadata.mime_type, "image/png");
        assert!(
            !serde_json::to_string(&file.metadata)
                .unwrap()
                .contains(&spool.path().to_string_lossy().to_string())
        );
        let repeated = capture(root.path(), spool.path(), "cntr_test");
        assert_eq!(file.metadata.id, repeated.files[0].metadata.id);
        std::fs::write(root.path().join("plot.png"), [2, 3]).unwrap();
        assert_eq!(std::fs::read(&file.file).unwrap(), [0, 255, 1]);
        assert_ne!(
            file.metadata.id,
            capture(root.path(), spool.path(), "cntr_test").files[0]
                .metadata
                .id
        );
    }

    #[cfg(unix)]
    #[test]
    fn links_never_export_an_external_file() {
        let root = tempfile::tempdir().unwrap();
        let spool = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.path().join("escape")).unwrap();
        let result = capture(root.path(), spool.path(), "cntr_test");
        assert!(result.error.is_some());
        assert!(result.files.is_empty());
    }

    #[test]
    fn oversize_files_are_explicit_errors_not_truncated_artifacts() {
        let root = tempfile::tempdir().unwrap();
        let spool = tempfile::tempdir().unwrap();
        File::create(root.path().join("large.bin"))
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        let result = capture(root.path(), spool.path(), "cntr_test");
        assert!(result.error.is_some());
        assert!(result.files.is_empty());
    }

    #[test]
    fn listing_cursors_export_unicode_files_without_becoming_paths() {
        let root = tempfile::tempdir().unwrap();
        let spool = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), b"first").unwrap();
        std::fs::write(root.path().join("雪.txt"), [0, 255]).unwrap();
        let first = list(root.path(), None, 1).unwrap();
        assert!(first.has_more);
        let second = list(root.path(), first.next_cursor.as_deref(), 1).unwrap();
        assert!(!second.has_more);
        assert_eq!(second.files[0].filename, "雪.txt");
        let artifact = export(
            root.path(),
            spool.path(),
            "cntr_test",
            &second.files[0].cursor,
        )
        .unwrap();
        assert_eq!(std::fs::read(artifact.file).unwrap(), [0, 255]);
        assert!(
            export(
                root.path(),
                spool.path(),
                "cntr_test",
                &hex::encode("../outside")
            )
            .is_err()
        );
    }
}
