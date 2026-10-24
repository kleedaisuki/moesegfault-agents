use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{self, IsTerminal, Read, Write},
    path::{Component, Path, PathBuf},
};
use tempfile::NamedTempFile;

use crate::client::Api;

pub const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TRANSCRIPT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 100_001;
const MAX_EXPANDED_BYTES: u64 = MAX_FILE_BYTES + 100_000 * 8192 + 16 * 1024 * 1024;
const MANIFEST_LIMIT: u64 = 16 * 1024 * 1024;
const MANIFEST: &str = ".magents-manifest.json";

#[derive(Clone, Copy, ValueEnum)]
pub enum ExportFormat {
    Json,
    Jsonl,
    Markdown,
}

impl ExportFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Jsonl => "jsonl",
            Self::Markdown => "markdown",
        }
    }
}

pub struct Output {
    file: NamedTempFile,
    destination: PathBuf,
}

impl Output {
    pub fn new(destination: &Path, binary: bool) -> Result<Self> {
        if destination.as_os_str() == "-" {
            if binary && io::stdout().is_terminal() {
                bail!("Refusing binary output to a terminal; choose --output FILE or pipe stdout");
            }
        } else {
            match std::fs::symlink_metadata(destination) {
                Ok(_) => bail!("Output already exists; choose a new filename"),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("Cannot inspect output destination"),
            }
        }
        let directory = if destination.as_os_str() == "-" {
            std::env::temp_dir()
        } else {
            parent(destination).to_owned()
        };
        let file = tempfile::Builder::new()
            .prefix(".magents-")
            .tempfile_in(directory)
            .context("Cannot create a private output file")?;
        Ok(Self {
            file,
            destination: destination.to_owned(),
        })
    }

    pub fn writer(&mut self) -> &mut std::fs::File {
        self.file.as_file_mut()
    }

    pub fn finish(mut self) -> Result<()> {
        self.file.as_file_mut().flush()?;
        if self.destination.as_os_str() == "-" {
            use std::io::Seek;
            self.file.as_file_mut().rewind()?;
            let mut stdout = io::stdout().lock();
            io::copy(self.file.as_file_mut(), &mut stdout)?;
            stdout.flush()?;
        } else {
            self.file.as_file().sync_all()?;
            self.file
                .persist_noclobber(&self.destination)
                .map_err(|_| anyhow::anyhow!("Cannot publish output without overwriting a file"))?;
        }
        Ok(())
    }
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

pub fn write_bytes(destination: &Path, bytes: &[u8]) -> Result<()> {
    let mut output = Output::new(destination, false)?;
    output.writer().write_all(bytes)?;
    output.finish()
}

pub async fn transcript(
    api: &Api,
    id: &str,
    format: ExportFormat,
    destination: &Path,
) -> Result<()> {
    let mut output = Output::new(destination, false)?;
    api.download_to(
        &format!("/v1/sessions/{id}/export?format={}", format.as_str()),
        output.writer(),
        MAX_TRANSCRIPT_BYTES,
        false,
    )
    .await?;
    output.finish()
}

pub async fn workspace(api: &Api, id: &str, destination: &Path) -> Result<()> {
    let mut output = Output::new(destination, true)?;
    api.download_to(
        &format!("/v1/sessions/{id}/workspace/archive"),
        output.writer(),
        MAX_ARCHIVE_BYTES,
        true,
    )
    .await?;
    output.finish()
}

pub async fn workspace_file(api: &Api, id: &str, selected: &str, destination: &Path) -> Result<()> {
    validate_selector(selected)?;
    let mut output = Output::new(destination, true)?;
    let directory = output.file.path().parent().unwrap();
    let mut archive = tempfile::Builder::new()
        .prefix(".magents-archive-")
        .tempfile_in(directory)?;
    api.download_to(
        &format!("/v1/sessions/{id}/workspace/archive"),
        archive.as_file_mut(),
        MAX_ARCHIVE_BYTES,
        true,
    )
    .await?;
    use std::io::Seek;
    archive.as_file_mut().rewind()?;
    extract_file(archive.as_file_mut(), selected, output.writer())?;
    output.finish()
}

fn validate_selector(path: &str) -> Result<()> {
    if path.is_empty()
        || path.contains(['\\', ':', '\0'])
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || !Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        bail!(
            "Workspace path must be a normalized relative file path, not a link or archive metadata"
        );
    }
    Ok(())
}

