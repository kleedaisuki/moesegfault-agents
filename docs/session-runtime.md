# Session runtime and recovery boundaries

## Ownership

| State | Authority | Lifetime |
| --- | --- | --- |
| Agent configuration, account/plan, quota admission | D1 | User/account data |
| Pinned instructions, full resolved tool/skill versions | Session Durable Object | Immutable for one session, including explicit resume |
| Canonical conversation events | Append-only DO journal entries | User data; never discarded by compaction |
| Active model prompt and compaction summaries | DO state chunks | Rebuildable intermediate state |
| Run phase, pending provider result, tool dispatch identity | DO durable journal | Recovery state; contains user data, not telemetry |
| Workspace archive + manifest and conversation exports | R2 | User data; no blanket intermediate-data expiry |
| Reservations, observed quantities, exact ledger, retained-byte clocks | D1 | Accounting/provenance, immutable events |
| Sanitized trace/log metadata | Cloudflare Observability | Separate operational retention |

The management D1 session row is a projection, not the active conversation owner. A private namespace routes one session to one Rust Durable Object. A session owns at most one lazily started Container.

## Execution and streaming

`POST /v1/chat/completions` uses an agent ID as `model`. `x-agents-session` selects an existing server-owned conversation or returns the new ID. SDK requests append user text; callers cannot replace pinned system/developer instructions, registry schemas or tool results. The dashboard's `POST /v1/sessions/{id}/messages {content}` starts an idle run or steers the existing run.

An initial durable alarm owns background execution, rather than a detached HTTP `waitUntil`. This matters because HTTP post-response lifetime is only 30 seconds. The alarm processes local transitions without scheduler round trips, yields after a five-minute budget, and bounds any one provider operation to five minutes. Thus the next outstanding operation remains below the fifteen-minute alarm wall limit. Provider SSE is parsed incrementally using `eventsource-stream` and forwarded as OpenAI chat-completion chunks. A disconnected subscriber does not own or cancel the durable task.

Cloudflare sources:

