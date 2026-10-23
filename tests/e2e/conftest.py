import json
import http.client
import os
import pathlib
import subprocess
import time
import urllib.error
import urllib.request
import uuid

import pytest


ROOT = pathlib.Path(__file__).resolve().parents[2]


class Http:
    def __init__(self, origin, token):
        self.origin = origin.rstrip("/")
        self.token = token

    def request(self, method, path, data=None, headers=None, authenticated=True, timeout=180):
        values = {"Authorization": f"Bearer {self.token}"} if authenticated else {}
        if data is not None and not isinstance(data, bytes):
            data = json.dumps(data).encode()
            values["Content-Type"] = "application/json"
        values.update(headers or {})
        request = urllib.request.Request(self.origin + path, data, values, method=method)
        try:
            response = urllib.request.urlopen(request, timeout=timeout)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            return response.status, response.headers, response.read()

    def json(self, method, path, data=None, expected=200, **kwargs):
        status, _, body = self.request(method, path, data, **kwargs)
        # Do not expose response bodies: diagnostics can contain user content.
        assert status == expected, f"{method} {path}: expected {expected}, received {status}"
        return json.loads(body) if body else None


class DockerDaemon(Http):
    def __init__(self, name, token):
        super().__init__("http://127.0.0.1:1", token)
        self.container_name = name
        self.endpoint_history = []

    def refresh_endpoint(self):
        bindings = json.loads(subprocess.check_output([
            "docker", "inspect", "--format", "{{json .NetworkSettings.Ports}}", self.container_name,
        ], timeout=10))
        port = int(bindings["8080/tcp"][0]["HostPort"])
        assert 1 <= port <= 65535
        self.origin = f"http://127.0.0.1:{port}"
        if not self.endpoint_history or self.endpoint_history[-1] != port:
            self.endpoint_history.append(port)

    def diagnostics(self):
        # Never inspect Config.Env or dump raw daemon/user process output.
        template = ('{"status":{{json .State.Status}},"running":{{json .State.Running}},'
                    '"oom_killed":{{json .State.OOMKilled}},"exit_code":{{json .State.ExitCode}},'
                    '"started_at":{{json .State.StartedAt}},"ports":{{json .NetworkSettings.Ports}}}')
        try:
            state = json.loads(subprocess.check_output([
                "docker", "inspect", "--format", template, self.container_name,
            ], timeout=10))
        except (OSError, subprocess.SubprocessError, ValueError):
            state = {"inspection_unavailable": True}
        events = []
        try:
            output = subprocess.run(["docker", "logs", "--tail", "30", self.container_name],
                                    capture_output=True, timeout=10, check=True)
            for line in (output.stdout + output.stderr).splitlines():
                try:
                    fields = json.loads(line).get("fields", {})
                    event = fields.get("event")
                    if event not in {"workspace.ready", "workspace.http_complete", "workspace.exec_complete",
                                     "workspace.error", "workspace.restore_rejected", "workspace.snapshot_rejected"}:
                        continue
                    entry = {"event": event}
                    for key in ("status", "duration_ms", "timed_out", "cancelled", "truncated"):
                        if type(fields.get(key)) in (int, bool):
                            entry[key] = fields[key]
                    events.append(entry)
                except (ValueError, AttributeError):
                    continue
        except (OSError, subprocess.SubprocessError):
            pass
        return json.dumps({"docker_state": state, "daemon_events": events, "endpoint_ports": self.endpoint_history}, sort_keys=True)

    def wait_ready(self, timeout=20):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                # Docker can allocate a new ephemeral host port on restart.
                self.refresh_endpoint()
                status, _, body = self.request("GET", "/health", timeout=1)
                if status == 200:
                    value = json.loads(body)
                    assert value.get("boot_id"), "daemon health must identify the process boot"
                    return value
            except (OSError, http.client.HTTPException, subprocess.SubprocessError, ValueError, KeyError, IndexError, TypeError):
                pass
            time.sleep(0.1)
        pytest.fail("daemon did not become healthy; " + self.diagnostics(), pytrace=False)

    def restart(self):
        before = self.json("GET", "/health")["boot_id"]
        previous_port = self.endpoint_history[-1]
        subprocess.run(["docker", "restart", self.container_name], check=True, capture_output=True, timeout=30)
        after = self.wait_ready()
        assert after["boot_id"] != before, "restart must create a new service process, not merely reconnect"
        print(f"Daemon restart: loopback port {previous_port} -> {self.endpoint_history[-1]}; process boot changed")
        return after


@pytest.hookimpl(hookwrapper=True)
def pytest_runtest_makereport(item, call):
    outcome = yield
    if outcome.get_result().failed and "daemon" in item.fixturenames:
        item.session._magents_daemon_failed = True


@pytest.fixture(scope="session")
def deployed():
    origin = os.getenv("MAGENTS_TEST_API_ORIGIN")
    token = os.getenv("MAGENTS_TEST_API_KEY")
    if not origin or not token:
        pytest.skip("deployed journey requires MAGENTS_TEST_API_ORIGIN and MAGENTS_TEST_API_KEY")
    assert origin.startswith("https://") or origin.startswith("http://127.0.0.1:"), "test origin must use HTTPS or explicit loopback"
    return Http(origin, token)


@pytest.fixture(scope="session")
def daemon(request):
    image = os.getenv("MAGENTS_DAEMON_IMAGE")
    if not image:
        pytest.skip("daemon integration requires a built MAGENTS_DAEMON_IMAGE (Linux Docker container)")
    token = uuid.uuid4().hex + uuid.uuid4().hex
    name = "magents-e2e-" + uuid.uuid4().hex
    subprocess.run(["docker", "run", "--detach", "--name", name, "--publish", "127.0.0.1::8080", "--env", "MAGENTS_DAEMON_TOKEN", image],
                   env={**os.environ, "MAGENTS_DAEMON_TOKEN": token}, check=True, capture_output=True)
    http = DockerDaemon(name, token)
    try:
        http.wait_ready()
        yield http
    finally:
        if getattr(request.session, "_magents_daemon_failed", False):
            print("Sanitized daemon diagnostics: " + http.diagnostics())
        subprocess.run(["docker", "rm", "--force", name], check=True, capture_output=True)


@pytest.fixture
def execute(daemon):
    def run(command, **options):
        return daemon.json("POST", "/v1/exec", {"id": uuid.uuid4().hex, "command": command, **options})
    return run
