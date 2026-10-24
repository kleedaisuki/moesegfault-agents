# Client contract and durable sessions

## Request boundary

This reference covers `POST /v1/chat/completions`; for the separate Responses contract read [responses.md](responses.md). Chat accepts only 1–128 nonempty text-only `user` messages; the JSON body limit is 1 MiB. Do not send system/developer/assistant/tool messages, multimodal content, client tool schemas, or arbitrary generation options to Chat. Unsupported options are rejected, not silently ignored. Server-managed tools execute in the harness, not the SDK client.

All HTTP examples below use origin `https://agents.moesegfault.dev` and `Authorization: Bearer <platform-api-key>`. JSON mutations use `Content-Type: application/json`.

For requested staging integration, use origin `https://agents-staging.moesegfault.dev`
and its separately provisioned key/agent/session IDs; replace the example's `origin`
accordingly. Do not silently use production or put `/v1` in both origin and paths.

```http
POST /v1/agents/<agent-id>/sessions

{}
```

The 201 response is the new session record; persist its `id` before submitting work.

```http
POST /v1/chat/completions
X-Agents-Session: <session-id>

{"model":"<agent-id>","messages":[{"role":"user","content":"Inspect the project"}],"stream":true}
```

Responses include `X-Agents-Session`. Omitting it creates a session; for short nonstream calls, `client.chat.completions.with_raw_response.create(...)` exposes it via `raw.headers["x-agents-session"]`, then `raw.parse()` returns the completion. Explicit creation is safer for long work because a failed initial observer may not deliver headers.

## Minimal Python journey

Requires installed `openai` and `httpx` packages, `MAGENTS_API_KEY` and `MAGENTS_AGENT_ID` from an already provisioned account. Run on a trusted backend. This example creates a session and executes a real turn when deliberately run; do not use it as an unapproved live smoke test.

```python
import os
from urllib.parse import quote

import httpx
from openai import OpenAI

origin = "https://agents.moesegfault.dev"
api_key = os.environ["MAGENTS_API_KEY"]
agent_id = os.environ["MAGENTS_AGENT_ID"]
headers = {"Authorization": f"Bearer {api_key}"}

with httpx.Client(base_url=origin, headers=headers, timeout=30.0) as management:
    created = management.post(f"/v1/agents/{quote(agent_id, safe='')}/sessions", json={})
    created.raise_for_status()
    session_id = str(created.json()["id"])
    print(f"Save this session ID: {session_id}", flush=True)
    terminal = False
    try:
        with OpenAI(base_url=f"{origin}/v1", api_key=api_key, max_retries=0) as client:
            with client.chat.completions.with_streaming_response.create(
                model=agent_id,
                messages=[{"role": "user", "content": "Call the time tool and report UTC."}],
                stream=True,
                extra_headers={"X-Agents-Session": session_id},
            ) as response:
                for chunk in response.parse():
                    for choice in chunk.choices:
                        print(choice.delta.content or "", end="", flush=True)
                        terminal = terminal or choice.finish_reason == "stop"
        if not terminal:
            raise RuntimeError("No successful terminal chunk; inspect the saved session")
    finally:
        state = management.get(f"/v1/sessions/{quote(session_id, safe='')}")
        state.raise_for_status()
        print(f"\nSession status: {state.json()['status']}")
```

The `finally` block observes status; it never resubmits input or closes a possibly running task. Persist the ID in application storage rather than relying on console output in production. A failed status read also does not justify replay. There is no implicit close here, leaving the session available for intentional continuation.

## Continue, steer, finish

| Intent | Request |
|---|---|
| Read state and source journal | `GET /v1/sessions/{id}` |
| Next turn while idle | Same completion request/header with **only new user input** |
| Add input during active run | `POST /v1/sessions/{id}/steer` with `{"content":"Prioritize the failing test"}` |
| Independent parallel work | Create another session; never start a second loop in a busy session |
| Request safe closure/checkpoint | `DELETE /v1/sessions/{id}`; inspect readiness before downloading |
| Intentionally reopen closed session | `POST /v1/sessions/{id}/resume`; preserve its existing pin |

Agent edits affect future sessions, not existing pins. Closed sessions cannot be resumed just by appending. User appends and steering reset the ten-minute idle clock; tool/model output does not. An active run may outlast the deadline and close after finishing.

For lightweight management polling, `GET /v1/sessions/{id}/view` omits full history/pin and includes message count, latest response ID, and outstanding local calls. ETag plus `If-None-Match` supports 304. Fetch source records incrementally using `GET /v1/sessions/{id}/messages` with one opaque session-bound `before`/`after` cursor and `limit` 1–100 (default latest 50). Records retain stable source sequences through compaction; pages include `start`, `end`, `high_water`, both cursors, and older/newer flags. Keep the legacy full status endpoint when its complete representation is actually needed.

## Observe without duplicating execution

SSE uses Chat Completions chunks and a final `data: [DONE]`. Ignore `: keepalive` comment frames (sent after 15 quiet seconds); they are not tokens, progress, or success. The SDK ignores comments. Errors may arrive inside an already-open stream; `[DONE]` alone is not a successful run. Require a successful terminal chunk; never invent `[DONE]` after EOF or convert an error into success. A custom parser must distinguish error envelopes, terminal chunks, and incomplete transport.

Set `max_retries=0`. A timeout, disconnect, EOF, or 5xx may follow accepted user input or tool side effects. Preserve partial output and inspect the **original** session's status, journal, and pending input before choosing the next user-visible action. Do not automatically replay the prompt, full history, or side-effecting commands. `Idempotency-Key` is reserved, not a general exactly-once guarantee. Increasing SDK timeout cannot control every intermediary.

Handle 400 by correcting input; 401/403 by fixing authentication/context; 404 without probing another owner's resource; 409 by inspecting busy/closed state; 429 by checking admission/capacity. A 502/503 does not by itself prove execution never began.

Canonical contracts: [service API](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/api.md). SDK mechanics: [official Python SDK reference](https://developers.openai.com/api/reference/python); its general API examples do not expand this service's subset.
