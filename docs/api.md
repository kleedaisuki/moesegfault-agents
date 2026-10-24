# HTTP API v1

Base URL: `https://agents.moesegfault.dev`. Management endpoints and the
OpenAI-compatible subset share owner-scoped authentication. JSON request bodies
are limited to 1 MiB. Error responses use
`{"error":{"message":"…","type":"…","code":"…"}}` and meaningful HTTP status
codes. API responses are private and `Cache-Control: no-store`.

This document distinguishes the implemented subset from the complete upstream
OpenAI API: the platform runs its own tools and maintains session history. It is
not a transparent proxy accepting arbitrary OpenAI request fields.

## Authentication

* SDK: `Authorization: Bearer <platform-api-key>`.
* CLI: moeSegFault native-client OIDC authorization code + PKCE, tokens stored in
  the operating-system credential store, not a plaintext config file.
* Browser: BFF-managed secure HttpOnly cookie; mutations require `X-CSRF-Token`.
  Only the configured dashboard origin receives credentialed CORS permission.

The platform key is not an upstream OpenRouter/DeepSeek key. Create/revoke platform
keys through the account API or dashboard. A BYOK credential is stored separately
and never returned as plaintext after creation.

## OpenAI SDK journey

```python
import os
from openai import OpenAI

client = OpenAI(
    base_url="https://agents.moesegfault.dev/v1",
    api_key=os.environ["MAGENTS_API_KEY"],
)
agent_id = "<id from agents create or dashboard>"
raw = client.chat.completions.with_raw_response.create(
    model=agent_id,
    messages=[{"role": "user", "content": "What time is it?"}],
)
session_id = raw.headers["x-agents-session"]
print(raw.parse().choices[0].message.content)

# Append only NEW user input. The service already owns previous history.
reply = client.chat.completions.create(
    model=agent_id,
    messages=[{"role": "user", "content": "Now summarize that in UTC."}],
    extra_headers={"X-Agents-Session": session_id},
)
print(reply.choices[0].message.content)
```

`GET /v1/models` returns the caller's agent IDs as OpenAI model records. Its
`available_models` extension lists server-registered upstream model configurations
for management clients, including models usable through BYOK. The additive
`available_platform_models` array has the same `ModelConfig` shape but includes only
registered non-BYOK routes with a valid configured platform price. Management
clients should use it for platform-funded provider/model choices and keep the full
`available_models` for BYOK, filtered by the stored key's provider. Missing or
malformed pricing exposes an empty platform list, not apparently supported models;
invalid individual prices are excluded. Older servers may omit the additive field.
This is configured pricing support, not a promise of upstream health, available
credentials, or funded execution admission. In `POST /v1/chat/completions`, `model` always identifies an
**agent**, not an upstream model.

### Chat Completions

`POST /v1/chat/completions`

```json
{
  "model": "<agent-id>",
  "messages": [{"role": "user", "content": "Inspect the project"}],
  "stream": false
}
```

| Input | Semantics |
|---|---|
| `model` | Existing agent ID owned by caller |
| `messages` | 1–128 new, nonempty, text-only user messages |
| `stream` | Standard Chat Completions chunk stream when true |
| `X-Agents-Session` | Continue this owned session; omitted creates a session |

