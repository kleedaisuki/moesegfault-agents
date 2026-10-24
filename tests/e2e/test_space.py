"""Deployed Space journeys through the real OpenAI Files client and owner APIs.

These tests store only generated test data. They do not invoke a model unless
the workspace journey's separate inference/workspace opt-ins are enabled.
"""

from collections.abc import Iterator
import hashlib
import json
import os
import time
import uuid
from typing import Any

from openai import NotFoundError, OpenAI
import pytest

from conftest import Http, cleanup_deployed_agent, correlation_headers
from stream_usage import reported_stream_usage


@pytest.fixture
def space_client(deployed: Http) -> Iterator[OpenAI]:
    with OpenAI(
        api_key=deployed.token,
        base_url=deployed.origin + "/v1",
        timeout=90,
        max_retries=0,
        default_headers=correlation_headers() or None,
    ) as client:
        yield client


@pytest.fixture
def space_uploads(space_client: OpenAI) -> Iterator[list[str]]:
    created: list[str] = []
    yield created
    failures: list[str] = []
    for file_id in created:
        try:
            deleted = space_client.files.delete(file_id)
            if not deleted.deleted:
                failures.append("delete_not_confirmed")
        except NotFoundError:
            continue
        except Exception as error:
            failures.append(type(error).__name__)
    if failures:
        pytest.fail("Generated upload cleanup incomplete: " + ", ".join(failures), pytrace=False)


def binary_corpus() -> bytes:
    return bytes(range(256)) * 1280 + "\x00\r\nSpace byte fidelity: 雪🧪\n".encode()


def test_sdk_files_binary_roundtrip_rename_and_delete(
    deployed: Http, space_client: OpenAI, space_uploads: list[str],
) -> None:
    payload = binary_corpus()
    digest = hashlib.sha256(payload).hexdigest()
    filename = "space-" + uuid.uuid4().hex + ".bin"
    uploaded = space_client.files.create(
        file=(filename, payload, "application/octet-stream"), purpose="user_data",
    )
    space_uploads.append(uploaded.id)
    assert uploaded.object == "file"
    assert uploaded.filename == filename
    assert uploaded.bytes == len(payload)
    assert uploaded.purpose == "user_data"
    assert uploaded.status == "processed"
    assert uploaded.created_at > 0
    assert uploaded.model_dump()["sha256"] == digest
    assert "object_key" not in uploaded.model_dump()

    metadata = space_client.files.retrieve(uploaded.id)
    assert metadata.model_dump() == uploaded.model_dump()
    copied = bytearray()
    with space_client.files.with_streaming_response.content(uploaded.id) as response:
        assert int(response.headers["content-length"]) == len(payload)
        assert response.headers["x-content-sha256"] == digest
        for chunk in response.iter_bytes(chunk_size=8192):
            copied.extend(chunk)
    assert bytes(copied) == payload
    assert hashlib.sha256(copied).hexdigest() == digest

    renamed = "renamed-" + uuid.uuid4().hex + ".bin"
    changed = deployed.json("PATCH", "/v1/space/files/" + uploaded.id, {"filename": renamed})
    assert changed["id"] == uploaded.id
    assert changed["filename"] == renamed
    assert changed["bytes"] == len(payload)
    assert changed["sha256"] == digest
    assert space_client.files.retrieve(uploaded.id).filename == renamed
    assert space_client.files.content(uploaded.id).content == payload

    alias = deployed.json("GET", "/v1/space/files?limit=100")
    assert any(item["id"] == uploaded.id for item in alias["data"])
    removed = space_client.files.delete(uploaded.id)
    assert removed.id == uploaded.id and removed.deleted is True
    assert removed.object == "file"
    with pytest.raises(NotFoundError):
        space_client.files.retrieve(uploaded.id)
    with pytest.raises(NotFoundError):
        space_client.files.content(uploaded.id)


