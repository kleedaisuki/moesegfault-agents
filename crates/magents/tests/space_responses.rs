use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

fn directory() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.temp/cli-tests");
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
    request_fixture(args, 200, response, extra_headers, None)
}

fn request_fixture(
    args: &[&str],
    status: u16,
    response: &[u8],
    extra_headers: &str,
    input: Option<&[u8]>,
) -> (Output, Vec<u8>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let mut wire = format!(
        "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
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
    let mut child = Command::new(env!("CARGO_BIN_EXE_magents"))
        .args(["--api-url", &origin])
        .args(args)
        .env("MAGENTS_API_KEY", "fixture-only-key")
        .env("TMP", temp.path())
        .env("TEMP", temp.path())
        .env("TMPDIR", temp.path())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = input {
        child.stdin.take().unwrap().write_all(input).unwrap();
    }
    let output = child.wait_with_output().unwrap();
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
fn responses_bind_an_existing_session_without_changing_the_request_body() {
    let temp = directory();
    let path = temp.path().join("request.json");
    let request_body = json!({"model":"a_1","input":"hello"});
    std::fs::write(&path, serde_json::to_vec(&request_body).unwrap()).unwrap();
    for args in [
        vec![
            "responses",
            "create",
            path.to_str().unwrap(),
            "--session",
            "s_1",
        ],
        vec!["responses", "run", "a_1", "hello", "--session", "s_1"],
    ] {
        let (output, raw) = request(
            &args,
            br#"{"id":"resp_1","status":"completed","output":[]}"#,
            "Content-Type: application/json\r\n",
        );
        success(&output);
        assert!(
            headers(&raw)
                .to_ascii_lowercase()
                .contains("x-agents-session: s_1")
        );
        let accepted: Value = serde_json::from_slice(body(&raw)).unwrap();
        assert_eq!(accepted["model"], "a_1");
        assert_eq!(accepted["input"], "hello");
        assert!(accepted.get("session").is_none());
    }
}

#[test]
fn responses_reject_invalid_session_before_authentication_or_dispatch() {
    let output = offline(&[
        "responses",
        "run",
        "a_1",
        "hello",
        "--session",
        "../invalid",
    ]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Identifier must contain only letters, numbers, '_' or '-'")
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
        !headers(&raw)
            .to_ascii_lowercase()
            .contains("x-agents-session:")
    );
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

#[test]
fn synchronous_response_failure_exits_nonzero_and_preserves_full_json_result() {
    let temp = directory();
    let path = temp.path().join("request.json");
    std::fs::write(&path, br#"{"model":"a_1","input":"hello"}"#).unwrap();
    for status in ["failed", "incomplete", "cancelled"] {
        let result = json!({"id":"resp_failed","status":status,"output":[],"error":{"message":"private-provider-detail"}});
        for args in [
            vec!["responses", "run", "a_1", "hello"],
            vec!["responses", "create", path.to_str().unwrap()],
        ] {
            let (output, raw) = request(
                &args,
                &serde_json::to_vec(&result).unwrap(),
                "Content-Type: application/json\r\n",
            );
            assert!(!output.status.success());
            assert!(headers(&raw).starts_with("POST /v1/responses HTTP/1.1"));
            assert_eq!(
                serde_json::from_slice::<Value>(&output.stdout).unwrap(),
                result
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains(&format!("Response execution {status}")));
            assert!(!stderr.contains("private-provider-detail"));
        }
    }
}

#[test]
fn background_response_submission_exits_zero_without_claiming_completion() {
    for status in ["queued", "in_progress"] {
        let result = json!({"id":"resp_pending","status":status,"output":[]});
        let (output, _) = request(
            &["responses", "run", "a_1", "hello", "--background"],
            &serde_json::to_vec(&result).unwrap(),
            "Content-Type: application/json\r\n",
        );
        success(&output);
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            result
        );
    }
}

#[test]
fn response_failure_inspection_exits_zero_for_a_valid_http_object() {
    let result = json!({"id":"resp_failed","status":"failed","output":[],"error":{"message":"private-provider-detail"}});
    let (output, raw) = request(
        &["responses", "show", "resp_failed"],
        &serde_json::to_vec(&result).unwrap(),
        "Content-Type: application/json\r\n",
    );
    success(&output);
    assert!(headers(&raw).starts_with("GET /v1/responses/resp_failed HTTP/1.1"));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        result
    );
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

#[test]
fn auxiliary_commands_preserve_native_requests_and_complete_compacted_windows() {
    let temp = directory();
    let path = temp.path().join("request.json");
    let input = json!({"model":"owned_agent","instructions":"Keep this exact layer.  研究",
        "input":[{"role":"user","content":"Original user request"},
        {"type":"compaction","id":"cmp_old","encrypted_content":"opaque-old"}]});
    let bytes = serde_json::to_vec(&input).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    let window = json!({"object":"response.compaction","id":"cmp_new","created_at":1,
        "output":[{"role":"user","content":"Original user request"},
        {"type":"compaction","id":"cmp_new","encrypted_content":"opaque-new"}],
        "usage":{"input_tokens":23,"output_tokens":7,"total_tokens":30}});
    for file in [path.to_str().unwrap(), "-"] {
        for (command, route, response) in [
            (
                "count",
                "input_tokens",
                json!({"object":"response.input_tokens","input_tokens":23}),
            ),
            ("compact", "compact", window.clone()),
        ] {
            let mut args = vec!["responses", command, file];
            if command == "compact" {
                args.extend(["--idempotency-key", "explicit-compact-1"]);
            }
            let (output, raw) = request_fixture(
                &args,
                200,
                &serde_json::to_vec(&response).unwrap(),
                "Content-Type: application/json\r\n",
                (file == "-").then_some(bytes.as_slice()),
            );
            success(&output);
            assert!(headers(&raw).starts_with(&format!("POST /v1/responses/{route} HTTP/1.1")));
            assert_eq!(serde_json::from_slice::<Value>(body(&raw)).unwrap(), input);
            assert_eq!(
                serde_json::from_slice::<Value>(&output.stdout).unwrap(),
                response
            );
            assert!(output.stderr.is_empty());
            if command == "compact" {
                assert!(
                    headers(&raw)
                        .to_ascii_lowercase()
                        .contains("idempotency-key: explicit-compact-1")
                );
            }
        }
    }
}

#[test]
fn native_conversation_crud_items_and_container_routes_preserve_json() {
    let temp = directory();
    let path = temp.path().join("request.json");
    let file = path.to_str().unwrap();
    let request_body = json!({"metadata":{"label":"Research"},"items":[]});
    std::fs::write(&path, serde_json::to_vec(&request_body).unwrap()).unwrap();
    for (args, method, route, expected) in [
        (
            vec![
                "conversations",
                "create",
                file,
                "--idempotency-key",
                "explicit-create-1",
            ],
            "POST",
            "/v1/conversations",
            Some(request_body.clone()),
        ),
        (
            vec!["conversations", "show", "conv_1"],
            "GET",
            "/v1/conversations/conv_1",
            None,
        ),
        (
            vec!["conversations", "update", "conv_1", file],
            "POST",
            "/v1/conversations/conv_1",
            Some(json!({"metadata":{"label":"Updated"}})),
        ),
        (
            vec!["conversations", "delete", "conv_1"],
            "DELETE",
            "/v1/conversations/conv_1",
            None,
        ),
        (
            vec!["conversations", "purge", "conv_1"],
            "POST",
            "/v1/conversations/conv_1/purge",
            None,
        ),
        (
            vec![
                "conversations",
                "items",
                "conv_1",
                "list",
                "--limit",
                "7",
                "--after",
                "item_1",
                "--order",
                "desc",
            ],
            "GET",
            "/v1/conversations/conv_1/items?limit=7&after=item_1&order=desc",
            None,
        ),
        (
            vec!["conversations", "items", "conv_1", "show", "item_1"],
            "GET",
            "/v1/conversations/conv_1/items/item_1",
            None,
        ),
        (
            vec!["conversations", "items", "conv_1", "create", file],
            "POST",
            "/v1/conversations/conv_1/items",
            Some(json!({"items":[{"role":"user","content":"Preserved original"}]})),
        ),
        (
            vec!["conversations", "items", "conv_1", "delete", "item_1"],
            "DELETE",
            "/v1/conversations/conv_1/items/item_1",
            None,
        ),
        (
            vec!["containers", "show", "cntr_1"],
            "GET",
            "/v1/containers/cntr_1",
            None,
        ),
    ] {
        if let Some(expected) = &expected {
            std::fs::write(&path, serde_json::to_vec(expected).unwrap()).unwrap();
        }
        let response = json!({"id":"owned_object","typed":{"nested":[1,"unchanged"]}});
        let (output, raw) = request(
            &args,
            &serde_json::to_vec(&response).unwrap(),
            "Content-Type: application/json\r\n",
        );
        success(&output);
        assert!(headers(&raw).starts_with(&format!("{method} {route} HTTP/1.1")));
        assert!(
            headers(&raw)
                .to_ascii_lowercase()
                .contains("authorization: bearer fixture-only-key")
        );
        if let Some(expected) = expected {
            assert_eq!(
                serde_json::from_slice::<Value>(body(&raw)).unwrap(),
                expected
            );
        } else {
            assert!(body(&raw).is_empty());
        }
        if args[1] == "create" {
            assert!(
                headers(&raw)
                    .to_ascii_lowercase()
                    .contains("idempotency-key: explicit-create-1")
            );
        }
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            response
        );
    }
    let (output, raw) = request_fixture(&["containers", "delete", "cntr_1"], 204, b"", "", None);
    success(&output);
    assert!(headers(&raw).starts_with("DELETE /v1/containers/cntr_1 HTTP/1.1"));
    assert!(output.stdout.is_empty());
}

#[test]
fn native_context_help_and_local_rejections_work_without_login() {
    for (args, expected) in [
        (vec!["--help"], "conversations"),
        (vec!["--help"], "containers"),
        (vec!["responses", "--help"], "count"),
        (vec!["responses", "--help"], "compact"),
        (vec!["responses", "compact", "--help"], "--idempotency-key"),
        (vec!["conversations", "--help"], "purge"),
        (vec!["conversations", "items", "conv_1", "--help"], "list"),
        (vec!["containers", "--help"], "delete"),
    ] {
        let output = offline(&args);
        success(&output);
        assert!(String::from_utf8_lossy(&output.stdout).contains(expected));
    }
    for args in [
        vec!["conversations", "show", "../account"],
        vec!["conversations", "purge", "conv_1?other"],
        vec!["conversations", "items", "conv_1", "show", "../../account"],
        vec!["conversations", "items", "conv_1", "list", "--limit", "101"],
        vec![
            "conversations",
            "items",
            "conv_1",
            "list",
            "--after",
            "item?other",
        ],
        vec![
            "conversations",
            "items",
            "conv_1",
            "list",
            "--order",
            "random",
        ],
        vec!["containers", "delete", "../account"],
    ] {
        let output = offline(&args);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
    let temp = directory();
    let path = temp.path().join("request.json");
    std::fs::write(&path, "[]").unwrap();
    for command in ["count", "compact"] {
        let output = offline(&["responses", command, path.to_str().unwrap()]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("JSON object"));
    }
    std::fs::write(&path, "{}").unwrap();
    for command in ["compact", "create"] {
        let output = offline(&[
            "responses",
            command,
            path.to_str().unwrap(),
            "--idempotency-key",
            "invalid key",
        ]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Idempotency key"));
    }
}

#[test]
fn failed_native_operations_report_http_failure_without_replay_or_body_leaks() {
    let temp = directory();
    let path = temp.path().join("request.json");
    std::fs::write(&path, r#"{"model":"owned_agent","input":"hello"}"#).unwrap();
    for (args, code) in [
        (
            vec![
                "responses",
                "compact",
                path.to_str().unwrap(),
                "--idempotency-key",
                "explicit-compact-1",
            ],
            "idempotency_conflict",
        ),
        (
            vec!["responses", "count", path.to_str().unwrap()],
            "token_count_unavailable",
        ),
        (
            vec!["conversations", "purge", "conv_1"],
            "conversation_purge_conflict",
        ),
    ] {
        let response = json!({"error":{"code":code,"message":"Explicit recovery required"}});
        let (output, _) = request_fixture(
            &args,
            409,
            &serde_json::to_vec(&response).unwrap(),
            "Content-Type: application/json\r\n",
            None,
        );
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("409"));
        assert!(error.contains("correlation ID"));
        assert!(!error.contains(code));
        assert!(!error.contains("Explicit recovery required"));
    }
}