fn extract_file(source: impl Read, selected: &str, destination: &mut impl Write) -> Result<()> {
    validate_selector(selected)?;
    let mut decoder = zstd::stream::read::Decoder::new(source)
        .context("Workspace is not a valid zstd archive")?;
    decoder.window_log_max(23)?;
    let guarded = TarGuard::new(decoder.take(MAX_EXPANDED_BYTES + 1));
    let mut archive = tar::Archive::new(guarded);
    let manifest_key: String = selected
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let mut selected_record = None;
    let mut manifest = None;
    let selected = Path::new("data").join(selected);
    for (count, entry) in archive.entries()?.enumerate() {
        if count >= MAX_ARCHIVE_ENTRIES {
            bail!("Workspace archive exceeds entry limit");
        }
        let mut entry = entry.context("Workspace archive is truncated or invalid")?;
        if manifest.is_some() {
            bail!("Workspace manifest must be the final archive entry");
        }
        let path = entry.path()?.into_owned();
        if path == Path::new(MANIFEST) {
            if !entry.header().entry_type().is_file() || entry.size() > MANIFEST_LIMIT {
                bail!("Invalid workspace manifest entry");
            }
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            let parsed: Manifest = serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("Invalid workspace manifest"))?;
            if parsed.version != 1 || parsed.entries.len() >= MAX_ARCHIVE_ENTRIES {
                bail!("Unsupported workspace manifest version or entry count");
            }
            manifest = Some(parsed);
            continue;
        }
        if path != selected {
            continue;
        }
        if selected_record.is_some() || !entry.header().entry_type().is_file() {
            bail!("Selected workspace path is duplicated or is not a regular file");
        }
        let size = entry.size();
        if size > MAX_FILE_BYTES {
            bail!("Selected workspace file exceeds download limit");
        }
        let mut copied = 0u64;
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let size = entry.read(&mut buffer)?;
            if size == 0 {
                break;
            }
            destination.write_all(&buffer[..size])?;
            hash.update(&buffer[..size]);
            copied += size as u64;
        }
        if copied != size {
            bail!("Selected workspace file is truncated");
        }
        selected_record = Some((copied, format!("{:x}", hash.finalize())));
    }
    let mut guarded = archive.into_inner();
    let mut rest = io::sink();
    io::copy(&mut guarded, &mut rest).context("Workspace archive stream is invalid")?;
    // Finish zstd validation after tar's end marker, including its frame checksum.
    let mut compressed = guarded.inner;
    io::copy(&mut compressed, &mut rest).context("Workspace archive stream is invalid")?;
    if compressed.limit() == 0 {
        bail!("Workspace archive exceeds expanded download limit");
    }
    let (size, digest) =
        selected_record.context("Workspace file not found in the committed snapshot")?;
    let manifest = manifest.context("Workspace snapshot is missing its final manifest")?;
    let record = manifest
        .entries
        .get(&manifest_key)
        .context("Selected workspace file is missing from the manifest")?;
    if record.directory
        || record.fifo
        || record.target.is_some()
        || record.size != size
        || record.sha256 != digest
    {
        bail!("Selected workspace file does not match its manifest receipt");
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    entries: BTreeMap<String, ManifestRecord>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestRecord {
    size: u64,
    sha256: String,
    directory: bool,
    #[serde(default)]
    fifo: bool,
    #[serde(default)]
    target: Option<String>,
}

// Mirror the daemon's snapshot framing boundary before tar allocates GNU/PAX payloads.
// The archive is parsed by tar; this guard only enforces our fixed wire-format limits.
struct TarGuard<R> {
    inner: R,
    block: [u8; 512],
    position: usize,
    payload: u64,
    headers: usize,
    ended: bool,
}

impl<R: Read> TarGuard<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            block: [0; 512],
            position: 512,
            payload: 0,
            headers: 0,
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
        self.headers += 1;
        if self.headers > MAX_ARCHIVE_ENTRIES * 3 {
            return Err(io::Error::other(
                "Workspace archive exceeds raw header limit",
            ));
        }
        let header = tar::Header::from_byte_slice(&self.block);
        let checksum: u32 = self
            .block
            .iter()
            .enumerate()
            .map(|(index, byte)| {
                u32::from(if (148..156).contains(&index) {
                    b' '
                } else {
                    *byte
                })
            })
            .sum();
        if header.cksum()? != checksum {
            return Err(io::Error::other("Workspace tar header checksum mismatch"));
        }
        let size = header.entry_size()?;
        match header.entry_type().as_byte() {
            b'L' | b'K' if size <= 64 * 1024 => (),
            b'0' | 0 if size <= MAX_FILE_BYTES => (),
            b'2' | b'5' | b'6' if size == 0 => (),
            _ => {
                return Err(io::Error::other(
                    "Unsupported workspace tar entry type or size",
                ));
            }
        }
        self.payload = size
            .checked_add(511)
            .map(|value| value / 512 * 512)
            .ok_or_else(|| io::Error::other("Workspace tar entry size overflow"))?;
        Ok(())
    }
}

