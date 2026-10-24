use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Output, Stdio},
    thread,
    time::Duration,
};

fn directory() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.cache/cli-tests");
    std::fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}

fn command(args: &[&str]) -> Output {
    let cache = directory();
    Command::new(env!("CARGO_BIN_EXE_magents"))
        .args(args)
        .env_remove("MAGENTS_API_KEY")
        .env_remove("MAGENTS_API_URL")
        .env("TMP", cache.path())
        .env("TEMP", cache.path())
        .env("TMPDIR", cache.path())
        .output()
        .unwrap()
}

fn request(args: &[&str], response: &[u8], status: &str, headers: &str) -> (Output, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let mut wire = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
        response.len()
    )
    .into_bytes();
    wire.extend_from_slice(response);
    let server = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "CLI did not request fixture"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("Cannot accept fixture: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut raw = Vec::new();
        let mut buffer = [0; 1024];
        loop {
            let count = stream.read(&mut buffer).unwrap();
            assert_ne!(count, 0);
            raw.extend_from_slice(&buffer[..count]);
            if let Some(end) = raw.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
                let size: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap_or("0")
                    .parse()
                    .unwrap();
                if raw.len() >= end + 4 + size {
                    break;
                }
            }
        }
        // The client may refuse invalid response headers before reading the body.
        let _ = stream.write_all(&wire);
        String::from_utf8(raw).unwrap()
    });
    let cache = directory();
    let output = Command::new(env!("CARGO_BIN_EXE_magents"))
        .args(["--api-url", &origin])
        .args(args)
        .env("MAGENTS_API_KEY", "fixture-key")
        .env("TMP", cache.path())
        .env("TEMP", cache.path())
        .env("TMPDIR", cache.path())
        .output()
        .unwrap();
    (output, server.join().unwrap())
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn body(raw: &str) -> Value {
    serde_json::from_str(raw.split_once("\r\n\r\n").unwrap().1).unwrap()
}

fn archive() -> Vec<u8> {
    archive_with_manifest(None)
}

fn archive_with_manifest(replacement: Option<&[u8]>) -> Vec<u8> {
    let encoder = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
    let mut builder = tar::Builder::new(encoder);
    let mut records = serde_json::Map::new();
    for (path, data) in [
        ("data/project/data.bin", b"hello\0workspace\xff".as_slice()),
        ("data/other", b"other".as_slice()),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o600);
        header.set_cksum();
        builder.append_data(&mut header, path, data).unwrap();
        let key: String = path
            .strip_prefix("data/")
            .unwrap()
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        records.insert(key, json!({"size":data.len(),"sha256":format!("{:x}", Sha256::digest(data)),"directory":false}));
    }
    let manifest = replacement
        .map(Vec::from)
        .unwrap_or_else(|| serde_json::to_vec(&json!({"version":1,"entries":records})).unwrap());
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o600);
    header.set_size(manifest.len() as u64);
    header.set_cksum();
    builder
        .append_data(&mut header, ".magents-manifest.json", manifest.as_slice())
        .unwrap();
    builder.into_inner().unwrap().finish().unwrap()
}

#[test]
fn real_binary_toml_init_validate_publish_download_replace_roundtrip() {
    let dir = directory();
    let source = dir.path().join("agent.toml");
    let downloaded = dir.path().join("downloaded.toml");
    success(&command(&["agents", "init", source.to_str().unwrap()]));
    let validation = command(&["agents", "validate", source.to_str().unwrap()]);
    success(&validation);
    assert_eq!(
        serde_json::from_slice::<Value>(&validation.stdout).unwrap()["valid"],
        true
    );
    let (published, raw) = request(
        &["agents", "publish", source.to_str().unwrap()],
        br#"{"id":"a_owned"}"#,
        "201 Created",
        "Content-Type: application/json\r\n",
    );
    success(&published);
    assert!(raw.starts_with("POST /v1/agents HTTP/1.1"));
    assert!(
        raw.to_ascii_lowercase()
            .contains("authorization: bearer fixture-key")
    );
    let editable = body(&raw);
    assert!(editable.get("schema_version").is_none());
    assert_eq!(editable["name"], "My agent");
    let mut stored = editable.clone();
    for (key, value) in [
        ("id", json!("a_owned")),
        ("owner_id", json!("private_owner")),
        ("revision", json!(4)),
        ("api_key", json!("must_not_export")),
    ] {
        stored[key] = value;
    }
    let (download, raw) = request(
        &[
            "agents",
            "download",
            "a_owned",
            "--output",
            downloaded.to_str().unwrap(),
        ],
        &serde_json::to_vec(&stored).unwrap(),
        "200 OK",
        "Content-Type: application/json\r\n",
    );
    success(&download);
    assert!(download.stdout.is_empty());
    assert!(raw.starts_with("GET /v1/agents/a_owned HTTP/1.1"));
    let text = std::fs::read_to_string(&downloaded).unwrap();
    assert!(!text.contains("must_not_export") && !text.contains("private_owner"));
    success(&command(&[
        "agents",
        "validate",
        downloaded.to_str().unwrap(),
    ]));
    let (replaced, raw) = request(
        &[
            "agents",
            "publish",
            downloaded.to_str().unwrap(),
            "--agent",
            "a_owned",
        ],
        &serde_json::to_vec(&stored).unwrap(),
        "200 OK",
        "Content-Type: application/json\r\n",
    );
    success(&replaced);
    assert!(raw.starts_with("PUT /v1/agents/a_owned HTTP/1.1"));
    assert_eq!(body(&raw), editable);
}

