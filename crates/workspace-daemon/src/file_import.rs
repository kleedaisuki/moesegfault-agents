use std::{
    io::{self, Read},
    path::Path,
};

use anyhow::ensure;
use axum::{
    Json,
    body::Body,
    extract::{Path as RoutePath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use futures_util::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::{App, error, internal, process, record_tree};

pub(super) const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Serialize)]
pub(super) struct ImportedFile {
    file_id: String,
    path: String,
    bytes: u64,
    sha256: String,
}

pub(super) async fn import(
    State(state): State<std::sync::Arc<App>>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if !valid_id(&id) {
        return error(StatusCode::BAD_REQUEST, "invalid file id");
    }
    let Some(bytes) = header_text(&headers, "content-length").and_then(|v| v.parse::<u64>().ok())
    else {
        return error(StatusCode::LENGTH_REQUIRED, "content-length required");
    };
    if bytes > MAX_FILE_BYTES {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "file exceeds 64 MiB");
    }
    let Some(hash) = header_text(&headers, "x-content-sha256").filter(|v| valid_hash(v)) else {
        return error(StatusCode::BAD_REQUEST, "valid x-content-sha256 required");
    };
    let hash = hash.to_ascii_lowercase();
    let Ok(guard) = state.gate.clone().try_lock_owned() else {
        return error(StatusCode::CONFLICT, "workspace busy");
    };
    let incoming = match state.store.incoming() {
        Ok(file) => file,
        Err(err) => return internal(err),
    };
    let mut output = match incoming.reopen() {
        Ok(file) => tokio::fs::File::from_std(file),
        Err(err) => return internal(err),
    };
    let mut stream = body.into_data_stream();
    let mut received = 0_u64;
    let mut digest = Sha256::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            return error(StatusCode::BAD_REQUEST, "incomplete file body");
        };
        received = received.saturating_add(chunk.len() as u64);
        if received > bytes {
            return error(StatusCode::BAD_REQUEST, "file length mismatch");
        }
        digest.update(&chunk);
        if let Err(err) = output.write_all(&chunk).await {
            return internal(err);
        }
    }
    if received != bytes || hex::encode(digest.finalize()) != hash {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "file integrity mismatch");
    }
    if let Err(err) = output.sync_all().await {
        return internal(err);
    }
    drop(output);
    // Once verified, the task owns publication and dirty tracking even when its
    // HTTP observer disconnects. No new execution can interleave that boundary.
    let task = tokio::spawn(async move {
        let store = state.store.clone();
        let (outcome, tree, guard) = tokio::task::spawn_blocking(move || {
            process::quiesce()?;
            let result = install(&store.current()?, &id, incoming.path(), bytes, &hash);
            drop(incoming);
            let tree = store.fingerprint().ok();
            Ok::<_, anyhow::Error>((result, tree, guard))
        })
        .await??;
        let _guard = guard;
        record_tree(&state, tree).await;
        outcome
    });
    match task.await {
        Ok(Ok(file)) => Json(file).into_response(),
        Ok(Err(err)) if err.downcast_ref::<Conflict>().is_some() => error(
            StatusCode::CONFLICT,
            "file destination conflicts with existing workspace data",
        ),
        Ok(Err(err)) => internal(err),
        Err(err) => internal(err),
    }
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || b"_-".contains(&v))
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|v| v.is_ascii_hexdigit())
}

#[derive(Debug)]
struct Conflict;
impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("file destination conflict")
    }
}
impl std::error::Error for Conflict {}

fn install(
    root: &Path,
    id: &str,
    incoming: &Path,
    bytes: u64,
    hash: &str,
) -> anyhow::Result<ImportedFile> {
    ensure!(
        valid_id(id) && valid_hash(hash) && bytes <= MAX_FILE_BYTES,
        "invalid import metadata"
    );
    let root = Dir::open_ambient_dir(root, ambient_authority())?;
    let uploads = directory(&root, "uploads")?;
    let destination = directory(&uploads, id)?;
    if let Ok(metadata) = destination.symlink_metadata("content") {
        if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() != bytes {
            return Err(Conflict.into());
        }
        let mut file = destination.open("content")?;
        if file_hash(&mut file, bytes)? != hash {
            return Err(Conflict.into());
        }
        return Ok(metadata_value(id, bytes, hash));
    }
    // Protected incoming files and generations share the state filesystem. A
    // hard link publishes without a second file copy or replacing user data.
    let source = Dir::open_ambient_dir(incoming.parent().ok_or(Conflict)?, ambient_authority())?;
    let name = incoming.file_name().ok_or(Conflict)?;
    let file = source
        .open_with(name, OpenOptions::new().read(true).write(true))?
        .into_std();
    ensure!(file.metadata()?.len() == bytes, "import length changed");
    assign(&file, 0o600)?;
    file.sync_all()?;
    source
        .hard_link(name, &destination, "content")
        .map_err(|err| {
            if err.kind() == io::ErrorKind::AlreadyExists {
                anyhow::Error::from(Conflict)
            } else {
                err.into()
            }
        })?;
    sync_dir(&destination)?;
    Ok(metadata_value(id, bytes, hash))
}