def test_sdk_same_name_files_have_distinct_identity_and_bounded_pages(
    space_client: OpenAI, space_uploads: list[str],
) -> None:
    filename = "duplicate-" + uuid.uuid4().hex + ".txt"
    expected: dict[str, bytes] = {}
    user_data_ids: set[str] = set()
    for payload, purpose in ((b"first\n", "user_data"), (b"second\n", "user_data"), (b"third\x00\n", "assistants")):
        uploaded = space_client.files.create(file=(filename, payload), purpose=purpose)
        space_uploads.append(uploaded.id)
        assert uploaded.id not in expected, "a display filename must not replace an existing file"
        expected[uploaded.id] = payload
        if purpose == "user_data":
            user_data_ids.add(uploaded.id)

    page = space_client.files.list(limit=1)
    seen: set[str] = set()
    pages = 0
    while True:
        assert page.object == "list"
        assert len(page.data) <= 1
        for item in page.data:
            assert item.id not in seen, "cursor traversal must not repeat a file"
            seen.add(item.id)
        pages += 1
        assert pages <= 1100, "bounded account traversal did not terminate"
        if not page.has_next_page():
            break
        assert page.data, "has_more cannot advance an empty cursor page"
        page = page.get_next_page()
    assert expected.keys() <= seen
    filtered = list(space_client.files.list(purpose="user_data", limit=100))
    assert all(item.purpose == "user_data" for item in filtered)
    assert {item.id for item in filtered} & expected.keys() == user_data_ids
    for file_id, payload in expected.items():
        assert space_client.files.content(file_id).content == payload


def test_sdk_zero_byte_file_and_unicode_filename(
    space_client: OpenAI, space_uploads: list[str],
) -> None:
    filename = "空文件-" + uuid.uuid4().hex + ".txt"
    uploaded = space_client.files.create(file=(filename, b"", "text/plain"), purpose="user_data")
    space_uploads.append(uploaded.id)
    assert uploaded.filename == filename
    assert uploaded.bytes == 0
    assert uploaded.model_dump()["sha256"] == hashlib.sha256(b"").hexdigest()
    assert space_client.files.retrieve(uploaded.id).filename == filename
    assert space_client.files.content(uploaded.id).content == b""


def test_raw_checksum_rejection_releases_upload_quota_and_reservation(deployed: Http) -> None:
    before = deployed.json("GET", "/v1/space")["usage"]
    reserved_before = int(deployed.json("GET", "/v1/usage")["wallet"]["reserved_microusd"])
    filename = "rejected-digest-" + uuid.uuid4().hex + ".bin"
    payload = b"Generated checksum mismatch evidence; never retry this upload.\x00"
    code, _, _ = deployed.request(
        "POST", "/v1/files?filename=" + filename + "&purpose=user_data", payload,
        headers={"Content-Type": "application/octet-stream", "X-Content-Sha256": "0" * 64},
    )
    assert code in (400, 422), "a definitively invalid checksum needs a client error, not a successful or ambiguous upload"
    deadline = time.monotonic() + 20
    while True:
        after = deployed.json("GET", "/v1/space")["usage"]
        reserved_after = int(deployed.json("GET", "/v1/usage")["wallet"]["reserved_microusd"])
        if after["bytes"] == before["bytes"] and after["files"] == before["files"] and reserved_after <= reserved_before:
            break
        assert time.monotonic() < deadline, "rejected bytes left a quota allocation or new financial hold behind"
        time.sleep(0.25)
    listing = deployed.json("GET", "/v1/files?limit=100")
    assert all(item["filename"] != filename for item in listing["data"])


def test_space_is_singleton_renameable_and_uploads_are_owner_isolated(
    deployed: Http, space_client: OpenAI, space_uploads: list[str],
) -> None:
    original = deployed.json("GET", "/v1/space")
    assert original["object"] == "space"
    assert original["id"]
    assert deployed.json("GET", "/v1/space")["id"] == original["id"]
    new_name = "Acceptance " + uuid.uuid4().hex[:12]
    try:
        renamed = deployed.json("PATCH", "/v1/space", {"name": new_name})
        assert renamed["id"] == original["id"]
        assert renamed["name"] == new_name
        assert deployed.json("GET", "/v1/space")["name"] == new_name
    finally:
        deployed.json("PATCH", "/v1/space", {"name": original["name"]})

    payload = b"owner-only-" + uuid.uuid4().hex.encode()
    uploaded = space_client.files.create(file=("owner.txt", payload), purpose="user_data")
    space_uploads.append(uploaded.id)
    assert deployed.request("GET", "/v1/space", authenticated=False)[0] == 401
    assert deployed.request("GET", "/v1/files", authenticated=False)[0] == 401
    for suffix in ("", "/content"):
        assert deployed.request("GET", "/v1/files/" + uploaded.id + suffix, authenticated=False)[0] == 401

    token = os.getenv("MAGENTS_TEST_OTHER_API_KEY")
    if not token:
        pytest.skip("cross-account Space checks need MAGENTS_TEST_OTHER_API_KEY")
    other = Http(deployed.origin, token)
    other_space = other.json("GET", "/v1/space")
    assert other_space["id"] != original["id"], "the second credential must belong to a different account"
    assert other.request("GET", "/v1/files/" + uploaded.id)[0] == 404
    assert other.request("GET", "/v1/files/" + uploaded.id + "/content")[0] == 404
    assert other.request("PATCH", "/v1/space/files/" + uploaded.id, {"filename": "hijacked.txt"})[0] == 404
    assert other.request("DELETE", "/v1/files/" + uploaded.id)[0] == 404
    assert uploaded.id not in {item["id"] for item in other.json("GET", "/v1/files?limit=100")["data"]}
    assert space_client.files.retrieve(uploaded.id).filename == "owner.txt"
    assert space_client.files.content(uploaded.id).content == payload