#[test]
fn real_binary_conversation_formats_and_explicit_stdout() {
    let dir = directory();
    for (format, content) in [
        (
            "json",
            "{\"messages\":[{\"role\":\"tool\",\"content\":\"original\"}],\"pending_messages\":[\"steer\"]}",
        ),
        ("jsonl", "{\"role\":\"user\",\"content\":\"hello\"}\n"),
        ("markdown", "# Conversation\n\n## Tool\n\noriginal\n"),
    ] {
        let path = dir.path().join(format!("conversation.{format}"));
        let (output, raw) = request(
            &[
                "sessions",
                "export",
                "s_owned",
                "--format",
                format,
                "--output",
                path.to_str().unwrap(),
            ],
            content.as_bytes(),
            "200 OK",
            "Content-Type: text/plain\r\n",
        );
        success(&output);
        assert!(output.stdout.is_empty());
        assert_eq!(std::fs::read(path).unwrap(), content.as_bytes());
        assert!(raw.starts_with(&format!(
            "GET /v1/sessions/s_owned/export?format={format} HTTP/1.1"
        )));
    }
    let (output, _) = request(
        &["sessions", "export", "s_owned", "--output", "-"],
        b"{}",
        "200 OK",
        "Content-Type: application/json\r\n",
    );
    success(&output);
    assert_eq!(output.stdout, b"{}");
}

#[test]
fn real_binary_workspace_archive_and_single_file_use_integrity_receipts() {
    let dir = directory();
    let archive = archive();
    let digest = format!("{:x}", Sha256::digest(&archive));
    let headers = format!(
        "Content-Type: application/zstd\r\nX-Snapshot-Sha256: {digest}\r\nX-Snapshot-Generation: 1\r\n"
    );
    let path = dir.path().join("workspace.tar.zst");
    let (output, raw) = request(
        &[
            "sessions",
            "workspace",
            "download",
            "s_owned",
            "--output",
            path.to_str().unwrap(),
        ],
        &archive,
        "200 OK",
        &headers,
    );
    success(&output);
    assert!(output.stdout.is_empty());
    assert!(raw.starts_with("GET /v1/sessions/s_owned/workspace/archive HTTP/1.1"));
    assert_eq!(std::fs::read(&path).unwrap(), archive);
    let path = dir.path().join("one.bin");
    let (output, _) = request(
        &[
            "sessions",
            "workspace",
            "file",
            "s_owned",
            "project/data.bin",
            "--output",
            path.to_str().unwrap(),
        ],
        &archive,
        "200 OK",
        &headers,
    );
    success(&output);
    assert_eq!(std::fs::read(path).unwrap(), b"hello\0workspace\xff");
}

