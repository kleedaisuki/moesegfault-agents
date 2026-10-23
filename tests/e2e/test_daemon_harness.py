"""Portable regressions for Docker test orchestration, not daemon acceptance."""
import json
import subprocess
import urllib.error

from conftest import DockerDaemon


def test_restart_refreshes_docker_ephemeral_port_and_checks_new_boot(monkeypatch):
    published = {"port": 32768, "boot": "first-process"}
    daemon = DockerDaemon("fixture-container", "fixture-only")

    def inspect(args, **kwargs):
        assert args[:3] == ["docker", "inspect", "--format"]
        assert args[3] == "{{json .NetworkSettings.Ports}}"
        return json.dumps({"8080/tcp": [{"HostIp": "127.0.0.1", "HostPort": str(published["port"])}]}).encode()

    def restart(args, **kwargs):
        assert args == ["docker", "restart", "fixture-container"]
        published.update(port=32769, boot="second-process")

    def health(method, path, data=None, **kwargs):
        assert method == "GET" and path == "/health"
        if daemon.origin != f"http://127.0.0.1:{published['port']}":
            raise urllib.error.URLError("stale ephemeral port")
        return 200, {}, json.dumps({"status": "ok", "boot_id": published["boot"]}).encode()

    monkeypatch.setattr(subprocess, "check_output", inspect)
    monkeypatch.setattr(subprocess, "run", restart)
    monkeypatch.setattr(daemon, "request", health)
    daemon.refresh_endpoint()
    assert daemon.origin.endswith(":32768")
    assert daemon.restart()["boot_id"] == "second-process"
    assert daemon.origin.endswith(":32769")
    assert daemon.endpoint_history == [32768, 32769]


def test_docker_diagnostics_omit_environment_and_untrusted_log_fields(monkeypatch):
    daemon = DockerDaemon("fixture-container", "secret-token")

    def inspect(args, **kwargs):
        assert "Config" not in args[3] and "Env" not in args[3]
        assert ".State.Health" not in args[3]
        return b'{"status":"exited","running":false,"exit_code":1,"ports":{}}'

    events = [
        {"fields": {"event": "workspace.error", "error": "secret-token", "message": "private-user-command"}},
        {"fields": {"event": "workspace.http_complete", "status": 401, "command": "private-user-command"}},
        {"fields": {"event": "secret-token"}},
    ]
    monkeypatch.setattr(subprocess, "check_output", inspect)
    monkeypatch.setattr(subprocess, "run", lambda *args, **kwargs: subprocess.CompletedProcess(args, 0, stdout=b"\n".join(json.dumps(e).encode() for e in events), stderr=b"raw-private-user-command"))
    diagnostics = daemon.diagnostics()
    assert "secret-token" not in diagnostics
    assert "private-user-command" not in diagnostics
    assert json.loads(diagnostics)["daemon_events"] == [{"event": "workspace.error"}, {"event": "workspace.http_complete", "status": 401}]