@pytest.fixture
def space_workspace_agent(deployed: Http) -> Iterator[tuple[dict[str, Any], list[str]]]:
    if os.getenv("MAGENTS_TEST_INFERENCE") != "1" or os.getenv("MAGENTS_TEST_WORKSPACE") != "1":
        pytest.skip("workspace versions require separate inference and workspace opt-ins")
    agent = deployed.json("POST", "/v1/agents", {
        "name": "space-version-" + uuid.uuid4().hex[:12],
        "instructions": "Use the exact tools requested. Run exact bash commands, and report actual tool results. "
                        "Never infer file contents from conversation history. Do not persist or restore unless requested.",
        "tools": ["time", "bash", "workspace_commit", "workspace_versions", "workspace_restore"],
        "workspace": True,
        "compaction_percent": 80,
    }, expected=201)
    sessions: list[str] = []
    try:
        yield agent, sessions
    finally:
        failures = cleanup_deployed_agent(deployed, agent["id"], sessions)
        if failures:
            pytest.fail("Workspace fixture cleanup incomplete: " + json.dumps(failures, sort_keys=True), pytrace=False)


def named_tool_outputs(state: dict[str, Any], name: str, start: int = 0) -> list[Any]:
    messages: list[dict[str, Any]] = state["messages"]
    pending: dict[str, str] = {}
    outputs: list[Any] = []
    for index, message in enumerate(messages):
        for call in message.get("tool_calls", []):
            pending[call["id"]] = call["function"]["name"]
        if message["role"] == "tool":
            matched = pending.pop(message["tool_call_id"], None)
            if index >= start and matched == name:
                outputs.append(message["content"])
    return outputs


def version_pages(deployed: Http) -> list[dict[str, Any]]:
    found: list[dict[str, Any]] = []
    after: str | None = None
    seen: set[str] = set()
    for _ in range(100):
        path = "/v1/space/workspace-versions?limit=100"
        if after is not None:
            path += "&after=" + after
        page = deployed.json("GET", path)
        assert page["object"] == "list"
        assert len(page["data"]) <= 100
        for item in page["data"]:
            assert item["id"] not in seen, "version pagination repeated an immutable version"
            seen.add(item["id"])
            found.append(item)
        if not page["has_more"]:
            return found
        assert page["data"] and page["last_id"]
        after = page["last_id"]
    pytest.fail("version listing did not terminate within its bounded traversal")


