"""Offline operator probe tests; no real provider request is permitted."""
from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import sys
from typing import Any

import pytest

ROOT: Path = Path(__file__).resolve().parents[2]
SPEC: Any = importlib.util.spec_from_file_location("deepseek_usage_probe", ROOT / "scripts/deepseek-usage-probe.py")
assert SPEC is not None and SPEC.loader is not None
probe: Any = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(probe)
SOURCE: str = "a" * 40


def test_one_request_has_bounded_budget_no_tools_and_no_stored_content(tmp_path: Path) -> None:
    calls: list[dict[str, Any]] = []

    def transport(raw: bytes, key: str) -> tuple[int, bytes]:
        body: dict[str, Any] = json.loads(raw)
        calls.append(body)
        assert key == "fixture-private-key"
        assert body["max_output_tokens"] == 64 and body["reasoning"] == {"effort": "none"}
        assert "tools" not in body
        return 200, json.dumps({"model": "deepseek-flash", "output": "PRIVATE_REPLY_SENTINEL", "usage": {
            "input_tokens": 8, "output_tokens": 2, "total_tokens": 10,
            "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0},
        }}).encode()

    path: Path = tmp_path / "attempt.json"
    result: dict[str, Any] = probe.probe_once(path, SOURCE, "123", "fixture-private-key", transport)
    assert len(calls) == 1 and result["status"] == "provider_usage_observed"
    assert result["usage"]["reported_integers"]["usage.input_tokens_details.cache_write_tokens"] == 0
    assert result["usage"]["cache_write_state"] == "reported_integer"
    text: str = path.read_text()
    assert "PRIVATE_REPLY_SENTINEL" not in text and "fixture-private-key" not in text and "Reply OK." not in text
    with pytest.raises(FileExistsError):
        probe.probe_once(path, SOURCE, "123", "fixture-private-key", transport)
    assert len(calls) == 1


def test_missing_cache_write_is_not_zero() -> None:
    value: dict[str, Any] = probe.usage_evidence({"input_tokens": 8, "input_tokens_details": {"cached_tokens": 0}})
    assert value["cache_write_state"] == "not_reported_as_integer"
    assert "usage.input_tokens_details.cache_write_tokens" not in value["reported_integers"]


def test_unknown_outcome_is_recorded_without_retry_or_raw_error(tmp_path: Path) -> None:
    count: int = 0

    def transport(raw: bytes, key: str) -> tuple[int, bytes]:
        nonlocal count
        count += 1
        raise TimeoutError("PRIVATE_ERROR_SENTINEL")

    path: Path = tmp_path / "attempt.json"
    result: dict[str, Any] = probe.probe_once(path, SOURCE, "123", "fixture", transport)
    assert count == 1 and result["status"] == "outcome_unresolved_do_not_retry"
    assert "usage" not in result and "PRIVATE_ERROR_SENTINEL" not in path.read_text()


@pytest.mark.parametrize("status", [302, 400, 401, 429, 500, 503])
def test_http_failure_does_not_persist_raw_body_or_create_user_ledger(tmp_path: Path, status: int) -> None:
    def transport(raw: bytes, key: str) -> tuple[int, bytes]:
        return status, b"PRIVATE_ERROR_SENTINEL"

    path: Path = tmp_path / "attempt.json"
    result: dict[str, Any] = probe.probe_once(path, SOURCE, "123", "fixture", transport)
    assert result["status"] == "upstream_non_success_do_not_retry"
    assert result["http_status"] == status and not result["user_ledger_written"]
    assert "PRIVATE_ERROR_SENTINEL" not in path.read_text()


def test_redirect_handler_never_forwards_bearer_to_another_origin() -> None:
    assert probe.NoRedirect().redirect_request(None, None, 302, "", None, "https://other.invalid") is None


@pytest.mark.parametrize("body", [
    b'{"usage":{"input_tokens_details":{"cache_write_tokens":4,"cache_write_tokens":0}}}',
    b'{"usage":{"input_tokens":NaN}}',
])
def test_ambiguous_vendor_usage_is_not_promoted_to_authentic_zero(tmp_path: Path, body: bytes) -> None:
    def transport(raw: bytes, key: str) -> tuple[int, bytes]:
        return 200, body

    result: dict[str, Any] = probe.probe_once(tmp_path / "attempt.json", SOURCE, "123", "fixture", transport)
    assert result["status"] == "outcome_unresolved_do_not_retry" and "usage" not in result


def test_atomic_receipt_retry_never_repeats_provider_request(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    requests: int = 0
    replacements: int = 0
    replace: Any = Path.replace

    def transport(raw: bytes, key: str) -> tuple[int, bytes]:
        nonlocal requests
        requests += 1
        return 200, b'{"usage":{"input_tokens":8}}'

    def transient_replace(self: Path, target: Path) -> Path:
        nonlocal replacements
        replacements += 1
        if replacements < 3:
            raise PermissionError("fixture-only sharing violation")
        return replace(self, target)

    def no_sleep(seconds: float) -> None:
        return None

    monkeypatch.setattr(Path, "replace", transient_replace)
    monkeypatch.setattr(probe.time, "sleep", no_sleep)
    result: dict[str, Any] = probe.probe_once(tmp_path / "attempt.json", SOURCE, "123", "fixture", transport)
    assert result["status"] == "provider_usage_observed" and requests == 1 and replacements == 3


@pytest.mark.parametrize("environment,attempt", [("production", "1"), ("staging", "2")])
def test_operator_environment_and_rerun_gates_precede_any_probe(
    monkeypatch: pytest.MonkeyPatch, environment: str, attempt: str,
) -> None:
    called: bool = False

    def forbidden(*args: Any, **kwargs: Any) -> dict[str, Any]:
        nonlocal called
        called = True
        raise AssertionError("probe must not be dispatched")

    monkeypatch.setattr(sys, "argv", ["probe", "--source", SOURCE, "--ci-run", "123", "--operator-cost-confirmed"])
    monkeypatch.setenv("MAGENTS_DEEPSEEK_PROBE_CLEARED_SOURCE", SOURCE)
    monkeypatch.setenv("DEPLOY_ENVIRONMENT", environment)
    monkeypatch.setenv("GITHUB_RUN_ATTEMPT", attempt)
    monkeypatch.setattr(probe, "probe_once", forbidden)
    with pytest.raises(ValueError):
        probe.main()
    assert not called