impl<R: Read> Read for TarGuard<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.position < 512 {
            let size = output.len().min(512 - self.position);
            output[..size].copy_from_slice(&self.block[self.position..self.position + size]);
            self.position += size;
            return Ok(size);
        }
        if self.payload != 0 {
            let limit = output.len().min(self.payload as usize);
            let size = self.inner.read(&mut output[..limit])?;
            if size == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Truncated workspace tar entry",
                ));
            }
            self.payload -= size as u64;
            return Ok(size);
        }
        if self.ended {
            return Ok(0);
        }
        self.next_header()?;
        self.read(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(path: &str, bytes: &[u8], kind: tar::EntryType) -> Vec<u8> {
        let encoder = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
        let mut archive = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_size(bytes.len() as u64);
        header.set_mode(0o600);
        header.set_cksum();
        archive.append_data(&mut header, path, bytes).unwrap();
        let relative = path.strip_prefix("data/").unwrap();
        let key: String = relative
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let manifest = serde_json::to_vec(&serde_json::json!({"version":1,"entries":{
            key:{"size":bytes.len(),"sha256":format!("{:x}", Sha256::digest(bytes)),"directory":false}
        }})).unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o600);
        header.set_size(manifest.len() as u64);
        header.set_cksum();
        archive
            .append_data(&mut header, MANIFEST, manifest.as_slice())
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn file_selection_is_exact_and_does_not_unpack_paths() {
        let data = b"hello\0workspace\xff";
        let source = archive("data/project/note.bin", data, tar::EntryType::Regular);
        let mut output = Vec::new();
        extract_file(source.as_slice(), "project/note.bin", &mut output).unwrap();
        assert_eq!(output, data);
        assert!(extract_file(source.as_slice(), "note.bin", &mut Vec::new()).is_err());
        for invalid in ["../note.bin", "/note.bin", "a//b", "a/./b", "C:/x", "a\\b"] {
            assert!(validate_selector(invalid).is_err());
        }
    }

    #[test]
    fn links_are_never_resolved_and_corruption_is_not_published() {
        let source = archive("data/link", b"", tar::EntryType::Symlink);
        assert!(extract_file(source.as_slice(), "link", &mut Vec::new()).is_err());
        let source = archive("data/file", b"value", tar::EntryType::Regular);
        assert!(extract_file(&source[..source.len() / 2], "file", &mut Vec::new()).is_err());
    }

    #[test]
    fn hostile_extension_headers_fail_before_allocating_their_payloads() {
        for kind in [
            tar::EntryType::GNULongName,
            tar::EntryType::GNULongLink,
            tar::EntryType::XHeader,
            tar::EntryType::XGlobalHeader,
            tar::EntryType::GNUSparse,
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_path("hostile").unwrap();
            header.set_entry_type(kind);
            header.set_size(128 * 1024 * 1024);
            header.set_cksum();
            let source = zstd::stream::encode_all(header.as_bytes().as_slice(), 1).unwrap();
            let error = extract_file(source.as_slice(), "file", &mut Vec::new()).unwrap_err();
            assert!(error.to_string().contains("invalid"));
        }
    }

    #[test]
    fn long_unicode_paths_use_daemon_data_prefix_and_preserve_binary_bytes() {
        let selected = format!("{}/可莉.txt", "long-directory/".repeat(20));
        let selected = selected.replace("//", "/");
        let source = archive(
            &format!("data/{selected}"),
            b"contents",
            tar::EntryType::Regular,
        );
        let mut output = Vec::new();
        extract_file(source.as_slice(), &selected, &mut output).unwrap();
        assert_eq!(output, b"contents");
    }

    #[test]
    fn missing_malformed_nonfinal_or_mismatched_manifests_reject_selected_data() {
        let payload = b"selected";
        let key = "66696c65";
        let valid = serde_json::json!({"version":1,"entries":{
            key:{"size":payload.len(),"sha256":format!("{:x}",Sha256::digest(payload)),"directory":false,"fifo":false,"target":null}
        }});
        let mut variants = Vec::new();
        for (pointer, value) in [
            ("/version", serde_json::json!(2)),
            ("/entries", serde_json::json!({})),
            ("/entries/66696c65/size", serde_json::json!(1)),
            (
                "/entries/66696c65/sha256",
                serde_json::json!("0".repeat(64)),
            ),
            ("/entries/66696c65/directory", serde_json::json!(true)),
            ("/entries/66696c65/fifo", serde_json::json!(true)),
            ("/entries/66696c65/target", serde_json::json!("elsewhere")),
        ] {
            let mut invalid = valid.clone();
            *invalid
                .pointer_mut(pointer)
                .unwrap_or_else(|| panic!("Missing pointer: {pointer}")) = value;
            variants.push((Some(serde_json::to_vec(&invalid).unwrap()), false));
        }
        variants.extend([
            (Some(b"{".to_vec()), false),
            (None, false),
            (Some(serde_json::to_vec(&valid).unwrap()), true),
        ]);
        for (manifest, trailing) in variants {
            let encoder = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
            let mut builder = tar::Builder::new(encoder);
            let append = |builder: &mut tar::Builder<_>, path, bytes: &[u8]| {
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o600);
                header.set_cksum();
                builder.append_data(&mut header, path, bytes).unwrap();
            };
            append(&mut builder, "data/file", payload);
            if let Some(manifest) = manifest {
                append(&mut builder, MANIFEST, &manifest);
            }
            if trailing {
                append(&mut builder, "data/after", b"after");
            }
            let source = builder.into_inner().unwrap().finish().unwrap();
            assert!(extract_file(source.as_slice(), "file", &mut Vec::new()).is_err());
        }
    }
}
