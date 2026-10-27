"""Validate actual Worker observations against the pinned OpenAI SDK schemas."""

import json
import sys
import traceback
from pathlib import Path
from typing import Any

from pydantic import TypeAdapter, ValidationError
from openai.types.responses import ResponseStreamEvent

from response_checks import checked_response


class ObservationFailure(Exception):
    def __init__(self, path: tuple[str | int, ...], cause: Exception) -> None:
        super().__init__()
        self.path = path
        self.cause = cause


def failure_details(error: Exception) -> dict[str, Any]:
    details: dict[str, Any] = {"valid": False, "error": type(error).__name__}
    if isinstance(error, ObservationFailure):
        details["observation_path"] = list(error.path)
        error = error.cause
        details["error"] = type(error).__name__
    if isinstance(error, ValidationError):
        details["violations"] = [
            {"path": list(detail["loc"]), "type": detail["type"]}
            for detail in error.errors(include_input=False, include_url=False)
        ]
    else:
        # Static repository source identifies the failed invariant without
        # rendering exception arguments, payload values or local absolute paths.
        frames = [frame for frame in traceback.extract_tb(error.__traceback__)
                  if Path(frame.filename).name in {"response_checks.py", "validate_responses_wire.py"}]
        if frames:
            frame = frames[-1]
            details["check"] = {
                "file": Path(frame.filename).name,
                "line": frame.lineno,
                "source": frame.line,
            }
    return details


def validate(observed: dict[str, Any]) -> dict[str, int | bool]:
    assert set(observed) == {"responses", "events"}
    assert isinstance(observed["responses"], list) and isinstance(observed["events"], list)
    assert observed["responses"] or observed["events"], "validation requires actual wire observations"
    for index, value in enumerate(observed["responses"]):
        try:
            checked_response(value)
        except (ValidationError, AssertionError, KeyError, TypeError, ValueError) as error:
            raise ObservationFailure(("responses", index), error) from error
    schema: TypeAdapter[ResponseStreamEvent] = TypeAdapter(ResponseStreamEvent)
    for index, value in enumerate(observed["events"]):
        try:
            event = schema.validate_python(value)
        except (ValidationError, AssertionError, KeyError, TypeError, ValueError) as error:
            raise ObservationFailure(("events", index), error) from error
        if event.type in {"response.created", "response.in_progress", "response.completed", "response.failed", "response.incomplete"}:
            try:
                checked_response(event.response)
            except (ValidationError, AssertionError, KeyError, TypeError, ValueError) as error:
                raise ObservationFailure(("events", index, "response"), error) from error
    return {"valid": True, "responses": len(observed["responses"]), "events": len(observed["events"])}


def main() -> int:
    try:
        result = validate(json.load(sys.stdin))
    except (ObservationFailure, ValidationError, AssertionError, KeyError, TypeError, ValueError) as error:
        # The subprocess is also usable for opt-in private acceptance. Do not
        # print payloads, exception renderings or Pydantic's copied input values.
        print(json.dumps(failure_details(error)))
        return 1
    print(json.dumps(result))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