- [DO wall limits](https://developers.cloudflare.com/durable-objects/platform/limits/)
- [At-least-once alarms](https://developers.cloudflare.com/durable-objects/api/alarms/)
- [Rust worker SDK](https://docs.rs/worker/0.8.7/worker/)

The Rust SDK's `Storage::set_alarm(i64)` interprets its integer as a **relative offset**, unlike JavaScript's numeric epoch API. All absolute session deadlines use `util::alarm_at(epoch_ms)`, which constructs an explicit JavaScript `Date`. The constructor and regular scheduling share `alarm_deadline`: restored active/preparing work receives a near-term wakeup if its alarm is missing or too late; an already-earlier alarm is preserved. Closed sessions without pending cleanup delete their alarm. This repairs the crash window between committing accepted work and publishing its wakeup without replaying an uncertain external operation.

Run phases journal a provider result *before* billing or executing its tool calls. A successful terminal OpenAI response (including usage), or a terminal error, is committed before asynchronous final settlement. Recovery replays that terminal result rather than dispatching another inference; the worker wall-time endpoint is pinned before settlement for idempotent accounting. Tool execution IDs combine run, response round and provider call ID: provider IDs such as `call_0` are not globally unique. Queued steering enters the active prompt only at a safe boundary after pending tool results. Accepted steering during final billing starts a new durable continuation rather than becoming stranded in an idle queue.

The run safety ceiling is 128 model rounds; this is distinct from inactivity. Provider continuation fields, including DeepSeek reasoning and OpenRouter signed reasoning blocks, remain private conversation data and are preserved verbatim.

## Inactivity, archival, and resume

Only accepted user input advances `last_input_at_ms`. Model tokens, tool calls, reads and keepalives do not. A running task continues after ten minutes without new input. When it finishes, it archives immediately if that deadline has passed. Idle sessions archive at the same deadline.

A normal archive checkpoints the workspace, writes an immutable conversation export, registers its retained-byte clock, commits `Closed`, and releases the Container. A persisted `archive_pending` phase makes release retryable after a crash. A successfully closed session can be explicitly resumed using `POST /v1/sessions/{id}/resume`; D1 readmits it under live-session quotas and the DO preserves its exact pinned prefix and workspace. The explicit resume gesture starts a new idle window.

Physical release has its own durable receipt after checkpoint publication. A successful stop acknowledgement pins the end timestamp; if the stop is confirmed but its acknowledgement was lost, no exit time is invented. The original clock, lease and uncertainty remain in an accounting journal with their budget hold. A saved release outcome is replayed before detaching the old active clock/lease, allowing a later workspace generation without accidentally destroying or charging it as the old one.

Once the archive is committed and the Container is confirmed stopped, financial reconciliation does not keep `archive_pending` true. The session remains closed/resumable, with `billing_pending` and `last_error: resource_reconciliation_pending` visible to its owner. Snapshot failure or an unexpected stopped active clock without a prior checkpoint/release intent remains a data-recovery error, not a fabricated successful archive. Unit tests cover the production state guards and serialized recovery receipt; they do not claim to inject a real Cloudflare Container host failure.

Workspace compute stays warm behind an independent funded lease, not an active-run quota slot. See [runtime-workspaces.md](runtime-workspaces.md) for renewal, snapshot streaming and tool boot fences.

## Prompt-cache and compaction invariants

Instructions, sorted registry definitions and skill contents are resolved once at session creation. No live registry lookup mutates an existing session. OpenRouter receives a stable opaque session identifier for routing/cache affinity. Dynamic user conversation and summaries follow the fixed prefix.

The compaction threshold is a validated percentage of authoritative model context, also bounded by the reserved output budget. Last reported provider token usage is authoritative for the measured prefix. Only newly appended content uses a conservative UTF-8-byte upper bound until the next usage report. The complete source event journal is separate from this compacted prompt.

DO KV values have a 2 MiB bound. Active state and individual large journal events use bounded chunks with index publication last. Journal entries are written only once; compaction does not rewrite or erase source history. A per-object async persistence mutex publishes the latest in-memory state, preventing a steering write from being overwritten by a stale pre-I/O snapshot.

## Financial and external-side-effect uncertainty

Cloudflare alarms are at least once, but generic model APIs do not provide a portable exactly-once inference guarantee. Before dispatch, a durable in-flight fence is committed. If a process is replaced after dispatch but before its result is saved, the platform reports `provider_outcome_unknown` or `compaction_outcome_unknown` and retains the outstanding reservation. It does **not** blindly reissue an inference and hide a second upstream charge behind a deduplicated ledger event.

The Container daemon's execution cache is process-local. The DO persists a boot-associated dispatch fence. An ambiguous command whose daemon has changed is not automatically replayed; the authenticated user/operator must inspect and recover it. This is a deliberate safety boundary, not an exactly-once claim for arbitrary bash/network side effects.

Missing provider usage, missing OpenRouter actual cost, unconfigured tariffs, unobserved resource quantities and storage debt fail closed. An ended execution is distinct from its still-reserved financial hold: `execution_finished_at` removes it from concurrency admission without releasing uncertain money. A known answer is still delivered with `agents_billing_status: reconciliation_pending` if financial measurements need recovery. OpenRouter cost is parsed from its raw JSON decimal using integer arithmetic (`serde_json/arbitrary_precision`). Direct DeepSeek platform-funded traffic requires an authoritative schedule-aware tariff; BYOK still pays observed platform resources.

## Cleanup funding

Before admitting paid work, reserve a session/archive-generation maintenance hold from fixed R2 PUT and bounded D1 row tariffs plus 25% safety, not the much larger run reservation. Its complete tariff snapshot and original allowance vintage survive deployment and monthly-credit rotation. A warm workspace's existing R2 safety budget is preferred for final export; the unused tiny backstop is then released with vintage-safe refund semantics. An empty current wallet must not require a fresh compute lease merely to checkpoint and release an already-funded workspace.

Archive preparation persists the selected funding source before export and before changing the management pointer. A reset resumes that preparation and temporarily rejects new appends, rather than accidentally selecting the next archive generation or needing fresh credit. Pin-only sessions with no event journal/workspace data close using existing DO durability and require no R2 operation or maintenance funding.

## Remaining operational boundary

A checkpoint error never authorizes destruction of unpersisted user files. The owner receives bounded `checkpoint_error` and `checkpoint_pending` diagnostics; global logs receive no paths, commands, content or keys. Archive retries renew the funded compute lease. If credit or archive-format recovery is unavailable, this becomes an explicit operator repair incident constrained by deployment-wide Container capacity, not an invented successful close or silent data deletion. Production rollout must exercise this incident path as well as happy-path write/release/resume.

## Terminal delivery and management projection

The DO is authoritative; D1's session row is a management projection. Finalization
captures an observer owned by its run ID before yielding. It retains the live
terminal run until the completed DO snapshot has committed, then publishes the
known JSON result or SSE stop/`[DONE]`. A failed D1 projection after that boundary
cannot discard the answer or repeat inference, tools, or financial settlement.

Completed snapshots carry backward-compatible projection-pending metadata and a
fresh opaque projection generation. A successful D1 update writes only a small local
acknowledgement for that generation, rather than rewriting the conversation context.
Cold load recognizes only the matching acknowledgement. A stale ack cannot clear
a newer pending generation, even if its public session fields are identical.

All projections and acknowledgements are serialized by the existing persistence
mutex. Alarm repair publishes the current authoritative revision, not a captured
old Idle row. Failed repairs back off from one second to sixty seconds and reset
after acknowledgement. Queued input remains durable and starts through normal
run admission without advancing the user's append clock. A transport disconnect
still does not authorize replay, and channel acceptance is not a network-delivery
guarantee.

## Responses execution and immutable receipts (v0.3)

Responses and Chat Completions share the existing durable execution loop. A
Responses ID is also its financial run identity, but is not a session ID. The DO
publishes each completed response under its own chunked key before clearing the
active run. Retrieval never substitutes a later session answer for an earlier
response. D1 contains owner-scoped routing/idempotency metadata, not the canonical
response body. A warm retry of an accepted active ID republishes state and its
alarm before acknowledgement; it does not append input again.

Only the current response head may continue. Starting an ordinary Chat run
invalidates that head; arbitrary historical or workspace forks are unsupported.
Client function calls finish the response with public `function_call` items and
release inference concurrency. The session retains their exact outstanding call
IDs. Continuation requires one result per call, rejects unknown/duplicate/missing
results before mutation, and creates a new response. Server tools execute inside
the same loop and are not advertised as work the client must execute again.
Fresh manually supplied history must contain complete, chronologically linked
call/result groups; provider call IDs may be reused in different completed groups.

For a valid archived continuation, the DO returns a private pre-admission
`response_session_closed` signal only after validating the current head, pending
results and generation options. The edge then uses existing quota admission and
retries the same response ID once. Every asynchronous preparation boundary is
followed by a fresh check of the canonical status, archive phase, revision and
head. Domain append/start checks happen before any instruction or pending-call
mutation. Rejected stale requests cannot silently extend or consume a session.

Instructions have three distinct lifetimes. The server/agent pin is immutable for
the session. System/developer messages supplied as input are persistent historical
instructions. Top-level `instructions` apply to that response only, as in the
Responses protocol. The active prefix includes all applicable layers and survives
every compaction; constructing the next response removes only the request-local
layer. Canonical source messages and original input items remain separate from the
compacted inference view. File input receipts retain references rather than
duplicating expanded text/image content in the public input-items representation.

Background cancellation is a durable request observed at a safe execution
boundary. A known pending model/compaction result is journaled and financially
recorded first; cancellation does not turn consumed inference into fictitious zero
usage. No further tool is dispatched, and interrupted tool groups receive explicit
linked terminal results so a later turn cannot accidentally execute them. The
canonical response becomes `cancelled`. Since the standard SSE event union has no
`response.cancelled` event, a cancelled observer receives the standard `error`
event with `code: response_cancelled`, then EOF; the client retrieves the same ID.

### Event sequence fencing and bounded replay

Live deltas are buffered with the provider round rather than causing a durable
write per token. Their sequence allocation range is committed **before** upstream
I/O. A cold load skips the old range and durably reserves a fresh range before
exposing the restored object; this also protects a second reset during recovery.
No event can allocate beyond its published range. Finalization renews a durable
range before emitting its terminal event, so range exhaustion cannot strand a run.
An unknown in-flight provider outcome becomes an explicit failure with a later
sequence, not an automatic inference replay or a fabricated successful answer.

Replay retains at most 65,536 events and 16 MiB, evicting an amortized prefix.
An expired replay cursor returns 410 and directs the owner to retrieve the complete
immutable response. Public response output is bounded to 4 MiB before new output
items are emitted. At most eight observers attach; each live stream has a bounded
32-frame queue. A slow observer may disconnect without cancelling execution and
must recover from the original ID. These bounds are protocol/resource policy, not
claims that channel acceptance proves delivery to the remote client.

### Prompt planning is not billing

The text-only working estimator preserves the existing conservative JSON-byte
budget without allocating another serialized prompt. Native image transport bytes
are masked and receive an 8,192-token working reservation per image; base64 size
must not be misclassified as millions of textual model tokens. This reservation is
an approximation, not a tokenizer, a model capability guarantee, or a billable
quantity. Reported upstream usage remains authoritative. Aggregate decoded inline
attachments/images are bounded to 4 MiB to protect the Worker while the inference
view and source journal coexist; larger binary files stream into a funded workspace.

Configured semantic compaction remains independent from `truncation`. After that
policy runs, `auto` may drop whole older interaction groups from the active view
if necessary; `disabled` rejects a view that still exceeds its working budget.
Neither mode drops the pinned instruction prefix, splits a tool group, deletes
source history, or changes financial token counts.
