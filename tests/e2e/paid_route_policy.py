"""Pre-admission policy for billed acceptance; not a product routing policy."""
from __future__ import annotations

from functools import lru_cache
import os
from pathlib import Path
import re
import subprocess
from typing import Any, Protocol

ROOT: Path = Path(__file__).resolve().parents[2]
DIRECT_MODELS: frozenset[str] = frozenset({"deepseek-flash", "deepseek-v4-pro"})
MODEL_FIELDS: frozenset[str] = frozenset({"provider", "model", "context_window", "max_output_tokens", "credential_id", "protocol"})


class AcceptanceApi(Protocol):
    def json(self, method: str, path: str, data: Any = None, **kwargs: Any) -> Any: ...


@lru_cache(maxsize=4)
def source_provider_origin(build: str) -> str:
    assert re.fullmatch(r"[0-9a-f]{40}", build), "an exact serving build is required"
    source = subprocess.run(
        ["git", "show", f"{build}:crates/agents-worker/src/provider_config.rs"],
        cwd=ROOT, capture_output=True, text=True, check=True, timeout=15,
    ).stdout
    selector = re.search(
        r"let\s*\(credential_protocol,\s*base_url,\s*api_key\)\s*=\s*match\s+model\.provider\s*\{",
        source,
    )
    assert selector is not None, "the exact candidate must select its origin from model.provider"
    match = re.search(r"Provider::DeepSeek\s*=>\s*\((.*?)\),\s*Provider::Byok", source[selector.end():], re.S)
    assert match is not None, "the exact candidate's DeepSeek resolver must be reviewed"
    assert re.fullmatch(
        r'\s*ProviderProtocol::Openai,\s*"https://api\.deepseek\.com"\.into\(\),\s*'
        r'env\.secret\("DEEPSEEK_API_KEY"\)\?\.to_string\(\),\s*', match.group(1),
    ), "the exact candidate must resolve direct DeepSeek to its fixed official origin"
    return "https://api.deepseek.com"


def verify_serving_origin(api: AcceptanceApi) -> str:
    expected = os.environ.get("MAGENTS_TEST_EXPECTED_BUILD", "")
    assert re.fullmatch(r"[0-9a-f]{40}", expected), "paid acceptance requires MAGENTS_TEST_EXPECTED_BUILD"
    health = api.json("GET", "/healthz", authenticated=False)
    assert health.get("status") == "ok" and health.get("build_id") == expected, "serving build differs from the accepted source"
    return source_provider_origin(expected)


def direct_deepseek_model(api: AcceptanceApi) -> dict[str, Any]:
    verify_serving_origin(api)
    selected = os.environ.get("MAGENTS_TEST_DIRECT_MODEL", "deepseek-flash")
    assert selected in DIRECT_MODELS, "paid acceptance permits only explicitly selected direct DeepSeek Flash/Pro"
    registry = api.json("GET", "/v1/models")
    available = registry.get("available_platform_models")
    assert isinstance(available, list), "effective platform registry is required before paid acceptance"
    matches = [model for model in available if model.get("provider") == "deepseek" and model.get("model") == selected]
    assert len(matches) == 1, "selected direct DeepSeek model must be uniquely provisioned and priced"
    model = matches[0]
    assert_direct_model(model)
    return dict(model)


def assert_direct_model(model: dict[str, Any]) -> None:
    assert model.get("provider") == "deepseek", "paid LLM acceptance must not use OpenRouter or unverified BYOK"
    assert model.get("model") in DIRECT_MODELS, "unknown DeepSeek model is not an acceptance fallback"
    assert model.get("protocol") in (None, "openai", "responses"), "paid DeepSeek acceptance requires a compatible OpenAI-family transport"
    assert not model.get("credential_id"), "paid platform acceptance must not override credential routing"
    assert not any(key.lower().replace("_", "") in {"baseurl", "endpoint", "origin", "url"} for key in model), "paid platform acceptance must not override the provider origin"
    assert model.keys() <= MODEL_FIELDS, "paid platform acceptance contains an unreviewed model field"


def assert_direct_deepseek_pin(api: AcceptanceApi, session: dict[str, Any]) -> None:
    expected = direct_deepseek_model(api)
    session_id = session.get("id")
    assert isinstance(session_id, str) and session_id, "a canonical pin-only session is required"
    current = api.json("GET", "/v1/sessions/" + session_id)
    pin = current.get("pinned")
    assert isinstance(pin, dict) and pin == session.get("pinned"), "immutable session pin must match canonical state"
    model = pin.get("model")
    assert isinstance(model, dict), "session model pin is required"
    assert_direct_model(model)
    assert (model["provider"], model["model"]) == (expected["provider"], expected["model"]), "session must pin the selected effective platform route"
    for field in ("context_window", "max_output_tokens"):
        selected = model.get(field)
        available = expected.get(field)
        assert (type(selected) is int and type(available) is int
                and 0 < selected <= available), "session model bounds must fit the effective platform registry"
