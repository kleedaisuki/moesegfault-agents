# Architecture and invariants

## Scope

`agents.moesegfault.dev` is the OpenAI-compatible API and management control plane.
`adash.moesegfault.dev` is the React dashboard; `magents` is the Rust CLI. Users
authenticate through moeSegFault Identity; API clients receive revocable platform
keys. The service defaults to Free entitlements and uses the existing Billing
service for subscription state, rather than implementing another payment system.

The system is Rust at the Worker, Durable Object (DO), workspace daemon, and CLI
boundaries. TypeScript is limited to the dashboard and deployment/test tooling.
The domain crate has no Cloudflare dependency, so invariants can be tested cheaply
without a network or a container.

```text
OpenAI SDK ── platform API key ─┐
dashboard / CLI ── Identity ───┼─ Rust edge Worker
                              │   ├─ D1: owners, configs, credentials, usage ledger
                              │   ├─ Billing: subscriptions and entitlements
                              │   └─ Session DO (one owner + session)
                              │         ├─ persisted journal, pinned context, inbox
                              │         ├─ provider HTTP: OpenRouter / DeepSeek / BYOK
                              │         ├─ harness tools: time
                              │         └─ on-demand Container: Rust daemon → bash
                              │                   └─ packed checkpoint → R2
                              └─ Cloudflare logs / traces (no user payloads)
```

## State ownership

| State | Authority | Lifetime / recovery |
|---|---|---|
| Account identity | Identity subject (`sub`) | Never derive ownership from email or request body |
| Subscription | moeSegFault Billing | Cache only bounded, validated entitlement snapshots |
| Agent configuration | D1 owner-scoped row and revision log | Instruction/tool edits reach existing sessions at safe boundaries |
| Session and execution journal | Session DO storage | Survives isolate eviction and reconnect |
| Creation snapshot | Historical `PinnedContext` in session | Reference provenance; not a live prompt/tool invariant |
| Effective instructions/tool catalog and active layout | Session DO, sealed at Response boundaries | Versioned live context; retained through compact/cold recovery |
| Steering input | DO sequenced inbox | Durable before acceptance; consumed at model/tool boundaries |
| Workspace | Container while live; committed R2 snapshot while released | Explicit checkpoint before planned release |
| Billable consumption | Append-only D1 usage ledger | Unique event IDs; never derived from sampled telemetry |
| Observability | Cloudflare telemetry | Bounded retention and sampled independently of billing |

IDs are opaque. An incoming request must resolve the authenticated owner before
routing; a DO name or R2 object key alone is not an authorization grant. Public
API credentials, provider credentials, and dashboard sessions are separate
credential classes. Provider credentials are encrypted at rest and never sent to
workspace subprocesses.

## Protocol-routed credentials and portable agents

BYOK is a wire-protocol choice, not a list of upstream vendors. New credentials
select `openai` (Chat Completions) or `anthropic` (Messages), a public HTTPS base
URL and an API key. Opaque owner-scoped credential IDs allow several endpoints
using the same protocol. The credential owns protocol and endpoint; an agent
references its credential and model, never embeds the key. Legacy provider-key
routes and already pinned model records remain readable during migration.
Platform-funded model routes retain their price and capability registry checks.

The protocol adapter normalizes model messages, tool calls and reported usage for
the existing durable harness. It must not execute incomplete streamed tool input
or drop original journal messages. Anthropic tool results and cache token counts
are protocol concerns, not new session states. Private network bindings and
platform credentials are not available to user-selected provider endpoints;
redirects never forward authorization. Workers' native global-fetch egress
boundary complements strict URL validation, rather than a separate DNS check
that cannot pin the subsequent connection.

Agent publication means creating or replacing an agent in the authenticated
account. Versioned TOML contains editable configuration, not server identity,
revision, owner metadata or credentials. Existing JSON APIs remain the authority
for validation, quotas and pinning. The CLI's offline validation and safe local
file writes improve the workflow without creating a second server data model.

Exports are reads of durable user data. Transcript exports use the original
append-only journal, not only the compacted prompt. Workspace downloads use the
committed private R2 checkpoint and do not start a container, run Bash, advance
the input clock or silently close a running task. Individual file selection
belongs in the native CLI's bounded archive reader; streaming the existing
tar.zst avoids decompressing whole workspaces in a memory-constrained Worker.