def test_workspace_tools_publish_readonly_versions_and_restore_actual_files(
    deployed: Http,
    space_client: OpenAI,
    space_workspace_agent: tuple[dict[str, Any], list[str]],
) -> None:
    agent, sessions = space_workspace_agent
    session = deployed.json("POST", f"/v1/agents/{agent['id']}/sessions", {}, expected=201)
    sessions.append(session["id"])
    marker = uuid.uuid4().hex
    changed_marker = uuid.uuid4().hex
    version_name = "acceptance-" + uuid.uuid4().hex[:12]

    def turn(content: str, session_id: str = session["id"]) -> None:
        with space_client.chat.completions.create(
            model=agent["id"], messages=[{"role": "user", "content": content}],
            extra_headers={"X-Agents-Session": session_id}, timeout=240, stream=True,
        ) as chunks:
            reported_stream_usage(chunks)

    turn(f"Run bash exactly `mkdir -p acceptance; printf {marker} > acceptance/version.txt`. "
         f"Then call workspace_commit with name {version_name}. "
         "After persistence succeeds, call workspace_versions and finish.")
    first = deployed.json("GET", "/v1/sessions/" + session["id"])
    assert named_tool_outputs(first, "bash"), "the persisted content must originate from actual workspace execution"
    assert named_tool_outputs(first, "workspace_commit"), "a management-only checkpoint does not establish tool support"
    assert named_tool_outputs(first, "workspace_versions"), "the agent must be able to discover user-space versions"
    matches = [item for item in version_pages(deployed) if item["session_id"] == session["id"] and item["name"] == version_name]
    assert len(matches) == 1
    version = matches[0]
    assert version["object"] == "workspace.version"
    assert "object_key" not in version and "operation_id" not in version
    assert version["archive_bytes"] > 0 and version["logical_bytes"] >= len(marker)
    assert version["format_version"] == 1 and version["generation"] >= 1
    path = "/v1/space/workspace-versions/" + version["id"]
    assert deployed.json("GET", path) == version
    code, headers, archive = deployed.request("GET", path + "/content")
    assert code == 200
    assert headers.get_content_type() == "application/zstd"
    assert len(archive) == int(headers["Content-Length"]) == version["archive_bytes"]
    assert hashlib.sha256(archive).hexdigest() == headers["X-Snapshot-Sha256"] == version["sha256"]
    for method, body in (("PATCH", {"name": "mutated"}), ("DELETE", None), ("POST", {"name": "replace"})):
        assert deployed.request(method, path, body)[0] == 405
    assert deployed.json("GET", path) == version

    turn(f"Run bash exactly `printf {changed_marker} > acceptance/version.txt`. Do not persist or restore.")
    before_restore = deployed.json("GET", "/v1/sessions/" + session["id"])
    turn(f"Call workspace_restore with version_id {version['id']}. "
         "Then run bash exactly `cat acceptance/version.txt` and finish.")
    restored = deployed.json("GET", "/v1/sessions/" + session["id"])
    restore_outputs = named_tool_outputs(restored, "workspace_restore", len(before_restore["messages"]))
    assert restore_outputs, "restoration must use the built-in tool, not a model-authored bash reconstruction"
    receipts = [json.loads(output) if isinstance(output, str) else output for output in restore_outputs]
    receipt = next(item for item in receipts if item.get("restored", {}).get("id") == version["id"])
    safety_id = receipt["safety_version_id"]
    assert safety_id and safety_id != version["id"]
    safety = deployed.json("GET", "/v1/space/workspace-versions/" + safety_id)
    assert safety["session_id"] == session["id"]
    assert safety["sha256"] != version["sha256"], "dirty pre-restore bytes must be preserved as a separate version"
    readbacks = named_tool_outputs(restored, "bash", len(before_restore["messages"]))
    assert any(marker in str(output) and changed_marker not in str(output) for output in readbacks)
    assert deployed.json("GET", path) == version, "restoration must not rewrite its immutable source version"
    assert deployed.request("GET", path + "/content")[2] == archive

    target = deployed.json("POST", f"/v1/agents/{agent['id']}/sessions", {}, expected=201)
    sessions.append(target["id"])
    turn(f"Call workspace_versions. Then call workspace_restore with version_id {safety_id}. "
         "Then run bash exactly `cat acceptance/version.txt`. "
         f"After reading it, call workspace_restore with version_id {version['id']} and run the same cat command again. "
         "Do not write any files; finish after both reads.", target["id"])
    across_sessions = deployed.json("GET", "/v1/sessions/" + target["id"])
    assert named_tool_outputs(across_sessions, "workspace_versions")
    assert named_tool_outputs(across_sessions, "workspace_restore")
    assert any(marker in str(output) for output in named_tool_outputs(across_sessions, "bash"))
    assert any(changed_marker in str(output) for output in named_tool_outputs(across_sessions, "bash")), \
        "the safety version must actually retain the overwritten dirty file, not only metadata"

    token = os.getenv("MAGENTS_TEST_OTHER_API_KEY")
    if token:
        other = Http(deployed.origin, token)
        other.json("GET", "/v1/space")
        assert other.request("GET", path)[0] == 404
        assert other.request("GET", path + "/content")[0] == 404
        assert version["id"] not in {item["id"] for item in version_pages(other)}
