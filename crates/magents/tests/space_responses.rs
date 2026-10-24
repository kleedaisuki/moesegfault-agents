use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Output},
    thread,
    time::{Duration, Instant},
};

fn directory() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.cache/cli-tests");
    std::fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}

fn offline(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_magents"))
        .args(args)
        .env_remove("MAGENTS_API_KEY")
        .env_remove("MAGENTS_API_URL")
        .output()
        .unwrap()
}

fn request(args: &[&str], response: &[u8], extra_headers: &str) -> (Output, Vec<u8>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let mut wire = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
        response.len()
    )
    .into_bytes();
    wire.extend_from_slice(response);
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let (mut socket, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "CLI did not reach fixture");
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("Cannot accept fixture connection: {error}"),
            }
        };
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut request = Vec::new();
        loop {
            let mut chunk = [0u8; 16 * 1024];
            let size = socket.read(&mut chunk).unwrap();
            assert_ne!(size, 0);
            request.extend_from_slice(&chunk[..size]);
            if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                let length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap_or("0")
                    .parse::<usize>()
                    .unwrap();
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        let _ = socket.write_all(&wire);
        request
    });
    let temp = directory();
    let output = Command::new(env!("CARGO_BIN_EXE_magents"))
        .args(["--api-url", &origin])
        .args(args)
        .env("MAGENTS_API_KEY", "fixture-only-key")
        .env("TMP", temp.path())
        .env("TEMP", temp.path())
        .env("TMPDIR", temp.path())
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

fn headers(raw: &[u8]) -> String {
    let end = raw.windows(4).position(|part| part == b"\r\n\r\n").unwrap();
    String::from_utf8(raw[..end].to_vec()).unwrap()
}

fn body(raw: &[u8]) -> &[u8] {
    let end = raw.windows(4).position(|part| part == b"\r\n\r\n").unwrap();
    &raw[end + 4..]
}

#[test]
fn streamed_binary_upload_has_exact_bytes_name_length_and_hash() {
    let temp = directory();
    let path = temp.path().join("input.bin");
    let bytes: Vec<u8> = (0..150_001).map(|index| (index % 251) as u8).collect();
    std::fs::write(&path, &bytes).unwrap();
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let response =
        serde_json::to_vec(&json!({"id":"file_1","bytes":bytes.len(),"sha256":digest})).unwrap();
    let (output, raw) = request(
        &[
            "space",
            "files",
            "upload",
            path.to_str().unwrap(),
            "--name",
            "研究 notes.bin",
        ],
        &response,
        "Content-Type: application/json\r\n",
    );
    success(&output);
    let head = headers(&raw).to_ascii_lowercase();
    assert!(head.starts_with(
        "post /v1/space/files?filename=%e7%a0%94%e7%a9%b6+notes.bin&purpose=user_data http/1.1"
    ));
    assert!(head.contains(&format!("x-content-sha256: {digest}")));
    assert!(head.contains("content-length: 150001"));
    assert!(head.contains("authorization: bearer fixture-only-key"));
    assert_eq!(body(&raw), bytes);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["id"],
        "file_1"
    );
}

#[test]
fn upload_rejects_mismatched_receipt_without_retry() {
    let temp = directory();
    let path = temp.path().join("input.txt");
    std::fs::write(&path, b"hello").unwrap();
    let (output, _) = request(
        &["space", "files", "upload", path.to_str().unwrap()],
        br#"{"id":"file_1","bytes":5,"sha256":"wrong"}"#,
        "Content-Type: application/json\r\n",
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("inspect Space files before retrying")
    );
}

#[test]
fn downloads_publish_only_after_valid_integrity_and_never_clobber() {
    let temp = directory();
    let bytes = b"user\0file\xff\n";
    let digest = format!("{:x}", Sha256::digest(bytes));
    let path = temp.path().join("download.bin");
    let args = [
        "space",
        "files",
        "download",
        "file_1",
        "--output",
        path.to_str().unwrap(),
    ];
    let (output, raw) = request(&args, bytes, &format!("X-Content-Sha256: {digest}\r\n"));
    success(&output);
    assert!(headers(&raw).starts_with("GET /v1/files/file_1/content HTTP/1.1"));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let no_clobber = offline(&args);
    assert!(!no_clobber.status.success());
    assert!(String::from_utf8_lossy(&no_clobber.stderr).contains("already exists"));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);

    let invalid = temp.path().join("invalid.bin");
    let (output, _) = request(
        &[
            "space",
            "files",
            "download",
            "file_1",
            "--output",
            invalid.to_str().unwrap(),
        ],
        bytes,
        &format!("X-Content-Sha256: {}\r\n", "0".repeat(64)),
    );
    assert!(!output.status.success());
    assert!(!invalid.exists());
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
}

