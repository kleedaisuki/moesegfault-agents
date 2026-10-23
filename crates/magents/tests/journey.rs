use std::{
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

fn request(
    args: &[&str],
    body: Option<&str>,
    response: &str,
    status: &str,
    content_type: &str,
    extra_headers: &str,
) -> (std::process::Output, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n{response}",
        response.len()
    );
    let server = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "CLI never contacted fixture"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("Cannot accept fixture connection: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut raw = Vec::new();
        let mut buffer = [0; 1024];
        loop {
            let size = stream.read(&mut buffer).unwrap();
            assert_ne!(size, 0);
            raw.extend_from_slice(&buffer[..size]);
            if let Some(end) = raw.windows(4).position(|v| v == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&raw[..end]).to_lowercase();
                let length: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap_or("0")
                    .parse()
                    .unwrap();
                if raw.len() >= end + 4 + length {
                    break;
                }
            }
        }
        stream.write_all(response.as_bytes()).unwrap();
        String::from_utf8(raw).unwrap()
    });
    let mut command = Command::new(env!("CARGO_BIN_EXE_magents"));
    command
        .env("MAGENTS_API_KEY", "test-key")
        .arg("--api-url")
        .arg(base)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::piped());
    let mut child = command.spawn().unwrap();
    if let Some(body) = body {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    (output, server.join().unwrap())
}

#[test]
fn real_binary_agent_session_and_steering_journey() {
    let (output, raw) = request(
        &["agents", "create", "-"],
        Some(r#"{"name":"research"}"#),
        r#"{"id":"a_1"}"#,
        "201 Created",
        "application/json",
        "",
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(raw.starts_with("POST /v1/agents HTTP/1.1"));
    assert!(
        raw.to_lowercase()
            .contains("authorization: bearer test-key")
    );
    assert!(raw.contains("research"));
    let (output, raw) = request(
        &["sessions", "start", "a_1"],
        None,
        r#"{"id":"s_1"}"#,
        "201 Created",
        "application/json",
        "",
    );
    assert!(output.status.success());
    assert!(raw.starts_with("POST /v1/agents/a_1/sessions HTTP/1.1"));
    let events = "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\r\n\r\ndata: [DONE]\r\n\r\n";
    let (output, raw) = request(
        &["run", "a_1", "hello", "--session", "s_1"],
        None,
        events,
        "200 OK",
        "text/event-stream",
        "x-agents-session: s_1\r\n",
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "hello world\n");
    assert!(String::from_utf8(output.stderr).unwrap().contains("s_1"));
    assert!(raw.to_lowercase().contains("x-agents-session: s_1"));
    let (output, raw) = request(
        &["sessions", "steer", "s_1", "focus on caching"],
        None,
        r#"{"accepted":true}"#,
        "202 Accepted",
        "application/json",
        "",
    );
    assert!(output.status.success());
    assert!(raw.starts_with("POST /v1/sessions/s_1/steer HTTP/1.1"));
    assert!(raw.contains("focus on caching"));
    let (output, raw) = request(
        &["sessions", "close", "s_1"],
        None,
        "",
        "204 No Content",
        "application/json",
        "",
    );
    assert!(output.status.success());
    assert!(raw.starts_with("DELETE /v1/sessions/s_1 HTTP/1.1"));
}

#[test]
fn diagnostics_preview_is_offline_and_requires_upload_consent() {
    let output = Command::new(env!("CARGO_BIN_EXE_magents"))
        .arg("diagnostics")
        .env_remove("MAGENTS_API_KEY")
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report.as_object().unwrap().len(), 4);
    assert_eq!(report["consent"], true);
    let output = Command::new(env!("CARGO_BIN_EXE_magents"))
        .args(["diagnostics", "--upload"])
        .output()
        .unwrap();
    assert!(!output.status.success());
}

#[test]
fn upstream_error_never_dumps_sensitive_body() {
    let (output, _) = request(
        &["usage"],
        None,
        r#"{"message":"secret-provider-key"}"#,
        "502 Bad Gateway",
        "application/json",
        "x-moesegfault-correlation-id: trace_123\r\n",
    );
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("trace_123"));
    assert!(!error.contains("secret-provider-key"));
}

