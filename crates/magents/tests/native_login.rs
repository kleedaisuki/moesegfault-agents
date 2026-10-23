use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use url::Url;

struct RunningCli(Child);

impl Drop for RunningCli {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn manual_login_is_discoverable_without_a_network_or_credentials() {
    let output = Command::new(env!("CARGO_BIN_EXE_magents"))
        .args(["auth", "login", "--help"])
        .env_remove("MAGENTS_API_KEY")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("--no-browser")
    );
}

#[test]
#[ignore = "opt-in public deployment discovery; no account or interactive browser is used"]
fn manual_login_real_binary_receives_a_validated_loopback_denial() {
    let origin = std::env::var("MAGENTS_TEST_NATIVE_LOGIN_API_URL")
        .expect("Set MAGENTS_TEST_NATIVE_LOGIN_API_URL to the approved deployment API origin");
    let child = Command::new(env!("CARGO_BIN_EXE_magents"))
        .args(["--api-url", &origin, "auth", "login", "--no-browser"])
        .env_remove("MAGENTS_API_KEY")
        .env_remove("MAGENTS_OIDC_ISSUER")
        .env_remove("MAGENTS_OIDC_CLIENT_ID")
        .env_remove("MAGENTS_OIDC_REDIRECT_URI")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut cli = RunningCli(child);
    let stdout = cli.0.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).unwrap();
        let _ = sender.send(line);
    });
    let line = receiver
        .recv_timeout(Duration::from_secs(75))
        .expect("CLI did not emit its authorization URL");
    let authorization =
        Url::parse(line.trim()).expect("stdout must contain a standalone authorization URL");
    assert_eq!(authorization.scheme(), "https");
    let fields: HashMap<_, _> = authorization.query_pairs().into_owned().collect();
    assert_eq!(fields["response_type"], "code");
    assert_eq!(fields["code_challenge_method"], "S256");
    for name in ["state", "nonce", "code_challenge", "client_id"] {
        assert!(!fields[name].is_empty());
    }
    for name in [
        "code",
        "code_verifier",
        "client_secret",
        "access_token",
        "refresh_token",
    ] {
        assert!(
            !fields.contains_key(name),
            "authorization URL disclosed a credential field"
        );
    }
    let mut redirect = Url::parse(&fields["redirect_uri"]).unwrap();
    assert!(matches!(redirect.host_str(), Some("127.0.0.1" | "[::1]")));
    assert!(redirect.port().is_some());
    let issuer = format!("{}://{}", authorization.scheme(), authorization.authority());
    redirect
        .query_pairs_mut()
        .append_pair("state", &fields["state"])
        .append_pair("iss", &issuer)
        .append_pair("error", "access_denied");
    let mut connection =
        TcpStream::connect(redirect.socket_addrs(|| None).unwrap().as_slice()).unwrap();
    connection
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    write!(
        connection,
        "GET {}?{} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        redirect.path(),
        redirect.query().unwrap(),
        redirect.authority()
    )
    .unwrap();
    let mut response = String::new();
    connection
        .take(16384)
        .read_to_string(&mut response)
        .unwrap();
    assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
    assert!(response.contains("Login not completed"));
    assert!(response.contains("Cache-Control: no-store"));
    assert!(!response.contains(&fields["state"]));
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = cli.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "CLI did not finish after callback"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert!(!status.success());
    let mut stderr = String::new();
    cli.0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(stderr.contains("Authorization was denied"));
    assert!(!stderr.contains("Opening your system browser"));
    assert!(!stderr.contains(&fields["state"]));
}