#[test]
fn space_management_pages_and_workspace_operations_use_owned_resource_routes() {
    for (args, method, path, expected) in [
        (vec!["space", "show"], "GET", "/v1/space", None),
        (
            vec!["space", "rename", "Research"],
            "PATCH",
            "/v1/space",
            Some(json!({"name":"Research"})),
        ),
        (
            vec![
                "space", "files", "list", "--limit", "7", "--after", "file_1",
            ],
            "GET",
            "/v1/files?limit=7&after=file_1",
            None,
        ),
        (
            vec!["space", "files", "show", "file_1"],
            "GET",
            "/v1/files/file_1",
            None,
        ),
        (
            vec!["space", "files", "rename", "file_1", "new.txt"],
            "PATCH",
            "/v1/files/file_1",
            Some(json!({"filename":"new.txt"})),
        ),
        (
            vec!["space", "files", "delete", "file_1"],
            "DELETE",
            "/v1/files/file_1",
            None,
        ),
        (
            vec!["space", "versions", "list"],
            "GET",
            "/v1/space/workspace-versions?limit=20",
            None,
        ),
        (
            vec!["space", "versions", "show", "wv_1"],
            "GET",
            "/v1/space/workspace-versions/wv_1",
            None,
        ),
        (
            vec![
                "sessions",
                "workspace",
                "commit",
                "s_1",
                "--name",
                "Before migration",
                "--operation-id",
                "commit_1",
            ],
            "POST",
            "/v1/sessions/s_1/workspace/commit",
            Some(json!({"name":"Before migration","operation_id":"commit_1"})),
        ),
        (
            vec![
                "sessions",
                "workspace",
                "restore",
                "s_1",
                "wv_1",
                "--operation-id",
                "restore_1",
            ],
            "POST",
            "/v1/sessions/s_1/workspace/restore",
            Some(json!({"version_id":"wv_1","operation_id":"restore_1"})),
        ),
    ] {
        let (output, raw) = request(&args, b"{}", "Content-Type: application/json\r\n");
        success(&output);
        assert!(headers(&raw).starts_with(&format!("{method} {path} HTTP/1.1")));
        if let Some(expected) = expected {
            assert_eq!(
                serde_json::from_slice::<Value>(body(&raw)).unwrap(),
                expected
            );
        }
    }
}

#[test]
fn workspace_operation_identity_is_generated_once_and_preserved_for_explicit_recovery() {
    let mut generated = Vec::new();
    for _ in 0..2 {
        let (output, raw) = request(
            &["sessions", "workspace", "commit", "s_1"],
            br#"{"id":"wv_1"}"#,
            "Content-Type: application/json\r\n",
        );
        success(&output);
        let request: Value = serde_json::from_slice(body(&raw)).unwrap();
        let id = request["operation_id"].as_str().unwrap();
        assert!(uuid::Uuid::parse_str(id).is_ok());
        assert!(String::from_utf8_lossy(&output.stderr).contains(&format!("Operation: {id}")));
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap()["operation_id"],
            id
        );
        generated.push(id.to_owned());
    }
    assert_ne!(generated[0], generated[1]);
    let mut prior_request = None;
    for _ in 0..2 {
        let (output, raw) = request(
            &[
                "sessions",
                "workspace",
                "restore",
                "s_1",
                "wv_1",
                "--operation-id",
                "recover_restore_1",
            ],
            br#"{"version_id":"wv_1","operation_id":"recover_restore_1"}"#,
            "Content-Type: application/json\r\n",
        );
        success(&output);
        let value: Value = serde_json::from_slice(body(&raw)).unwrap();
        assert_eq!(value["operation_id"], "recover_restore_1");
        if let Some(prior) = &prior_request {
            assert_eq!(prior, &value);
        }
        prior_request = Some(value);
    }
    for invalid in ["", "../bad", "contains space", &"x".repeat(129)] {
        let output = offline(&[
            "sessions",
            "workspace",
            "commit",
            "s_1",
            "--operation-id",
            invalid,
        ]);
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("Operation:"));
    }
}