fn directory(parent: &Dir, name: &str) -> anyhow::Result<Dir> {
    let created = match parent.create_dir(name) {
        Ok(()) => true,
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => false,
        Err(err) => return Err(err.into()),
    };
    let metadata = parent.symlink_metadata(name)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Conflict.into());
    }
    let dir = parent.open_dir(name)?;
    if created {
        assign(&dir.try_clone()?.into_std_file(), 0o700)?;
        sync_dir(parent)?;
    }
    Ok(dir)
}

fn metadata_value(id: &str, bytes: u64, hash: &str) -> ImportedFile {
    ImportedFile {
        file_id: id.into(),
        path: format!("uploads/{id}/content"),
        bytes,
        sha256: hash.into(),
    }
}

fn file_hash(file: &mut impl Read, bytes: u64) -> anyhow::Result<String> {
    let mut digest = Sha256::new();
    let copied = io::copy(&mut file.take(bytes.saturating_add(1)), &mut digest)?;
    ensure!(copied == bytes, "import length mismatch");
    Ok(hex::encode(digest.finalize()))
}

fn assign(file: &std::fs::File, mode: u32) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::geteuid() } == 0
            && unsafe { libc::fchown(file.as_raw_fd(), 1000, 1000) } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
    }
    let _ = (file, mode);
    Ok(())
}

fn sync_dir(dir: &Dir) -> anyhow::Result<()> {
    #[cfg(unix)]
    dir.try_clone()?.into_std_file().sync_all()?;
    let _ = dir;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf, String) {
        let directory = crate::test_dir();
        let incoming = directory.path().join("input");
        std::fs::write(&incoming, b"binary\0payload").unwrap();
        let hash = hex::encode(Sha256::digest(b"binary\0payload"));
        (directory, incoming, hash)
    }

    #[test]
    fn ids_cannot_name_paths_or_oversized_components() {
        assert!(valid_id("file_012-abc"));
        for id in ["", ".", "..", "a/b", "a\\b", "C:foo", "é", &"a".repeat(129)] {
            assert!(!valid_id(id), "{id}");
        }
    }

    #[test]
    fn import_is_exact_idempotent_and_never_clobbers() {
        let (directory, incoming, hash) = fixture();
        let first = install(directory.path(), "file_1", &incoming, 14, &hash).unwrap();
        assert_eq!(first.path, "uploads/file_1/content");
        install(directory.path(), "file_1", &incoming, 14, &hash).unwrap();
        let output = directory.path().join(&first.path);
        std::fs::write(&output, b"user edits here").unwrap();
        assert!(install(directory.path(), "file_1", &incoming, 14, &hash).is_err());
        assert_eq!(std::fs::read(output).unwrap(), b"user edits here");
    }

    #[test]
    fn idempotent_import_does_not_change_workspace_fingerprint() {
        use std::io::Write as _;
        let directory = crate::test_dir();
        let store =
            crate::snapshot::SnapshotStore::open(directory.path().to_owned(), 1024 * 1024, 100)
                .unwrap();
        let hash = hex::encode(Sha256::digest(b"attachment"));
        let mut previous = None;
        for _ in 0..3 {
            let mut incoming = store.incoming().unwrap();
            incoming.write_all(b"attachment").unwrap();
            install(
                &store.current().unwrap(),
                "file-repeat",
                incoming.path(),
                10,
                &hash,
            )
            .unwrap();
            drop(incoming);
            let fingerprint = store.fingerprint().unwrap();
            if let Some(expected) = &previous {
                assert_eq!(&fingerprint, expected);
            }
            previous = Some(fingerprint);
        }
    }

    #[cfg(unix)]
    #[test]
    fn imported_paths_reject_symlinked_directory_and_destination() {
        use std::os::unix::fs::symlink;
        let (directory, incoming, hash) = fixture();
        let outside = crate::test_dir();
        symlink(outside.path(), directory.path().join("uploads")).unwrap();
        assert!(install(directory.path(), "file_1", &incoming, 14, &hash).is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        std::fs::remove_file(directory.path().join("uploads")).unwrap();
        std::fs::create_dir_all(directory.path().join("uploads/file_1")).unwrap();
        symlink(&incoming, directory.path().join("uploads/file_1/content")).unwrap();
        assert!(install(directory.path(), "file_1", &incoming, 14, &hash).is_err());
    }
}
