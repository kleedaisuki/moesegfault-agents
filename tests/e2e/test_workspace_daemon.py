import concurrent.futures
import hashlib
import subprocess
import time
import uuid

import pytest


def test_auth_and_credential_isolated_bash(daemon, execute):
    assert daemon.request("GET", "/health", authenticated=False)[0] == 401
    assert daemon.request("GET", "/health", headers={"Authorization": "Bearer wrong"})[0] == 401
    result = execute('printf "%s|%s|%s" "$(id -u)" "${MAGENTS_DAEMON_TOKEN-unset}" "$HOME"; printf error >&2')
    assert result["exit_code"] == 0
    assert result["stdout"].startswith("1000|unset|/")
    assert result["stderr"] == "error"
    assert not result["cancelled"] and not result["timed_out"]


@pytest.mark.parametrize("options", [{"timeout_ms": 0}, {"timeout_ms": 600001}, {"max_output_bytes": 0}, {"max_output_bytes": 4194305}, {"id": "../escape"}, {"command": ""}, {"unexpected": True}])
def test_invalid_exec_never_runs(daemon, options):
    request = {"id": uuid.uuid4().hex, "command": "true", **options}
    assert daemon.request("POST", "/v1/exec", request)[0] in (400, 422)


def test_output_timeout_and_child_cleanup(execute):
    result = execute("yes x | head -c 100000", max_output_bytes=128)
    assert result["truncated"]
    assert len(result["stdout"].encode()) + len(result["stderr"].encode()) <= 128
    result = execute("sleep 30 & wait", timeout_ms=150)
    assert result["timed_out"] and not result["cancelled"]
    assert result["duration_ms"] < 10000
    assert execute("printf ready")["stdout"] == "ready"


def test_busy_cancel_and_reconnect(daemon):
    run_id = uuid.uuid4().hex
    with concurrent.futures.ThreadPoolExecutor() as pool:
        running = pool.submit(daemon.json, "POST", "/v1/exec", {"id": run_id, "command": "sleep 30"})
        for _ in range(100):
            if daemon.request("GET", "/v1/exec/" + run_id)[0] == 202:
                break
            time.sleep(0.05)
        else:
            pytest.fail("execution never became observable as running")
        assert daemon.request("GET", "/v1/snapshot")[0] == 409
        assert daemon.request("POST", "/v1/exec", {"id": uuid.uuid4().hex, "command": "true"})[0] == 409
        assert daemon.request("DELETE", "/v1/exec/" + run_id)[0] == 204
        result = running.result(timeout=10)
    assert result["cancelled"] and not result["timed_out"]
    assert daemon.json("GET", "/v1/exec/" + run_id) == result
    assert daemon.request("POST", "/v1/exec", {"id": run_id, "command": "true"})[0] == 409


