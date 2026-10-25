"""Real credential-isolated Jupyter and artifact journeys in the tested image."""

from __future__ import annotations

import concurrent.futures
import hashlib
import json
from pathlib import Path
import time
from typing import Any
import uuid

import pytest

from conftest import DockerDaemon


def cell(daemon: DockerDaemon, code: str, container: str | None = None, **options: Any) -> dict[str, Any]:
    request: dict[str, Any] = {"id": "cell_" + uuid.uuid4().hex, "code": code, **options}
    if container is not None:
        request["container_id"] = container
    result: dict[str, Any] = daemon.json("POST", "/v1/python", request, timeout=90)
    assert result["id"] == request["id"]
    assert result["error"] is None, "known kernel infrastructure error is not a Python result"
    return result


def logs(result: dict[str, Any]) -> str:
    return "".join(output["logs"] for output in result["outputs"] if output["type"] == "logs")


def test_kernel_namespace_and_credential_boundary_survive_bash_and_snapshot(daemon: DockerDaemon) -> None:
    marker = "value_" + uuid.uuid4().hex
    first = cell(daemon, f"import os, resource\n{marker} = 40\nprint(str(os.getuid()) + '|' + os.environ.get('MAGENTS_DAEMON_TOKEN', 'unset'))\nprint(resource.getrlimit(resource.RLIMIT_NPROC)[0])")
    assert "1001|unset" in logs(first) and "128" in logs(first)
    container = first["container_id"]
    assert container.startswith("cntr_")
    bash = daemon.json("POST", "/v1/exec", {"id": uuid.uuid4().hex, "command": "printf bash-ok"})
    assert bash["stdout"] == "bash-ok"
    assert daemon.request("GET", "/v1/snapshot")[0] == 200
    second = cell(daemon, f"print({marker} + 2)", container)
    assert second["container_id"] == container and "42" in logs(second)
    result = cell(daemon, "import os\nfor uid in (0, 1000):\n try:\n  os.setuid(uid)\n  print('BAD')\n except PermissionError:\n  print('denied')\ntry:\n open('/proc/1/environ', 'rb').read()\n print('BAD')\nexcept PermissionError:\n print('denied')", container)
    assert "BAD" not in logs(result) and logs(result).count("denied") == 3


def test_generated_artifacts_are_immutable_and_rich_outputs_use_opaque_ids(daemon: DockerDaemon) -> None:
    name = "plot_" + uuid.uuid4().hex + ".png"
    first = cell(daemon, f"import matplotlib\nmatplotlib.use('Agg')\nimport matplotlib.pyplot as plt\nfrom IPython.display import display, Image\nplt.figure()\nplt.plot([0,1],[0,1])\nplt.savefig({name!r})\nplt.close()\ndisplay(Image(filename={name!r}))")
    assert first["artifact_error"] is None
    artifact = next(entry for entry in first["artifacts"] if entry["filename"] == name)
    status, headers, body = daemon.request("GET", "/v1/python/artifacts/" + artifact["id"])
    assert status == 200 and body.startswith(b"\x89PNG\r\n\x1a\n")
    assert len(body) == artifact["bytes"] and hashlib.sha256(body).hexdigest() == artifact["sha256"]
    assert headers["X-Content-Sha256"] == artifact["sha256"]
    image = next(output for output in first["outputs"] if output["type"] == "image")
    assert image["artifact_id"] in {entry["id"] for entry in first["artifacts"]}
    assert "base64" not in json.dumps(image) and "/var/lib" not in json.dumps(image)
    repeated = cell(daemon, "pass", first["container_id"])
    assert next(entry for entry in repeated["artifacts"] if entry["filename"] == name)["id"] == artifact["id"]
    changed = cell(daemon, f"open({name!r}, 'wb').write(b'changed')", first["container_id"])
    assert next(entry for entry in changed["artifacts"] if entry["filename"] == name)["id"] != artifact["id"]
    assert daemon.request("GET", "/v1/python/artifacts/" + artifact["id"])[2] == body
    assert daemon.request("DELETE", "/v1/python/artifacts/" + artifact["id"])[0] == 204
    assert daemon.request("DELETE", "/v1/python/artifacts/" + artifact["id"])[0] == 204
    assert daemon.request("GET", "/v1/python/artifacts/" + artifact["id"])[0] == 404


