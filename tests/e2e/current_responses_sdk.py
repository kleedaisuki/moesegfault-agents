"""Current SDK validation; live mode never creates a model run or compaction."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import sys
from typing import Any

import httpx2
from httpx2 import Client
import openai
from openai import OpenAI
from openai.types.beta.beta_responses_server_event import BetaResponsesServerEvent
from openai.types.conversations.conversation import Conversation
from openai.types.conversations.conversation_item_list import ConversationItemList
from openai.types.responses import Response, ResponseStreamEvent
from openai.types.responses.compacted_response import CompactedResponse
from openai.types.responses.input_token_count_response import InputTokenCountResponse
from pydantic import TypeAdapter
from paid_route_policy import assert_direct_deepseek_pin

STAGING: str = "https://agents-staging.moesegfault.dev"
ROOT: Path = Path(__file__).resolve().parents[2]
TERMINALS: set[str] = {"response.completed", "response.failed", "response.incomplete"}
RESPONSE_EVENT: TypeAdapter[Any] = TypeAdapter(ResponseStreamEvent)
WEBSOCKET_EVENT: TypeAdapter[Any] = TypeAdapter(BetaResponsesServerEvent)
MAX_WIRE_BYTES: int = 16 * 1024 * 1024
TextKey = tuple[str, int, int]


def require(value: object, code: str) -> None:
    if not value:
        raise ValueError(code)


def unique_fields(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    fields: dict[str, Any] = {}
    for key, value in pairs:
        require(key not in fields, "duplicate_wire_field")
        fields[key] = value
    return fields


def reject_constant(value: str) -> None:
    raise ValueError("invalid_json_constant")


def read_wire(filename: Path) -> dict[str, Any]:
    if str(filename) == "-":
        raw: bytes = sys.stdin.buffer.read(MAX_WIRE_BYTES + 1)
    else:
        with filename.open("rb") as handle:
            raw = handle.read(MAX_WIRE_BYTES + 1)
    require(len(raw) <= MAX_WIRE_BYTES, "wire_bundle_too_large")
    bundle: Any = json.loads(raw.decode("utf-8"), object_pairs_hook=unique_fields, parse_constant=reject_constant)
    require(isinstance(bundle, dict), "wire_bundle_must_be_object")
    return bundle


def event_lane(event: dict[str, Any], websocket: bool) -> str | None:
    if not websocket:
        require("stream_id" not in event, "http_event_has_lane")
        return None
    if "stream_id" not in event:
        return None
    lane: Any = event["stream_id"]
    require(isinstance(lane, str) and re.fullmatch(r"[A-Za-z0-9_.-]{1,256}", lane), "invalid_websocket_lane")
    return lane


def text_key(event: dict[str, Any]) -> TextKey:
    item_id: str = event["item_id"]
    output_index: int = event["output_index"]
    content_index: int = event["content_index"]
    require(item_id and output_index >= 0 and content_index >= 0, "invalid_stream_text_identity")
    return (item_id, output_index, content_index)


def terminal_text(response: dict[str, Any]) -> dict[TextKey, str]:
    parts: dict[TextKey, str] = {}
    for output_index, item in enumerate(response["output"]):
        if item["type"] != "message":
            continue
        for content_index, part in enumerate(item["content"]):
            if part["type"] == "output_text":
                parts[(item["id"], output_index, content_index)] = part["text"]
    return parts


def validate_run(events: list[dict[str, Any]]) -> dict[str, Any]:
    created: dict[str, Any] = events[0]["response"]
    require(created.get("status") in {"queued", "in_progress"}, "created_response_not_pending")
    sequences: list[int] = [event["sequence_number"] for event in events]
    require(all(type(sequence) is int and sequence >= 0 for sequence in sequences), "invalid_response_sequence")
    require(sequences == sorted(set(sequences)), "response_sequence_not_strict")
    require(events[-1]["type"] == "response.completed", "response_not_completed")
    response: dict[str, Any] = events[-1]["response"]
    require(response.get("status") == "completed", "terminal_status_mismatch")
    deltas: dict[TextKey, list[str]] = {}
    done: dict[TextKey, str] = {}
    for event in events:
        if "response" in event:
            require(event["response"]["id"] == created["id"], "response_identity_changed")
            require(event["response"]["model"] == created["model"], "response_model_changed")
        if "response_id" in event:
            require(event["response_id"] == created["id"], "response_identity_changed")
        if event["type"] == "response.output_text.delta":
            key: TextKey = text_key(event)
            require(key not in done, "text_delta_after_done")
            deltas.setdefault(key, []).append(event["delta"])
        elif event["type"] == "response.output_text.done":
            key = text_key(event)
            require(key not in done, "duplicate_text_done")
            done[key] = event["text"]
    actual: dict[TextKey, str] = {key: "".join(chunks) for key, chunks in deltas.items()}
    expected: dict[TextKey, str] = terminal_text(response)
    require(all(key in expected and value == expected[key] for key, value in actual.items()), "streamed_text_differs_from_terminal")
    require(all(actual.get(key, "") == value for key, value in expected.items()), "terminal_text_missing_from_stream")
    require(all(key in expected and value == expected[key] for key, value in done.items()), "text_done_differs_from_terminal")
    return response


def validate_events(events: list[dict[str, Any]], websocket: bool) -> tuple[list[dict[str, Any]], int]:
    require(isinstance(events, list) and events, "missing_native_events")
    active: dict[str | None, list[dict[str, Any]]] = {}
    identities: set[str] = set()
    lanes: set[str | None] = set()
    completed: list[dict[str, Any]] = []
    adapter: TypeAdapter[Any] = WEBSOCKET_EVENT if websocket else RESPONSE_EVENT
    for event in events:
        require(isinstance(event, dict), "invalid_native_event")
        adapter.validate_python(event, strict=True)
        lane: str | None = event_lane(event, websocket)
        lanes.add(lane)
        if event["type"] == "response.created":
            require(lane not in active, "overlapping_lane_responses")
            response_id: str = event["response"]["id"]
            require(response_id not in identities, "reused_response_identity")
            identities.add(response_id)
            active[lane] = []
        require(lane in active, "native_event_without_active_response")
        active[lane].append(event)
        if event["type"] in TERMINALS:
            completed.append(validate_run(active.pop(lane)))
    require(not active, "native_response_missing_terminal")
    require(completed, "missing_authentic_terminal")
    return completed, len(lanes)


def validate_wire(bundle: dict[str, Any]) -> dict[str, Any]:
    """Validate original wire objects, never filling defaults or synthesizing events."""
    require(isinstance(bundle, dict), "wire_bundle_must_be_object")
    response: Response = Response.model_validate(bundle["response"], strict=True)
    require(response.status == "completed", "response_not_completed")
    events: list[dict[str, Any]] = bundle["events"]
    http_responses, _ = validate_events(events, websocket=False)
    require(len(http_responses) == 1 and http_responses[0] == bundle["response"], "terminal_snapshot_differs")
    websocket_events: list[dict[str, Any]] = bundle["websocket_events"]
    websocket_responses, websocket_lanes = validate_events(websocket_events, websocket=True)
    compact: CompactedResponse = CompactedResponse.model_validate(bundle["compact"], strict=True)
    require(any(item.type == "compaction" for item in compact.output), "missing_compaction_item")
    counted: InputTokenCountResponse = InputTokenCountResponse.model_validate(bundle["count"], strict=True)
    require(counted.input_tokens >= 0, "negative_input_count")
    conversation: Conversation = Conversation.model_validate(bundle["conversation"], strict=True)
    items: ConversationItemList = ConversationItemList.model_validate(bundle["conversation_items"], strict=True)
    require(items.data, "missing_conversation_items")
    return {"sdk": openai.__version__, "scope": "strict_current_sdk_wire", "response_events": len(events),
            "websocket_events": len(websocket_events), "conversation_items": len(items.data),
            "websocket_lanes": websocket_lanes, "websocket_responses": len(websocket_responses),
            "conversation_object": conversation.object, "compact_object": compact.object,
            "count_object": counted.object, "passed": True, "live_execution_proven": False}


class HttpApi:
    def __init__(self, client: httpx2.Client, key: str) -> None:
        self.client: httpx2.Client = client
        self.key: str = key

    def json(self, method: str, path: str, data: Any = None, **kwargs: Any) -> Any:
        authenticated: bool = kwargs.pop("authenticated", True)
        expected: int = kwargs.pop("expected", 200)
        headers: dict[str, str] = {"Authorization": f"Bearer {self.key}"} if authenticated else {}
        response: httpx2.Response = self.client.request(method, path, json=data, headers=headers, **kwargs)
        require(response.status_code == expected, "preflight_http_status")
        return response.json()


def inspect_staging(source: str, session_id: str, response_id: str, agent_id: str, receipt_path: Path | None = None) -> dict[str, Any]:
    """Reuse an accepted paid trajectory through non-generative SDK operations."""
    recovery: dict[str, Any] = {"source": source, "session": session_id, "response": response_id,
                                "agent": agent_id, "phase": "preflight", "passed": False}
    if receipt_path is not None:
        require(receipt_path.resolve().is_relative_to(ROOT / ".temp"), "receipt_outside_project_temp")
        require(not receipt_path.exists(), "existing_receipt_requires_reconciliation")

    def save(phase: str) -> None:
        recovery["phase"] = phase
        if receipt_path is not None:
            receipt_path.parent.mkdir(parents=True, exist_ok=True)
            temporary: Path = receipt_path.with_suffix(receipt_path.suffix + ".partial")
            temporary.write_text(json.dumps(recovery, sort_keys=True), encoding="utf-8")
            temporary.replace(receipt_path)

    save("preflight")
    require(re.fullmatch(r"[0-9a-f]{40}", source), "exact_source_required")
    require(os.environ.get("MAGENTS_TEST_EXPECTED_BUILD", source) == source, "expected_source_conflict")
    os.environ["MAGENTS_TEST_EXPECTED_BUILD"] = source
    key: str = os.environ.get("MAGENTS_TEST_API_KEY", "")
    require(key, "existing_staging_key_required")
    require(all(re.fullmatch(r"[A-Za-z0-9_-]+", value) for value in (session_id, response_id, agent_id)), "invalid_owned_handle")
    with Client(base_url=STAGING, timeout=60) as http:
        health: httpx2.Response = http.get("/healthz")
        health.raise_for_status()
        require(health.json().get("build_id") == source, "serving_source_mismatch")
        raw_session: httpx2.Response = http.get(f"/v1/sessions/{session_id}", headers={"Authorization": f"Bearer {key}"})
        raw_session.raise_for_status()
        session: dict[str, Any] = raw_session.json()
        require(session.get("agent_id") == agent_id, "session_agent_mismatch")
        assert_direct_deepseek_pin(HttpApi(http, key), session)
        pin: dict[str, Any] = session["pinned"]["model"]
    with OpenAI(api_key=key, base_url=STAGING + "/v1", max_retries=0, timeout=60,
                _strict_response_validation=True) as client:
        raw: Any = client.responses.with_raw_response.retrieve(response_id)
        response: Response = raw.parse()
        require(response.status == "completed" and response.model == agent_id, "owned_completed_response_required")
        require(raw.headers.get("x-agents-session") == session_id, "response_session_mismatch")
        initial: list[Any] = list(client.responses.input_items.list(response_id, limit=2))
        require(initial, "response_inputs_missing")
        events: list[Any] = list(client.responses.retrieve(response_id, stream=True))
        require(events and events[-1].type == "response.completed", "canonical_replay_terminal_missing")
        require(events[-1].response.model_dump(mode="json", by_alias=True) == response.model_dump(mode="json", by_alias=True), "canonical_replay_snapshot_differs")
        count: InputTokenCountResponse = client.responses.input_tokens.count(model=agent_id, previous_response_id=response_id, input="Count only; do not generate.")
        require(count.input_tokens > 0, "invalid_count_result")
        save("conversation_create_submitting_do_not_replay_if_ambiguous")
        conversation: Conversation = client.conversations.create(metadata={"purpose": "current-sdk-no-inference"})
        recovery["conversation"] = conversation.id
        save("conversation_created")
        try:
            created: ConversationItemList = client.conversations.items.create(conversation.id, items=[{"type": "message", "role": "user", "content": "Current SDK non-generative conversation item."}])
            require(len(created.data) == 1, "conversation_create_count")
            item_id: str = created.data[0].id
            listed: list[Any] = list(client.conversations.items.list(conversation.id, limit=1))
            require([item.id for item in listed] == [item_id], "conversation_list_mismatch")
            require(client.conversations.items.retrieve(item_id, conversation_id=conversation.id).id == item_id, "conversation_retrieve_mismatch")
            client.conversations.update(conversation.id, metadata={"purpose": "current-sdk-updated"})
            require(client.conversations.retrieve(conversation.id).metadata == {"purpose": "current-sdk-updated"}, "conversation_metadata_mismatch")
            client.conversations.items.delete(item_id, conversation_id=conversation.id)
            require(not list(client.conversations.items.list(conversation.id)), "conversation_item_delete_failed")
        finally:
            deleted: Any = client.conversations.delete(conversation.id)
            require(deleted.deleted is True, "conversation_delete_failed")
            save("conversation_deleted")
    recovery["passed"] = True
    save("completed")
    return {"sdk": openai.__version__, "source": source, "scope": "current_sdk_non_generative_staging",
            "pin_provider": pin["provider"], "pin_model": pin["model"], "endpoint_proof": "exact_serving_source_static_provider_mapping",
            "response_inputs": len(initial), "canonical_events": len(events), "count": count.input_tokens,
            "conversation_crud": True, "new_model_runs": 0, "passed": True,
            "full_acceptance": False, "remaining": ["paid_create_owned_by_main_runner", "websocket_lanes", "compact_execution", "hosted_tools"]}


def main() -> None:
    parser: argparse.ArgumentParser = argparse.ArgumentParser()
    commands: Any = parser.add_subparsers(dest="mode", required=True)
    wire: argparse.ArgumentParser = commands.add_parser("wire")
    wire.add_argument("--input", type=Path, required=True)
    stage: argparse.ArgumentParser = commands.add_parser("staging-inspect")
    stage.add_argument("--receipt", type=Path, required=True)
    for flag in ("source", "session", "response", "agent"):
        stage.add_argument("--" + flag, required=True)
    args: argparse.Namespace = parser.parse_args()
    if args.mode == "wire":
        result: dict[str, Any] = validate_wire(read_wire(args.input))
    else:
        result = inspect_staging(args.source, args.session, args.response, args.agent, args.receipt)
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        # SDK exceptions and validation details can contain private response bodies.
        print(json.dumps({"passed": False, "error_type": type(error).__name__}))
        raise SystemExit(1) from None

