"""Validate actual Worker observations against the pinned OpenAI SDK schemas."""

import json
import sys
from typing import Any

from pydantic import TypeAdapter, ValidationError
from openai.types.responses import ResponseStreamEvent

from response_checks import checked_response


def validate(observed: dict[str, Any]) -> dict[str, int | bool]:
    assert set(observed) == {"responses", "events"}
    assert isinstance(observed["responses"], list) and isinstance(observed["events"], list)
    assert observed["responses"] or observed["events"], "validation requires actual wire observations"
    for value in observed["responses"]:
        checked_response(value)
    schema: TypeAdapter[ResponseStreamEvent] = TypeAdapter(ResponseStreamEvent)
    for value in observed["events"]:
        event = schema.validate_python(value)
        if event.type in {"response.created", "response.in_progress", "response.completed", "response.failed", "response.incomplete"}:
            checked_response(event.response)
    return {"valid": True, "responses": len(observed["responses"]), "events": len(observed["events"])}


def main() -> int:
    try:
        result = validate(json.load(sys.stdin))
    except ValidationError as error:
        print(json.dumps({"valid": False, "error": "ValidationError", "violations": [
            {"path": list(detail["loc"]), "type": detail["type"]}
            for detail in error.errors(include_input=False, include_url=False)
        ]}))
        return 1
    except (AssertionError, KeyError, TypeError, ValueError) as error:
        # The subprocess is also usable for opt-in private acceptance. Do not
        # print payloads, exception renderings or Pydantic's copied input values.
        print(json.dumps({"valid": False, "error": type(error).__name__}))
        return 1
    print(json.dumps(result))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
