"""Offline admission-policy checks; these are not paid provider acceptance."""
from __future__ import annotations

import copy
import json
import subprocess
from typing import Any
from unittest.mock import Mock

import pytest

from conftest import Http
import paid_route_policy as policy

BUILD: str = "a" * 40
MODEL: dict[str, Any] = {"provider": "deepseek", "model": "deepseek-flash", "context_window": 1000000, "max_output_tokens": 8192}
SOURCE: str = '''let (credential_protocol, base_url, api_key) = match model.provider {
Provider::DeepSeek => (
    ProviderProtocol::Openai,
    "https://api.deepseek.com".into(),
    env.secret("DEEPSEEK_API_KEY")?.to_string(),
),
Provider::Byok'''


@pytest.fixture
def configured(monkeypatch: pytest.MonkeyPatch) -> Mock:
    monkeypatch.setenv("MAGENTS_TEST_EXPECTED_BUILD", BUILD)
    monkeypatch.delenv("MAGENTS_TEST_DIRECT_MODEL", raising=False)
    source = Mock(return_value=subprocess.CompletedProcess([], 0, stdout=SOURCE, stderr=""))
    monkeypatch.setattr(policy.subprocess, "run", source)
    policy.source_provider_origin.cache_clear()
    return source


class Api:
    def __init__(self) -> None:
        self.health: dict[str, Any] = {"status": "ok", "build_id": BUILD}
        self.model: dict[str, Any] = copy.deepcopy(MODEL)
        self.session: dict[str, Any] = {"id": "pin-only", "pinned": {"model": copy.deepcopy(MODEL)}}
        self.calls: list[tuple[str, str]] = []

    def json(self, method: str, path: str, data: Any = None, **kwargs: Any) -> Any:
        self.calls.append((method, path))
        assert method == "GET", "route policy must not dispatch model work"
        if path == "/healthz":
            return self.health
        if path == "/v1/models":
            return {"available_platform_models": [self.model]}
        assert path == "/v1/sessions/pin-only"
        return self.session


def test_exact_source_origin_and_effective_pin_before_admission(configured: Mock) -> None:
    api = Api()
    policy.assert_direct_deepseek_pin(api, api.session)
    assert api.calls == [("GET", "/healthz"), ("GET", "/v1/models"), ("GET", "/v1/sessions/pin-only")]
    assert configured.call_args.args[0] == ["git", "show", f"{BUILD}:crates/agents-worker/src/provider_config.rs"]
    assert configured.call_args.kwargs["cwd"] == policy.ROOT


@pytest.mark.parametrize("origin", ["https://openrouter.ai/api/v1", "https://api.deepseek.com.attacker.invalid", "https://api.deepseek.com/v1"])
def test_source_origin_is_not_inferred_from_provider_label(configured: Mock, origin: str) -> None:
    configured.return_value.stdout = SOURCE.replace("https://api.deepseek.com", origin)
    with pytest.raises(AssertionError, match="fixed official origin"):
        policy.direct_deepseek_model(Api())


@pytest.mark.parametrize("selector", ["model.protocol", "untrusted.provider"])
def test_source_origin_selector_must_be_the_pinned_model_provider(configured: Mock, selector: str) -> None:
    configured.return_value.stdout = SOURCE.replace("model.provider", selector)
    with pytest.raises(AssertionError, match="model.provider"):
        policy.direct_deepseek_model(Api())


@pytest.mark.parametrize("protocol", [None, "openai", "responses"])
def test_openai_family_transport_is_explicitly_allowed(protocol: str | None) -> None:
    policy.assert_direct_model({**MODEL, "protocol": protocol})


@pytest.mark.parametrize("protocol", ["anthropic", "realtime", ""])
def test_registry_transport_is_rejected_before_session_execution(configured: Mock, protocol: str) -> None:
    api = Api()
    api.model["protocol"] = protocol
    api.session["pinned"]["model"]["protocol"] = protocol
    with pytest.raises(AssertionError, match="compatible OpenAI-family transport"):
        policy.assert_direct_deepseek_pin(api, api.session)
    assert ("GET", "/v1/sessions/pin-only") not in api.calls


@pytest.mark.parametrize("override", ["credential_id", "base_url", "baseURL", "endpoint", "origin", "url"])
def test_pin_route_override_is_rejected_before_execution(configured: Mock, override: str) -> None:
    api = Api()
    api.session["pinned"]["model"][override] = "unverified"
    with pytest.raises(AssertionError):
        policy.assert_direct_deepseek_pin(api, api.session)


@pytest.mark.parametrize("field,value", [("provider", "openrouter"), ("provider", "byok"), ("model", "unverified-flash")])
def test_registry_cannot_silently_substitute_routes(configured: Mock, field: str, value: str) -> None:
    api = Api()
    api.model[field] = value
    with pytest.raises(AssertionError):
        policy.direct_deepseek_model(api)


def test_missing_exact_build_never_queries_live_route(configured: Mock, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("MAGENTS_TEST_EXPECTED_BUILD")
    api = Api()
    with pytest.raises(AssertionError, match="EXPECTED_BUILD"):
        policy.direct_deepseek_model(api)
    assert not api.calls


def test_serving_source_mismatch_does_not_read_registry(configured: Mock) -> None:
    api = Api()
    api.health["build_id"] = "b" * 40
    with pytest.raises(AssertionError, match="serving build"):
        policy.direct_deepseek_model(api)
    assert api.calls == [("GET", "/healthz")]


def test_canonical_pin_mismatch_rejected(configured: Mock) -> None:
    api = Api()
    supplied = copy.deepcopy(api.session)
    api.session["pinned"]["instructions"] = "different pin"
    with pytest.raises(AssertionError, match="immutable session pin"):
        policy.assert_direct_deepseek_pin(api, supplied)


def test_actual_http_session_creation_applies_guard(configured: Mock, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("MAGENTS_TEST_INFERENCE", "1")
    api = Api()
    http = Http("https://fixture.invalid", "fixture-only")

    def request(method: str, path: str, data: Any = None, **kwargs: Any) -> tuple[int, dict[str, str], bytes]:
        if method == "POST":
            assert path == "/v1/agents/agent-fixture/sessions"
            return 201, {}, json.dumps(api.session).encode()
        return 200, {}, json.dumps(api.json(method, path, data, **kwargs)).encode()

    monkeypatch.setattr(http, "request", request)
    assert http.json("POST", "/v1/agents/agent-fixture/sessions", {}, expected=201) == api.session
    assert all(method == "GET" for method, _ in api.calls)
