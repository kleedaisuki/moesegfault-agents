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


def test_prepare_context_pins_the_live_kernel_before_any_cell(daemon: DockerDaemon) -> None:
    request: dict[str, Any] = {"container_id": None, "network_policy": {"type": "disabled"}}
    prepared: dict[str, Any] = daemon.json("POST", "/v1/python/contexts", request, timeout=90)
    assert prepared["container_id"].startswith("cntr_")
    assert daemon.json("POST", "/v1/python/contexts", request) == prepared
    assert daemon.json("POST", "/v1/python/contexts", {**request, **prepared}) == prepared
    listing: dict[str, Any] = daemon.json("GET", "/v1/python/files")
    assert listing["container_id"] == prepared["container_id"]
    assert listing["files"] == []
    result: dict[str, Any] = cell(daemon, "prepared_namespace = 42\nprint(prepared_namespace)", prepared["container_id"])
    assert logs(result) == "42\n"
    assert daemon.json("POST", "/v1/python/contexts", request) == prepared
    assert logs(cell(daemon, "print(prepared_namespace)", prepared["container_id"])) == "42\n"
    unknown: str = "cntr_" + uuid.uuid4().hex
    assert daemon.request("POST", "/v1/python/contexts", {**request, "container_id": unknown})[0] == 404
    assert logs(cell(daemon, "print(prepared_namespace)", prepared["container_id"])) == "42\n"


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


def test_cancel_before_post_is_a_known_receipt_without_running_the_cell(daemon: DockerDaemon) -> None:
    warm: dict[str, Any] = cell(daemon, "kept_before_late_post = 41")
    execution_id: str = "late_" + uuid.uuid4().hex
    path: str = "/v1/python/" + execution_id
    assert daemon.request("DELETE", path)[0] == 204
    assert daemon.request("DELETE", path)[0] == 204
    receipt: dict[str, Any] = daemon.json("GET", path)
    assert receipt["cancelled"] is True
    assert receipt["error"] == "code_cancelled_before_dispatch"
    assert receipt["container_id"] is None
    assert receipt["outputs"] == [] and receipt["artifacts"] == []
    assert receipt["duration_ms"] == 0
    payload: dict[str, Any] = {
        "id": execution_id,
        "container_id": warm["container_id"],
        "code": "late_post_must_not_run = True\nkept_before_late_post = -1",
    }
    assert daemon.json("POST", "/v1/python", payload) == receipt
    assert daemon.json("POST", "/v1/python", payload) == receipt
    assert daemon.json("POST", "/v1/python", {**payload, "container_id": "cntr_" + uuid.uuid4().hex}) == receipt
    assert daemon.json("GET", path) == receipt
    verified: dict[str, Any] = cell(
        daemon,
        "print(kept_before_late_post)\nprint('late_post_must_not_run' in globals())",
        warm["container_id"],
    )
    assert verified["container_id"] == warm["container_id"]
    assert logs(verified) == "41\nFalse\n"


def test_repeated_resets_reclaim_contexts_but_preserve_unpublished_artifacts(daemon: DockerDaemon) -> None:
    first = cell(daemon, "from pathlib import Path\nPath('retained.txt').write_bytes(b'unpublished artifact')")
    artifact = next(entry for entry in first["artifacts"] if entry["filename"] == "retained.txt")
    identities: set[str] = {first["container_id"]}
    for index in range(10):
        warm = cell(daemon, "reset_marker = 42")
        current = warm["container_id"]
        if index:
            assert current not in identities
        identities.add(current)
        run_id = "reset_" + uuid.uuid4().hex
        request: dict[str, Any] = {
            "id": run_id, "container_id": current, "code": "import time\ntime.sleep(30)",
        }
        if index % 2:
            request["timeout_ms"] = 150
            result: dict[str, Any] = daemon.json("POST", "/v1/python", request)
            assert result["timed_out"]
        else:
            with concurrent.futures.ThreadPoolExecutor() as pool:
                running = pool.submit(daemon.json, "POST", "/v1/python", request)
                for _ in range(100):
                    if daemon.request("GET", "/v1/python/" + run_id)[0] == 202:
                        break
                    time.sleep(0.05)
                else:
                    pytest.fail("reset execution never became observable")
                assert daemon.request("DELETE", "/v1/python/" + run_id)[0] == 204
                result = running.result(timeout=10)
            assert result["cancelled"]
        assert daemon.request("POST", "/v1/python", {
            "id": uuid.uuid4().hex, "container_id": current, "code": "print('BAD')",
        })[0] == 404
        assert daemon.request("GET", "/v1/python/artifacts/" + artifact["id"])[2] == b"unpublished artifact"
    fresh = cell(daemon, "print('reset_marker' in globals())")
    assert fresh["container_id"] not in identities and "False" in logs(fresh)