The response includes `X-Agents-Session` for both JSON and streaming calls. Normal
OpenAI completion/chunk object shapes are used; SSE ends with `data: [DONE]`.
Quiet streaming responses send the [standard SSE comment](https://html.spec.whatwg.org/multipage/server-sent-events.html#authoring-notes) `: keepalive\n\n`
after 15 seconds without a data chunk. SSE parsers, including the OpenAI SDK,
ignore comments; these are not user messages or model tokens. Heartbeats stop
when the event channel closes or `[DONE]` is sent.

For long tasks, create a session with `POST /v1/agents/{id}/sessions` first and
pass its ID in `X-Agents-Session` with `stream: true`. A non-streaming observer can
lose its connection through client or intermediary timeouts even when a durable
run is still executing. Increasing the SDK timeout does not control every proxy.
A disconnected observer does **not** cancel the run: inspect
`GET /v1/sessions/{id}` before deciding what to do next, and do not automatically
replay the completion request or its user messages. Non-streaming responses do
not send whitespace heartbeats, preserving normal JSON and HTTP error statuses.
Harness tool calls and workspace execution are managed on the server. System,
developer, assistant, tool-result, and arbitrary multimodal message injection is
not supported by this subset. Tools, instructions, model budgets, and compaction
policy are pinned from the agent configuration, not overridden per request.
Unsupported generation options are rejected rather than silently changing the
configured behavior. Do not replay the complete conversation on a continuing
session; doing so would append duplicate user input.

A session has one active run. Start a different session for parallel work; use
the steering endpoint to add information during a run. Subscription limits apply
across all of an owner's sessions, including API keys and browser/CLI callers.

## Agent configurations

| Method | Path | Result |
|---|---|---|
| GET | `/v1/agents` | `{"object":"list","data":[AgentConfig,…]}` |
| POST | `/v1/agents` | Create; 201 + `AgentConfig` |
| GET | `/v1/agents/{id}` | `AgentConfig` or 404 |
| PUT | `/v1/agents/{id}` | Replace editable configuration; increment revision |
| DELETE | `/v1/agents/{id}` | Soft-delete configuration; existing session pins remain independent |

Creation/replacement body:

```json
{
  "name": "Research",
  "model": {
    "provider": "openrouter",
    "model": "deepseek/deepseek-v4.1-flash-1017"
  },
  "instructions": "Investigate carefully and present a concise result.",
  "tools": ["time", "bash"],
  "skills": [],
  "compaction_percent": 80,
  "workspace": true
}
```

`provider` is `openrouter`, `deepseek`, or `byok`. The older Rust-style spellings
`open_router` and `deep_seek` are accepted as input aliases.

Model context/output limits are validated and replaced with server registry
metadata; sending larger numbers cannot increase the actual allowance. A BYOK
configuration includes a `credential_id` referencing the caller's encrypted stored
credential. Omit `model` for the default OpenRouter
`deepseek/deepseek-v4.1-flash-1017` route (1,048,576-token context and 8,192-token
default output budget). Direct DeepSeek and BYOK routes remain explicit choices;
they require the corresponding configured credential. `instructions` defaults to
empty; tools to `["time"]`; skills to `[]`; compaction to 80; workspace to false.
Names contain 1–100 bytes; instructions at most 64 KiB. `bash` requires a workspace.
Unknown tools, skills, and unregistered models are rejected.

Returned `AgentConfig` adds `id`, `owner_id`, `revision`, `created_at_ms`, and
`updated_at_ms`. Agent updates do not mutate existing sessions. `compaction_percent`
is an integer from 1 through 99, relative to the selected model context window.
The service can compact earlier to preserve output headroom. Model registry entries
and skill content are deployment-managed in the initial version.

## Sessions and steering

| Method | Path | Body / meaning |
|---|---|---|
| GET | `/v1/sessions` | List caller's sessions |
| GET | `/v1/agents/{id}/sessions` | List caller's sessions for this agent |
| POST | `/v1/agents/{id}/sessions` | Create an idle session with a pinned configuration |
| GET | `/v1/sessions/{id}` | Current session status |
| POST | `/v1/sessions/{id}/resume` | Explicitly readmit an archived session under current plan limits; preserve pinned config and workspace |
| POST | `/v1/sessions/{id}/messages` | `{"messages":[{"role":"user","content":"…"}],"stream":false}`; append and run |
| POST | `/v1/sessions/{id}/steer` | `{"content":"Prioritize the failing test first"}`; durable input for active run |
| DELETE | `/v1/sessions/{id}` | Request safe close and workspace checkpoint |

Steering text is nonempty and at most 64 KiB. It is consumed at a safe model/tool
boundary, not by launching a concurrent loop. An in-flight external command need
not be immediately cancellable. Clients should poll status rather than assuming
that a transport disconnect cancels an accepted operation.

`SessionRecord` contains `id`, `owner_id`, `agent_id`, `status`, `created_at_ms`,
`last_input_at_ms`, `input_revision`, `pinned`, and optional `workspace_snapshot`.
Status values are `idle`, `running`, and `closed`. The pinned record contains the
agent revision, full resolved tool/skill definitions, model configuration,
instructions, compaction percentage, and workspace setting.

Status `messages` is the preserved append-only conversation journal, not the
compacted model prompt. Compaction replaces only the derived active context;
original user messages, assistant messages, tool calls, and tool results remain in
the journal and conversation archive. Pending steering remains separate until a
safe execution boundary consumes it. A summary is not a replacement for the
source conversation or an indication that those messages were deleted.

The idle deadline is ten minutes after the last accepted user append, including
steering. Agent output and tools do not reset the timer. An active task continues
past the deadline; when it completes, a session already idle for ten minutes is
archived and its container released immediately. A new append during checkpoint
invalidates the pending idle close. Closed sessions cannot silently be resumed by
appending; explicitly resume them or create a new session. An explicit resume is
user activity and starts a new ten-minute idle period without changing the pin.

## Account endpoints

The exact account/key/credential wire shapes live with the authentication module.
Canonical management resources are:

| Method | Resource | Purpose |
|---|---|---|
| GET | `/v1/account` | Owner profile, effective plan/limits, credits, CSRF metadata |
| GET/POST | `/v1/api-keys` | List redacted platform keys / create one-time-displayed secret |
| DELETE | `/v1/api-keys/{id}` | Revoke a key |
| GET | `/v1/provider-keys` | List redacted upstream credentials |
| PUT | `/v1/provider-keys/{provider}` | Store encrypted `{"api_key":"…"}`; provider is `openrouter` or `deepseek` |
| DELETE | `/v1/provider-keys/{provider}` | Revoke stored upstream credential |
| POST | `/v1/diagnostics` | Explicitly authorized sanitized diagnostic upload |

`/auth/config` exposes public native-client OIDC metadata. `/auth/login` and
`/auth/callback` implement the dashboard's browser flow; `POST /auth/logout` revokes
its local BFF session. Subscription checkout is delegated to moeSegFault Billing,
not implemented as a payment form in this API.

Usage is integer micro-USD and measured resources, not binary floating-point
credits. Cached-input/model-output/infrastructure charges are separate. BYOK model
usage is visible without charging the same upstream model request twice; container
and persistence resources remain metered. Account budget, usage visibility, and
subscription status must be wired to actual pricing/entitlement configuration
before enabling paid production traffic.

| Plan | Agent configurations | Concurrent sessions | Concurrent runs |
|---|---:|---:|---:|
| Free | 2 | 2 | 1 |
| Go | 10 | 5 | 2 |
| Plus | 30 | 15 | 5 |
| Pro | 200 | 100 | 20 |

Deployment-wide container capacity is separate from account entitlement limits.
A subscription does not bypass a full deployment's resource admission control.

## Client error handling

| Status | Typical meaning | Action |
|---|---|---|
| 400 | Invalid input, unsupported options, unknown registry item | Correct the request; do not retry unchanged |
| 401/403 | Missing credentials, CSRF failure, disallowed origin | Authenticate / verify caller context |
| 404 | Unknown or other-owner resource | Do not infer whether another owner has this ID |
| 409 | Concurrent config update, busy/closed session, incompatible transition | Refresh status; steer or create a new session |
| 413 | Request exceeds body limit | Reduce payload |
| 429 | Plan capacity or usage admission denied | Wait for capacity or upgrade/check allowance |
| 502/503 | Upstream/container/persistence unavailable | Inspect run status before retrying side effects |

Do not automatically retry ambiguous execution submissions without a supported
idempotency contract. `Idempotency-Key` is a reserved public header, not a blanket
exactly-once guarantee for arbitrary bash side effects. The release journey must
test actual SDK JSON and SSE responses, steering under tool execution, session
closure, cross-owner access, and checkpoint/restore against deployed bindings.