def test_owned_inputs_are_integrity_checked_readonly_and_not_reexported(daemon: DockerDaemon) -> None:
    file_id = "file_" + uuid.uuid4().hex
    source = bytes(range(256)) + b"\x00\xff"
    digest = hashlib.sha256(source).hexdigest()
    wrong = daemon.request("PUT", "/v1/python/files/" + file_id, source,
                           headers={"X-Content-Sha256": "0" * 64})
    assert wrong[0] == 422
    uploaded: dict[str, Any] = daemon.json("PUT", "/v1/python/files/" + file_id, source,
                                          headers={"X-Content-Sha256": digest})
    assert uploaded["file_id"] == file_id and uploaded["sha256"] == digest
    assert uploaded["path"] == "/mnt/inputs/" + file_id + "/content"
    assert daemon.json("PUT", "/v1/python/files/" + file_id, source, headers={"X-Content-Sha256": digest}) == uploaded
    name = "copy_" + uuid.uuid4().hex + ".bin"
    result = cell(daemon, f"from pathlib import Path\nimport hashlib\np = Path({uploaded['path']!r})\nprint(hashlib.sha256(p.read_bytes()).hexdigest())\ntry:\n p.write_bytes(b'BAD')\n print('BAD')\nexcept PermissionError:\n print('readonly')\nPath({name!r}).write_bytes(p.read_bytes())", uploaded["container_id"])
    assert digest in logs(result) and "readonly" in logs(result) and "BAD" not in logs(result)
    assert all(not entry["path"].startswith("inputs/") for entry in result["artifacts"])
    artifact = next(entry for entry in result["artifacts"] if entry["filename"] == name)
    assert daemon.request("GET", "/v1/python/artifacts/" + artifact["id"])[2] == source


def test_code_ids_results_output_bounds_and_python_errors(daemon: DockerDaemon) -> None:
    request = {"id": "cell_" + uuid.uuid4().hex, "code": "print('x' * 10000)", "max_output_bytes": 100}
    result: dict[str, Any] = daemon.json("POST", "/v1/python", request)
    assert result["error"] is None and result["truncated"]
    assert len(logs(result).encode()) <= 100
    assert daemon.json("GET", "/v1/python/" + request["id"]) == result
    assert daemon.request("POST", "/v1/python", request)[0] == 409
    failed = cell(daemon, "raise ValueError('controlled user error')", result["container_id"])
    assert failed["python_error"] and "controlled user error" in logs(failed)
    assert cell(daemon, "print(6 * 7)", result["container_id"])["container_id"] == result["container_id"]


def test_code_busy_cancel_and_stale_context_never_restart_implicitly(daemon: DockerDaemon) -> None:
    warm = cell(daemon, "retained_before_cancel = 1")
    run_id = "cell_" + uuid.uuid4().hex
    with concurrent.futures.ThreadPoolExecutor() as pool:
        running = pool.submit(daemon.json, "POST", "/v1/python", {
            "id": run_id, "container_id": warm["container_id"], "code": "import time\ntime.sleep(30)",
        })
        for _ in range(100):
            if daemon.request("GET", "/v1/python/" + run_id)[0] == 202:
                break
            time.sleep(0.05)
        else:
            pytest.fail("code execution never became observable")
        assert daemon.request("GET", "/v1/snapshot")[0] == 409
        assert daemon.request("POST", "/v1/exec", {"id": uuid.uuid4().hex, "command": "true"})[0] == 409
        assert daemon.request("DELETE", "/v1/python/" + run_id)[0] == 204
        result = running.result(timeout=10)
    assert result["cancelled"] and not result["timed_out"]
    assert daemon.json("GET", "/v1/python/" + run_id) == result
    assert daemon.request("POST", "/v1/python", {"id": uuid.uuid4().hex, "container_id": warm["container_id"], "code": "print('BAD')"})[0] == 404
    fresh = cell(daemon, "print('retained_before_cancel' in globals())")
    assert fresh["container_id"] != warm["container_id"] and "False" in logs(fresh)


def test_large_files_remain_discoverable_and_exports_do_not_follow_links(daemon: DockerDaemon) -> None:
    large = "large_" + uuid.uuid4().hex + ".bin"
    name = "export_" + uuid.uuid4().hex + ".txt"
    result = cell(daemon, f"from pathlib import Path\nwith open({large!r}, 'wb') as f:\n f.truncate(16*1024*1024+1)\nPath({name!r}).write_text('known export')")
    assert result["artifact_error"] is not None
    found: list[dict[str, Any]] = []
    cursor: str | None = None
    while True:
        suffix = "?limit=2" + ("&after=" + cursor if cursor is not None else "")
        page: dict[str, Any] = daemon.json("GET", "/v1/python/files" + suffix)
        found.extend(page["files"])
        if not page["has_more"]:
            break
        assert page["next_cursor"] != cursor
        cursor = page["next_cursor"]
    oversized = next(entry for entry in found if entry["filename"] == large)
    assert not oversized["exportable"] and oversized["bytes"] == 16 * 1024 * 1024 + 1
    small = next(entry for entry in found if entry["filename"] == name)
    assert daemon.request("GET", "/v1/python/source/" + small["cursor"])[2] == b"known export"
    assert daemon.request("GET", "/v1/python/source/" + b"../outside".hex())[0] == 422
    cell(daemon, f"Path({large!r}).unlink()", result["container_id"])


def test_python_timeout_starts_a_new_explicit_logical_container(daemon: DockerDaemon) -> None:
    warm = cell(daemon, "before_timeout = 123")
    result: dict[str, Any] = daemon.json("POST", "/v1/python", {
        "id": uuid.uuid4().hex, "container_id": warm["container_id"], "code": "import time\ntime.sleep(30)", "timeout_ms": 150,
    })
    assert result["timed_out"] and not result["cancelled"] and result["duration_ms"] < 10000
    fresh = cell(daemon, "print('before_timeout' in globals())")
    assert fresh["container_id"] != warm["container_id"] and "False" in logs(fresh)
