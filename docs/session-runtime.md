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

Run phases journal a provider result *before* billing or executing its tool calls. Tool execution IDs combine run, response round and provider call ID: provider IDs such as `call_0` are not globally unique. Queued steering enters the active prompt only at a safe boundary after pending tool results. Accepted steering during final billing starts a new durable continuation rather than becoming stranded in an idle queue.

The run safety ceiling is 128 model rounds; this is distinct from inactivity. Provider continuation fields, including DeepSeek reasoning and OpenRouter signed reasoning blocks, remain private conversation data and are preserved verbatim.

## Inactivity, archival, and resume

Only accepted user input advances `last_input_at_ms`. Model tokens, tool calls, reads and keepalives do not. A running task continues after ten minutes without new input. When it finishes, it archives immediately if that deadline has passed. Idle sessions archive at the same deadline.

A normal archive checkpoints the workspace, writes an immutable conversation export, registers its retained-byte clock, commits `Closed`, and releases the Container. A persisted `archive_pending` phase makes release retryable after a crash. A successfully closed session can be explicitly resumed using `POST /v1/sessions/{id}/resume`; D1 readmits it under live-session quotas and the DO preserves its exact pinned prefix and workspace. The explicit resume gesture starts a new idle window.

Workspace compute stays warm behind an independent funded lease, not an active-run quota slot. See [runtime-workspaces.md](runtime-workspaces.md) for renewal, snapshot streaming and tool boot fences.

## Prompt-cache and compaction invariants

Instructions, sorted registry definitions and skill contents are resolved once at session creation. No live registry lookup mutates an existing session. OpenRouter receives a stable opaque session identifier for routing/cache affinity. Dynamic user conversation and summaries follow the fixed prefix.

The compaction threshold is a validated percentage of authoritative model context, also bounded by the reserved output budget. Last reported provider token usage is authoritative for the measured prefix. Only newly appended content uses a conservative UTF-8-byte upper bound until the next usage report. The complete source event journal is separate from this compacted prompt.

DO KV values have a 2 MiB bound. Active state and individual large journal events use bounded chunks with index publication last. Journal entries are written only once; compaction does not rewrite or erase source history. A per-object async persistence mutex publishes the latest in-memory state, preventing a steering write from being overwritten by a stale pre-I/O snapshot.

## Financial and external-side-effect uncertainty

Cloudflare alarms are at least once, but generic model APIs do not provide a portable exactly-once inference guarantee. Before dispatch, a durable in-flight fence is committed. If a process is replaced after dispatch but before its result is saved, the platform reports `provider_outcome_unknown` or `compaction_outcome_unknown` and retains the outstanding reservation. It does **not** blindly reissue an inference and hide a second upstream charge behind a deduplicated ledger event.

The Container daemon's execution cache is process-local. The DO persists a boot-associated dispatch fence. An ambiguous command whose daemon has changed is not automatically replayed; the authenticated user/operator must inspect and recover it. This is a deliberate safety boundary, not an exactly-once claim for arbitrary bash/network side effects.

Missing provider usage, missing OpenRouter actual cost, unconfigured tariffs, unobserved resource quantities and storage debt fail closed. OpenRouter cost is parsed from its raw JSON decimal using integer arithmetic (`serde_json/arbitrary_precision`). Direct DeepSeek platform-funded traffic requires an authoritative schedule-aware tariff; BYOK still pays observed platform resources.

## Remaining operational boundary

A checkpoint error never authorizes destruction of unpersisted user files. The owner receives bounded `checkpoint_error` and `checkpoint_pending` diagnostics; global logs receive no paths, commands, content or keys. Archive retries renew the funded compute lease. If credit or archive-format recovery is unavailable, this becomes an explicit operator repair incident constrained by deployment-wide Container capacity, not an invented successful close or silent data deletion. Production rollout must exercise this incident path as well as happy-path write/release/resume.