#[test]
fn failed_downloads_never_publish_partial_files_or_expose_error_bodies() {
    let dir = directory();
    for (suffix, status, headers, contents) in [
        (
            "wrong-hash",
            "200 OK",
            format!(
                "Content-Type: application/zstd\r\nX-Snapshot-Sha256: {}\r\n",
                "0".repeat(64)
            ),
            b"archive".as_slice(),
        ),
        (
            "no-hash",
            "200 OK",
            "Content-Type: application/zstd\r\n".into(),
            b"archive".as_slice(),
        ),
        (
            "unauthorized",
            "403 Forbidden",
            "Content-Type: application/json\r\n".into(),
            b"sensitive-upstream-secret".as_slice(),
        ),
        (
            "busy",
            "409 Conflict",
            "Content-Type: application/json\r\n".into(),
            b"sensitive-upstream-secret".as_slice(),
        ),
    ] {
        let path = dir.path().join(suffix);
        let (output, _) = request(
            &[
                "sessions",
                "workspace",
                "download",
                "s_owned",
                "--output",
                path.to_str().unwrap(),
            ],
            contents,
            status,
            &headers,
        );
        assert!(!output.status.success());
        assert!(!path.exists());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("sensitive-upstream-secret"));
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn malformed_final_manifest_never_publishes_selected_file() {
    let dir = directory();
    for (index, manifest) in [
        b"{".as_slice(),
        br#"{"version":2,"entries":{}}"#.as_slice(),
        br#"{"version":1,"entries":{}}"#.as_slice(),
    ]
    .iter()
    .enumerate()
    {
        let archive = archive_with_manifest(Some(manifest));
        let headers = format!(
            "Content-Type: application/zstd\r\nX-Snapshot-Sha256: {:x}\r\n",
            Sha256::digest(&archive)
        );
        let path = dir.path().join(format!("selected-{index}"));
        let (output, _) = request(
            &[
                "sessions",
                "workspace",
                "file",
                "s_owned",
                "project/data.bin",
                "--output",
                path.to_str().unwrap(),
            ],
            &archive,
            "200 OK",
            &headers,
        );
        assert!(!output.status.success());
        assert!(!path.exists());
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn offline_toml_stdin_limits_and_private_errors_use_the_real_binary() {
    let dir = directory();
    let path = dir.path().join("template.toml");
    success(&command(&["agents", "init", path.to_str().unwrap()]));
    let source = std::fs::read(&path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_magents"))
        .args(["agents", "validate", "-"])
        .env_remove("MAGENTS_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&source).unwrap();
    success(&child.wait_with_output().unwrap());
    let mut private = b"api_key = 'never-echo-this-private-value'\n".to_vec();
    private.extend_from_slice(&source);
    std::fs::write(&path, private).unwrap();
    let output = command(&["agents", "validate", path.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("never-echo-this-private-value"));
    std::fs::write(&path, vec![b'x'; 1024 * 1024 + 1]).unwrap();
    let output = command(&["agents", "validate", path.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("exceeds 1 MiB"));
}

#[test]
fn output_paths_are_never_clobbered_and_invalid_inputs_need_no_network() {
    let dir = directory();
    let path = dir.path().join("precious");
    std::fs::write(&path, b"precious data").unwrap();
    for args in [
        vec!["agents", "init", path.to_str().unwrap()],
        vec![
            "agents",
            "download",
            "a_owned",
            "--output",
            path.to_str().unwrap(),
        ],
        vec![
            "sessions",
            "export",
            "s_owned",
            "--output",
            path.to_str().unwrap(),
        ],
        vec![
            "sessions",
            "workspace",
            "download",
            "s_owned",
            "--output",
            path.to_str().unwrap(),
        ],
    ] {
        let output = command(&args);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("already exists"));
        assert_eq!(std::fs::read(&path).unwrap(), b"precious data");
    }
    for selector in ["../private", "/etc/passwd", "C:\\private", "a//b"] {
        let output = command(&[
            "sessions",
            "workspace",
            "file",
            "s_owned",
            selector,
            "--output",
            dir.path().join("selected").to_str().unwrap(),
        ]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("normalized relative"));
    }
}

#[test]
fn progressive_help_exposes_toml_protocols_and_export_space_without_login() {
    for (args, expected) in [
        (vec!["agents", "--help"], "publish"),
        (vec!["agents", "publish", "--help"], "--agent"),
        (vec!["agents", "download", "--help"], "Never overwrites"),
        (vec!["sessions", "export", "--help"], "jsonl"),
        (vec!["sessions", "workspace", "--help"], "file"),
        (vec!["byok", "add", "--help"], "anthropic"),
    ] {
        let output = command(&args);
        success(&output);
        assert!(String::from_utf8(output.stdout).unwrap().contains(expected));
    }
}