def test_snapshot_restore_tamper_rollback_and_restart(daemon, execute):
    marker = uuid.uuid4().hex
    assert execute(f"mkdir -p nested; printf {marker} > nested/state; chmod 600 nested/state")["exit_code"] == 0
    status, headers, archive = daemon.request("GET", "/v1/snapshot")
    assert status == 200
    digest = headers["x-snapshot-sha256"]
    assert hashlib.sha256(archive).hexdigest() == digest
    assert int(headers["x-snapshot-archive-bytes"]) == len(archive)
    assert int(headers["x-snapshot-logical-bytes"]) >= len(marker)
    assert headers["x-snapshot-format-version"] == "1"
    execute("printf modified > nested/state; touch after-checkpoint")
    damaged = bytearray(archive)
    damaged[len(damaged) // 2] ^= 1
    assert daemon.request("POST", "/v1/restore", bytes(damaged), {"x-snapshot-sha256": digest})[0] == 422
    assert execute("cat nested/state")["stdout"] == "modified"
    daemon.json("POST", "/v1/restore", archive, headers={"x-snapshot-sha256": digest})
    assert execute("cat nested/state; test ! -e after-checkpoint")["stdout"] == marker
    assert execute("test ! -e after-checkpoint; test $(stat -c %a nested/state) = 600")["exit_code"] == 0
    subprocess.run(["docker", "restart", daemon.container_name], check=True, capture_output=True)
    for _ in range(100):
        try:
            if daemon.request("GET", "/health")[0] == 200:
                break
        except (OSError, ConnectionError):
            pass
        time.sleep(0.1)
    else:
        pytest.fail("daemon failed to recover after restart")
    assert execute("cat nested/state")["stdout"] == marker


def test_snapshot_rejects_escaping_links_without_losing_state(daemon, execute):
    execute("ln -s /etc/passwd unsafe-link")
    try:
        assert daemon.request("GET", "/v1/snapshot")[0] >= 400
    finally:
        execute("rm -f unsafe-link")
    assert daemon.request("GET", "/v1/snapshot")[0] == 200


def checkpoint_and_ack(daemon):
    status, headers, archive = daemon.request("GET", "/v1/snapshot")
    assert status == 200
    digest = headers["x-snapshot-sha256"]
    assert hashlib.sha256(archive).hexdigest() == digest
    assert daemon.request("POST", "/v1/checkpoint-ack", {"sha256": digest})[0] == 204
    return digest, archive


def test_setsid_descendant_cannot_write_after_execution_completes(execute):
    result = execute("rm -f orphan; setsid bash -c 'sleep .5; echo orphan > \"$HOME/orphan\"' >/dev/null 2>&1 &")
    assert result["exit_code"] == 0
    time.sleep(1)
    assert execute("test ! -e orphan")["exit_code"] == 0


def test_read_only_large_workspace_does_not_accumulate_dirty_bytes(daemon, execute):
    size = 64 * 1024 * 1024
    assert execute(f"truncate -s {size} large.bin")["exit_code"] == 0
    try:
        checkpoint_and_ack(daemon)
        assert daemon.json("GET", "/v1/status")["dirty_bytes"] == 0
        for _ in range(12):
            result = execute("test -f large.bin && wc -c < large.bin")
            assert result["exit_code"] == 0
            assert int(result["stdout"].strip()) == size
            assert daemon.json("GET", "/v1/status")["dirty_bytes"] == 0
        assert execute("printf x | dd of=large.bin bs=1 conv=notrunc status=none")["exit_code"] == 0
        assert daemon.json("GET", "/v1/status")["dirty_bytes"] >= size
        checkpoint_and_ack(daemon)
        assert execute("touch empty-file")["exit_code"] == 0
        assert daemon.json("GET", "/v1/status")["dirty_bytes"] >= 1
        checkpoint_and_ack(daemon)
        assert execute("rm empty-file")["exit_code"] == 0
        assert daemon.json("GET", "/v1/status")["dirty_bytes"] >= 1
    finally:
        assert execute("rm -f large.bin empty-file")["exit_code"] == 0


def test_workspace_owned_hardlinks_materialize_but_root_owned_links_fail(daemon, execute):
    assert execute("printf shared > first; ln first second")["exit_code"] == 0
    try:
        digest, archive = checkpoint_and_ack(daemon)
        assert execute("printf changed > first")["exit_code"] == 0
        daemon.json("POST", "/v1/restore", archive, headers={"x-snapshot-sha256": digest})
        result = execute("test \"$(cat first)\" = shared && test \"$(cat second)\" = shared && "
                         "test \"$(stat -c %u first)\" = 1000 && test \"$(stat -c %u second)\" = 1000 && "
                         "test \"$(stat -c %i first)\" != \"$(stat -c %i second)\"")
        assert result["exit_code"] == 0
        root_workspace = 'root="${MAGENTS_STATE_DIR:-/var/lib/magents}"; cd "$root/generations/$(cat "$root/CURRENT")"; '
        subprocess.run(["docker", "exec", "--user", "0", daemon.container_name, "sh", "-c",
                        root_workspace + "printf private > root-first; ln root-first root-second"], check=True, capture_output=True)
        try:
            code, _, body = daemon.request("GET", "/v1/snapshot")
            assert code == 422
            assert b"1000" in body, "snapshot rejection must explain the supported workspace ownership boundary"
        finally:
            subprocess.run(["docker", "exec", "--user", "0", daemon.container_name, "sh", "-c",
                            root_workspace + "rm -f root-first root-second"], check=True, capture_output=True)
    finally:
        assert execute("rm -f first second")["exit_code"] == 0
    assert daemon.request("GET", "/v1/snapshot")[0] == 200
