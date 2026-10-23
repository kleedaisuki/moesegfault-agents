import json
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

    def request(self, method, path, data=None, headers=None, authenticated=True):
        values = {"Authorization": f"Bearer {self.token}"} if authenticated else {}
        if data is not None and not isinstance(data, bytes):
            data = json.dumps(data).encode()
            values["Content-Type"] = "application/json"
        values.update(headers or {})
        request = urllib.request.Request(self.origin + path, data, values, method=method)
        try:
            response = urllib.request.urlopen(request, timeout=180)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            return response.status, response.headers, response.read()

    def json(self, method, path, data=None, expected=200, **kwargs):
        status, _, body = self.request(method, path, data, **kwargs)
        # Do not expose response bodies: diagnostics can contain user content.
        assert status == expected, f"{method} {path}: expected {expected}, received {status}"
        return json.loads(body) if body else None


@pytest.fixture(scope="session")
def deployed():
    origin = os.getenv("MAGENTS_TEST_API_ORIGIN")
    token = os.getenv("MAGENTS_TEST_API_KEY")
    if not origin or not token:
        pytest.skip("deployed journey requires MAGENTS_TEST_API_ORIGIN and MAGENTS_TEST_API_KEY")
    assert origin.startswith("https://") or origin.startswith("http://127.0.0.1:"), "test origin must use HTTPS or explicit loopback"
    return Http(origin, token)


@pytest.fixture(scope="session")
def daemon():
    image = os.getenv("MAGENTS_DAEMON_IMAGE")
    if not image:
        pytest.skip("daemon integration requires a built MAGENTS_DAEMON_IMAGE (Linux Docker container)")
    token = uuid.uuid4().hex + uuid.uuid4().hex
    name = "magents-e2e-" + uuid.uuid4().hex
    subprocess.run(["docker", "run", "--detach", "--name", name, "--publish", "127.0.0.1::8080", "--env", f"MAGENTS_DAEMON_TOKEN={token}", image], check=True, capture_output=True)
    try:
        bindings = json.loads(subprocess.check_output(["docker", "inspect", "--format", "{{json .NetworkSettings.Ports}}", name]))
        http = Http("http://127.0.0.1:" + bindings["8080/tcp"][0]["HostPort"], token)
        for _ in range(100):
            try:
                if http.request("GET", "/health")[0] == 200:
                    break
            except (urllib.error.URLError, ConnectionError):
                pass
            time.sleep(0.1)
        else:
            pytest.fail("daemon container did not become healthy")
        http.container_name = name
        yield http
    finally:
        subprocess.run(["docker", "rm", "--force", name], check=True, capture_output=True)


@pytest.fixture
def execute(daemon):
    def run(command, **options):
        return daemon.json("POST", "/v1/exec", {"id": uuid.uuid4().hex, "command": command, **options})
    return run