References: [Cloudflare binding/egress boundary](https://blog.cloudflare.com/workers-environment-live-object-bindings/),
[Anthropic streaming](https://platform.claude.com/docs/en/build-with-claude/streaming),
and [TOML specification](https://toml.io/en/v1.0.0).

## Session lifecycle and steering

The user-confirmed inactivity rule is precise:

* `last_input_at_ms` advances only when a new user message or steering append is
  accepted. Provider tokens, tool output, heartbeats, and polling do not advance it.
* The idle deadline is `last_input_at_ms + 600_000`.
* An active long-running task is **not** terminated because that deadline passed.
  It may checkpoint and rearm its alarm without changing the input clock.
* At run completion, if no new input has arrived for ten minutes, checkpoint,
  archive conversation state, and release the container immediately.
* An idle session closes at its deadline. A closed session remains readable but
  does not silently accept new input. Explicit resume readmits it under current
  quotas, restores history/workspace, catches up compatible configuration, and resets the idle period as user activity;
  alternatively create a new session.

```text
              accepted input + admission
  Idle ──────────────────────────────────→ Running
   │                                         │
   │ idle deadline                           │ complete
   │                                         ↓
   └─ checkpoint + revision check ──────── Idle / immediate archive
                      ↓
                    Closed
```

Only one model/tool loop owns a session at a time. Steering appends to the inbox,
not a second loop. A fresh append invalidates a pending idle close through the
`input_revision` fence: capture revision, perform snapshot I/O, then recheck the
revision and status before publishing closure. Do not hold a DO concurrency gate
across provider, container, or R2 I/O. DO event interleaving still occurs across
awaits; single-threaded execution is not a substitute for a state transition.

Alarms are at-least-once. Schedule the earliest actual deadline rather than a
one-second polling loop. Each handler re-reads persisted state and is idempotent.
Long execution must be split into durable continuation steps; an in-memory future
or a response stream is not a crash-recovery mechanism. Never blindly retry a
possibly executed `bash` command after an ambiguous transport failure: retain the
tool-call ID and report an indeterminate outcome unless the daemon can prove its
execution state. Exactly-once external side effects are not promised.

## Context, caching, and compaction

The server registry resolves tools and personalized skills at session creation.
The session stores a resolved creation snapshot for historical provenance. Its
effective Agent instructions and registered tool catalog are separate, versioned
state: accepted edits apply at safe execution boundaries, not by freezing the
snapshot or trusting a textual marker. Active layout is sealed with historical
Response boundaries. Model, physical workspace authority and selected skills
retain their creation scope unless a separate explicit lifecycle changes them.

```text
minimal platform policy + fixed real dispatch schema
    + evolving Agent instruction guidance
    + ordinary versioned tool catalog, skills and runtime references
    + derived summary and recent conversation, actual calls/results, and steering
```

Never inject current time, trace IDs, current credits, or mutable account data into
the prefix. Time is a harness tool result in the conversation tail. Forward an
owner/session-derived provider cache-routing key outside the prompt. Provider
caching is an optimization, not an availability or pricing guarantee; report
actual cached-input usage rather than assuming a hit from a matching local hash.

`CompactionPercent` is a JSON integer in `[1,99]`, default 80. The user chooses a
percentage, not a model-specific token threshold. Model context limits come from
server-validated model metadata. Trigger at the earlier of the selected context
percentage or the input budget after reserving the output allowance. Include
tools, instructions, summaries, pending tool exchanges, and safety reserve in token
accounting. If the required active context itself cannot fit, reject instead of endlessly
compacting. Keep a complete source journal; a model summary is a derived view and
must not overwrite the sole copy of user conversation. Compact only complete
tool-call/result exchanges, retain the latest effective guidance/catalog, and make the summary visible
in provenance through its source sequence range and digest. The original pin is
not required to remain the live rendered prefix. The native registered dispatcher
does not replace caller-local tool ownership or confer new workspace permissions;
see [dynamic context](dynamic-tool-context.md) for wire and validation boundaries.

The research rationale is [Prompt Cache (MLSys 2024)](https://arxiv.org/abs/2311.04934):
attention-state reuse benefits repeated prompt segments, but implementing its
inference-server mechanism is outside an API-based harness. Here the directly
useful decision is deterministic prefix construction and measured cache reuse.
[KVFlow](https://arxiv.org/abs/2507.07400) similarly motivates workflow-aware cache
reuse; we do not introduce GPU cache scheduling into a hosted-provider client.

## Workspace checkpoints

Start a container only when a workspace tool is invoked. Harness-only sessions do
not pay a container startup cost. A running container is not necessarily ready:
wait for daemon readiness and a verified restore before admitting `bash`.

The first implementation packs a checkpoint into one bounded `tar.zst` archive
with a SHA-256 manifest, rather than making an R2 request for every file. Limits
are 512 MiB logical data and 256 MiB compressed archive. This fits common source
workspaces without designing a distributed filesystem. Exclude ephemeral caches
and credentials using explicit policy, not hidden accidental omissions. Preserve
normal file modes, deletions, empty directories, and safe relative symlinks; reject
archive traversal and device entries during restore.

Publication order:

1. Quiesce workspace mutations at a tool boundary and create a complete snapshot.
2. Upload the immutable content-addressed archive to an owner-scoped R2 key.
3. Verify checksum/metadata; atomically publish the owner/session R2 current
   manifest, then update the DO's metadata cache.
4. Only then acknowledge a planned release and stop the container. Restore uses
   the R2 current manifest as the authority, not the DO metadata cache.

An upload failure retains the previous committed generation and must not be
reported as a successful durable close. A crash before current-manifest publication leaves
an unreferenced object, never a half-published workspace. Garbage collection uses
reachable generations plus a grace period, not age alone. Restore verifies the
archive in a private staging generation and atomically publishes its `CURRENT`
pointer. Read through the R2 binding, not a potentially stale public CDN URL.

Full archives are an explicit initial performance trade-off. Measure packing time,
uploaded bytes, changed-byte ratio, restore time, and R2 operations before adopting
incremental chunks. If multi-hundred-MiB workspaces dominate, evolve the manifest
to content-addressed packs with bounded compaction; avoid per-file object storms.
Unexpected VM loss can lose changes since the last committed checkpoint. Planned
release is stronger than crash recovery; periodic safe-boundary checkpoints bound
that recovery point without pretending there is a persistent VM disk.

## Admission, quotas, and charging

The subscription catalog has four tiers. These are per-owner product limits, not
deployment-wide container capacity or security boundaries.

| Plan | Configurations | Live sessions | Active runs |
|---|---:|---:|---:|
| Free | 2 | 2 | 1 |
| Go | 10 | 5 | 2 |
| Plus | 30 | 15 | 5 |
| Pro | 200 | 100 | 20 |

The independently configured deployment container ceiling can deny admission even
when an account has unused entitlement capacity. Do not collapse an unavailable
container slot into a subscription upgrade requirement.
Configuration creation and run/session admission must be atomic per owner. Do not
`SELECT count` and then independently `INSERT`. Admission uses an owner-scoped
serialized authority or a transaction with unique reservation rows. Reservations
have idempotency keys, expiry, and a reconciliation path after crashes. BYOK does
not bypass infrastructure quotas or infrastructure charging.

Store money as integer micro-USD, not binary floating point. Reserve a bounded
maximum before starting work and reconcile against actual provider usage. Account
for cached input separately from uncached input and retain the effective price
revision, provider/model, BYOK flag, and upstream usage provenance. BYOK model
consumption is reported but is not billed again by this service. Infrastructure
usage remains chargeable: container CPU/memory/disk time, R2 storage and operations,
D1 reads/writes, and DO compute/storage/requests as covered by the deployed tariff.

The charge ledger is append-only with a unique event ID. Retrying a usage publish
cannot debit twice. Never charge merely because a sampled log says a request
happened, nor price every request at uncached input rates. If upstream cost or a
resource meter is unavailable, preserve an unreconciled record rather than
inventing a final debit. Subscription grants policy; it does not imply unlimited
unmetered consumption.

## Data classes and observability

| Class | Examples | Policy |
|---|---|---|
| User data | Configs, conversation, workspace snapshots | Owner authorization, export/delete semantics, no TTL-only deletion |
| Recoverable intermediate state | Inbox receipts, pending reservations, unreferenced archives | State-aware retention + grace period; cleanup only when unreferenced |
| Financial/provenance records | Usage events, rate revisions, execution IDs | Durable audit retention; separate from sampled traces |
| Telemetry | Spans, timings, error codes, queue depth, cache ratios | Bounded retention, redacted, sampled; never authoritative for charging |
| Opt-in diagnostic uploads | CLI version/OS/arch and sanitized failure metadata | Explicit consent per upload; no prompt/workspace/secret collection by default |

Enable Cloudflare Workers automatic tracing and propagate W3C `traceparent` across
edge → DO → daemon → upstream provider where supported. Instrument logical spans
for admission, registry resolution, provider call, tool execution, checkpoint
upload/commit, compaction, and usage reconciliation. Include request/run/tool IDs,
input revision, config revision, prefix digest, snapshot generation, usage event
ID, status, and byte/token counts. Do not log bearer tokens, BYOK values, command
text, tool output, prompts, or workspace paths. Use structured safe error codes.
Cross-request runs need stable run IDs and span links, not invented parentage.

The minimum useful operational views are: end-to-end success and latency; active
run leases; steering queue delay; provider cached-input fraction; checkpoint and
restore p95/bytes/failures; uncommitted snapshot age; unreconciled charges; rejected
admissions; and authorization failures. Keep telemetry sampling independent of
unsampled financial event persistence. See deployment configuration and rollout
checks for the subset currently wired into the platform.

## Implementation and migration order

1. Shared pure domain contracts and property tests; identity, owner-scoped storage,
   migrations, and model registry validation.
2. OpenAI SDK request journey through one session DO and a harness time tool.
3. Steering, durable continuations, inactivity alarms, and reconnect semantics.
4. Container startup, bash, checkpoint publication, verified restore, and failures.
5. Actual usage settlement, quotas under concurrent requests, dashboard, and CLI.
6. Live Cloudflare smoke tests and release gates before claiming production-ready.

Version archive formats and persisted session schemas. Deploy readers that accept
both old and new versions before enabling new writers. Never reuse or delete a DO
class migration tag. Preserve old snapshot readers until retained objects have
migrated; rollback can point at an older committed snapshot but must not rewrite
financial events. New API fields should be optional for old clients; error on
unsupported mutating request options rather than silently ignoring behavior.

CI should target changed crates and cache Cargo/npm builds; do not rebuild all
platforms on documentation changes. The important release tests are actual user
journeys: login → configure → SDK call → steer → tool → checkpoint → release →
restore → usage, plus interrupted uploads, concurrent quota admission, duplicate
delivery, cross-owner access, and a run longer than ten minutes. Domain properties
test percentage arithmetic, stable prefixes, monotonic input clocks, and revision
fencing. Mocks alone do not prove Cloudflare Container binding support.

## Primary engineering references

* [Cloudflare DO rules](https://developers.cloudflare.com/durable-objects/best-practices/rules-of-durable-objects/): model the coordination atom, persist state, avoid external I/O inside concurrency gates.
* [DO alarms](https://developers.cloudflare.com/durable-objects/api/alarms/): at-least-once retries and one scheduled alarm per object.
* [Direct Container API](https://developers.cloudflare.com/containers/api/durable-object-container/): use platform lifecycle controls; start is not readiness.
* [R2 consistency](https://developers.cloudflare.com/r2/reference/consistency/): strong object operations do not make multi-object publication transactional.
* [DeepSeek context caching](https://api-docs.deepseek.com/guides/kv_cache/) and [OpenRouter caching](https://openrouter.ai/docs/guides/best-practices/prompt-caching): provider-specific cache accounting and sticky routing.
* [OpenRouter default-model metadata](https://openrouter.ai/deepseek/deepseek-v4.1-flash-1017) and [DeepSeek model metadata](https://api-docs.deepseek.com/quick_start/pricing/): the deployed default uses the configured OpenRouter credential and snapshot route; direct DeepSeek is explicit. Provider budgets must be maintained, not trusted from clients.
* [Workers tracing](https://developers.cloudflare.com/observability/traces/workers-tracing/) and [OTel export](https://developers.cloudflare.com/workers/observability/opentelemetry-export/): use platform capture and optional external export rather than an extra home-grown telemetry store.

References checked 2026-10-23. Runtime bindings and tariff data still require a
deployment against the actual account; documentation is not evidence of an
end-to-end deployed service.
