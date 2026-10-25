"""Current SDK validation; live mode never creates a model run or compaction."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
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


def require(value: object, code: str) -> None:
    if not value:
        raise ValueError(code)


def validate_wire(bundle: dict[str, Any]) -> dict[str, Any]:
    """Validate all named surfaces strictly; absence is a failure, never a skip."""
    response: Response = Response.model_validate(bundle["response"])
    require(response.status == "completed", "response_not_completed")
    events: list[dict[str, Any]] = bundle["events"]
    require(events, "missing_response_events")
    sequences: list[int] = []
    for raw in events:
        event: Any = RESPONSE_EVENT.validate_python(raw)
        sequences.append(event.sequence_number)
    require(sequences == sorted(set(sequences)), "response_sequence_not_strict")
    require(events[-1]["type"] == "response.completed", "missing_authentic_terminal")
    require(events[-1]["response"] == bundle["response"], "terminal_snapshot_differs")
    websocket_events: list[dict[str, Any]] = bundle["websocket_events"]
    require(websocket_events, "missing_websocket_events")
    for raw in websocket_events:
        WEBSOCKET_EVENT.validate_python(raw)
    require(any(event["type"] in TERMINALS for event in websocket_events), "missing_websocket_terminal")
    compact: CompactedResponse = CompactedResponse.model_validate(bundle["compact"])
    require(any(item.type == "compaction" for item in compact.output), "missing_compaction_item")
    counted: InputTokenCountResponse = InputTokenCountResponse.model_validate(bundle["count"])
    require(counted.input_tokens >= 0, "negative_input_count")
    conversation: Conversation = Conversation.model_validate(bundle["conversation"])
    items: ConversationItemList = ConversationItemList.model_validate(bundle["conversation_items"])
    require(items.data, "missing_conversation_items")
    return {"sdk": openai.__version__, "scope": "strict_current_sdk_wire", "response_events": len(events),
            "websocket_events": len(websocket_events), "conversation_items": len(items.data),
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
        require(args.input.stat().st_size <= 16 * 1024 * 1024, "wire_bundle_too_large")
        result: dict[str, Any] = validate_wire(json.loads(args.input.read_text(encoding="utf-8")))
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

