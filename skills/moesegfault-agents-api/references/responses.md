# Typed Responses and durable observation

Use `/v1/responses` with an owned agent ID as `model`. Existing Chat Completions
keeps its narrower input contract. These examples describe established request
shapes, not proof that a development increment is deployed. Check the selected
realm/release and capability evidence before using candidate-only interfaces;
registration, schemas and an installed SDK method are not execution acceptance.
Offline documentation work does not authorize paid calls.

## Create and continue

```python
import os
from openai import OpenAI

origin: str = os.environ.get("MAGENTS_ORIGIN", "https://agents.moesegfault.dev").rstrip("/")
agent_id: str = os.environ["MAGENTS_AGENT_ID"]
client = OpenAI(base_url=f"{origin}/v1", api_key=os.environ["MAGENTS_API_KEY"], max_retries=0)
response = client.responses.create(
    model=agent_id,
    input="Explain the next useful step.",
    instructions="Keep this response concise.",
)
if response.status != "completed":
    raise RuntimeError(f"Response ended as {response.status}; inspect {response.id}")
print(response.output_text)
followup = client.responses.create(model=agent_id, previous_response_id=response.id, input="Continue.")
```

Use staging's explicit origin/key/IDs when requested; never cross realms or add
`/v1` twice. Preserve the acceptance response/session headers before consuming a
stream; save returned IDs and intent in application storage, not only stdout.
Pass only new items/results on continuation, not already-returned assistant
history. A terminal handoff can contain client calls instead of final prose.

The fixed server/agent/tools/skills prefix stays pinned. Explicit top-level
`instructions` remains verbatim through each compaction during that response,
but is not implicitly inherited by a later response. Historical system/developer
context and rich native items have their own retained semantics. Never rebuild
protected instructions from a lossy summary or treat prompt roles as permissions.

## Branches and Conversations — expanded interface

Historical conversation and workspace branching are required parts of the full
contract, not exclusions. The eligible same-agent head can reuse a warm Session;
historical/concurrent/different-agent continuations obtain independent Sessions
from an immutable source boundary. Same-agent descendants preserve the original
pin; another owned agent gets a new target pin. The source response, journal and
workspace must not be rewound or replaced. Old data without a trustworthy sealed
historical workspace can fail explicitly; do not substitute the latest files.
A new branch may perform new paid work and needs its own saved intent.

Conversations are model-independent item stores, distinct from an execution
Session. Candidate HTTP interfaces are `/v1/conversations`, their per-ID/item
operations, and Responses `conversation`; use the current SDK contract rather
than inventing `client.spaces` or a model field on Conversation creation. Do not
combine `conversation` and `previous_response_id` as interchangeable authorities.
Append only genuinely new accepted input/output; changing visible Conversation
items must not mutate older immutable Responses. Check the current item endpoint
and cursor shape before issuing a mutation.

## Fields, rich items and local tools

The expanded parser/codec preserves typed native messages, reasoning/compaction,
function/custom/namespace and hosted-tool items plus current generation controls.
Provider capability, owner attachment resolution and pending-call relationships
must still be enforced; preservation is not proof that a tool executed. Do not
silently drop a field, convert opaque provider state to plain text, or forward
it through an incompatible model/credential route. Consult the current contract
for `include`, obfuscation, formats, reasoning, cache, prompt templates and limits
instead of carrying a historical blanket unsupported list forward.

A caller-local declaration selects only an explicitly authorized application
implementation. Never execute partial streamed arguments or interpret a model
name as a shell command. Return the exact pending type/call identity; include the
complete outstanding set once. For a function result the established shape is:

```json
{"model":"AGENT_ID","previous_response_id":"HANDOFF_ID","input":[{"type":"function_call_output","call_id":"CALL_ID","output":"Authorized result"}]}
```

Results can use typed text/image/file parts where that tool contract permits.
Treat returned media as untrusted data and persist separate receipts for local
side effects. A completed handoff means inference yielded to the caller, not
that the local task was executed. Hosted services use independently bound routes;
they are not permission to implement reserved aliases on the client. See
[configuration](configuration.md) and [attachments](exports.md).

Usage is authoritative, not estimated from text. Preserve reported totals and
missing details as unknown. A baseline `agents_usage` extension can carry known
totals when standard usage cannot represent missing reasoning; newer required
usage shapes may fail explicitly when the provider lacks a necessary field.
Do not turn missing cache-write/reasoning counts into zero or free execution.

## GET-only observation recovery