#[test]
fn truncated_stream_is_not_silent_success() {
    let events = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
    let (output, _) = request(
        &["run", "a_1", "hello"],
        None,
        events,
        "200 OK",
        "text/event-stream",
        "",
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("completion marker")
    );
}

#[test]
fn account_management_uses_deployed_contracts() {
    for (args, expected) in [
        (vec!["keys", "list"], "GET /v1/api-keys HTTP/1.1"),
        (vec!["byok", "list"], "GET /v1/provider-keys HTTP/1.1"),
        (vec!["sessions", "list"], "GET /v1/sessions HTTP/1.1"),
        (
            vec!["sessions", "resume", "s_1"],
            "POST /v1/sessions/s_1/resume HTTP/1.1",
        ),
    ] {
        let (output, raw) = request(&args, None, "{}", "200 OK", "application/json", "");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(raw.starts_with(expected));
    }
    let (output, raw) = request(
        &["diagnostics", "--upload", "--consent"],
        None,
        "{}",
        "200 OK",
        "application/json",
        "",
    );
    assert!(output.status.success());
    assert!(raw.starts_with("POST /v1/diagnostics HTTP/1.1"));
    let report: serde_json::Value =
        serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(report["consent"], true);
    assert_eq!(report["client"], "magents");
    assert_eq!(report.as_object().unwrap().len(), 4);
}

#[test]
fn personalized_skill_registry_lifecycle_supports_stdin_and_files() {
    let initial = r#"{"name":"review-rust","instructions":"Check ownership and compatibility."}"#;
    let first = r#"{"name":"review-rust","version":"digest-1","instructions":"Check ownership and compatibility.","source":"account"}"#;
    let (output, raw) = request(
        &["skills", "create", "-"],
        Some(initial),
        first,
        "201 Created",
        "application/json",
        "",
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(raw.starts_with("POST /v1/skills HTTP/1.1"));
    let submitted: serde_json::Value =
        serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(submitted["name"], "review-rust");
    assert!(submitted.get("version").is_none());

    for args in [
        vec!["skills", "list"],
        vec!["skills", "show", "review-rust"],
    ] {
        let expected = if args[1] == "list" {
            "/v1/skills"
        } else {
            "/v1/skills/review-rust"
        };
        let (output, raw) = request(&args, None, first, "200 OK", "application/json", "");
        assert!(output.status.success());
        assert!(raw.starts_with(&format!("GET {expected} HTTP/1.1")));
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .contains("digest-1")
        );
    }

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let cache = root.join(".cache/cli-tests");
    std::fs::create_dir_all(&cache).unwrap();
    let path = cache.join(format!("skill-update-{}.json", std::process::id()));
    std::fs::write(&path, r#"{"instructions":"Also inspect session pinning."}"#).unwrap();
    let (output, raw) = request(
        &["skills", "update", "review-rust", path.to_str().unwrap()],
        None,
        r#"{"name":"review-rust","version":"digest-2","instructions":"Also inspect session pinning.","source":"account"}"#,
        "200 OK",
        "application/json",
        "",
    );
    std::fs::remove_file(path).unwrap();
    assert!(output.status.success());
    assert!(raw.starts_with("PUT /v1/skills/review-rust HTTP/1.1"));
    let submitted: serde_json::Value =
        serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(submitted.as_object().unwrap().len(), 1);
    assert_eq!(submitted["instructions"], "Also inspect session pinning.");

    let (output, raw) = request(
        &["skills", "delete", "review-rust"],
        None,
        "",
        "204 No Content",
        "application/json",
        "",
    );
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(raw.starts_with("DELETE /v1/skills/review-rust HTTP/1.1"));
}

#[test]
fn skills_progressive_help_describes_json_without_authentication() {
    for (args, expected) in [
        (vec!["skills", "--help"], "create"),
        (vec!["skills", "create", "--help"], "instructions"),
        (vec!["skills", "update", "--help"], "stdin"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_magents"))
            .args(&args)
            .env_remove("MAGENTS_API_KEY")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8(output.stdout).unwrap().contains(expected));
    }
}
