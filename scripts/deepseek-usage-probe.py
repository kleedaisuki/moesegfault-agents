"""One authorized operator probe; never a user-ledger or retry mechanism."""
from __future__ import annotations

import argparse
from collections.abc import Callable
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import time
from typing import Any
import urllib.error
import urllib.request

ROOT: Path = Path(__file__).resolve().parents[1]
ORIGIN: str = "https://api.deepseek.com"
MODEL: str = "deepseek-flash"
MAX_REPLY_BYTES: int = 256 * 1024
Transport = Callable[[bytes, str], tuple[int, bytes]]


def unique_fields(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("ambiguous_provider_json")
        result[key] = value
    return result


def reject_constant(value: str) -> None:
    raise ValueError("invalid_provider_json_constant")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req: urllib.request.Request, fp: Any, code: int,
                         msg: str, headers: Any, newurl: str) -> None:
        return None


def native_request(body: bytes, key: str) -> tuple[int, bytes]:
    opener: urllib.request.OpenerDirector = urllib.request.build_opener(NoRedirect())
    request: urllib.request.Request = urllib.request.Request(
        ORIGIN + "/responses", data=body, method="POST",
        headers={"Authorization": "Bearer " + key, "Content-Type": "application/json"},
    )
    try:
        with opener.open(request, timeout=120) as response:
            return response.status, response.read(MAX_REPLY_BYTES + 1)
    except urllib.error.HTTPError as error:
        # Failure bodies may echo request data. They are not probe evidence.
        return error.code, b""


def usage_evidence(value: object) -> dict[str, Any]:
    integers: dict[str, int] = {}
    kinds: dict[str, str] = {}

    def visit(item: object, path: str, depth: int) -> None:
        if depth > 6 or len(kinds) >= 128:
            raise ValueError("usage_shape_exceeds_bound")
        if isinstance(item, dict):
            for key, child in item.items():
                if not isinstance(key, str) or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]{0,63}", key):
                    raise ValueError("invalid_usage_field_name")
                visit(child, path + "." + key, depth + 1)
        elif type(item) is int:
            if item < 0 or item > 2**63 - 1:
                raise ValueError("invalid_usage_integer")
            integers[path] = item
            kinds[path] = "integer"
        else:
            kinds[path] = type(item).__name__

    if not isinstance(value, dict):
        raise ValueError("provider_usage_missing")
    visit(value, "usage", 0)
    cache: str = "usage.input_tokens_details.cache_write_tokens"
    return {"reported_integers": integers, "field_types": kinds,
            "cache_write_state": "reported_integer" if cache in integers else "not_reported_as_integer"}


def save(path: Path, value: dict[str, Any]) -> None:
    partial: Path = path.with_suffix(path.suffix + ".partial")
    with partial.open("x", encoding="utf-8") as handle:
        json.dump(value, handle, sort_keys=True)
    # Windows file scanners can briefly deny delete-sharing during atomic replace.
    for attempt in range(5):
        try:
            partial.replace(path)
            return
        except PermissionError:
            if attempt == 4:
                raise
            time.sleep(0.05)


def probe_once(path: Path, source: str, ci_run: str, key: str,
               transport: Transport = native_request) -> dict[str, Any]:
    if not re.fullmatch(r"[0-9a-f]{40}", source) or not re.fullmatch(r"[1-9][0-9]{0,14}", ci_run):
        raise ValueError("exact_source_and_ci_required")
    if not key:
        raise ValueError("paired_deepseek_secret_required")
    if not path.resolve().is_relative_to(ROOT / ".temp"):
        raise ValueError("attempt_outside_project_temp")
    path.parent.mkdir(parents=True, exist_ok=True)
    evidence: dict[str, Any] = {
        "purpose": "operator_direct_deepseek_usage_capability", "source": source,
        "ci_run": ci_run, "origin": ORIGIN, "model": MODEL,
        "status": "dispatching_do_not_retry", "attempted_at": datetime.now(timezone.utc).isoformat(),
        "operator_cost": True, "user_ledger_written": False, "maximum_submissions": 1,
    }
    with path.open("x", encoding="utf-8") as handle:
        json.dump(evidence, handle, sort_keys=True)
    body: bytes = json.dumps({"model": MODEL, "input": "Reply OK.", "max_output_tokens": 64,
                              "reasoning": {"effort": "none"}, "stream": False}).encode("utf-8")
    try:
        status, raw = transport(body, key)
        evidence["http_status"] = status
        if status != 200:
            evidence["status"] = "upstream_non_success_do_not_retry"
        else:
            if len(raw) > MAX_REPLY_BYTES:
                raise ValueError("provider_reply_too_large")
            reply: Any = json.loads(raw, object_pairs_hook=unique_fields, parse_constant=reject_constant)
            if not isinstance(reply, dict):
                raise ValueError("provider_reply_not_object")
            evidence["usage"] = usage_evidence(reply.get("usage"))
            model: Any = reply.get("model")
            if isinstance(model, str) and re.fullmatch(r"[A-Za-z0-9_.:/-]{1,128}", model):
                evidence["reported_model"] = model
            elif model is not None:
                evidence["reported_model_sha256"] = hashlib.sha256(str(model).encode()).hexdigest()
            evidence["status"] = "provider_usage_observed"
    except Exception as error:
        evidence["status"] = "outcome_unresolved_do_not_retry"
        evidence["error_type"] = type(error).__name__
    evidence["finished_at"] = datetime.now(timezone.utc).isoformat()
    save(path, evidence)
    return evidence


def main() -> None:
    parser: argparse.ArgumentParser = argparse.ArgumentParser()
    parser.add_argument("--source", required=True)
    parser.add_argument("--ci-run", required=True)
    parser.add_argument("--operator-cost-confirmed", action="store_true")
    args: argparse.Namespace = parser.parse_args()
    if not args.operator_cost_confirmed or os.environ.get("MAGENTS_DEEPSEEK_PROBE_CLEARED_SOURCE") != args.source:
        raise ValueError("exact_candidate_probe_clearance_required")
    if os.environ.get("DEPLOY_ENVIRONMENT") != "staging":
        raise ValueError("paired_staging_operator_environment_required")
    if os.environ.get("GITHUB_RUN_ATTEMPT", "1") != "1":
        raise ValueError("actions_rerun_must_not_repeat_operator_probe")
    github_sha: str | None = os.environ.get("GITHUB_SHA")
    if github_sha is not None and github_sha != args.source:
        raise ValueError("probe_source_differs_from_actions_source")
    attempt: Path = ROOT / ".temp/v0.3-full-acceptance/direct-deepseek-capability/attempt.json"
    result: dict[str, Any] = probe_once(attempt, args.source, args.ci_run,
                                      os.environ.get("DEEPSEEK_API_KEY", ""))
    print(json.dumps(result, sort_keys=True))
    if result["status"] != "provider_usage_observed":
        raise SystemExit(1)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(json.dumps({"status": "probe_not_completed", "error_type": type(error).__name__}))
        raise SystemExit(1) from None