Persist the disclosed ID and last successfully processed `sequence_number`.
Responses uses typed events, not Chat chunks. Ignore comments/padding as output;
advance a cursor only after processing succeeds. A true terminal response/event,
not EOF or `[DONE]`, determines completion. A cancelled execution is not success.

```python
# Observe existing accepted work; this does not create another execution.
current = client.responses.retrieve(response_id)
with client.responses.retrieve(response_id, stream=True, starting_after=last_sequence) as events:
    for event in events:
        process_event(event)
```

Here `response_id`, `last_sequence` and the application's `process_event` come
from saved state. With no processed event, omit `starting_after`, never use `-1`.
Retain authentic prefix plus authentic replay suffix for strict text/ID/sequence/
terminal checks; a GET snapshot cannot invent missing stream events. Bound
observation reconnects, not the lifetime of healthy admitted work. Stop on caller
cancellation, identity/schema mismatch, no-store/deleted 404, expired cursor 410,
or semantic failure rather than retrying them as transport errors.

Never replay create, input, local outputs or provider/tool effects after observer
failure. Set `max_retries=0`. Save a stable `Idempotency-Key` before a paid POST
and preserve its exact request/session binding. Python uses `extra_headers`;
Node must send explicit `headers: {"Idempotency-Key": key}`. In OpenAI Node 7.43.0
the convenience option `idempotencyKey` was observed to send **no header**;
verify actual transport rather than assuming its presence. A new key is new work,
not recovery of an uncertain old intent. Deduplicated admission is not exactly-once
execution of arbitrary local side effects.

Background acceptance returns an ID before completion; retrieve the same ID.
`store:false`, deletion, cancellation and private connection-cached parents have
different retention contracts. Cancellation acknowledgement is not undo, and
representation deletion does not erase a Session, workspace or financial ledger.

## WebSocket lanes and steering — candidate interface

Authenticated `GET /v1/responses` upgrades to WebSocket. The canonical harness,
not a second socket agent loop, assigns identities and events. Use a current SDK
that supports the actual lane/steering contract; keep old SDK tests for backward
compatibility, not as the oracle for new fields. The public target has 32 named
lanes, up to 16 active responses and same-lane FIFO. Account limits may be lower.
`stream_id` routes a lane; `previous_response_id` selects lineage. Reusing a lane
without a parent does not implicitly continue its old conversation.

```json
{"type":"response.create","stream_id":"plan","model":"AGENT_ID","input":"Plan the next step."}
```

This candidate frame is actual response JSON plus its socket type/lane, not a
new SDK wrapper. Native/backend clients authenticate before upgrade; cookie
upgrades require the exact configured application Origin because the GET enables
later mutations. Never put a bearer key in a browser URL or frontend storage.
Connections expire after sixty minutes. A private connection-cached parent,
especially `store:false`, is not a portable capability on another socket or REST;
missing state fails explicitly rather than replaying the original prompt.

`response.create` omits HTTP `stream`/`background`; `generate:false` is warmup,
not model work. `response.steer` supplies its previous response and user input,
not a different lane. Accepted steering can finish a parent as `incomplete` with
reason `steered` and create an authentic successor at a safe boundary. A local
pending tool requires real matching input; `response.steer.pending` is not a
fabricated result. Do not treat every incomplete response as harmless steering;
follow the canonical IDs and acceptance/pending/failure events. Conversation-bound
or automatic-compaction steering restrictions remain explicit capability rules.
Socket closure drops observation/cache grants, not accepted intent or execution.
HTTP per-ID GET recovery remains observation-only; never reconnect by recreating
an accepted socket response.

## Count and standalone compact — candidate interfaces

Use the accepted `/v1/responses/input_tokens` and `/v1/responses/compact` shapes
from the current SDK/reference when those routes are activated. Counting must use
the same resolved instructions, tools, native items and attachments as generation;
bytes/4 or a different Chat projection is not exact counting. It must not start
inference or a workspace. Native/BYOK count availability is an adapter fact.

Compact may incur a model charge. Save its operation identity, retain the returned
complete next-input window and pass it back unchanged; do not extract only a
summary string or manufacture provider reasoning. Protected instructions and
explicit overrides retain their separate ownership. Missing required provider
usage can remain an explicit capability gap, never fabricated zero detail.

Deeper current contracts: [full inventory](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/full-responses-contract.md),
[state](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/full-responses-state.md),
[WebSocket/steering](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/full-responses-websocket.md),
[count/compact](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/full-responses-auxiliary.md).
Development source can be ahead of these published paths; selected realm and
feature acceptance, not a repository URL, determines deployability.