#[test]
fn response_json_bound_is_distinct_from_portable_configuration_bound() {
    let large = serde_json::to_vec(&json!({"id":"resp_1","output":[{"type":"message","content":[{"type":"output_text","text":"x".repeat(2*1024*1024)}]}]})).unwrap();
    for args in [
        vec!["responses", "show", "resp_1"],
        vec!["responses", "run", "a_1", "hello"],
    ] {
        let (output, _) = request(&args, &large, "Content-Type: application/json\r\n");
        success(&output);
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap()["id"],
            "resp_1"
        );
    }
    let oversized = vec![b' '; 16 * 1024 * 1024 + 1];
    let (output, _) = request(
        &["responses", "show", "resp_1"],
        &oversized,
        "Content-Type: application/json\r\n",
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("exceeds 16 MiB"));
    assert!(output.stdout.is_empty());

    let temp = directory();
    let destination = temp.path().join("agent.toml");
    let (output, _) = request(
        &[
            "agents",
            "download",
            "a_1",
            "--output",
            destination.to_str().unwrap(),
        ],
        &large,
        "Content-Type: application/json\r\n",
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("exceeds 1 MiB"));
    assert!(!destination.exists());
}

#[test]
fn immutable_versions_download_verify_archive_receipt_without_extraction() {
    let temp = directory();
    let path = temp.path().join("version.tar.zst");
    let bytes = b"opaque snapshot payload\0\xff";
    let digest = format!("{:x}", Sha256::digest(bytes));
    let (output, raw) = request(
        &[
            "space",
            "versions",
            "download",
            "wv_1",
            "--output",
            path.to_str().unwrap(),
        ],
        bytes,
        &format!("Content-Type: application/zstd\r\nX-Snapshot-Sha256: {digest}\r\n"),
    );
    success(&output);
    assert!(headers(&raw).starts_with("GET /v1/space/workspace-versions/wv_1/content HTTP/1.1"));
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    assert!(
        !offline(&["space", "versions", "delete", "wv_1"])
            .status
            .success()
    );
}

#[test]
fn responses_create_preserves_local_tool_contract_without_executing_it() {
    let temp = directory();
    let path = temp.path().join("request.json");
    let request_body = json!({
        "model":"a_1", "previous_response_id":"resp_1",
        "input":[{"type":"function_call_output","call_id":"call_1","output":"authorized result"}],
        "tools":[{"type":"function","name":"local_action","description":"A local function","parameters":{"type":"object","properties":{}}}]
    });
    std::fs::write(&path, serde_json::to_vec(&request_body).unwrap()).unwrap();
    let (output, raw) = request(
        &["responses", "create", path.to_str().unwrap(), "--idempotency-key", "owned-operation-1"],
        br#"{"id":"resp_2","status":"completed","output":[{"type":"function_call","name":"local_action","arguments":"{}","call_id":"call_2"}]}"#,
        "Content-Type: application/json\r\n",
    );
    success(&output);
    assert!(headers(&raw).starts_with("POST /v1/responses HTTP/1.1"));
    assert!(
        headers(&raw)
            .to_ascii_lowercase()
            .contains("idempotency-key: owned-operation-1")
    );
    assert_eq!(
        serde_json::from_slice::<Value>(body(&raw)).unwrap(),
        request_body
    );
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["output"][0]["name"],
        "local_action"
    );
}

#[test]
fn responses_background_retrieval_cancellation_and_input_paging_are_discoverable() {
    let (output, raw) = request(
        &[
            "responses",
            "run",
            "a_1",
            "Continue",
            "--previous",
            "resp_1",
            "--background",
        ],
        br#"{"id":"resp_2","status":"queued"}"#,
        "Content-Type: application/json\r\n",
    );
    success(&output);
    assert_eq!(
        serde_json::from_slice::<Value>(body(&raw)).unwrap(),
        json!({"model":"a_1","input":"Continue","previous_response_id":"resp_1","stream":false,"background":true})
    );
    for (args, method, path) in [
        (
            vec!["responses", "show", "resp_2"],
            "GET",
            "/v1/responses/resp_2",
        ),
        (
            vec!["responses", "cancel", "resp_2"],
            "POST",
            "/v1/responses/resp_2/cancel",
        ),
        (
            vec!["responses", "delete", "resp_2"],
            "DELETE",
            "/v1/responses/resp_2",
        ),
        (
            vec![
                "responses",
                "inputs",
                "resp_2",
                "--limit",
                "9",
                "--after",
                "item_1",
            ],
            "GET",
            "/v1/responses/resp_2/input_items?limit=9&after=item_1",
        ),
    ] {
        let (output, raw) = request(&args, b"{}", "Content-Type: application/json\r\n");
        success(&output);
        assert!(headers(&raw).starts_with(&format!("{method} {path} HTTP/1.1")));
    }
}

fn response_events(terminal: &str) -> Vec<u8> {
    format!(
        "event: response.created\ndata: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp_1\",\"status\":\"in_progress\"}}}}\n\n\
         : keepalive\n\n\
         event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"Hello 世界\"}}\n\n{terminal}"
    ).into_bytes()
}

#[test]
fn response_stream_text_and_full_events_require_authoritative_terminal() {
    let terminal = "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[]}}\n\n";
    for events in [false, true] {
        let mut args = vec!["responses", "run", "a_1", "Hello", "--stream"];
        if events {
            args.push("--events");
        }
        let (output, raw) = request(
            &args,
            &response_events(terminal),
            "Content-Type: text/event-stream\r\nx-agents-session: s_1\r\n",
        );
        success(&output);
        assert_eq!(
            serde_json::from_slice::<Value>(body(&raw)).unwrap()["stream"],
            true
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("Response: resp_1"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("Session: s_1"));
        if events {
            let values: Vec<Value> = String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(values.len(), 3);
            assert_eq!(values[2]["type"], "response.completed");
        } else {
            assert_eq!(String::from_utf8(output.stdout).unwrap(), "Hello 世界\n");
        }
    }
    for terminal in [
        "data: [DONE]\n\n",
        "",
        "data: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp_1\",\"error\":{\"message\":\"private-upstream-body\"}}}\n\n",
    ] {
        let (output, _) = request(
            &["responses", "run", "a_1", "Hello", "--stream"],
            &response_events(terminal),
            "Content-Type: text/event-stream\r\n",
        );
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private-upstream-body"));
    }
}

#[test]
fn response_reconnect_uses_saved_event_cursor_without_replaying_input() {
    let event = b"event: response.completed\ndata: {\"type\":\"response.completed\",\"sequence_number\":18,\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[]}}\n\n";
    let (output, raw) = request(
        &[
            "responses",
            "show",
            "resp_1",
            "--stream",
            "--starting-after",
            "17",
            "--events",
        ],
        event,
        "Content-Type: text/event-stream\r\n",
    );
    success(&output);
    assert!(
        headers(&raw)
            .starts_with("GET /v1/responses/resp_1?stream=true&starting_after=17 HTTP/1.1")
    );
    assert!(body(&raw).is_empty());
    let event: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(event["sequence_number"], 18);
}

#[test]
fn background_stream_acceptance_without_terminal_is_not_execution_success() {
    let events = b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_queued\",\"status\":\"queued\"}}\n\n";
    let (output, raw) = request(
        &[
            "responses",
            "run",
            "a_1",
            "hello",
            "--background",
            "--stream",
        ],
        events,
        "Content-Type: text/event-stream\r\n",
    );
    assert!(!output.status.success());
    let request: Value = serde_json::from_slice(body(&raw)).unwrap();
    assert_eq!(request["background"], true);
    assert_eq!(request["stream"], true);
    assert!(String::from_utf8_lossy(&output.stderr).contains("Response: resp_queued"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("without a terminal response"));
}

#[test]
fn progressive_help_and_local_rejections_require_no_login() {
    for (args, expected) in [
        (vec!["--help"], "space"),
        (vec!["space", "--help"], "versions"),
        (vec!["space", "files", "--help"], "upload"),
        (vec!["space", "files", "upload", "--help"], "--name"),
        (vec!["space", "versions", "--help"], "download"),
        (vec!["responses", "--help"], "inputs"),
        (vec!["responses", "create", "--help"], "never auto-executed"),
        (
            vec!["sessions", "workspace", "restore", "--help"],
            "safety version",
        ),
    ] {
        let output = offline(&args);
        success(&output);
        assert!(String::from_utf8_lossy(&output.stdout).contains(expected));
    }
    for args in [
        vec!["space", "files", "list", "--limit", "0"],
        vec!["space", "versions", "list", "--limit", "101"],
        vec!["space", "files", "rename", "file_1", "../unsafe"],
        vec!["responses", "show", "resp_1", "--starting-after", "7"],
        vec!["responses", "show", "../account"],
    ] {
        assert!(!offline(&args).status.success());
    }
    let temp = directory();
    let path = temp.path().join("large.bin");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(64 * 1024 * 1024 + 1).unwrap();
    let output = offline(&["space", "files", "upload", path.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no larger than 64 MiB"));
}
